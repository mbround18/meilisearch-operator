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
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct PolicyStatus {
    pub applied: bool,
    pub message: Option<String>,
}
