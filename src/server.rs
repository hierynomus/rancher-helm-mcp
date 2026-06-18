use anyhow::Result;
use rmcp::{
    ServerHandler,
    handler::server::wrapper::Parameters,
    model::{Implementation, InitializeResult, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::k8s::HelmJobRunner;
use crate::rancher::RancherApi;

#[derive(Clone)]
pub struct HelmMcpServer {
    rancher: Arc<dyn RancherApi>,
    k8s: Arc<dyn HelmJobRunner>,
}

impl HelmMcpServer {
    pub fn new(
        rancher: impl RancherApi + 'static,
        k8s: impl HelmJobRunner + 'static,
    ) -> Self {
        Self {
            rancher: Arc::new(rancher),
            k8s: Arc::new(k8s),
        }
    }
}

// ── Tool input types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListClustersInput {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InstallChartInput {
    #[schemars(description = "Rancher cluster ID (e.g. c-m-abcd1234) or cluster name")]
    pub cluster: String,
    #[schemars(description = "Kubernetes namespace to install into (created if absent)")]
    pub namespace: String,
    #[schemars(description = "Helm release name")]
    pub release_name: String,
    #[schemars(description = "Chart name (e.g. nginx or myrepo/nginx)")]
    pub chart: String,
    #[schemars(description = "Chart version; omit for latest")]
    pub version: Option<String>,
    #[schemars(
        description = "Helm chart repository URL (required when chart is not prefixed with a repo alias)"
    )]
    pub repo: Option<String>,
    #[schemars(description = "Values as a JSON object, passed to helm via --values (optional)")]
    pub values: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetJobStatusInput {
    #[schemars(description = "Job name returned by install_helm_chart")]
    pub job_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListReleasesInput {
    #[schemars(description = "Rancher cluster ID or cluster name")]
    pub cluster: String,
    #[schemars(description = "Namespace to filter; omit to list all namespaces")]
    pub namespace: Option<String>,
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router]
impl HelmMcpServer {
    #[tool(description = "List all Rancher-managed clusters available to the configured API token")]
    async fn list_clusters(
        &self,
        Parameters(_): Parameters<ListClustersInput>,
    ) -> Result<String, rmcp::ErrorData> {
        let clusters = self
            .rancher
            .list_clusters()
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        if clusters.is_empty() {
            return Ok("No clusters found.".to_string());
        }

        let lines: Vec<String> = clusters
            .iter()
            .map(|c| {
                format!(
                    "- id={} name={} state={} {}",
                    c.id,
                    c.name,
                    c.state.as_deref().unwrap_or("unknown"),
                    c.description
                        .as_deref()
                        .map(|d| format!("({d})"))
                        .unwrap_or_default(),
                )
            })
            .collect();

        Ok(lines.join("\n"))
    }

    #[tool(
        description = "Install or upgrade a Helm chart on a Rancher-managed cluster. \
        Fetches the kubeconfig from Rancher, stores it in a Kubernetes Secret, and spawns \
        a Kubernetes Job that runs helm upgrade --install. Returns the Job name immediately; \
        use get_job_status to check progress and retrieve logs."
    )]
    async fn install_helm_chart(
        &self,
        Parameters(input): Parameters<InstallChartInput>,
    ) -> Result<String, rmcp::ErrorData> {
        let cluster_id = self.resolve_cluster_id(&input.cluster).await?;
        info!(cluster_id, release = %input.release_name, chart = %input.chart, "Fetching kubeconfig");

        let kubeconfig = self
            .rancher
            .generate_kubeconfig(&cluster_id)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let values_json = input
            .values
            .as_ref()
            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default());

        // Build the helm argument list (credentials flags are appended by spawn_helm_job).
        let mut helm_args = vec![
            "upgrade".to_string(),
            "--install".to_string(),
            input.release_name.clone(),
            input.chart.clone(),
            "--namespace".to_string(),
            input.namespace.clone(),
            "--create-namespace".to_string(),
        ];
        if let Some(v) = &input.version {
            helm_args.extend(["--version".to_string(), v.clone()]);
        }
        if let Some(r) = &input.repo {
            helm_args.extend(["--repo".to_string(), r.clone()]);
        }

        let job_name = self
            .k8s
            .spawn_helm_job(
                "install",
                &input.release_name,
                helm_args,
                &kubeconfig,
                values_json.as_deref(),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(format!(
            "Helm install job created.\n\
             Job:       {job_name}\n\
             Namespace: {}\n\n\
             Use get_job_status(job_name=\"{job_name}\") to check progress and retrieve logs.",
            self.k8s.namespace()
        ))
    }

    #[tool(
        description = "Check the status of a helm Job created by install_helm_chart. \
        Returns the phase (Pending / Running / Succeeded / Failed) and, once the Job \
        has finished, the helm output from its logs."
    )]
    async fn get_job_status(
        &self,
        Parameters(input): Parameters<GetJobStatusInput>,
    ) -> Result<String, rmcp::ErrorData> {
        let out = self
            .k8s
            .get_job_output(&input.job_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut response = format!("Job:    {}\nStatus: {}\n", input.job_name, out.phase);
        if let Some(logs) = &out.logs {
            response.push_str("\nLogs:\n");
            response.push_str(logs);
        }
        Ok(response)
    }

    #[tool(
        description = "List Helm releases on a Rancher-managed cluster. Runs helm list \
        as a Kubernetes Job (waits up to 60 s) and returns the JSON output."
    )]
    async fn list_helm_releases(
        &self,
        Parameters(input): Parameters<ListReleasesInput>,
    ) -> Result<String, rmcp::ErrorData> {
        let cluster_id = self.resolve_cluster_id(&input.cluster).await?;
        let kubeconfig = self
            .rancher
            .generate_kubeconfig(&cluster_id)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut helm_args = vec!["list".to_string(), "--output".to_string(), "json".to_string()];
        if let Some(ns) = &input.namespace {
            helm_args.extend(["--namespace".to_string(), ns.clone()]);
        } else {
            helm_args.push("--all-namespaces".to_string());
        }

        // Use the cluster ID as the "release name" for the job label (no real release involved).
        let job_name = self
            .k8s
            .spawn_helm_job("list", &cluster_id, helm_args, &kubeconfig, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let out = self
            .k8s
            .wait_for_job(&job_name, Duration::from_secs(60))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        match (out.succeeded, out.logs) {
            (true, Some(logs)) => Ok(logs),
            (true, None) => Ok("[]".to_string()),
            (false, logs) => Err(rmcp::ErrorData::internal_error(
                format!(
                    "helm list failed:\n{}",
                    logs.unwrap_or_else(|| "(no logs)".to_string())
                ),
                None,
            )),
        }
    }
}

