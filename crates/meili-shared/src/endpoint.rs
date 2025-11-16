use std::{collections::HashMap, sync::Mutex, time::{Duration, Instant}};
use once_cell::sync::Lazy;
use kube::{Api, Client};
use k8s_openapi::api::core::v1::Service;

static EP_CACHE: Lazy<Mutex<HashMap<String, (String, Instant)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn cache_key(ns: &str, name: &str, port: u16) -> String {
    format!("{}/{}:{}", ns, name, port)
}

fn pick_ip(svc: &Service) -> Option<String> {
    if let Some(spec) = &svc.spec {
        // Prefer clusterIPs (dual-stack) then clusterIP
        if let Some(ips) = &spec.cluster_ips {
            // pick first IPv4 if possible
            if let Some(ip) = ips.iter().find(|ip| ip.contains('.')) {
                return Some(ip.clone());
            }
            if let Some(ip) = ips.first() {
                return Some((*ip).clone());
            }
        }
        if let Some(ip) = &spec.cluster_ip
            && ip != "None" && !ip.is_empty()
        {
            return Some(ip.clone());
        }
    }
    None
}

/// Resolve a Meilisearch endpoint for a given Server using Service ClusterIP when available,
/// falling back to kube DNS FQDN. Result is cached for a short TTL to reduce API/DNS load.
pub async fn meili_endpoint(client: &Client, ns: &str, name: &str, port: u16) -> String {
    let key = cache_key(ns, name, port);
    // Cache TTL 5 minutes
    const TTL: Duration = Duration::from_secs(300);
    if let Some((url, ts)) = EP_CACHE.lock().unwrap().get(&key).cloned()
        && Instant::now().duration_since(ts) < TTL
    {
        return url;
    }
    // Try Service lookup
    let services: Api<Service> = Api::namespaced(client.clone(), ns);
    let url = match services.get_opt(name).await {
        Ok(Some(svc)) => {
            if let Some(ip) = pick_ip(&svc) {
                format!("http://{}:{}", ip, port)
            } else {
                format!("http://{}.{}.svc.cluster.local:{}", name, ns, port)
            }
        }
        _ => format!("http://{}.{}.svc.cluster.local:{}", name, ns, port),
    };
    EP_CACHE.lock().unwrap().insert(key, (url.clone(), Instant::now()));
    url
}