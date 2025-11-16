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

use meili_crds::index::{Index, IndexStatus};
use meili_shared::error::ReconcileError;
use meili_shared::name::normalize_kebab_dedup;
use time::OffsetDateTime;

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub pod_name: String,
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
    tracing::Span::current().record("resource.name", tracing::field::display(&name));
    tracing::Span::current().record("resource.namespace", tracing::field::display(&ns));
    tracing::Span::current().record("server.ref", tracing::field::display(server));
    let mut status_message: Option<String> = None;

    // Throttle repeated HTTP checks using annotations with degradable intervals
    const LAST_CHECK_ANN: &str = "meili.operator.dev/last-check";
    const CHECK_COUNT_ANN: &str = "meili.operator.dev/check-count";
    const CHECK_INTERVAL_ANN: &str = "meili.operator.dev/check-interval"; // seconds
    const BASE_INTERVAL_SECS: i64 = 30;
    const DEGRADE_AFTER_CHECKS: i64 = 10; // ~5 minutes at 30s
    const DEGRADED_INTERVAL_SECS: i64 = 900; // 15 minutes

    let mut current_interval = idx
        .annotations()
        .get(CHECK_INTERVAL_ANN)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(BASE_INTERVAL_SECS);
    // If we've already crossed the degrade threshold, ensure interval is degraded
    if let Some(cc) = idx
        .annotations()
        .get(CHECK_COUNT_ANN)
        .and_then(|s| s.parse::<i64>().ok())
        && cc >= DEGRADE_AFTER_CHECKS
    {
        current_interval = DEGRADED_INTERVAL_SECS;
    }
    if let Some(ts) = idx.annotations().get(LAST_CHECK_ANN) {
        if let Ok(then) = OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)
        {
            let now = OffsetDateTime::now_utc();
            let elapsed = now - then;
            if elapsed.whole_seconds() < current_interval {
                let wait = (current_interval - elapsed.whole_seconds()) as u64;
                return Ok(Action::requeue(std::time::Duration::from_secs(wait)));
            }
        }
    } else {
        // Missing annotation -> set baseline so we don't block reconcile
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "".into());
        let patch = serde_json::json!({
            "metadata": {"annotations": {
                LAST_CHECK_ANN: now,
                CHECK_COUNT_ANN: "0",
                CHECK_INTERVAL_ANN: BASE_INTERVAL_SECS.to_string()
            }}
        });
        let pp = kube::api::PatchParams::apply("meilisearch-operator");
        let api: Api<Index> = Api::namespaced(ctx.client.clone(), &ns);
        let _ = api
            .patch(&name, &pp, &kube::api::Patch::Merge(&patch))
            .await;
    }

    // Gate by server readiness
    if !meili_shared::readiness::server_ready(&ctx.client, &ns, server).await? {
        return Ok(Action::requeue(Duration::from_secs(10)));
    }

    // Acquire per-object lock (annotation-based)
    let lock = meili_shared::lock::LockSpec {
        namespace: &ns,
        name: &name,
        holder: &ctx.pod_name,
        ttl: time::Duration::seconds(30),
    };
    if !meili_shared::lock::acquire::<Index>(&ctx.client, &lock).await? {
        return Ok(Action::requeue(Duration::from_secs(5)));
    }
    // Ensure release on exit
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    let client_cloned = ctx.client.clone();
    let ns_cloned = ns.clone();
    let name_cloned = name.clone();
    let holder = ctx.pod_name.clone();
    let _guard = Guard(Some(move || {
        let client = client_cloned.clone();
        let ns = ns_cloned.clone();
        let name = name_cloned.clone();
        let holder = holder.clone();
        tokio::spawn(async move {
            let _ = meili_shared::lock::release::<Index>(
                &client,
                &meili_shared::lock::LockSpec {
                    namespace: &ns,
                    name: &name,
                    holder: &holder,
                    ttl: time::Duration::seconds(0),
                },
            )
            .await;
        });
    }));

    if idx.metadata.deletion_timestamp.is_some() {
        if !server_is_deleting(&ctx.client, &ns, server).await? && idx.spec.delete_on_finalize {
            let endpoint = meili_shared::endpoint::meili_endpoint(&ctx.client, &ns, server, 7700).await;
            let master_key = get_master_key(&ctx.client, &ns, server).await?;
            let client = MeiliClient::new(&endpoint, Some(&master_key))?;
            let task = client.delete_index(&idx.spec.uid).await?;
            let _ = task.wait_for_completion(&client, None, None).await?;
        }
        remove_finalizer(&ctx.client, &ns, &name).await?;
        return Ok(Action::await_change());
    }

    ensure_finalizer(&ctx.client, &ns, &name, &idx).await?;

    let endpoint = meili_shared::endpoint::meili_endpoint(&ctx.client, &ns, server, 7700).await;
    let master_key = get_master_key(&ctx.client, &ns, server).await?;
    let client = MeiliClient::new(&endpoint, Some(&master_key))?;
    // Idempotent index ensure with tracking
    let did_index_http;
    if !index_exists_http(&endpoint, &master_key, &idx.spec.uid).await? {
        let task = client
            .create_index(&idx.spec.uid, idx.spec.primary_key.as_deref())
            .await?;
        let _ = task.wait_for_completion(&client, None, None).await?;
        did_index_http = true;
    } else {
        tracing::info!(index=%idx.spec.uid, server=%server, "index already exists; skipping creation");
        did_index_http = true;
    }

    if let Some(ak) = &idx.spec.admin_key
        && ak.create
    {
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
            kb.with_name(normalize_kebab_dedup(&format!("{}-admin", idx.spec.uid)));
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

    // Update last-check + count + interval annotations and adjust requeue interval
    let mut next_interval = BASE_INTERVAL_SECS;
    if let Some(cc) = idx
        .annotations()
        .get(CHECK_COUNT_ANN)
        .and_then(|s| s.parse::<i64>().ok())
    {
        let new_cc = cc + 1;
        if new_cc >= DEGRADE_AFTER_CHECKS {
            next_interval = DEGRADED_INTERVAL_SECS;
        }
        if did_index_http {
            let now = OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "".into());
            if !now.is_empty() {
                let patch = serde_json::json!({
                    "metadata": {"annotations": {
                        LAST_CHECK_ANN: now,
                        CHECK_COUNT_ANN: new_cc.to_string(),
                        CHECK_INTERVAL_ANN: next_interval.to_string()
                    }}
                });
                let _ = api
                    .patch(&name, &pp, &kube::api::Patch::Merge(&patch))
                    .await;
            }
        }
    } else if did_index_http {
        // First time
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "".into());
        let patch = serde_json::json!({
            "metadata": {"annotations": {
                LAST_CHECK_ANN: now,
                CHECK_COUNT_ANN: "1",
                CHECK_INTERVAL_ANN: BASE_INTERVAL_SECS.to_string()
            }}
        });
        let _ = api
            .patch(&name, &pp, &kube::api::Patch::Merge(&patch))
            .await;
    }

    Ok(Action::requeue(Duration::from_secs(next_interval as u64)))
}

