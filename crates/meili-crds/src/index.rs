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
    pub server_ref: String,
    pub uid: String,
    pub primary_key: Option<String>,
    #[serde(default)]
    pub delete_on_finalize: bool,
    pub admin_key: Option<IndexAdminKeySpec>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct IndexStatus {
    pub ready: bool,
    pub message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct IndexAdminKeySpec {
    #[serde(default)]
    pub create: bool,
    pub secret_namespace: Option<String>,
    pub secret_name: Option<String>,
}
