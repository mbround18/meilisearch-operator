use kube::{Api, Client};
use meili_crds::server::Server;

pub async fn server_ready(client: &Client, ns: &str, name: &str) -> anyhow::Result<bool> {
    let api: Api<Server> = Api::namespaced(client.clone(), ns);
    if let Some(srv) = api.get_opt(name).await? {
        Ok(srv.status.as_ref().map(|s| s.ready).unwrap_or(false))
    } else {
        Ok(false)
    }
}
