use kube::{
    Api, Client, ResourceExt,
    runtime::controller::{Action, Controller},
};
use meilisearch_sdk::{
    client::Client as MeiliClient,
    key::{Action as MeiliAction, KeyBuilder},
};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::time::Duration;
use tracing::error;

use meili_crds::key::{Key, KeyStatus};
use meili_shared::error::ReconcileError;
use meili_shared::name::normalize_kebab_dedup;

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
}

pub fn controller(client: Client) -> Controller<Key> {
    let api: Api<Key> = Api::all(client.clone());
    Controller::new(api, Default::default()).shutdown_on_signal()
}

const FINALIZER: &str = "meili.operator.dev/finalizer";

pub async fn reconcile(key: Arc<Key>, ctx: Arc<Ctx>) -> Result<Action, ReconcileError> {
    let ns = key.namespace().unwrap();
    let name = key.name_any();
    let server = &key.spec.server_ref;
    let endpoint = format!("http://{}.{}.svc:7700", server, ns);
    let master_key = get_master_key(&ctx.client, &ns, server).await?;
    let client = MeiliClient::new(&endpoint, Some(&master_key))?;
    let mut status_message: Option<String> = None;

    if key.metadata.deletion_timestamp.is_some() {
        if !server_is_deleting(&ctx.client, &ns, server).await?
            && let Some(uid) = key.status.as_ref().and_then(|s| s.uid.as_ref())
        {
            client.delete_key(uid).await?;
        }
        remove_finalizer(&ctx.client, &ns, &name).await?;
        return Ok(Action::await_change());
    }

    ensure_finalizer(&ctx.client, &ns, &name, &key).await?;

    if let Some(secret_key) = existing_secret_key(&ctx.client, &key).await?
        && key_exists_by_value_http(&endpoint, &master_key, &secret_key).await?
    {
        store_key_secret(
            &ctx.client,
            &ns,
            &name,
            &key.spec.secret_namespace,
            &key.spec.secret_name,
            &secret_key,
        )
        .await?;
        let status = KeyStatus {
            uid: None,
            ready: true,
            message: Some("using key from existing Secret".into()),
        };
        let pp = kube::api::PatchParams::apply("meilisearch-operator");
        let api: Api<Key> = Api::namespaced(ctx.client.clone(), &ns);
        let _ = api
            .patch_status(
                &name,
                &pp,
                &kube::api::Patch::Merge(serde_json::json!({"status": status })),
            )
            .await?;
        return Ok(Action::requeue(Duration::from_secs(1200)));
    }

    if let Some(existing) = find_matching_key_http(&endpoint, &master_key, &key).await? {
        store_key_secret(
            &ctx.client,
            &ns,
            &name,
            &key.spec.secret_namespace,
            &key.spec.secret_name,
            &existing.key,
        )
        .await?;
        status_message = Some("adopted existing key".into());
        let status = KeyStatus {
            uid: None,
            ready: true,
            message: status_message.clone(),
        };
        let pp = kube::api::PatchParams::apply("meilisearch-operator");
        let api: Api<Key> = Api::namespaced(ctx.client.clone(), &ns);
        let _ = api
            .patch_status(
                &name,
                &pp,
                &kube::api::Patch::Merge(serde_json::json!({"status": status })),
            )
            .await?;
        return Ok(Action::requeue(Duration::from_secs(1200)));
    } else if let Some(existing) =
        find_relaxed_matching_key_http(&endpoint, &master_key, &key).await?
    {
        store_key_secret(
            &ctx.client,
            &ns,
            &name,
            &key.spec.secret_namespace,
            &key.spec.secret_name,
            &existing.key,
        )
        .await?;
        status_message = Some("adopted similar existing key".into());
        let status = KeyStatus {
            uid: None,
            ready: true,
            message: status_message.clone(),
        };
        let pp = kube::api::PatchParams::apply("meilisearch-operator");
        let api: Api<Key> = Api::namespaced(ctx.client.clone(), &ns);
        let _ = api
            .patch_status(
                &name,
                &pp,
                &kube::api::Patch::Merge(serde_json::json!({"status": status })),
            )
            .await?;
        return Ok(Action::requeue(Duration::from_secs(1200)));
    }

    let mut kb = KeyBuilder::new();
    let desired_name = key.spec.name.clone().unwrap_or_else(|| name.clone());
    let desired_name = normalize_kebab_dedup(&desired_name);
    kb.with_name(&desired_name);
    if let Some(d) = &key.spec.description {
        kb.with_description(d);
    }
    kb.with_indexes(&key.spec.indexes);
    let actions: Vec<MeiliAction> = key
        .spec
        .actions
        .iter()
        .map(|s| match s.as_str() {
            "*" => MeiliAction::All,
            "search" => MeiliAction::Search,
            "documents.add" => MeiliAction::DocumentsAdd,
            "documents.get" => MeiliAction::DocumentsGet,
            "documents.delete" => MeiliAction::DocumentsDelete,
            "indexes.create" => MeiliAction::IndexesCreate,
            "indexes.get" => MeiliAction::IndexesGet,
            "indexes.update" => MeiliAction::IndexesUpdate,
            "indexes.delete" => MeiliAction::IndexesDelete,
            "tasks.get" => MeiliAction::TasksGet,
            "settings.get" => MeiliAction::SettingsGet,
            "settings.update" => MeiliAction::SettingsUpdate,
            "stats.get" => MeiliAction::StatsGet,
            "dumps.create" => MeiliAction::DumpsCreate,
            "dumps.get" => MeiliAction::DumpsGet,
            "version" => MeiliAction::Version,
            "keys.get" => MeiliAction::KeyGet,
            "keys.create" => MeiliAction::KeyCreate,
            "keys.update" => MeiliAction::KeyUpdate,
            "keys.delete" => MeiliAction::KeyDelete,
            other => MeiliAction::Unknown(other.to_string()),
        })
        .collect();
    kb.with_actions(actions);
    if let Some(exp) = &key.spec.expires_at
        && let Ok(dt) = OffsetDateTime::parse(exp, &time::format_description::well_known::Rfc3339)
    {
        kb.with_expires_at(dt);
    }

    let created = kb.execute(&client).await?;
    store_key_secret(
        &ctx.client,
        &ns,
        &name,
        &key.spec.secret_namespace,
        &key.spec.secret_name,
        &created.key,
    )
    .await?;

    let status = KeyStatus {
        uid: Some(created.uid.clone()),
        ready: true,
        message: status_message,
    };
    let pp = kube::api::PatchParams::apply("meilisearch-operator");
    let api: Api<Key> = Api::namespaced(ctx.client.clone(), &ns);
    let _ = api
        .patch_status(
            &name,
            &pp,
            &kube::api::Patch::Merge(serde_json::json!({"status": status })),
        )
        .await?;
    Ok(Action::requeue(Duration::from_secs(1200)))
}

