use anyhow::{Context, Result};
use async_trait::async_trait;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, Pod, PodSpec, PodTemplateSpec, Secret, SecretVolumeSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, ListParams, LogParams, PostParams};
use kube::Client;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
const MANAGED_BY_VALUE: &str = "rancher-helm-mcp";

pub struct K8sJobRunner {
    client: Client,
    pub namespace: String,
    helm_image: String,
}

pub struct JobOutput {
    pub phase: &'static str,
    pub succeeded: bool,
    pub logs: Option<String>,
}

#[async_trait]
pub trait HelmJobRunner: Send + Sync {
    fn namespace(&self) -> &str;

    async fn spawn_helm_job(
        &self,
        job_label: &str,
        release_name: &str,
        helm_args: Vec<String>,
        kubeconfig: &str,
        values_json: Option<&str>,
    ) -> Result<String>;

    async fn get_job_output(&self, job_name: &str) -> Result<JobOutput>;

    async fn wait_for_job(&self, job_name: &str, timeout: Duration) -> Result<JobOutput>;
}

impl K8sJobRunner {
    pub async fn try_new(namespace: String, helm_image: String) -> Result<Self> {
        let client = Client::try_default()
            .await
            .context("failed to build in-cluster k8s client; is the pod running with a ServiceAccount?")?;
        Ok(Self { client, namespace, helm_image })
    }

    async fn collect_pod_logs(&self, job_name: &str) -> Result<Option<String>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp =
            ListParams::default().labels(&format!("batch.kubernetes.io/job-name={job_name}"));
        let pod_list = pods.list(&lp).await.context("failed to list job pods")?;

        let mut all_logs = Vec::new();
        for pod in pod_list.items {
            let pod_name = match pod.metadata.name.as_deref() {
                Some(n) => n.to_string(),
                None => continue,
            };
            match pods
                .logs(
                    &pod_name,
                    &LogParams {
                        container: Some("helm".to_string()),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(log) => all_logs.push(log),
                Err(e) => warn!(pod = %pod_name, "Failed to fetch logs: {e}"),
            }
        }

        Ok(if all_logs.is_empty() {
            None
        } else {
            Some(all_logs.join("\n"))
        })
    }
}

