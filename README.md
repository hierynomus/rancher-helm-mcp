# rancher-helm-mcp

An [MCP](https://modelcontextprotocol.io) server that installs Helm charts on
**Rancher-managed clusters**. It asks Rancher for the right kubeconfig, then
runs `helm` inside a short-lived Kubernetes Job — so the server itself needs
no Helm binary and holds no long-running cluster connections.

## How it works

```
AI assistant (Claude Desktop, Claude Code, …)
        │
        │  MCP call: install_helm_chart(cluster="dev", chart="nginx/nginx", …)
        ▼
┌────────────────────────────────────────────────────────┐
│  rancher-helm-mcp  (MCP server, runs in your cluster)  │
│                                                        │
│  1. GET /v3/clusters → resolve cluster name to ID      │
│  2. POST /v3/clusters/<id>?action=generateKubeconfig   │
│  3. Create K8s Secret  (kubeconfig + values.json)      │
│  4. Create K8s Job     (alpine/helm container)         │
│  5. Return job name immediately                        │
└────────────────────────────────────────────────────────┘
        │                       │
        │ Rancher v3 API        │ K8s API (in-cluster)
        ▼                       ▼
  Rancher Manager          Kubernetes Job
                        ┌─────────────────┐
                        │  alpine/helm    │
                        │                 │
                        │  helm upgrade   │
                        │  --install …    │
                        │  --kubeconfig   │
                        │  /creds/config  │
                        └────────┬────────┘
                                 │ talks to target cluster
                                 ▼
                         downstream cluster
```

The AI assistant polls `get_job_status` to check progress; logs from the helm
container are returned once the Job finishes.

---

## MCP tools

| Tool | Description |
|---|---|
| `list_clusters` | List all Rancher-managed clusters visible to the API token |
| `install_helm_chart` | Install or upgrade a Helm chart; spawns a Job and returns the job name immediately |
| `get_job_status` | Check phase (Pending / Running / Succeeded / Failed) and fetch helm logs for a job |
| `list_helm_releases` | Run `helm list` as a Job (waits up to 60 s) and return JSON |

### Typical AI workflow

```
list_clusters
  → pick "dev"

install_helm_chart(cluster="dev", namespace="monitoring",
                   release_name="prometheus", chart="prometheus",
                   repo="https://prometheus-community.github.io/helm-charts")
  → "Job created: helm-install-prometheus-a3f2b1c0"

get_job_status(job_name="helm-install-prometheus-a3f2b1c0")
  → Status: Succeeded
    Logs: Release "prometheus" has been upgraded. Happy Helming! …
```

---

## Configuration

### Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `RANCHER_URL` | yes | — | Base URL of the Rancher Manager (`https://rancher.example.com`) |
| `RANCHER_TOKEN` | yes | — | Rancher API token (`token-xxxxx:yyyyyy`) |
| `RANCHER_TLS_VERIFY` | no | `true` | Set to `false` to skip TLS verification (lab/self-signed certs) |
| `PORT` | no | — | When set, serve MCP over streamable HTTP on this port; otherwise use stdio |
| `JOB_NAMESPACE` | no | auto | Namespace for helm Jobs and credential Secrets; auto-detected from pod ServiceAccount in-cluster |
| `HELM_IMAGE` | no | `alpine/helm:3` | Container image used inside helm Jobs; must have `helm` on `$PATH` |
| `RUST_LOG` | no | `info` | Log level (`trace`, `debug`, `info`, `warn`, `error`) |

### Generating a Rancher API token

In the Rancher UI: **avatar → Account & API Keys → Create API Key**.
The token needs at minimum read access to `clusters` and the ability to call
`generateKubeconfig` on each target cluster.

---

## Deployment

### Helm (recommended)

```sh
helm upgrade --install rancher-helm \
  ./charts/rancher-helm-mcp \
  --namespace rancher-helm --create-namespace \
  --set rancher.url=https://rancher.example.com \
  --set rancher.token=token-xxxxx:yyyyyyyyyyyyyyyyyy
```

For production, create the Secret yourself and reference it:

```sh
kubectl create secret generic my-rancher-creds \
  --from-literal=RANCHER_URL=https://rancher.example.com \
  --from-literal=RANCHER_TOKEN=token-xxxxx:yyyyyyyyyyyyyyyyyy \
  --namespace rancher-helm

helm upgrade --install rancher-helm \
  ./charts/rancher-helm-mcp \
  --namespace rancher-helm --create-namespace \
  --set rancher.existingSecret=my-rancher-creds
```

#### Ingress (optional)

```yaml
# values.yaml
ingress:
  enabled: true
  className: traefik
  host: rancher-helm-mcp.example.com
  tls:
    enabled: true
    certManager:
      enabled: true
      clusterIssuer: letsencrypt-prod
```

#### Custom helm image

```yaml
helmImage: alpine/helm:3.17.0   # pin to a specific version
```

### RBAC

The chart creates a namespace-scoped `Role` and binds it to the pod's
`ServiceAccount`. The minimum permissions required are:

| API group | Resource | Verbs | Why |
|---|---|---|---|
| `batch` | `jobs` | `create`, `get` | Spawn and inspect helm Jobs |
| *(core)* | `pods` | `list` | Find the Pod(s) belonging to a Job |
| *(core)* | `pods/log` | `get` | Retrieve helm stdout/stderr |
| *(core)* | `secrets` | `create` | Store kubeconfig + values per Job |

All permissions are in the **release namespace only** — no cluster-wide access
is needed.

> **Note on Secret cleanup:** Each Job is configured with
> `ttlSecondsAfterFinished: 3600`, so the Job object is deleted automatically
> one hour after it finishes. The associated credential Secret is not currently
> owned by the Job and will remain in the namespace. To clean up accumulated
> Secrets:
>
> ```sh
> kubectl delete secrets \
>   -l app.kubernetes.io/managed-by=rancher-helm-mcp \
>   -n rancher-helm
> ```

---

## Client configuration

### Claude Desktop (`claude_desktop_config.json`)

**In-cluster via HTTP (after Helm deploy):**

```json
{
  "mcpServers": {
    "rancher-helm": {
      "type": "http",
      "url": "https://rancher-helm-mcp.example.com"
    }
  }
}
```

**Local stdio (for development):**

```json
{
  "mcpServers": {
    "rancher-helm": {
      "command": "/path/to/rancher-helm-mcp",
      "env": {
        "RANCHER_URL": "https://rancher.example.com",
        "RANCHER_TOKEN": "token-xxxxx:yyyyyyyyyyyyyyyyyy"
      }
    }
  }
}
```

---

## Development

```sh
# Build
cargo build

# Run locally (stdio transport — no PORT set)
RANCHER_URL=https://rancher.example.com \
RANCHER_TOKEN=token-xxxxx:yyyyyy \
RANCHER_TLS_VERIFY=false \
cargo run

# Run locally (HTTP transport)
PORT=3000 \
RANCHER_URL=https://rancher.example.com \
RANCHER_TOKEN=token-xxxxx:yyyyyy \
JOB_NAMESPACE=default \
cargo run
```

The binary picks the transport automatically:

- **`PORT` set** → streamable HTTP on `0.0.0.0:<PORT>` with a `/health` endpoint
- **`PORT` not set** → stdio (MCP over stdin/stdout)

### Lint the Helm chart

```sh
helm lint charts/rancher-helm-mcp \
  --set rancher.url=https://rancher.example.com \
  --set rancher.token=token-abc:secret
```

### Reading Job logs manually

```sh
# find the Pod for a specific job
kubectl get pods -l batch.kubernetes.io/job-name=helm-install-nginx-a3f2b1c0 \
  -n rancher-helm

# tail the logs
kubectl logs -l batch.kubernetes.io/job-name=helm-install-nginx-a3f2b1c0 \
  -c helm -n rancher-helm
```
