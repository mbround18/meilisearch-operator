use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[kube(
    group = "meili.operator.dev",
    version = "v1beta1",
    kind = "Server",
    plural = "servers",
    namespaced,
    status = "ServerStatus",
    shortname = "msrv"
)]
pub struct ServerSpec {
    pub image: Option<String>,
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    /// Storage size, e.g. "10Gi"
    pub storage: Option<String>,
    /// Service type: ClusterIP, NodePort, LoadBalancer
    #[serde(default = "default_service_type")]
    pub service_type: String,
    /// Port for meilisearch HTTP, default 7700
    #[serde(default = "default_port")]
    pub port: u16,
    /// Optional webhook notifications configuration
    #[serde(default)]
    pub notifications: Option<NotificationsSpec>,
}

fn default_replicas() -> i32 {
    1
}
fn default_service_type() -> String {
    "ClusterIP".into()
}
fn default_port() -> u16 {
    7700
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct ServerStatus {
    pub ready: bool,
    pub endpoint: Option<String>,
    pub message: Option<String>,
    /// Last successfully emitted event timestamp (RFC3339)
    pub last_event_ts: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct NotificationsSpec {
    /// Webhook endpoint URL (http/https)
    pub webhook_url: String,
    /// Optional secret reference containing HMAC signing key
    pub secret_ref: Option<SecretRef>,
    /// Which events to emit; if empty defaults to ["ready", "status-change"]
    pub events: Option<Vec<String>>,
    /// Timeout seconds for webhook POST (default 5)
    #[serde(default = "default_webhook_timeout")]
    pub timeout_seconds: u64,
}

fn default_webhook_timeout() -> u64 { 5 }

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct SecretRef {
    pub name: String,
    pub namespace: Option<String>,
    /// Key in Secret data holding HMAC key (defaults to "hmacKey")
    pub key: Option<String>,
}