impl HelmMcpServer {
    /// Accepts a cluster ID (`c-…`, `local`) or a human-readable name.
    async fn resolve_cluster_id(&self, cluster: &str) -> Result<String, rmcp::ErrorData> {
        if cluster == "local" || cluster.starts_with("c-") {
            return Ok(cluster.to_string());
        }

        let clusters = self
            .rancher
            .list_clusters()
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        clusters
            .into_iter()
            .find(|c| c.name.eq_ignore_ascii_case(cluster))
            .map(|c| c.id)
            .ok_or_else(|| {
                rmcp::ErrorData::internal_error(
                    format!(
                        "cluster '{cluster}' not found; use list_clusters to see available clusters"
                    ),
                    None,
                )
            })
    }
}

// ── ServerHandler ─────────────────────────────────────────────────────────────

#[tool_handler]
impl ServerHandler for HelmMcpServer {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::JobOutput;
    use crate::rancher::Cluster;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    // ── Stub implementations ───────────────────────────────────────────────

    struct StubRancher {
        clusters: Vec<Cluster>,
        kubeconfig: String,
    }

    impl StubRancher {
        fn with_clusters(clusters: Vec<Cluster>) -> Self {
            Self { clusters, kubeconfig: "stub-kubeconfig".to_string() }
        }
        fn empty() -> Self {
            Self { clusters: vec![], kubeconfig: "stub-kubeconfig".to_string() }
        }
    }

