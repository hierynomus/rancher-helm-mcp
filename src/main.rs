use anyhow::{Context, Result};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod k8s;
mod rancher;
mod server;

use k8s::K8sJobRunner;
use rancher::RancherClient;
use server::HelmMcpServer;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring crypto provider");

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let rancher_url = std::env::var("RANCHER_URL")
        .context("RANCHER_URL environment variable is required")?;
    let rancher_token = std::env::var("RANCHER_TOKEN")
        .context("RANCHER_TOKEN environment variable is required")?;
    let tls_verify = std::env::var("RANCHER_TLS_VERIFY")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);

    // Namespace where helm Jobs and kubeconfig Secrets are created.
    // In-cluster: populated from the downward API (see Deployment template).
    // Locally: falls back to the SA namespace file, then "default".
    let job_namespace = std::env::var("JOB_NAMESPACE")
        .or_else(|_| {
            std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
                .map_err(anyhow::Error::from)
        })
        .unwrap_or_else(|_| "default".to_string());

    let helm_image = std::env::var("HELM_IMAGE")
        .unwrap_or_else(|_| "alpine/helm:3".to_string());

    info!(%rancher_url, tls_verify, %job_namespace, %helm_image, "Starting rancher-helm-mcp");

    let rancher = RancherClient::new(rancher_url, rancher_token, tls_verify)?;
    let k8s = K8sJobRunner::try_new(job_namespace, helm_image).await?;
    let server = HelmMcpServer::new(rancher, k8s);

    match std::env::var("PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
        Some(port) => serve_http(server, port).await,
        None => serve_stdio(server).await,
    }
}

async fn serve_stdio(server: HelmMcpServer) -> Result<()> {
    use rmcp::ServiceExt;
    info!("Transport: stdio");
    let transport = rmcp::transport::io::stdio();
    server.serve(transport).await?.waiting().await?;
    Ok(())
}

async fn serve_http(server: HelmMcpServer, port: u16) -> Result<()> {
    use axum::{Router, routing::get};
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    use tokio_util::sync::CancellationToken;

    info!(port, "Transport: streamable HTTP");

    let ct = CancellationToken::new();
    let svc = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_cancellation_token(ct.child_token())
            .disable_allowed_hosts(),
    );
    let app = Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(svc);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!(port, "rancher-helm-mcp listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("Shutting down");
            ct.cancel();
        })
        .await?;
    Ok(())
}
