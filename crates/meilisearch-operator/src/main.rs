use futures::StreamExt;
use kube::Client;
use meilisearch_operator::{
    index_controller as idx, key_controller as keyc, server_controller as srv,
};
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

    // Server controller
    let srv_ctx = Arc::new(srv::Ctx {
        client: client.clone(),
        operator_namespace: operator_namespace.clone(),
    });
    let srv_controller = srv::controller(client.clone(), operator_namespace.clone())
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

    tokio::select! {
        _ = srv_controller => {},
        _ = idx_controller => {},
        _ = key_controller => {},
        _ = tokio::signal::ctrl_c() => { info!("shutdown signal received"); }
    }
    Ok(())
}
