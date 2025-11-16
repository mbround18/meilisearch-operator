use kube::{Api, Client, Resource, ResourceExt};
use serde_json::json;
use time::{Duration, OffsetDateTime};

const ANN_HOLDER: &str = "meili.operator.dev/lock-holder";
const ANN_EXPIRES: &str = "meili.operator.dev/lock-expires";

pub struct LockSpec<'a> {
    pub namespace: &'a str,
    pub name: &'a str,
    pub holder: &'a str,
    pub ttl: Duration,
}

pub async fn acquire<T>(client: &Client, spec: &LockSpec<'_>) -> anyhow::Result<bool>
where
    T: Resource<Scope = kube::core::NamespaceResourceScope>
        + serde::de::DeserializeOwned
        + Clone
        + ResourceExt
        + std::fmt::Debug,
    <T as Resource>::DynamicType: Default,
{
    let api: Api<T> = Api::namespaced(client.clone(), spec.namespace);
    if let Some(obj) = api.get_opt(spec.name).await? {
        let anns = obj.annotations().clone();
        let now = OffsetDateTime::now_utc();
        let expired = match anns.get(ANN_EXPIRES) {
            Some(s) => OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .ok()
                .map(|t| t < now)
                .unwrap_or(true),
            None => true,
        };
    let free = expired || !anns.contains_key(ANN_HOLDER);
        let mine = anns
            .get(ANN_HOLDER)
            .map(|v| v == spec.holder)
            .unwrap_or(false);
        if free || mine {
            let expires = now + spec.ttl;
            let patch = json!({
                "metadata": { "annotations": {
                    ANN_HOLDER: spec.holder,
                    ANN_EXPIRES: expires.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
                }}
            });
            let pp = kube::api::PatchParams::default();
            let _ = api
                .patch(spec.name, &pp, &kube::api::Patch::Merge(&patch))
                .await?;
            return Ok(true);
        }
        Ok(false)
    } else {
        Ok(false)
    }
}

pub async fn release<T>(client: &Client, spec: &LockSpec<'_>) -> anyhow::Result<()>
where
    T: Resource<Scope = kube::core::NamespaceResourceScope>
        + serde::de::DeserializeOwned
        + Clone
        + ResourceExt
        + std::fmt::Debug,
    <T as Resource>::DynamicType: Default,
{
    let api: Api<T> = Api::namespaced(client.clone(), spec.namespace);
    if let Some(obj) = api.get_opt(spec.name).await? {
        let anns = obj.annotations().clone();
        // Only clear if we are the holder to avoid clobbering
        if anns
            .get(ANN_HOLDER)
            .map(|v| v == spec.holder)
            .unwrap_or(false)
        {
            let patch = json!({
                "metadata": { "annotations": {
                    ANN_HOLDER: serde_json::Value::Null,
                    ANN_EXPIRES: serde_json::Value::Null
                }}
            });
            let pp = kube::api::PatchParams::default();
            let _ = api
                .patch(spec.name, &pp, &kube::api::Patch::Merge(&patch))
                .await?;
        }
    }
    Ok(())
}
