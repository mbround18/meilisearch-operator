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
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct IndexStatus {
    pub ready: bool,
    pub message: Option<String>,
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
