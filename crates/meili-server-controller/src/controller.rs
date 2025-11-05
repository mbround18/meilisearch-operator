use std::sync::Arc;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{Container, EnvVar, EnvVarSource, PersistentVolumeClaim, PodSpec, PodTemplateSpec, Secret, SecretKeySelector, Service, Volume, VolumeMount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use kube::{
    Api, Client, ResourceExt,
    runtime::controller::{Action, Controller},
};
use meili_crds::server::{IncompatiblePolicy, Server, ServerSpec, ServerStatus};
use meili_shared::error::ReconcileError;
use rand::{Rng, distr::Alphanumeric};
use tokio::time::Duration;
use tracing::error;

const FINALIZER: &str = "meili.operator.dev/finalizer";

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub operator_namespace: String,
}

pub fn controller(client: Client) -> Controller<Server> {
    let api: Api<Server> = Api::all(client.clone());
    Controller::new(api, Default::default()).shutdown_on_signal()
}

pub async fn reconcile(server: Arc<Server>, ctx: Arc<Ctx>) -> Result<Action, ReconcileError> {
    let ns = server.namespace().unwrap();
    let name = server.name_any();

    if server.metadata.deletion_timestamp.is_some() {
        fast_delete_children(&ctx.client, &ns, &name).await?;
        delete_operator_copy(&ctx.client, &ctx.operator_namespace, &ns, &name).await?;
        remove_finalizer(&ctx.client, &ns, &name).await?;
        return Ok(Action::await_change());
    }

    ensure_finalizer(&ctx.client, &ns, &name, &server).await?;

    let owner = owner_ref(&server);
    let master_key = ensure_master_key_secret(&ctx.client, &ns, &name, &owner).await?;
    ensure_operator_copy(
        &ctx.client,
        &ctx.operator_namespace,
        &ns,
        &name,
        &master_key,
    )
    .await?;

    ensure_service(&ctx.client, &ns, &name, server.spec.port, &owner).await?;
    ensure_statefulset(&ctx.client, &ns, &name, &server.spec, &owner).await?;

    let endpoint = format!("http://{}.{}.svc:{}", name, ns, server.spec.port);
    match wait_meili_healthy(&endpoint, &master_key).await {
        Ok(_) => {}
        Err(e) => {
            let incompatible = is_meili_incompatible(&ctx.client, &ns, &name).await.unwrap_or(false);
            if incompatible {
                // Prefer migration when enabled; else follow incompatible_policy
                if server.spec.data.migrate_on_update {
                    let desired_image = server.spec.image.clone().unwrap_or_else(|| "getmeili/meilisearch:latest".into());
                    let last_image = server
                        .annotations()
                        .get("meili.operator.dev/last-image")
                        .cloned();
                    if let Some(old_image) = last_image {
                        run_migration(&ctx.client, &ns, &name, &old_image, &desired_image).await?;
                        return Ok(Action::requeue(Duration::from_secs(10)));
                    } else {
                        emit_event(&ctx.client, &ns, &name, "Normal", "MigrationSkipped", "No previous image annotation; cannot migrate").await.ok();
                    }
                }
                if matches!(server.spec.incompatible_policy, IncompatiblePolicy::ResetData) {
                    reset_meili_data(&ctx.client, &ns, &name, server.spec.replicas, server.spec.storage.is_some()).await?;
                    return Ok(Action::requeue(Duration::from_secs(10)));
                }
            }
            return Err(e);
        }
    }

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

    // Track last deployed image for future migrations
    if let Some(img) = server.spec.image.clone() {
        let patch = serde_json::json!({"metadata": {"annotations": {"meili.operator.dev/last-image": img}}});
        let _ = servers.patch(&name, &ss_apply, &kube::api::Patch::Merge(&patch)).await?;
    }

    Ok(Action::requeue(Duration::from_secs(300)))
}

