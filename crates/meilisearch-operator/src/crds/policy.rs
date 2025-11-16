use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[kube(
    group = "meili.operator.dev",
    version = "v1alpha1",
    kind = "Policy",
    plural = "policies",
    namespaced,
    status = "PolicyStatus",
    shortname = "mpol"
)]
pub struct PolicySpec {
    /// Reference to Server name in same namespace
    pub server_ref: String,
    /// Minimal: ensure a default set of keys
    #[serde(default)]
    pub default_search_key: bool,
    /// Optional webhook notifications configuration
    #[serde(default)]
    pub notifications: Option<NotificationsSpec>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct PolicyStatus {
    pub applied: bool,
    pub message: Option<String>,
    pub last_event_ts: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct NotificationsSpec {
    pub webhook_url: String,
    pub secret_ref: Option<SecretRef>,
    pub events: Option<Vec<String>>, // e.g. ["applied"]
    #[serde(default = "default_webhook_timeout")]
    pub timeout_seconds: u64,
}

fn default_webhook_timeout() -> u64 { 5 }

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct SecretRef {
    pub name: String,
    pub namespace: Option<String>,
    pub key: Option<String>,
}
