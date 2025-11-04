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
    pub server_ref: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub actions: Vec<String>,
    pub indexes: Vec<String>,
    pub expires_at: Option<String>,
    pub secret_namespace: String,
    pub secret_name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct KeyStatus {
    pub uid: Option<String>,
    pub ready: bool,
    pub message: Option<String>,
}
