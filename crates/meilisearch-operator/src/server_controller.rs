use std::sync::Arc;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Secret, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use kube::{
    Api, Client, ResourceExt,
    runtime::controller::{Action, Controller},
};
use rand::{Rng, distributions::Alphanumeric};
use tokio::time::Duration;
use tracing::error;

use crate::{
    crds::{
        index::Index,
        key::Key,
        server::{Server, ServerSpec, ServerStatus},
    },
    error::ReconcileError,
};

const FINALIZER: &str = "meili.operator.dev/finalizer";

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub operator_namespace: String,
}

pub fn controller(client: Client, _operator_namespace: String) -> Controller<Server> {
    let api: Api<Server> = Api::all(client.clone());
    Controller::new(api, Default::default()).shutdown_on_signal()
}

pub async fn reconcile(server: Arc<Server>, ctx: Arc<Ctx>) -> Result<Action, ReconcileError> {
    let ns = server.namespace().unwrap();
    let name = server.name_any();

    // Handle deletion with finalizer (cleanup cross-namespace secret)
    if server.metadata.deletion_timestamp.is_some() {
        // Fast-delete dependent Keys and Indexes that reference this server.
        // We remove their finalizers and delete the CRs since the backing data is going away.
        fast_delete_children(&ctx.client, &ns, &name).await?;
        // delete operator namespace copy secret (cannot use ownerRef across namespaces)
        delete_operator_copy(&ctx.client, &ctx.operator_namespace, &ns, &name).await?;
        // remove our finalizer
        remove_finalizer(&ctx.client, &ns, &name).await?;
        return Ok(Action::await_change());
    }

    // Ensure finalizer present early
    ensure_finalizer(&ctx.client, &ns, &name, &server).await?;

    // Ensure master key secret in app namespace
    let owner = owner_ref(&server);
    let mk = ensure_master_key_secret(&ctx.client, &ns, &name, &owner).await?;
    // Mirror master key into operator namespace for management
    ensure_operator_copy(&ctx.client, &ctx.operator_namespace, &ns, &name, &mk).await?;

    // Ensure Service + StatefulSet
    ensure_service(&ctx.client, &ns, &name, server.spec.port, &owner).await?;
    ensure_statefulset(&ctx.client, &ns, &name, &server.spec, &owner).await?;

    // Wait for meilisearch to be healthy
    let endpoint = format!(
        "http://{}.{}.svc.cluster.local:{}",
        name, ns, server.spec.port
    );
    wait_meili_healthy(&endpoint, &mk).await?;

    // Update status
    let status = ServerStatus {
        ready: true,
        endpoint: Some(endpoint),
        message: None,
    };
    let ss_apply = kube::api::PatchParams::apply("meilisearch-operator");
    let servers: Api<Server> = Api::namespaced(ctx.client.clone(), &ns);
    let _ = servers
        .patch_status(
            &name,
            &ss_apply,
            &kube::api::Patch::Merge(serde_json::json!({ "status": status })),
        )
        .await?;

    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn fast_delete_children(
    client: &Client,
    ns: &str,
    server_name: &str,
) -> Result<(), ReconcileError> {
    // Helper to remove finalizers and delete a named object, ignoring 404s
    async fn remove_finals_and_delete<
        T: kube::Resource<DynamicType = ()> + serde::de::DeserializeOwned + Clone + std::fmt::Debug,
    >(
        api: &Api<T>,
        name: &str,
    ) -> Result<(), ReconcileError> {
        let pp = kube::api::PatchParams::default();
        let patch = serde_json::json!({"metadata": {"finalizers": null}});
        match api.patch(name, &pp, &kube::api::Patch::Merge(&patch)).await {
            Ok(_) => (),
            Err(kube::Error::Api(ae)) if ae.code == 404 => (),
            Err(e) => return Err(e.into()),
        }
        let dp = kube::api::DeleteParams::default();
        match api.delete(name, &dp).await {
            Ok(_) => (),
            Err(kube::Error::Api(ae)) if ae.code == 404 => (),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    // Delete Keys
    {
        let api: Api<Key> = Api::namespaced(client.clone(), ns);
        let list = api.list(&kube::api::ListParams::default()).await?;
        for k in list
            .items
            .into_iter()
            .filter(|k| k.spec.server_ref == server_name)
        {
            if let Some(n) = k.metadata.name.as_deref() {
                let _ = remove_finals_and_delete(&api, n).await;
            }
        }
    }

    // Delete Indexes
    {
        let api: Api<Index> = Api::namespaced(client.clone(), ns);
        let list = api.list(&kube::api::ListParams::default()).await?;
        for i in list
            .items
            .into_iter()
            .filter(|i| i.spec.server_ref == server_name)
        {
            if let Some(n) = i.metadata.name.as_deref() {
                let _ = remove_finals_and_delete(&api, n).await;
            }
        }
    }

    Ok(())
}
pub fn error_policy(_server: Arc<Server>, err: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    error!(error = ?err, "reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

async fn ensure_master_key_secret(
    client: &Client,
    ns: &str,
    name: &str,
    owner: &OwnerReference,
) -> Result<String, ReconcileError> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let sec_name = format!("{}-meili-master", name);
    if let Some(sec) = secrets.get_opt(&sec_name).await?
        && let Some(data) = sec.data
        && let Some(bytes) = data.get("masterKey")
    {
        return Ok(String::from_utf8(bytes.0.clone())?);
    }
    let key: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    let sec = Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(sec_name.clone()),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        string_data: Some(std::collections::BTreeMap::from([(
            String::from("masterKey"),
            key.clone(),
        )])),
        ..Default::default()
    };
    let pp = kube::api::PostParams::default();
    match secrets.create(&pp, &sec).await {
        Ok(_) => Ok(key),
        Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(key),
        Err(e) => Err(e.into()),
    }
}

async fn ensure_operator_copy(
    client: &Client,
    op_ns: &str,
    ns: &str,
    name: &str,
    key: &str,
) -> Result<(), ReconcileError> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), op_ns);
    let sec_name = format!("{}-{}-meili-master", ns, name);
    let sec = Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(sec_name),
            ..Default::default()
        },
        string_data: Some(std::collections::BTreeMap::from([(
            String::from("masterKey"),
            key.to_string(),
        )])),
        ..Default::default()
    };
    let params = kube::api::PatchParams::apply("meilisearch-operator").force();
    let _ = secrets
        .patch(
            &sec.metadata.name.clone().unwrap(),
            &params,
            &kube::api::Patch::Apply(&sec),
        )
        .await?;
    Ok(())
}

