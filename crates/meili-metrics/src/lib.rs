use std::sync::Arc;

use actix_web::{App, HttpResponse, HttpServer, Responder, get, web};
use anyhow::Context;
use kube::{Api, Client, ResourceExt};
use meili_crds::server::Server;
use prometheus::{Encoder, Gauge, GaugeVec, Registry, TextEncoder};
use tokio::time::Duration;
use tracing::{error, info};

#[derive(Clone)]
pub struct Config {
    pub operator_namespace: String,
    pub bind_addr: String,
}

#[derive(Clone)]
struct Metrics {
    registry: Registry,
    servers_total: Gauge,
    indexes_total: Gauge,
    keys_total: Gauge,
    idx_docs_total: GaugeVec,
}

impl Metrics {
    fn new() -> Self {
        let registry = Registry::new();
        let servers_total =
            Gauge::new("meili_operator_servers_total", "Number of Server CRs").unwrap();
        let indexes_total =
            Gauge::new("meili_operator_indexes_total", "Number of Index CRs").unwrap();
        let keys_total = Gauge::new("meili_operator_keys_total", "Number of Key CRs").unwrap();
        let idx_docs_total = GaugeVec::new(
            prometheus::Opts::new(
                "meili_operator_index_documents_total",
                "Documents per index per server",
            ),
            &["namespace", "server", "index"],
        )
        .unwrap();

        registry.register(Box::new(servers_total.clone())).unwrap();
        registry.register(Box::new(indexes_total.clone())).unwrap();
        registry.register(Box::new(keys_total.clone())).unwrap();
        registry.register(Box::new(idx_docs_total.clone())).unwrap();

        Metrics {
            registry,
            servers_total,
            indexes_total,
            keys_total,
            idx_docs_total,
        }
    }
}

#[derive(Clone)]
pub struct Ctx {
    client: Client,
    cfg: Config,
    metrics: Arc<Metrics>,
}

#[get("/metrics")]
async fn metrics_handler(data: web::Data<Ctx>) -> impl Responder {
    let metric_families = data.metrics.registry.gather();
    let mut buf = Vec::new();
    let encoder = TextEncoder::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buf) {
        error!(error=?e, "metrics encode failed");
        return HttpResponse::InternalServerError().finish();
    }
    HttpResponse::Ok()
        .content_type(encoder.format_type())
        .body(buf)
}

async fn run_server(client: Client, cfg: Config) -> anyhow::Result<()> {
    let ctx = Ctx {
        client: client.clone(),
        cfg: cfg.clone(),
        metrics: Arc::new(Metrics::new()),
    };

    let poll_ctx = ctx.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = poll_once(&poll_ctx).await {
                error!(error=?e, "metrics poll failed");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

    let bind = cfg.bind_addr.clone();
    info!(%bind, "metrics server starting");
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(ctx.clone()))
            .service(metrics_handler)
    })
    .bind(bind)
    .context("bind metrics server")?
    .run()
    .await
    .context("run metrics server")
}

/// Start the metrics server on a dedicated Actix system thread. Returns a JoinHandle for the thread.
pub fn start_background(client: Client, cfg: Config) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let sys = actix_web::rt::System::new();
        sys.block_on(async move {
            if let Err(e) = run_server(client, cfg).await {
                error!(error=?e, "metrics server exited with error");
            }
        });
    })
}