pub fn error_policy(_server: Arc<Server>, err: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    error!(error=?err, "reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

async fn fast_delete_children(
    client: &Client,
    ns: &str,
    server_name: &str,
) -> Result<(), ReconcileError> {
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

    // Keys
    {
        use meili_crds::key::Key;
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
    // Indexes
    {
        use meili_crds::index::Index;
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
    let key: String = rand::rng()
        .sample_iter(Alphanumeric)
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
    // Compute a simple FNV-1a 64-bit hash over the ServerSpec JSON to force rollout on changes
    let spec_json = serde_json::to_string(spec).unwrap_or_default();
    let spec_hash = fnv1a64(&spec_json);
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
                    annotations: Some(std::collections::BTreeMap::from([(
                        String::from("meili.operator.dev/spec-hash"),
                        spec_hash,
                    )])),
                    ..Default::default()
                }),
                spec: Some(k8s_openapi::api::core::v1::PodSpec {
                    containers: vec![k8s_openapi::api::core::v1::Container {
                        name: "meilisearch".into(),
                        image: Some(image),
                        args: Some(vec![
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

// Simple FNV-1a 64-bit hash for stable annotation value without extra deps
fn fnv1a64(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325; // offset basis
    let prime: u64 = 0x100000001b3;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(prime);
    }
    format!("{hash:016x}")
}

async fn wait_meili_healthy(endpoint: &str, master_key: &str) -> Result<(), ReconcileError> {
    let _ = master_key; // unused for now
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

// Inspect pods to see if Meilisearch crashed due to version incompatibility
async fn is_meili_incompatible(client: &Client, ns: &str, name: &str) -> Result<bool, ReconcileError> {
    use k8s_openapi::api::core::v1::Pod;
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    // Pods are labeled app=name
    let lp = kube::api::ListParams::default().labels(&format!("app={}", name));
    let list = pods.list(&lp).await?;
    for p in list.items.iter() {
        if let Some(status) = &p.status {
            if let Some(cs) = &status.container_statuses {
                for c in cs {
                    if let Some(state) = &c.last_state {
                        if let Some(term) = &state.terminated {
                            let msg = term.message.clone().unwrap_or_default();
                            let reason = term.reason.clone().unwrap_or_default();
                            if msg.contains("Your database version")
                                && msg.contains("incompatible with your current engine version")
                            {
                                return Ok(true);
                            }
                            if reason.contains("Error") && msg.contains("incompatible") {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(false)
}

// Reset data by scaling down, deleting PVCs, and scaling back up
async fn reset_meili_data(
    client: &Client,
    ns: &str,
    name: &str,
    replicas: i32,
    has_storage: bool,
) -> Result<(), ReconcileError> {
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    // scale to 0
    let pp = kube::api::PatchParams::apply("meilisearch-operator");
    let scale0 = serde_json::json!({"spec": {"replicas": 0}});
    let _ = sts_api
        .patch(name, &pp, &kube::api::Patch::Merge(&scale0))
        .await?;

    // delete PVCs if using persistent storage
    if has_storage {
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
        // StatefulSet PVCs follow pattern: <claimName>-<podName>, claimName is "data"
        let prefix = format!("data-{}-", name);
        let list = pvcs.list(&kube::api::ListParams::default()).await?;
        for pvc in list.items.iter() {
            if let Some(pvc_name) = pvc.metadata.name.as_deref() {
                if pvc_name.starts_with(&prefix) {
                    let _ = pvcs.delete(pvc_name, &kube::api::DeleteParams::default()).await;
                }
            }
        }
    }

    // scale back to desired replicas
    let scale_up = serde_json::json!({"spec": {"replicas": replicas}});
    let _ = sts_api
        .patch(name, &pp, &kube::api::Patch::Merge(&scale_up))
        .await?;
    Ok(())
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
    let secrets: Api<Secret> = Api::namespaced(client.clone(), op_ns);
    let sec_name = format!("{}-{}-meili-master", ns, name);
    let dp = kube::api::DeleteParams::default();
    match secrets.delete(&sec_name, &dp).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn emit_event(client: &Client, ns: &str, name: &str, type_: &str, reason: &str, message: &str) -> Result<(), ReconcileError> {
    use k8s_openapi::api::core::v1::Event;
    let events: Api<Event> = Api::namespaced(client.clone(), ns);
    let ev = Event {
        metadata: kube::core::ObjectMeta {
            generate_name: Some(format!("{}-", name)),
            ..Default::default()
        },
        involved_object: k8s_openapi::api::core::v1::ObjectReference {
            api_version: Some("meili.operator.dev/v1beta1".into()),
            kind: Some("Server".into()),
            name: Some(name.into()),
            namespace: Some(ns.into()),
            ..Default::default()
        },
        reason: Some(reason.into()),
        message: Some(message.into()),
        type_: Some(type_.into()),
        event_time: None,
        first_timestamp: None,
        last_timestamp: None,
        ..Default::default()
    };
    let _ = events.create(&kube::api::PostParams::default(), &ev).await;
    Ok(())
}

async fn run_migration(client: &Client, ns: &str, name: &str, old_image: &str, new_image: &str) -> Result<(), ReconcileError> {
    // Only support single replica with PVC for now
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let sts = sts_api.get(name).await?;
    let replicas = sts.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let has_pvc = sts
        .spec
        .as_ref()
        .and_then(|s| s.volume_claim_templates.as_ref())
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if replicas != 1 || !has_pvc {
        emit_event(client, ns, name, "Warning", "MigrationUnsupported", "Migration requires replicas=1 with persistent storage").await.ok();
        return Ok(());
    }

    emit_event(client, ns, name, "Normal", "MigrationStarted", &format!("from {} to {}", old_image, new_image)).await.ok();

    // scale sts to 0
    let pp = kube::api::PatchParams::apply("meilisearch-operator");
    let _ = sts_api.patch(name, &pp, &kube::api::Patch::Merge(&serde_json::json!({"spec": {"replicas": 0}}))).await?;

    // Ensure dump job
    match ensure_dump_job(client, ns, name, old_image).await? {
        JobPhase::Failed => {
            emit_event(client, ns, name, "Warning", "MigrationFailed", "dump job failed").await.ok();
            return Ok(());
        }
        JobPhase::Running | JobPhase::Pending => return Ok(()),
        JobPhase::Succeeded => {}
    }

    // Ensure import job
    match ensure_import_job(client, ns, name, new_image).await? {
        JobPhase::Failed => {
            emit_event(client, ns, name, "Warning", "MigrationFailed", "import job failed").await.ok();
            return Ok(());
        }
        JobPhase::Running | JobPhase::Pending => return Ok(()),
        JobPhase::Succeeded => {}
    }

    emit_event(client, ns, name, "Normal", "MigrationComplete", &format!("from {} to {}", old_image, new_image)).await.ok();

    // scale back to original replicas
    let _ = sts_api.patch(name, &pp, &kube::api::Patch::Merge(&serde_json::json!({"spec": {"replicas": replicas}}))).await?;
    Ok(())
}

#[derive(PartialEq, Eq)]
enum JobPhase { Pending, Running, Succeeded, Failed }

async fn ensure_dump_job(client: &Client, ns: &str, name: &str, image: &str) -> Result<JobPhase, ReconcileError> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let job_name = format!("{}-migrate-dump", name);
    if let Some(job) = jobs.get_opt(&job_name).await? {
        return Ok(job_phase(&job));
    }
    let job = build_dump_job(&job_name, name, image);
    let params = kube::api::PatchParams::apply("meilisearch-operator").force();
    let _ = jobs.patch(&job_name, &params, &kube::api::Patch::Apply(&job)).await?;
    Ok(JobPhase::Pending)
}

async fn ensure_import_job(client: &Client, ns: &str, name: &str, image: &str) -> Result<JobPhase, ReconcileError> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let job_name = format!("{}-migrate-import", name);
    if let Some(job) = jobs.get_opt(&job_name).await? {
        return Ok(job_phase(&job));
    }
    let job = build_import_job(&job_name, name, image);
    let params = kube::api::PatchParams::apply("meilisearch-operator").force();
    let _ = jobs.patch(&job_name, &params, &kube::api::Patch::Apply(&job)).await?;
    Ok(JobPhase::Pending)
}

fn job_phase(job: &Job) -> JobPhase {
    if let Some(st) = &job.status {
        if let Some(s) = st.succeeded { if s > 0 { return JobPhase::Succeeded; } }
        if let Some(f) = st.failed { if f > 0 { return JobPhase::Failed; } }
        if let Some(a) = st.active { if a > 0 { return JobPhase::Running; } }
    }
    JobPhase::Pending
}

fn build_dump_job(job_name: &str, server_name: &str, image: &str) -> Job {
    let pvc = format!("data-{}-0", server_name);
    let cmd = vec![
        "/bin/sh".into(),
        "-c".into(),
        "set -e; MEILI_ADDR=127.0.0.1:7700; \
meilisearch --http-addr $MEILI_ADDR --db-path /meili_data --dump-dir /meili_data/dumps --master-key \"$MASTER_KEY\" & pid=$!; \
for i in $(seq 1 120); do curl -sf -H \"Authorization: Bearer $MASTER_KEY\" http://$MEILI_ADDR/health && break || true; sleep 1; done; \
curl -sf -X POST -H \"Authorization: Bearer $MASTER_KEY\" http://$MEILI_ADDR/dumps >/tmp/dump.json; \
for i in $(seq 1 600); do status=$(curl -sf -H \"Authorization: Bearer $MASTER_KEY\" http://$MEILI_ADDR/tasks?limit=1 | sed -n 's/.*\"status\":\"\\([a-z]*\\)\".*/\\1/p' | head -n1); [ \"$status\" = \"succeeded\" ] && break; [ \"$status\" = \"failed\" ] && exit 1; sleep 2; done; \
kill $pid || true; wait $pid || true; ls -t /meili_data/dumps/*.dump | head -n1 > /meili_data/dumps/LATEST;".into(),
    ];
    build_job(job_name, server_name, image, &pvc, &cmd)
}

fn build_import_job(job_name: &str, server_name: &str, image: &str) -> Job {
    let pvc = format!("data-{}-0", server_name);
    let cmd = vec![
        "/bin/sh".into(),
        "-c".into(),
        "set -e; dump=$(cat /meili_data/dumps/LATEST); [ -f \"$dump\" ]; \
meilisearch --http-addr 127.0.0.1:7700 --db-path /meili_data --dump-dir /meili_data/dumps --import-dump \"$dump\" --master-key \"$MASTER_KEY\" & pid=$!; \
for i in $(seq 1 300); do curl -sf http://127.0.0.1:7700/health && break || true; sleep 2; done; \
sleep 10; kill $pid || true; wait $pid || true;".into(),
    ];
    build_job(job_name, server_name, image, &pvc, &cmd)
}

fn build_job(job_name: &str, server_name: &str, image: &str, pvc_name: &str, command: &[String]) -> Job {
    let env = vec![EnvVar {
        name: "MASTER_KEY".into(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: format!("{}-meili-master", server_name),
                key: "masterKey".into(),
                optional: Some(false),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let container = Container {
        name: "migrate".into(),
        image: Some(image.into()),
        command: Some(command.to_vec()),
        volume_mounts: Some(vec![VolumeMount { name: "data".into(), mount_path: "/meili_data".into(), ..Default::default() }]),
        env: Some(env),
        ..Default::default()
    };
    let pod_spec = PodSpec {
        containers: vec![container],
        restart_policy: Some("OnFailure".into()),
        volumes: Some(vec![Volume { name: "data".into(), persistent_volume_claim: Some(k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource { claim_name: pvc_name.into(), ..Default::default() }), ..Default::default() }]),
        ..Default::default()
    };
    Job {
        metadata: kube::core::ObjectMeta { name: Some(job_name.into()), ..Default::default() },
        spec: Some(JobSpec {
            template: PodTemplateSpec { metadata: Some(kube::core::ObjectMeta { ..Default::default() }), spec: Some(pod_spec) },
            backoff_limit: Some(1),
            ..Default::default()
        }),
        status: None,
    }
}

#[cfg(test)]
mod tests_server_controller {
    use super::*;
    use axum::http::{StatusCode, header::CONTENT_TYPE};
    use axum::{Router, routing::get};
    use std::net::SocketAddr;

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
            incompatible_policy: IncompatiblePolicy::Fail,
            data: meili_crds::server::DataSpec { migrate_on_update: true },
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
        assert_eq!(c.args.as_ref().unwrap()[0], "--http-addr");
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
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let endpoint = format!("http://{}", local);
        let res = wait_meili_healthy_with(&endpoint, "unused", Duration::from_millis(10), 5).await;
        assert!(res.is_ok());
        server.abort();
    }
}
