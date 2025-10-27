use kube::{
    Api, Client, ResourceExt,
    runtime::controller::{Action, Controller},
};
use meilisearch_sdk::{
    client::Client as MeiliClient,
    key::{Action as MeiliAction, KeyBuilder},
};
use std::sync::Arc;
use tokio::time::Duration;
use tracing::error;

use crate::{
    crds::index::{Index, IndexStatus},
    error::ReconcileError,
};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
}

pub fn controller(client: Client) -> Controller<Index> {
    let api: Api<Index> = Api::all(client.clone());
    Controller::new(api, Default::default()).shutdown_on_signal()
}

const FINALIZER: &str = "meili.operator.dev/finalizer";

pub async fn reconcile(idx: Arc<Index>, ctx: Arc<Ctx>) -> Result<Action, ReconcileError> {
    let ns = idx.namespace().unwrap();
    let name = idx.name_any();
    let server = &idx.spec.server_ref;
    let mut status_message: Option<String> = None;

    // Handle deletion via finalizer
    if idx.metadata.deletion_timestamp.is_some() {
        // If the referenced Server is being deleted, skip Meilisearch calls and just remove our finalizer.
        if !server_is_deleting(&ctx.client, &ns, server).await?
            && idx.spec.delete_on_finalize
        {
            let endpoint = format!("http://{}.{}.svc.cluster.local:7700", server, ns);
            let master_key = get_master_key(&ctx.client, &ns, server).await?;
            let client = MeiliClient::new(&endpoint, Some(&master_key))?;
            let task = client.delete_index(&idx.spec.uid).await?;
            let _ = task.wait_for_completion(&client, None, None).await?;
        }
        remove_finalizer(&ctx.client, &ns, &name).await?;
        return Ok(Action::await_change());
    }

    ensure_finalizer(&ctx.client, &ns, &name, &idx).await?;

    let endpoint = format!("http://{}.{}.svc.cluster.local:7700", server, ns);
    let master_key = get_master_key(&ctx.client, &ns, server).await?;
    let client = MeiliClient::new(&endpoint, Some(&master_key))?;

    // Ensure index exists
    let task = client
        .create_index(&idx.spec.uid, idx.spec.primary_key.as_deref())
        .await?;
    let _ = task.wait_for_completion(&client, None, None).await?;

    // Optionally create an admin key scoped to this index and store it in a Secret
    if let Some(ak) = &idx.spec.admin_key
        && ak.create
    {
        // First, try to adopt an existing matching key to avoid duplicates
        if let Some(existing) =
            find_matching_admin_key_http(&endpoint, &master_key, &idx.spec.uid).await?
        {
            let target_ns = ak.secret_namespace.clone().unwrap_or_else(|| ns.clone());
            let secret_name = ak
                .secret_name
                .clone()
                .unwrap_or_else(|| format!("{}-admin-key", idx.spec.uid));
            store_index_key_secret(
                &ctx.client,
                &ns,
                &name,
                &target_ns,
                &secret_name,
                &existing.key,
                &idx,
            )
            .await?;
            status_message = Some("adopted existing admin key".into());
        } else {
            let mut kb = KeyBuilder::new();
            kb.with_actions(vec![MeiliAction::All]);
            kb.with_indexes(vec![idx.spec.uid.clone()]);
            kb.with_name(format!("{}-admin", idx.spec.uid));
            kb.with_description(format!("Admin key for index {}", idx.spec.uid));
            let created = kb.execute(&client).await?;

            let target_ns = ak.secret_namespace.clone().unwrap_or_else(|| ns.clone());
            let secret_name = ak
                .secret_name
                .clone()
                .unwrap_or_else(|| format!("{}-admin-key", idx.spec.uid));
            store_index_key_secret(
                &ctx.client,
                &ns,
                &name,
                &target_ns,
                &secret_name,
                &created.key,
                &idx,
            )
            .await?;
        }
    }

    // Update status
    let status = IndexStatus {
        ready: true,
        message: status_message,
    };
    let pp = kube::api::PatchParams::apply("meilisearch-operator");
    let api: Api<Index> = Api::namespaced(ctx.client.clone(), &ns);
    let _ = api
        .patch_status(
            &name,
            &pp,
            &kube::api::Patch::Merge(serde_json::json!({"status": status })),
        )
        .await?;

    Ok(Action::requeue(Duration::from_secs(600)))
}

pub fn error_policy(_idx: Arc<Index>, err: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    error!(error = ?err, "index reconcile failed");
    Action::requeue(Duration::from_secs(60))
}

async fn server_is_deleting(client: &Client, ns: &str, name: &str) -> Result<bool, ReconcileError> {
    use crate::crds::server::Server;
    let api: Api<Server> = Api::namespaced(client.clone(), ns);
    if let Some(srv) = api.get_opt(name).await? {
        Ok(srv.metadata.deletion_timestamp.is_some())
    } else {
        // Treat missing as deleted/going away
        Ok(true)
    }
}

