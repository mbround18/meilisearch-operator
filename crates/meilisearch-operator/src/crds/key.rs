use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[kube(
    group = "meili.operator.dev",
    version = "v1alpha1",
    kind = "Key",
    plural = "keys",
    namespaced,
    status = "KeyStatus",
    shortname = "mkey"
)]
pub struct KeySpec {
    /// Reference to Server name in same namespace
    pub server_ref: String,
    /// Meilisearch key name
    pub name: Option<String>,
    /// Description
    pub description: Option<String>,
    /// Actions like ["search", "documents.add"]
    pub actions: Vec<String>,
    /// Index restrictions, e.g. ["*"] or specific uids
    pub indexes: Vec<String>,
    /// Optional ISO8601 expiration
    pub expires_at: Option<String>,
    /// Where to store the created key secret
    pub secret_namespace: String,
    pub secret_name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct KeyStatus {
    /// UID of key on server
    pub uid: Option<String>,
    pub ready: bool,
    pub message: Option<String>,
}
