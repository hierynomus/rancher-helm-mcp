# CLAUDE.md

Guidance for Claude Code (or other AI agents) working in this repository.
See [README.md](README.md) for what this project does and how to run it.

## Project structure

```
src/
  main.rs      — entry point; selects stdio vs. HTTP transport from PORT env var
  server.rs    — MCP tool definitions (list_clusters, install_helm_chart,
                 get_job_status, list_helm_releases); ServerHandler impl
  rancher.rs   — Rancher v3 API client (list clusters, generate kubeconfig)
  k8s.rs       — Kubernetes job runner (create Secret + Job, poll status, fetch logs)

charts/rancher-helm-mcp/
  Chart.yaml
  values.yaml
  templates/
    _helpers.tpl       — name / fullname / label helpers
    serviceaccount.yaml
    role.yaml          — namespace-scoped; 4 rules only (see RBAC section in README)
    rolebinding.yaml
    secret.yaml        — Rancher credentials (skipped if rancher.existingSecret is set)
    deployment.yaml    — injects JOB_NAMESPACE via downward API fieldRef
    service.yaml
    ingress.yaml
```

## rmcp API (v1.7)

### Tool router pattern

Use `#[tool_router]` on the inherent impl and `#[tool_handler]` on the
`impl ServerHandler` block **separately**. Do NOT use `#[tool_router(server_handler)]`
if you also provide a custom `get_info()` — that flag generates a conflicting
`ServerHandler` impl.

```rust
#[tool_router]
impl MyServer {
    #[tool(description = "…")]
    async fn my_tool(&self, Parameters(input): Parameters<MyInput>) -> Result<String, rmcp::ErrorData> { … }
}

#[tool_handler]
impl ServerHandler for MyServer {
    fn get_info(&self) -> ServerInfo { … }  // only generated if absent
}
```

### `rmcp::Error` is deprecated

Use `rmcp::ErrorData` everywhere. `rmcp::Error` is a type alias that will be
renamed; the compiler emits deprecation warnings for it.

### `ServerInfo` is `InitializeResult`

`pub type ServerInfo = InitializeResult;` — construct it with:

```rust
InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
    .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
```

`InitializeResult` is `#[non_exhaustive]`, so struct literal syntax fails —
always use the constructor.

### Investigating rmcp internals

rmcp docs lag its actual behaviour. When in doubt, read the source:

```sh
find ~/.cargo/registry/src -maxdepth 1 -iname "rmcp-*"
```

See the sibling project [rancher-mcp-proxy](../rancher-mcp-proxy/CLAUDE.md)
for a documented gotcha around `_meta` / progress tokens.

## kube crate (v0.99)

### Version pinning

`kube = "0.99"` pulls in `k8s-openapi = "0.24"`. Our `Cargo.toml` must
declare `k8s-openapi = { version = "0.24", features = ["v1_30"] }` explicitly
so the feature flag is satisfied. Mismatching major versions (e.g. pulling
`0.23` alongside `0.24`) causes a build failure because both end up in the
tree but only one has the feature enabled.

If you upgrade `kube`, run `cargo tree -i k8s-openapi` to confirm only one
version is present and bump our direct `k8s-openapi` version accordingly.

### In-cluster auth

`kube::Client::try_default()` resolves credentials in this order:

1. In-cluster (`/var/run/secrets/kubernetes.io/serviceaccount/token` + CA)
2. `KUBECONFIG` env var
3. `~/.kube/config`

In a pod this is automatic; locally it uses your kubeconfig context.

### K8s object construction

`k8s-openapi` structs are `#[non_exhaustive]`. Use `..Default::default()` to
fill unset fields — do not try to enumerate every field.

## Job/Secret pattern

Each `install_helm_chart` call produces:

1. **A Secret** (`helm-install-<release>-<8hexchars>-creds`) with keys:
   - `kubeconfig` — YAML kubeconfig for the target cluster (from Rancher)
   - `values.json` — serialised values object (only present when values were supplied)

2. **A Job** (`helm-install-<release>-<8hexchars>`) that:
   - Uses image `alpine/helm:3` (configurable via `HELM_IMAGE`)
   - Overrides `command: ["helm"]` so any helm image works regardless of entrypoint
   - Mounts the Secret at `/helm-creds/`
   - Appends `--kubeconfig /helm-creds/kubeconfig` (and `--values /helm-creds/values.json`)
   - Has `backoff_limit: 0` (no retries) and `ttlSecondsAfterFinished: 3600`

`list_helm_releases` uses the same machinery but waits synchronously (up to 60 s)
and returns the Job's stdout as its tool result. Use `k8s.wait_for_job()` for this.

The credential Secret is **not** owned by the Job (no `ownerReference`), so it
persists after `ttlSecondsAfterFinished` deletes the Job. Clean up with:

```sh
kubectl delete secrets -l app.kubernetes.io/managed-by=rancher-helm-mcp -n <namespace>
```

If you add Secret cleanup, the right place is `K8sJobRunner::get_job_output` —
patch the Secret with an `ownerReference` pointing at the Job's UID after the
Job is created.

## Transport modes

`main.rs` selects transport based on `PORT`:

| `PORT` set | Transport | Use case |
|---|---|---|
| yes | Streamable HTTP (`axum` + `StreamableHttpService`) with `/health` | In-cluster Deployment |
| no | stdio (`rmcp::transport::io::stdio()`) | Local dev / Claude Desktop command mode |

The `HelmMcpServer` is `Clone` so the HTTP factory closure (`move || Ok(server.clone())`)
shares `Arc<RancherClient>` and `Arc<K8sJobRunner>` across sessions without
copying any real state.

## Helm chart conventions

Follow the same patterns as [rancher-mcp-proxy](../rancher-mcp-proxy/charts/rancher-mcp-proxy/):

- All resource names go through `rancher-helm-mcp.fullname` (supports `nameOverride` /
  `fullnameOverride`).
- Sensitive values (Rancher token) always reach the container via `secretKeyRef`,
  never as plain env strings.
- `secret.yaml` is suppressed entirely when `rancher.existingSecret` is set —
  test both paths with `helm template`.
- `JOB_NAMESPACE` uses `fieldRef: fieldPath: metadata.namespace` so the namespace
  is always the pod's own namespace at runtime, not whatever was set at deploy time.

## Common tasks

```sh
# Build
mise exec -- cargo build

# Lint chart (both credential modes)
helm lint charts/rancher-helm-mcp --set rancher.url=x --set rancher.token=y
helm lint charts/rancher-helm-mcp --set rancher.existingSecret=my-secret

# Render chart and inspect a specific resource
helm template dev charts/rancher-helm-mcp \
  --set rancher.url=x --set rancher.token=y \
  --namespace rancher-helm \
  | grep -A40 "kind: Role"
```
