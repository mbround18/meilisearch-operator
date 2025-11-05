use futures::StreamExt;
use kube::Client;
use meili_index_controller::controller as idx;
use meili_key_controller::controller as keyc;
use meili_metrics::{Config as MetricsConfig, start_background as start_metrics};
use meili_server_controller::controller as srv;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logging
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    info!("meilisearch-operator starting up");

    let client = Client::try_default().await?;
    let operator_namespace =
        std::env::var("OPERATOR_NAMESPACE").unwrap_or_else(|_| "meilisearch-operator".into());
    let metrics_bind = std::env::var("METRICS_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".into());

    // Server controller
    let srv_ctx = Arc::new(srv::Ctx {
        client: client.clone(),
        operator_namespace: operator_namespace.clone(),
    });
    let srv_controller = srv::controller(client.clone())
        .run(srv::reconcile, srv::error_policy, srv_ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::warn!(error=?e, "server reconcile error");
            }
        });

    // Index controller
    let idx_ctx = Arc::new(idx::Ctx {
        client: client.clone(),
    });
    let idx_controller = idx::controller(client.clone())
        .run(idx::reconcile, idx::error_policy, idx_ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::warn!(error=?e, "index reconcile error");
            }
        });

    // Key controller
    let key_ctx = Arc::new(keyc::Ctx {
        client: client.clone(),
    });
    let key_controller = keyc::controller(client.clone())
        .run(keyc::reconcile, keyc::error_policy, key_ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::warn!(error=?e, "key reconcile error");
            }
        });

    // Metrics server
    let metrics_client = client.clone();
    let metrics_ns = operator_namespace.clone();
    let metrics_handle = {
        let cfg = MetricsConfig {
            operator_namespace: metrics_ns,
            bind_addr: metrics_bind,
        };
        start_metrics(metrics_client, cfg)
    };

    // Unified shutdown: handle multiple Unix signals and Ctrl+C
    #[cfg(unix)]
    let shutdown = async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("sigint");
        let mut sigterm = signal(SignalKind::terminate()).expect("sigterm");
        let mut sigquit = signal(SignalKind::quit()).expect("sigquit");
        let mut sighup = signal(SignalKind::hangup()).expect("sighup");

        tokio::select! {
            _ = sigint.recv() => info!("SIGINT received, shutting down"),
            _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
            _ = sigquit.recv() => info!("SIGQUIT received, shutting down"),
            _ = sighup.recv() => info!("SIGHUP received, shutting down"),
        }
    };

    #[cfg(not(unix))]
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        info!("Ctrl+C received, shutting down");
    };

    tokio::select! {
        _ = srv_controller => {},
        _ = idx_controller => {},
        _ = key_controller => {},
        _ = shutdown => {}
    }
    info!("shutting down: stopping background tasks");
    // Best-effort join (non-blocking shutdown)
    let _ = metrics_handle.join();
    Ok(())
}
