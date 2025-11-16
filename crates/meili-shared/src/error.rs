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

impl ReconcileError {
    /// Produce a concise classification + root cause message (e.g. "dns: failed to lookup address information")
    pub fn summary(&self) -> String {
        // Try to downcast anyhow for common reqwest errors
        match self {
            ReconcileError::Anyhow(e) => classify_anyhow(e),
            ReconcileError::Kube(e) => format!("kube: {}", e),
            ReconcileError::Meili(e) => format!("meili: {}", e),
            ReconcileError::Utf8(e) => format!("utf8: {}", e),
        }
    }
}

fn classify_anyhow(e: &anyhow::Error) -> String {
    // reqwest connection/dns/timeouts
    if let Some(src) = e.source() {
        let msg = src.to_string();
        if msg.contains("dns error") || msg.contains("failed to lookup address") {
            return extract_leaf("dns", e);
        }
        if msg.contains("timed out") || msg.contains("timeout") {
            return extract_leaf("timeout", e);
        }
        if msg.contains("connection refused") || msg.contains("Connect") {
            return extract_leaf("connect", e);
        }
        if msg.contains("error decoding response body") {
            return extract_leaf("decode", e);
        }
    }
    // Try to pull HTTP status codes: reqwest::Error implements status()
    if let Some(status) = e.downcast_ref::<reqwest::Error>().and_then(|r| r.status()) {
        let code = status.as_u16();
        let class = match code {
            400..=499 => "http4xx",
            500..=599 => "http5xx",
            _ => "http",
        };
        return format!("{}:{}", class, code);
    }
    // Fall back to last line of error chain
    extract_leaf("anyhow", e)
}

fn extract_leaf(prefix: &str, e: &anyhow::Error) -> String {
    use std::error::Error as StdError;
    let mut cur: &dyn StdError = e.as_ref();
    let mut last = cur.to_string();
    while let Some(src) = cur.source() {
        last = src.to_string();
        cur = src;
    }
    // Trim verbose reqwest preamble
    let cleaned = last
        .replace("client error (Connect)", "connect")
        .replace("error sending request for url", "request")
        .replace("Caused by:", "")
        .trim()
        .to_string();
    format!("{}: {}", prefix, cleaned)
}