    #[async_trait]
    impl RancherApi for StubRancher {
        async fn list_clusters(&self) -> anyhow::Result<Vec<Cluster>> {
            Ok(self.clusters.clone())
        }
        async fn generate_kubeconfig(&self, _cluster_id: &str) -> anyhow::Result<String> {
            Ok(self.kubeconfig.clone())
        }
    }

    struct SpawnCall {
        helm_args: Vec<String>,
        values_json: Option<String>,
    }

    struct RecordingJobRunner {
        ns: String,
        calls: Arc<Mutex<Vec<SpawnCall>>>,
        wait_logs: Option<String>,
        get_output_phase: &'static str,
        get_output_succeeded: bool,
        get_output_logs: Option<String>,
    }

    impl RecordingJobRunner {
        fn new() -> Self {
            Self {
                ns: "test-ns".to_string(),
                calls: Arc::new(Mutex::new(vec![])),
                wait_logs: None,
                get_output_phase: "Succeeded",
                get_output_succeeded: true,
                get_output_logs: None,
            }
        }
        fn with_wait_logs(mut self, logs: &str) -> Self {
            self.wait_logs = Some(logs.to_string());
            self
        }
        fn with_get_logs(mut self, logs: &str) -> Self {
            self.get_output_logs = Some(logs.to_string());
            self
        }
    }

    #[async_trait]
    impl HelmJobRunner for RecordingJobRunner {
        fn namespace(&self) -> &str {
            &self.ns
        }
        async fn spawn_helm_job(
            &self,
            _job_label: &str,
            _release_name: &str,
            helm_args: Vec<String>,
            _kubeconfig: &str,
            values_json: Option<&str>,
        ) -> anyhow::Result<String> {
            self.calls.lock().unwrap().push(SpawnCall {
                helm_args,
                values_json: values_json.map(String::from),
            });
            Ok("helm-install-nginx-abc12345".to_string())
        }
        async fn get_job_output(&self, _job_name: &str) -> anyhow::Result<JobOutput> {
            Ok(JobOutput {
                phase: self.get_output_phase,
                succeeded: self.get_output_succeeded,
                logs: self.get_output_logs.clone(),
            })
        }
        async fn wait_for_job(
            &self,
            _job_name: &str,
            _timeout: Duration,
        ) -> anyhow::Result<JobOutput> {
            Ok(JobOutput {
                phase: "Succeeded",
                succeeded: true,
                logs: self.wait_logs.clone(),
            })
        }
    }

    fn cluster(id: &str, name: &str) -> Cluster {
        Cluster {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            state: Some("active".to_string()),
        }
    }

    fn make_server(
        rancher: impl RancherApi + 'static,
        k8s: impl HelmJobRunner + 'static,
    ) -> HelmMcpServer {
        HelmMcpServer::new(rancher, k8s)
    }