pub fn error_policy(_key: Arc<Key>, err: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    error!(error=?err, "key reconcile failed");
    Action::requeue(Duration::from_secs(60))
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

// If a Secret already exists at the target location, try to reuse that key value
async fn existing_secret_key(client: &Client, key: &Key) -> Result<Option<String>, ReconcileError> {
    use k8s_openapi::api::core::v1::Secret;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &key.spec.secret_namespace);
    match secrets.get(&key.spec.secret_name).await {
        Ok(sec) => {
            if let Some(sd) = sec.string_data.as_ref()
                && let Some(v) = sd.get("key")
            {
                return Ok(Some(v.clone()));
            }
            if let Some(data) = sec.data.as_ref()
                && let Some(v) = data.get("key")
            {
                return Ok(String::from_utf8(v.0.clone()).ok());
            }
            Ok(None)
        }
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// Verify if a key string exists on the Meilisearch server by listing all keys
async fn key_exists_by_value_http(
    endpoint: &str,
    master_key: &str,
    key_value: &str,
) -> Result<bool, ReconcileError> {
    let all = list_all_keys_http(endpoint, master_key).await?;
    Ok(all.iter().any(|k| k.key == key_value))
}

async fn store_key_secret(
    client: &Client,
    owner_ns: &str,
    owner_name: &str,
    target_ns: &str,
    name: &str,
    key: &str,
) -> Result<(), ReconcileError> {
    use k8s_openapi::api::core::v1::Secret;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), target_ns);
    let owner_ref = if owner_ns == target_ns {
        Some(vec![
            k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                api_version: "meili.operator.dev/v1alpha1".into(),
                kind: "Key".into(),
                name: owner_name.to_string(),
                uid: key_uid(client, owner_ns, owner_name)
                    .await
                    .unwrap_or_default(),
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

#[derive(Debug, serde::Deserialize)]
struct KeyItem {
    name: Option<String>,
    description: Option<String>,
    key: String,
    #[allow(dead_code)]
    uid: String,
    actions: Vec<String>,
    indexes: Vec<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<String>,
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

fn same_string_opt(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        (None, _) => true,
        _ => false,
    }
}

fn parse_rfc3339_opt(s: &Option<String>) -> Option<OffsetDateTime> {
    s.as_ref()
        .and_then(|v| OffsetDateTime::parse(v, &time::format_description::well_known::Rfc3339).ok())
}

fn eq_unordered<T: Eq + std::hash::Hash + Clone>(a: &[T], b: &[T]) -> bool {
    use std::collections::HashSet;
    let sa: HashSet<T> = a.iter().cloned().collect();
    let sb: HashSet<T> = b.iter().cloned().collect();
    sa == sb
}

fn normalize_actions(spec_actions: &[String]) -> Vec<String> {
    spec_actions.iter().map(|s| s.to_string()).collect()
}

fn matches_spec(item: &KeyItem, key: &Key) -> bool {
    if !same_string_opt(&key.spec.name, &item.name) {
        return false;
    }
    if !same_string_opt(&key.spec.description, &item.description) {
        return false;
    }
    if !eq_unordered(&normalize_actions(&key.spec.actions), &item.actions) {
        return false;
    }
    if !eq_unordered(&key.spec.indexes, &item.indexes) {
        return false;
    }
    match (&key.spec.expires_at, &item.expires_at) {
        (Some(se), Some(ie)) => {
            parse_rfc3339_opt(&Some(se.clone())) == parse_rfc3339_opt(&Some(ie.clone()))
        }
        (Some(_), None) => false,
        _ => true,
    }
}

async fn find_matching_key_http(
    endpoint: &str,
    master_key: &str,
    key: &Key,
) -> Result<Option<KeyItem>, ReconcileError> {
    let all = list_all_keys_http(endpoint, master_key).await?;
    Ok(all.into_iter().find(|k| matches_spec(k, key)))
}

fn matches_spec_relaxed(item: &KeyItem, key: &Key) -> bool {
    if !eq_unordered(&normalize_actions(&key.spec.actions), &item.actions) {
        return false;
    }
    if !eq_unordered(&key.spec.indexes, &item.indexes) {
        return false;
    }
    match (&key.spec.expires_at, &item.expires_at) {
        (Some(se), Some(ie)) => {
            parse_rfc3339_opt(&Some(se.clone())) == parse_rfc3339_opt(&Some(ie.clone()))
        }
        (Some(_), None) => false,
        _ => true,
    }
}

async fn find_relaxed_matching_key_http(
    endpoint: &str,
    master_key: &str,
    key: &Key,
) -> Result<Option<KeyItem>, ReconcileError> {
    let all = list_all_keys_http(endpoint, master_key).await?;
    Ok(all.into_iter().find(|k| matches_spec_relaxed(k, key)))
}

async fn ensure_finalizer(
    client: &Client,
    ns: &str,
    name: &str,
    key: &Key,
) -> Result<(), ReconcileError> {
    if key.finalizers().iter().any(|f| f == FINALIZER) {
        return Ok(());
    }
    let mut finals: Vec<String> = key.finalizers().to_vec();
    finals.push(FINALIZER.to_string());
    let api: Api<Key> = Api::namespaced(client.clone(), ns);
    let pp = kube::api::PatchParams::default();
    let patch = serde_json::json!({"metadata": {"finalizers": finals}});
    let _ = api
        .patch(name, &pp, &kube::api::Patch::Merge(&patch))
        .await?;
    Ok(())
}

async fn remove_finalizer(client: &Client, ns: &str, name: &str) -> Result<(), ReconcileError> {
    let api: Api<Key> = Api::namespaced(client.clone(), ns);
    let pp = kube::api::PatchParams::default();
    let patch = serde_json::json!({"metadata": {"finalizers": null}});
    let _ = api
        .patch(name, &pp, &kube::api::Patch::Merge(&patch))
        .await?;
    Ok(())
}

async fn key_uid(client: &Client, ns: &str, name: &str) -> Result<String, ReconcileError> {
    let api: Api<Key> = Api::namespaced(client.clone(), ns);
    let k = api.get(name).await?;
    Ok(k.metadata.uid.unwrap_or_default())
}

async fn server_is_deleting(client: &Client, ns: &str, name: &str) -> Result<bool, ReconcileError> {
    use meili_crds::server::Server;
    let api: Api<Server> = Api::namespaced(client.clone(), ns);
    if let Some(srv) = api.get_opt(name).await? {
        Ok(srv.metadata.deletion_timestamp.is_some())
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod tests_key_controller_helpers {
    use super::*;

    #[test]
    fn same_string_opt_works() {
        assert!(same_string_opt(&None, &Some("x".into())));
        assert!(same_string_opt(&Some("a".into()), &Some("a".into())));
        assert!(!same_string_opt(&Some("a".into()), &Some("b".into())));
    }

    #[test]
    fn parse_rfc3339_opt_parses() {
        let s = Some("2024-01-01T00:00:00Z".to_string());
        assert!(parse_rfc3339_opt(&s).is_some());
        assert!(parse_rfc3339_opt(&None).is_none());
    }

    #[test]
    fn eq_unordered_works() {
        assert!(eq_unordered(&["a", "b"], &["b", "a"]));
        assert!(!eq_unordered(&["a"], &["a", "b"]));
    }

    fn key_item(
        actions: &[&str],
        indexes: &[&str],
        name: Option<&str>,
        desc: Option<&str>,
        exp: Option<&str>,
    ) -> KeyItem {
        KeyItem {
            name: name.map(|s| s.to_string()),
            description: desc.map(|s| s.to_string()),
            key: "k".into(),
            uid: "u".into(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
            indexes: indexes.iter().map(|s| s.to_string()).collect(),
            expires_at: exp.map(|s| s.to_string()),
        }
    }

    fn key_spec(
        name: Option<&str>,
        desc: Option<&str>,
        actions: &[&str],
        indexes: &[&str],
        exp: Option<&str>,
    ) -> Key {
        Key {
            metadata: Default::default(),
            spec: meili_crds::key::KeySpec {
                server_ref: "s".into(),
                name: name.map(|s| s.to_string()),
                description: desc.map(|s| s.to_string()),
                actions: actions.iter().map(|s| s.to_string()).collect(),
                indexes: indexes.iter().map(|s| s.to_string()).collect(),
                expires_at: exp.map(|s| s.to_string()),
                secret_namespace: "ns".into(),
                secret_name: "n".into(),
            },
            status: None,
        }
    }

    #[test]
    fn matches_spec_true_when_all_match() {
        let item = key_item(
            &["*"],
            &["idx"],
            Some("name"),
            Some("desc"),
            Some("2024-01-01T00:00:00Z"),
        );
        let key = key_spec(
            Some("name"),
            Some("desc"),
            &["*"],
            &["idx"],
            Some("2024-01-01T00:00:00Z"),
        );
        assert!(matches_spec(&item, &key));
    }

    #[test]
    fn matches_spec_relaxed_ignores_name_desc() {
        let item = key_item(&["search"], &["idx"], Some("n1"), Some("d1"), None);
        let key = key_spec(Some("n2"), Some("d2"), &["search"], &["idx"], None);
        assert!(matches_spec_relaxed(&item, &key));
    }
}
