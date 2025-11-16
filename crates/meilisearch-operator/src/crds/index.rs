use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[kube(
    group = "meili.operator.dev",
    version = "v1alpha1",
    kind = "Index",
    plural = "indexes",
    namespaced,
    status = "IndexStatus",
    shortname = "midx"
)]
pub struct IndexSpec {
    /// Reference to Server name in same namespace
    pub server_ref: String,
    /// Index uid
    pub uid: String,
    /// Optional primary key
    pub primary_key: Option<String>,
    /// If true, delete index on CR deletion
    #[serde(default)]
    pub delete_on_finalize: bool,
    /// Optional: generate an admin key with actions ["*"] scoped to this index
    pub admin_key: Option<IndexAdminKeySpec>,
    /// Optional webhook notifications configuration
    #[serde(default)]
    pub notifications: Option<NotificationsSpec>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct IndexStatus {
    pub ready: bool,
    pub message: Option<String>,
    pub last_event_ts: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct IndexAdminKeySpec {
    /// Create an admin key scoped to this index
    #[serde(default)]
    pub create: bool,
    /// Namespace to store the Secret (defaults to CR namespace if None)
    pub secret_namespace: Option<String>,
    /// Name for the Secret (defaults to "<uid>-admin-key" if None)
    pub secret_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct NotificationsSpec {
    pub webhook_url: String,
    pub secret_ref: Option<SecretRef>,
    pub events: Option<Vec<String>>, // e.g. ["ready", "created"]
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