    // ── resolve_cluster_id ─────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_local_passes_through() {
        let server = make_server(StubRancher::empty(), RecordingJobRunner::new());
        assert_eq!(server.resolve_cluster_id("local").await.unwrap(), "local");
    }

    #[tokio::test]
    async fn resolve_cluster_id_prefix_passes_through() {
        let server = make_server(StubRancher::empty(), RecordingJobRunner::new());
        assert_eq!(
            server.resolve_cluster_id("c-m-abcd1234").await.unwrap(),
            "c-m-abcd1234"
        );
    }

    #[tokio::test]
    async fn resolve_name_looks_up_id() {
        let server = make_server(
            StubRancher::with_clusters(vec![cluster("c-m-abc", "dev")]),
            RecordingJobRunner::new(),
        );
        assert_eq!(server.resolve_cluster_id("dev").await.unwrap(), "c-m-abc");
    }

    #[tokio::test]
    async fn resolve_name_is_case_insensitive() {
        let server = make_server(
            StubRancher::with_clusters(vec![cluster("c-m-abc", "Dev")]),
            RecordingJobRunner::new(),
        );
        assert_eq!(server.resolve_cluster_id("DEV").await.unwrap(), "c-m-abc");
    }

    #[tokio::test]
    async fn resolve_unknown_cluster_returns_error() {
        let server = make_server(StubRancher::empty(), RecordingJobRunner::new());
        let err = server.resolve_cluster_id("unknown").await.unwrap_err();
        assert!(err.message.contains("not found"), "error was: {}", err.message);
    }

    // ── list_clusters tool ─────────────────────────────────────────────────

    #[tokio::test]
    async fn list_clusters_tool_formats_clusters() {
        let server = make_server(
            StubRancher::with_clusters(vec![cluster("c-m-abc", "prod")]),
            RecordingJobRunner::new(),
        );
        let out = server
            .list_clusters(Parameters(ListClustersInput {}))
            .await
            .unwrap();
        assert!(out.contains("c-m-abc"), "output: {out}");
        assert!(out.contains("prod"), "output: {out}");
    }

    #[tokio::test]
    async fn list_clusters_tool_empty() {
        let server = make_server(StubRancher::empty(), RecordingJobRunner::new());
        let out = server
            .list_clusters(Parameters(ListClustersInput {}))
            .await
            .unwrap();
        assert!(out.contains("No clusters"), "output: {out}");
    }

    // ── install_helm_chart ─────────────────────────────────────────────────

    fn install_input(release: &str, chart: &str) -> InstallChartInput {
        InstallChartInput {
            cluster: "local".to_string(),
            namespace: "default".to_string(),
            release_name: release.to_string(),
            chart: chart.to_string(),
            version: None,
            repo: None,
            values: None,
        }
    }

    #[tokio::test]
    async fn install_returns_job_name_in_output() {
        let server = make_server(StubRancher::empty(), RecordingJobRunner::new());
        let out = server
            .install_helm_chart(Parameters(install_input("nginx", "nginx/nginx")))
            .await
            .unwrap();
        assert!(out.contains("helm-install-nginx-abc12345"), "output: {out}");
    }

    #[tokio::test]
    async fn install_passes_version_flag() {
        let calls = Arc::new(Mutex::new(vec![]));
        let runner = RecordingJobRunner { calls: Arc::clone(&calls), ..RecordingJobRunner::new() };
        let server = make_server(StubRancher::empty(), runner);
        let mut input = install_input("nginx", "nginx/nginx");
        input.version = Some("1.2.3".to_string());
        server.install_helm_chart(Parameters(input)).await.unwrap();

        let guard = calls.lock().unwrap();
        let pos = guard[0]
            .helm_args
            .iter()
            .position(|a| a == "--version")
            .expect("--version missing");
        assert_eq!(guard[0].helm_args[pos + 1], "1.2.3");
    }

    #[tokio::test]
    async fn install_passes_repo_flag() {
        let calls = Arc::new(Mutex::new(vec![]));
        let runner = RecordingJobRunner { calls: Arc::clone(&calls), ..RecordingJobRunner::new() };
        let server = make_server(StubRancher::empty(), runner);
        let mut input = install_input("nginx", "nginx");
        input.repo = Some("https://charts.example.com".to_string());
        server.install_helm_chart(Parameters(input)).await.unwrap();

        let guard = calls.lock().unwrap();
        let pos = guard[0]
            .helm_args
            .iter()
            .position(|a| a == "--repo")
            .expect("--repo missing");
        assert_eq!(guard[0].helm_args[pos + 1], "https://charts.example.com");
    }

    #[tokio::test]
    async fn install_omits_values_when_none() {
        let calls = Arc::new(Mutex::new(vec![]));
        let runner = RecordingJobRunner { calls: Arc::clone(&calls), ..RecordingJobRunner::new() };
        let server = make_server(StubRancher::empty(), runner);
        server
            .install_helm_chart(Parameters(install_input("nginx", "nginx/nginx")))
            .await
            .unwrap();

        let guard = calls.lock().unwrap();
        assert!(guard[0].values_json.is_none());
        assert!(!guard[0].helm_args.contains(&"--values".to_string()));
    }

    #[tokio::test]
    async fn install_passes_values_as_json() {
        let calls = Arc::new(Mutex::new(vec![]));
        let runner = RecordingJobRunner { calls: Arc::clone(&calls), ..RecordingJobRunner::new() };
        let server = make_server(StubRancher::empty(), runner);
        let mut input = install_input("nginx", "nginx/nginx");
        input.values = Some(serde_json::json!({"replicas": 3}));
        server.install_helm_chart(Parameters(input)).await.unwrap();

        let guard = calls.lock().unwrap();
        let json_str = guard[0].values_json.as_ref().expect("values_json should be Some");
        let v: serde_json::Value = serde_json::from_str(json_str).unwrap();
        assert_eq!(v["replicas"], 3);
    }

    // ── get_job_status ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_job_status_formats_phase() {
        let server = make_server(StubRancher::empty(), RecordingJobRunner::new());
        let out = server
            .get_job_status(Parameters(GetJobStatusInput {
                job_name: "helm-install-nginx-abc12345".to_string(),
            }))
            .await
            .unwrap();
        assert!(out.contains("Succeeded"), "output: {out}");
        assert!(out.contains("helm-install-nginx-abc12345"), "output: {out}");
    }

    #[tokio::test]
    async fn get_job_status_includes_logs_when_present() {
        let runner = RecordingJobRunner::new()
            .with_get_logs("Release \"nginx\" has been upgraded.");
        let server = make_server(StubRancher::empty(), runner);
        let out = server
            .get_job_status(Parameters(GetJobStatusInput {
                job_name: "some-job".to_string(),
            }))
            .await
            .unwrap();
        assert!(out.contains("Release \"nginx\""), "output: {out}");
    }

    // ── list_helm_releases ─────────────────────────────────────────────────

    #[tokio::test]
    async fn list_releases_returns_logs_as_output() {
        let runner = RecordingJobRunner::new().with_wait_logs(r#"[{"name":"nginx"}]"#);
        let server = make_server(
            StubRancher::with_clusters(vec![cluster("c-m-abc", "dev")]),
            runner,
        );
        let out = server
            .list_helm_releases(Parameters(ListReleasesInput {
                cluster: "dev".to_string(),
                namespace: None,
            }))
            .await
            .unwrap();
        assert!(out.contains("nginx"), "output: {out}");
    }

    #[tokio::test]
    async fn list_releases_passes_all_namespaces_when_no_filter() {
        let calls = Arc::new(Mutex::new(vec![]));
        let runner = RecordingJobRunner {
            calls: Arc::clone(&calls),
            wait_logs: Some("[]".to_string()),
            ..RecordingJobRunner::new()
        };
        let server = make_server(StubRancher::empty(), runner);
        server
            .list_helm_releases(Parameters(ListReleasesInput {
                cluster: "local".to_string(),
                namespace: None,
            }))
            .await
            .unwrap();

        let guard = calls.lock().unwrap();
        assert!(
            guard[0].helm_args.contains(&"--all-namespaces".to_string()),
            "args: {:?}",
            guard[0].helm_args
        );
    }

    #[tokio::test]
    async fn list_releases_passes_namespace_filter() {
        let calls = Arc::new(Mutex::new(vec![]));
        let runner = RecordingJobRunner {
            calls: Arc::clone(&calls),
            wait_logs: Some("[]".to_string()),
            ..RecordingJobRunner::new()
        };
        let server = make_server(StubRancher::empty(), runner);
        server
            .list_helm_releases(Parameters(ListReleasesInput {
                cluster: "local".to_string(),
                namespace: Some("monitoring".to_string()),
            }))
            .await
            .unwrap();

        let guard = calls.lock().unwrap();
        let pos = guard[0]
            .helm_args
            .iter()
            .position(|a| a == "--namespace")
            .expect("--namespace missing");
        assert_eq!(guard[0].helm_args[pos + 1], "monitoring");
        assert!(!guard[0].helm_args.contains(&"--all-namespaces".to_string()));
    }
}
