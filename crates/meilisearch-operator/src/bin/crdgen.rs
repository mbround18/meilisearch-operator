use kube::core::CustomResourceExt;
use meilisearch_operator::crds::{index::Index, key::Key, server::Server};

fn main() {
    let crds = vec![Server::crd(), Index::crd(), Key::crd()];
    for (i, crd) in crds.into_iter().enumerate() {
        if i > 0 {
            println!("---");
        }
        println!("{}", serde_yaml::to_string(&crd).expect("serialize crd"));
    }
}