async fn get_master_key(client: &Client, ns: &str, server: &str) -> Result<String, ReconcileError> {
    use k8s_openapi::api::core::v1::Secret;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let name = format!("{}-meili-master", server);
    let sec = secrets.get(&name).await?;
    let data = sec
        .data
        .ok_or_else(|| anyhow::anyhow!("secret data missing"))?;
    let val = data
        .get("masterKey")
        .ok_or_else(|| anyhow::anyhow!("missing key"))?;
    Ok(String::from_utf8(val.0.clone())?)
}

async fn store_index_key_secret(
    client: &Client,
    owner_ns: &str,
    owner_name: &str,
    target_ns: &str,
    name: &str,
    key: &str,
    idx: &Index,
) -> Result<(), ReconcileError> {
    use k8s_openapi::api::core::v1::Secret;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), target_ns);
    let owner_ref = if owner_ns == target_ns {
        Some(vec![
            k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                api_version: "meili.operator.dev/v1alpha1".into(),
                kind: "Index".into(),
                name: owner_name.to_string(),
                uid: idx.metadata.uid.clone().unwrap_or_default(),
                controller: Some(true),
                block_owner_deletion: Some(true),
            },
        ])
    } else {
        None
    };
    let sec = Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(name.to_string()),
            owner_references: owner_ref,
            ..Default::default()
        },
        string_data: Some(std::collections::BTreeMap::from([(
            String::from("key"),
            key.to_string(),
        )])),
        ..Default::default()
    };
    let pp = kube::api::PostParams::default();
    let _ = secrets.create(&pp, &sec).await.or_else(|e| match e {
        kube::Error::Api(ae) if ae.code == 409 => Ok(Secret::default()),
        _ => Err(e),
    })?;
    Ok(())
}

// -------- Matching existing admin key via HTTP API --------

#[derive(Debug, serde::Deserialize)]
struct KeyItem {
    name: Option<String>,
    description: Option<String>,
    key: String,
    #[allow(dead_code)]
    uid: String,
    actions: Vec<String>,
    indexes: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct KeysPage {
    results: Vec<KeyItem>,
    #[allow(dead_code)]
    offset: usize,
    #[allow(dead_code)]
    limit: usize,
    total: usize,
}

async fn list_all_keys_http(
    endpoint: &str,
    master_key: &str,
) -> Result<Vec<KeyItem>, ReconcileError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(anyhow::Error::from)?;
    let mut out = Vec::new();
    let mut offset = 0usize;
    let limit = 1000usize;
    loop {
        let url = format!("{}/keys?offset={}&limit={}", endpoint, offset, limit);
        let resp = client
            .get(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", master_key),
            )
            .send()
            .await
            .map_err(anyhow::Error::from)?
            .error_for_status()
            .map_err(anyhow::Error::from)?
            .json::<KeysPage>()
            .await
            .map_err(anyhow::Error::from)?;
        offset += resp.results.len();
        out.extend(resp.results);
        if offset >= resp.total {
            break;
        }
    }
    Ok(out)
}

fn eq_unordered<T: Eq + std::hash::Hash + Clone>(a: &[T], b: &[T]) -> bool {
    use std::collections::HashSet;
    let sa: HashSet<T> = a.iter().cloned().collect();
    let sb: HashSet<T> = b.iter().cloned().collect();
    sa == sb
}

fn matches_admin(index_uid: &str, item: &KeyItem) -> bool {
    let expected_name = format!("{}-admin", index_uid);
    let expected_desc = format!("Admin key for index {}", index_uid);
    let actions_ok = item.actions.iter().any(|a| a == "*");
    let indexes_ok = eq_unordered(&[index_uid.to_string()], &item.indexes);
    let name_ok = item
        .name
        .as_ref()
        .map(|s| s == &expected_name)
        .unwrap_or(false);
    let desc_ok = item
        .description
        .as_ref()
        .map(|s| s == &expected_desc)
        .unwrap_or(false);
    actions_ok && indexes_ok && name_ok && desc_ok
}

async fn find_matching_admin_key_http(
    endpoint: &str,
    master_key: &str,
    index_uid: &str,
) -> Result<Option<KeyItem>, ReconcileError> {
    let all = list_all_keys_http(endpoint, master_key).await?;
    Ok(all.into_iter().find(|k| matches_admin(index_uid, k)))
}

async fn ensure_finalizer(
    client: &Client,
    ns: &str,
    name: &str,
    idx: &Index,
) -> Result<(), ReconcileError> {
    if idx.finalizers().iter().any(|f| f == FINALIZER) {
        return Ok(());
    }
    let mut finals: Vec<String> = idx.finalizers().to_vec();
    finals.push(FINALIZER.to_string());
    let api: Api<Index> = Api::namespaced(client.clone(), ns);
    let pp = kube::api::PatchParams::default();
    let patch = serde_json::json!({"metadata": {"finalizers": finals}});
    let _ = api
        .patch(name, &pp, &kube::api::Patch::Merge(&patch))
        .await?;
    Ok(())
}

async fn remove_finalizer(client: &Client, ns: &str, name: &str) -> Result<(), ReconcileError> {
    let api: Api<Index> = Api::namespaced(client.clone(), ns);
    let pp = kube::api::PatchParams::default();
    let patch = serde_json::json!({"metadata": {"finalizers": null}});
    let _ = api
        .patch(name, &pp, &kube::api::Patch::Merge(&patch))
        .await?;
    Ok(())
}
