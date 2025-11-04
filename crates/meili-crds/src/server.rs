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
    pub storage: Option<String>,
    #[serde(default = "default_service_type")]
    pub service_type: String,
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

#[cfg(test)]
mod tests_server_crd {
    use super::*;

    #[test]
    fn server_defaults_apply() {
        // Minimal YAML only specifying required fields
        let yaml = r#"
apiVersion: meili.operator.dev/v1beta1
kind: Server
metadata:
    name: test
spec:
    image: null
"#;
        let srv: Server = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(srv.spec.replicas, 1);
        assert_eq!(srv.spec.service_type, "ClusterIP");
        assert_eq!(srv.spec.port, 7700);
    }
}
