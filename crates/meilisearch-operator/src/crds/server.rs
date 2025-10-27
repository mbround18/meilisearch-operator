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
}