#[async_trait]
impl HelmJobRunner for K8sJobRunner {
    fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Creates a kubeconfig Secret and a helm Job, returning the Job name.
    ///
    /// `helm_args` is everything passed after the `helm` binary. The kubeconfig
    /// and optional values blob are injected via a mounted Secret;
    /// `--kubeconfig /helm-creds/kubeconfig` (and `--values /helm-creds/values.json`)
    /// are appended automatically.
    async fn spawn_helm_job(
        &self,
        job_label: &str,
        release_name: &str,
        mut helm_args: Vec<String>,
        kubeconfig: &str,
        values_json: Option<&str>,
    ) -> Result<String> {
        let suffix: String = uuid::Uuid::new_v4()
            .to_string()
            .replace('-', "")
            .chars()
            .take(8)
            .collect();
        let safe_release = sanitize_k8s_name(release_name);
        let job_name = format!("helm-{job_label}-{safe_release}-{suffix}");
        let secret_name = format!("{job_name}-creds");

        let mut labels = BTreeMap::new();
        labels.insert(MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string());

        // ── Secret ──────────────────────────────────────────────────────────
        let mut secret_data: BTreeMap<String, k8s_openapi::ByteString> = BTreeMap::new();
        secret_data.insert(
            "kubeconfig".to_string(),
            k8s_openapi::ByteString(kubeconfig.as_bytes().to_vec()),
        );
        if let Some(vals) = values_json {
            secret_data.insert(
                "values.json".to_string(),
                k8s_openapi::ByteString(vals.as_bytes().to_vec()),
            );
        }

        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        secrets
            .create(
                &PostParams::default(),
                &Secret {
                    metadata: ObjectMeta {
                        name: Some(secret_name.clone()),
                        namespace: Some(self.namespace.clone()),
                        labels: Some(labels.clone()),
                        ..Default::default()
                    },
                    data: Some(secret_data),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("failed to create Secret {secret_name}"))?;
        info!(secret = %secret_name, namespace = %self.namespace, "Created kubeconfig Secret");

        // Inject credential flags into the helm command.
        helm_args.extend([
            "--kubeconfig".to_string(),
            "/helm-creds/kubeconfig".to_string(),
        ]);
        if values_json.is_some() {
            helm_args.extend([
                "--values".to_string(),
                "/helm-creds/values.json".to_string(),
            ]);
        }

        // ── Job ─────────────────────────────────────────────────────────────
        let mut pod_labels = labels.clone();
        pod_labels.insert("rancher-helm-mcp/release".to_string(), safe_release);

        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        jobs.create(
            &PostParams::default(),
            &Job {
                metadata: ObjectMeta {
                    name: Some(job_name.clone()),
                    namespace: Some(self.namespace.clone()),
                    labels: Some(labels),
                    ..Default::default()
                },
                spec: Some(JobSpec {
                    // Auto-delete 1 h after completion so the namespace stays clean.
                    ttl_seconds_after_finished: Some(3600),
                    // No retries — a failed install should be inspected, not retried blindly.
                    backoff_limit: Some(0),
                    template: PodTemplateSpec {
                        metadata: Some(ObjectMeta {
                            labels: Some(pod_labels),
                            ..Default::default()
                        }),
                        spec: Some(PodSpec {
                            restart_policy: Some("Never".to_string()),
                            containers: vec![Container {
                                name: "helm".to_string(),
                                image: Some(self.helm_image.clone()),
                                // Override entrypoint so any helm image works.
                                command: Some(vec!["helm".to_string()]),
                                args: Some(helm_args),
                                volume_mounts: Some(vec![VolumeMount {
                                    name: "helm-creds".to_string(),
                                    mount_path: "/helm-creds".to_string(),
                                    read_only: Some(true),
                                    ..Default::default()
                                }]),
                                ..Default::default()
                            }],
                            volumes: Some(vec![Volume {
                                name: "helm-creds".to_string(),
                                secret: Some(SecretVolumeSource {
                                    secret_name: Some(secret_name),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }]),
                            ..Default::default()
                        }),
                    },
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("failed to create Job {job_name}"))?;
        info!(job = %job_name, namespace = %self.namespace, "Created helm Job");

        Ok(job_name)
    }

    /// Returns the current status and logs of a previously spawned Job.
    async fn get_job_output(&self, job_name: &str) -> Result<JobOutput> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let job = jobs
            .get(job_name)
            .await
            .with_context(|| format!("job {job_name} not found in namespace {}", self.namespace))?;

        let status = job.status.unwrap_or_default();
        let succeeded = status.succeeded.unwrap_or(0) > 0;
        let active = status.active.unwrap_or(0) > 0;
        let failed_count = status.failed.unwrap_or(0);
        let done = !active && (succeeded || failed_count > 0);

        let phase: &'static str = if succeeded {
            "Succeeded"
        } else if !active && failed_count > 0 {
            "Failed"
        } else if active {
            "Running"
        } else {
            "Pending"
        };

        let logs = if done {
            self.collect_pod_logs(job_name).await.unwrap_or_else(|e| {
                warn!(%job_name, "Failed to collect logs: {e}");
                None
            })
        } else {
            None
        };

        Ok(JobOutput { phase, succeeded, logs })
    }

    /// Polls until the Job finishes (succeeded or failed) or the timeout expires.
    async fn wait_for_job(&self, job_name: &str, timeout: Duration) -> Result<JobOutput> {
        let deadline = Instant::now() + timeout;
        loop {
            let out = self.get_job_output(job_name).await?;
            if out.phase == "Succeeded" || out.phase == "Failed" {
                return Ok(out);
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "job {job_name} did not complete within {}s",
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

/// Converts arbitrary strings to valid K8s name segments (lowercase alphanumeric + hyphens).
/// Truncated to 40 chars to leave room for job-type prefix and unique suffix.
fn sanitize_k8s_name(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    out.trim_matches('-').chars().take(40).collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_lowercases_uppercase() {
        assert_eq!(sanitize_k8s_name("MyApp"), "myapp");
    }

    #[test]
    fn sanitize_replaces_special_chars_with_hyphens() {
        assert_eq!(sanitize_k8s_name("my_app.v2"), "my-app-v2");
    }

    #[test]
    fn sanitize_replaces_spaces_with_hyphens() {
        assert_eq!(sanitize_k8s_name("my app"), "my-app");
    }

    #[test]
    fn sanitize_strips_leading_and_trailing_hyphens() {
        assert_eq!(sanitize_k8s_name("---foo---"), "foo");
    }

    #[test]
    fn sanitize_truncates_at_40_chars() {
        let long = "a".repeat(60);
        let result = sanitize_k8s_name(&long);
        assert_eq!(result.len(), 40);
        assert_eq!(result, "a".repeat(40));
    }

    #[test]
    fn sanitize_preserves_already_clean_names() {
        assert_eq!(sanitize_k8s_name("nginx"), "nginx");
        assert_eq!(sanitize_k8s_name("my-release-123"), "my-release-123");
    }

    #[test]
    fn sanitize_handles_numbers_and_dots() {
        assert_eq!(sanitize_k8s_name("v1.2.3"), "v1-2-3");
    }
}