async fn poll_once(ctx: &Ctx) -> anyhow::Result<()> {
    use meili_crds::{index::Index, key::Key};

    // Count CRDs
    let servers: Api<Server> = Api::all(ctx.client.clone());
    let indexes: Api<Index> = Api::all(ctx.client.clone());
    let keys: Api<Key> = Api::all(ctx.client.clone());

    let lp: kube::api::ListParams = Default::default();
    let (srv_list, idx_list, key_list) =
        tokio::try_join!(servers.list(&lp), indexes.list(&lp), keys.list(&lp),)?;

    ctx.metrics.servers_total.set(srv_list.items.len() as f64);
    ctx.metrics.indexes_total.set(idx_list.items.len() as f64);
    ctx.metrics.keys_total.set(key_list.items.len() as f64);

    // Reset per-index docs totals to avoid stale values
    ctx.metrics.idx_docs_total.reset();

    // For each server, query Meilisearch for per-index document counts
    for srv in srv_list.items.iter() {
        let ns = srv.namespace().unwrap_or_default();
        let name = srv.name_any();
        let port = srv.spec.port;
    let endpoint = meili_shared::endpoint::meili_endpoint(&ctx.client, &ns, &name, port).await;
        // Master key is stored in operator namespace as <ns>-<name>-meili-master
        let secret_name = format!("{}-{}-meili-master", ns, name);
        let master = get_master_key(&ctx.client, &ctx.cfg.operator_namespace, &secret_name).await;
        let master_key = match master {
            Ok(k) => k,
            Err(e) => {
                error!(server=%name, namespace=%ns, error=?e, "missing master key; skipping stats");
                continue;
            }
        };

        // Build Meili client and fetch indexes & stats
        if let Err(e) =
            collect_server_index_counts(&ctx.metrics, &endpoint, &master_key, &ns, &name).await
        {
            error!(server=%name, namespace=%ns, error=?e, "collect stats failed");
        }
    }

    Ok(())
}

async fn get_master_key(client: &Client, ns: &str, name: &str) -> anyhow::Result<String> {
    use k8s_openapi::api::core::v1::Secret;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let sec = secrets.get(name).await?;
    if let Some(sd) = sec.string_data.as_ref()
        && let Some(v) = sd.get("masterKey")
    {
        return Ok(v.clone());
    }
    let data = sec.data.context("secret has no data")?;
    let v = data.get("masterKey").context("missing masterKey")?;
    Ok(String::from_utf8(v.0.clone())?)
}

async fn collect_server_index_counts(
    metrics: &Metrics,
    endpoint: &str,
    master_key: &str,
    ns: &str,
    srv: &str,
) -> anyhow::Result<()> {
    use meilisearch_sdk::client::Client as MeiliClient;
    let client = MeiliClient::new(endpoint, Some(master_key))?;

    // List all indexes: try SDK first; if it fails, fall back to HTTP
    let indexes: Vec<String> = match client.get_indexes().await {
        Ok(resp) => resp.results.into_iter().map(|idx| idx.uid).collect(),
        Err(_) => {
            // Fallback via HTTP
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let res = http
                .get(format!("{}/indexes", endpoint))
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", master_key),
                )
                .send()
                .await
                .context("list indexes http")?;
            let v: serde_json::Value = res.json().await.context("parse indexes json")?;
            let arr = v
                .get("results")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            arr.into_iter()
                .filter_map(|o| o.get("uid").and_then(|u| u.as_str()).map(|s| s.to_string()))
                .collect()
        }
    };

    // For each index, get stats
    for uid in indexes {
        // Try SDK: index.get_stats()
        let docs = match client.index(&uid).get_stats().await {
            Ok(st) => st.number_of_documents as f64,
            Err(_) => {
                // Fallback to HTTP
                let http = reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap();
                let res = http
                    .get(format!("{}/indexes/{}/stats", endpoint, uid))
                    .header(
                        reqwest::header::AUTHORIZATION,
                        format!("Bearer {}", master_key),
                    )
                    .send()
                    .await
                    .context("get index stats http")?;
                let v: serde_json::Value = res.json().await.context("parse stats json")?;
                v.get("numberOfDocuments")
                    .and_then(|n| n.as_f64())
                    .unwrap_or(0.0)
            }
        };
        metrics
            .idx_docs_total
            .with_label_values(&[ns, srv, &uid])
            .set(docs);
    }

    Ok(())
}