async fn ensure_service(
    client: &Client,
    ns: &str,
    name: &str,
    port: u16,
    owner: &OwnerReference,
) -> Result<(), ReconcileError> {
    let services: Api<Service> = Api::namespaced(client.clone(), ns);
    let svc = build_service(name, port, owner);
    let params = kube::api::PatchParams::apply("meilisearch-operator").force();
    let _ = services
        .patch(name, &params, &kube::api::Patch::Apply(&svc))
        .await?;
    Ok(())
}

async fn ensure_statefulset(
    client: &Client,
    ns: &str,
    name: &str,
    spec: &ServerSpec,
    owner: &OwnerReference,
) -> Result<(), ReconcileError> {
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let sts = build_statefulset(name, spec, owner);
    let params = kube::api::PatchParams::apply("meilisearch-operator").force();
    let _ = sts_api
        .patch(name, &params, &kube::api::Patch::Apply(&sts))
        .await?;
    Ok(())
}

fn build_service(name: &str, port: u16, owner: &OwnerReference) -> Service {
    Service {
        metadata: kube::core::ObjectMeta {
            name: Some(name.to_string()),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::core::v1::ServiceSpec {
            selector: Some(std::collections::BTreeMap::from([(
                String::from("app"),
                name.to_string(),
            )])),
            ports: Some(vec![k8s_openapi::api::core::v1::ServicePort {
                port: port as i32,
                target_port: Some(
                    k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(port as i32),
                ),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_statefulset(name: &str, spec: &ServerSpec, owner: &OwnerReference) -> StatefulSet {
    let image = spec
        .image
        .clone()
        .unwrap_or_else(|| "getmeili/meilisearch:latest".into());
    let port = spec.port as i32;
    let has_storage = spec.storage.is_some();
    StatefulSet {
        metadata: kube::core::ObjectMeta {
            name: Some(name.to_string()),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::apps::v1::StatefulSetSpec {
            service_name: Some(name.to_string()),
            replicas: Some(spec.replicas),
            selector: LabelSelector {
                match_labels: Some(std::collections::BTreeMap::from([(
                    String::from("app"),
                    name.to_string(),
                )])),
                ..Default::default()
            },
            persistent_volume_claim_retention_policy: Some(
                k8s_openapi::api::apps::v1::StatefulSetPersistentVolumeClaimRetentionPolicy {
                    when_deleted: Some("Delete".into()),
                    when_scaled: Some("Retain".into()),
                },
            ),
            template: k8s_openapi::api::core::v1::PodTemplateSpec {
                metadata: Some(kube::core::ObjectMeta {
                    labels: Some(std::collections::BTreeMap::from([(
                        String::from("app"),
                        name.to_string(),
                    )])),
                    ..Default::default()
                }),
                spec: Some(k8s_openapi::api::core::v1::PodSpec {
                    containers: vec![k8s_openapi::api::core::v1::Container {
                        name: "meilisearch".into(),
                        image: Some(image),
                        args: Some(vec![
                            "meilisearch".into(),
                            "--http-addr".into(),
                            format!("0.0.0.0:{}", port),
                        ]),
                        ports: Some(vec![k8s_openapi::api::core::v1::ContainerPort {
                            container_port: port,
                            ..Default::default()
                        }]),
                        env_from: None,
                        env: Some(vec![k8s_openapi::api::core::v1::EnvVar {
                            name: "MEILI_MASTER_KEY".into(),
                            value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                                secret_key_ref: Some(
                                    k8s_openapi::api::core::v1::SecretKeySelector {
                                        name: format!("{}-meili-master", name),
                                        key: "masterKey".into(),
                                        optional: Some(false),
                                    },
                                ),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }]),
                        liveness_probe: Some(k8s_openapi::api::core::v1::Probe {
                            http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                                path: Some("/health".into()),
                                port:
                                    k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                        port,
                                    ),
                                scheme: Some("HTTP".into()),
                                ..Default::default()
                            }),
                            initial_delay_seconds: Some(5),
                            period_seconds: Some(5),
                            timeout_seconds: Some(2),
                            ..Default::default()
                        }),
                        readiness_probe: Some(k8s_openapi::api::core::v1::Probe {
                            http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                                path: Some("/health".into()),
                                port:
                                    k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                        port,
                                    ),
                                scheme: Some("HTTP".into()),
                                ..Default::default()
                            }),
                            initial_delay_seconds: Some(3),
                            period_seconds: Some(5),
                            timeout_seconds: Some(2),
                            ..Default::default()
                        }),
                        volume_mounts: if has_storage {
                            Some(vec![k8s_openapi::api::core::v1::VolumeMount {
                                name: "data".into(),
                                mount_path: "/meili_data".into(),
                                ..Default::default()
                            }])
                        } else {
                            None
                        },
                        ..Default::default()
                    }],
                    volumes: None,
                    ..Default::default()
                }),
            },
            volume_claim_templates: if let Some(size) = spec.storage.as_ref() {
                Some(vec![k8s_openapi::api::core::v1::PersistentVolumeClaim {
                    metadata: kube::core::ObjectMeta {
                        name: Some("data".into()),
                        ..Default::default()
                    },
                    spec: Some(k8s_openapi::api::core::v1::PersistentVolumeClaimSpec {
                        access_modes: Some(vec!["ReadWriteOnce".into()]),
                        resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                            requests: Some(std::collections::BTreeMap::from([(
                                String::from("storage"),
                                k8s_openapi::apimachinery::pkg::api::resource::Quantity(
                                    size.clone(),
                                ),
                            )])),
                            limits: None,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }])
            } else {
                None
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn wait_meili_healthy(endpoint: &str, master_key: &str) -> Result<(), ReconcileError> {
    wait_meili_healthy_with(endpoint, master_key, Duration::from_secs(2), 120).await
}

async fn wait_meili_healthy_with(
    endpoint: &str,
    _master_key: &str,
    interval: Duration,
    max_attempts: u32,
) -> Result<(), ReconcileError> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .map_err(anyhow::Error::from)?;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let res = http.get(format!("{}/health", endpoint)).send().await;
        if let Ok(r) = res
            && r.status().is_success()
        {
            return Ok(());
        }
        if attempts > max_attempts {
            return Err(ReconcileError::Anyhow(anyhow::anyhow!(
                "Meilisearch not healthy in time"
            )));
        }
        tokio::time::sleep(interval).await;
    }
}

fn owner_ref(server: &Server) -> OwnerReference {
    OwnerReference {
        api_version: "meili.operator.dev/v1beta1".into(),
        kind: "Server".into(),
        name: server.metadata.name.clone().unwrap_or_default(),
        uid: server.metadata.uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

async fn ensure_finalizer(
    client: &Client,
    ns: &str,
    name: &str,
    server: &Server,
) -> Result<(), ReconcileError> {
    if server.finalizers().iter().any(|f| f == FINALIZER) {
        return Ok(());
    }
    let api: Api<Server> = Api::namespaced(client.clone(), ns);
    let pp = kube::api::PatchParams::apply("meilisearch-operator");
    let patch = serde_json::json!({"metadata": {"finalizers": [FINALIZER]}});
    let _ = api
        .patch(name, &pp, &kube::api::Patch::Merge(&patch))
        .await?;
    Ok(())
}

async fn remove_finalizer(client: &Client, ns: &str, name: &str) -> Result<(), ReconcileError> {
    let api: Api<Server> = Api::namespaced(client.clone(), ns);
    // Remove by setting empty list if ours is the only one; otherwise filter requires GET first, but Merge with null removes all
    let pp = kube::api::PatchParams::apply("meilisearch-operator");
    let patch = serde_json::json!({"metadata": {"finalizers": null}});
    let _ = api
        .patch(name, &pp, &kube::api::Patch::Merge(&patch))
        .await?;
    Ok(())
}

async fn delete_operator_copy(
    client: &Client,
    op_ns: &str,
    ns: &str,
    name: &str,
) -> Result<(), ReconcileError> {
    use k8s_openapi::api::core::v1::Secret;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), op_ns);
    let sec_name = format!("{}-{}-meili-master", ns, name);
    let dp = kube::api::DeleteParams::default();
    match secrets.delete(&sec_name, &dp).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests_server_controller {
    use super::*;
    use axum::http::{StatusCode, header::CONTENT_TYPE};
    use axum::{Router, routing::get};
    use std::net::SocketAddr;
    use tokio::task::JoinHandle;

    fn owner() -> OwnerReference {
        OwnerReference {
            api_version: "meili.operator.dev/v1beta1".into(),
            kind: "Server".into(),
            name: "test".into(),
            uid: "uid".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    #[test]
    fn builds_service_and_statefulset_specs() {
        let spec = ServerSpec {
            image: Some("getmeili/meilisearch:v1.11.1".into()),
            replicas: 1,
            storage: Some("5Gi".into()),
            service_type: "ClusterIP".into(),
            port: 7700,
        };
        let svc = build_service("meili-a", 7700, &owner());
        assert_eq!(svc.metadata.name.as_deref(), Some("meili-a"));
        assert_eq!(
            svc.spec.as_ref().unwrap().ports.as_ref().unwrap()[0].port,
            7700
        );

        let sts = build_statefulset("meili-a", &spec, &owner());
        let tmpl = sts.spec.as_ref().unwrap().template.clone();
        let c = &tmpl.spec.as_ref().unwrap().containers[0];
        assert_eq!(c.args.as_ref().unwrap()[0], "meilisearch");
        assert!(matches!(
            sts.spec
                .as_ref()
                .unwrap()
                .persistent_volume_claim_retention_policy
                .as_ref()
                .unwrap()
                .when_deleted
                .as_deref(),
            Some("Delete")
        ));
    }

    #[tokio::test]
    async fn wait_meili_healthy_succeeds_quickly() {
        // Start a tiny HTTP server that always returns 200 for /health
        let app = Router::new().route(
            "/health",
            get(|| async {
                (
                    StatusCode::OK,
                    [(CONTENT_TYPE, "application/json")],
                    r#"{\"status\":\"available\"}"#,
                )
            }),
        );
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let local = listener.local_addr().unwrap();
        let server: JoinHandle<()> =
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let endpoint = format!("http://{}", local);
        let res = wait_meili_healthy_with(&endpoint, "unused", Duration::from_millis(10), 5).await;
        assert!(res.is_ok());
        server.abort();
    }
}
