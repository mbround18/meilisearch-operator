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
pub struct PolicySpec {}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct PolicyStatus {
    pub ready: bool,
    pub message: Option<String>,
}
