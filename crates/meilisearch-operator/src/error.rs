use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error(transparent)]
    Meili(#[from] meilisearch_sdk::errors::Error),
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
}