pub fn error_policy(_idx: Arc<Index>, err: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    let summary = err.summary();
    let allow = meili_shared::rate_limit::allow(&summary, std::time::Duration::from_secs(30));
    if allow {
        error!(summary=%summary, error=?err, "index reconcile failed");
    } else {
        tracing::debug!(summary=%summary, "suppressed duplicate error summary");
    }
    if summary.starts_with("timeout")
        || summary.starts_with("dns")
        || summary.starts_with("connect")
    {
        return Action::requeue(Duration::from_secs(10));
    }
    Action::requeue(Duration::from_secs(60))
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
    use meili_shared::http_retry::retry3_quiet;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(anyhow::Error::from)?;
    let mut out = Vec::new();
    let mut offset = 0usize;
    let limit = 1000usize;
    loop {
        let url = format!("{}/keys?offset={}&limit={}", endpoint, offset, limit);
        let resp = retry3_quiet("list_keys_page", || {
            let url = url.clone();
            let client = client.clone();
            let master_key = master_key.to_string();
            async move {
                let page = client
                    .get(url)
                    .header(
                        reqwest::header::AUTHORIZATION,
                        format!("Bearer {}", master_key),
                    )
                    .send()
                    .await?
                    .error_for_status()?;
                let parsed = page.json::<KeysPage>().await?;
                Ok(parsed)
            }
        })
    .await?;
        offset += resp.results.len();
        out.extend(resp.results);
        if offset >= resp.total {
            break;
        }
    }
    Ok(out)
}

async fn index_exists_http(
    endpoint: &str,
    master_key: &str,
    uid: &str,
) -> Result<bool, ReconcileError> {
    use meili_shared::http_retry::retry3_quiet;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(anyhow::Error::from)?;
    let url = format!("{}/indexes/{}", endpoint, uid);
    let resp = retry3_quiet("index_exists", || {
        let client = client.clone();
        let url = url.clone();
        let master_key = master_key.to_string();
        async move {
            let r = client
                .get(url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", master_key),
                )
                .send()
                .await?;
            Ok(r)
        }
    })
    .await?;
    if resp.status().is_success() {
        return Ok(true);
    }
    if resp.status().as_u16() == 404 {
        return Ok(false);
    }
    Err(ReconcileError::Anyhow(anyhow::anyhow!(
        "unexpected status checking index existence: {}",
        resp.status()
    )))
}

fn eq_unordered<T: Eq + std::hash::Hash + Clone>(a: &[T], b: &[T]) -> bool {
    use std::collections::HashSet;
    let sa: HashSet<T> = a.iter().cloned().collect();
    let sb: HashSet<T> = b.iter().cloned().collect();
    sa == sb
}

fn matches_admin(index_uid: &str, item: &KeyItem) -> bool {
    let expected_name = normalize_kebab_dedup(&format!("{}-admin", index_uid));
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

#[cfg(test)]
mod tests_index_controller {
    use super::*;

    #[test]
    fn eq_unordered_works() {
        assert!(eq_unordered(&[1, 2, 3], &[3, 2, 1]));
        assert!(!eq_unordered(&[1, 2], &[1, 2, 3]));
    }

    #[test]
    fn matches_admin_true_when_all_match() {
        let idx = "books";
        let item = KeyItem {
            name: Some(normalize_kebab_dedup(&format!("{}-admin", idx))),
            description: Some(format!("Admin key for index {}", idx)),
            key: "abc".into(),
            uid: "uid".into(),
            actions: vec!["*".into()],
            indexes: vec![idx.into()],
        };
        assert!(matches_admin(idx, &item));
    }

    #[test]
    fn matches_admin_false_when_index_differs() {
        let idx = "books";
        let item = KeyItem {
            name: Some(normalize_kebab_dedup(&format!("{}-admin", idx))),
            description: Some(format!("Admin key for index {}", idx)),
            key: "abc".into(),
            uid: "uid".into(),
            actions: vec!["*".into()],
            indexes: vec!["movies".into()],
        };
        assert!(!matches_admin(idx, &item));
    }
}
