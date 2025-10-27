# Meilisearch Operator (Rust)

Kubernetes operator (kube-rs, edition 2024) that manages Meilisearch clusters and access:

- Server (v1beta1): StatefulSet + Service, generates/stores master key, waits for health.
- Index (v1alpha1): creates indexes and can provision an admin key per index.
- Key (v1alpha1): creates API keys and writes them into Secrets.
- Policy (v1alpha1): placeholder for future policy management.

## Highlights

- Cross-namespace networking out of the box (Helm NetworkPolicies enabled by default).
- PVC retention tuned: delete storage on Server removal, retain on scale-down.
- Duplicate key protection: adopts existing Meili keys (exact or relaxed match) and writes Secrets; prevents flooding.
- Admin key adoption for Index: reuses pre-existing admin keys when found.
- Fast teardown: deleting a Server force-cleans related Keys/Indexes CRs without Meili calls.
- Minimal, static container: MUSL-linked binary on distroless:static.

## Quick start

You can deploy via Helm charts provided in `charts/` (operator and sample resources), or apply manifests under `manifests/` for a minimal setup.

Optional commands (adjust to your environment):

```bash
# Generate CRDs from code (optional; Helm includes CRDs too)
cargo run --bin crdgen > manifests/crds.yaml

# Apply base manifests (RBAC/Deployment/Samples)
kubectl apply -f manifests/crds.yaml
kubectl apply -f manifests/rbac.yaml
kubectl apply -f manifests/deployment.yaml
kubectl apply -f manifests/samples.yaml
```

Helm (recommended):

```bash
# Operator (values default to allow egress to DNS/API server and Meili namespaces)
helm upgrade --install meilisearch-operator charts/meilisearch-operator \
  --namespace meilisearch-operator --create-namespace

# Samples (a Server, Index, and Keys)
helm upgrade --install meilisearch-samples charts/meilisearch-samples \
  --namespace meilisearch-testing --create-namespace
```

NetworkPolicy note: if your API server IP/CIDR differs, set `charts/meilisearch-operator` value `networkPolicy.egress.kubeApi.cidrs` accordingly.

## Build and test

- Rust: stable 1.90+ (MSRV tracks CI; edition 2024)
- Lint: clippy (warnings as errors)

```bash
cargo build
cargo test
cargo clippy -- -D warnings
```

## Running locally

Outside the cluster, the operator uses your kubeconfig. Set the operator namespace for cross-namespace master key copies:

```bash
export OPERATOR_NAMESPACE=meilisearch-operator
export KUBECONFIG=$HOME/.kube/config
cargo run --bin meilisearch-operator
```

## Container image

The Dockerfile builds a static MUSL binary and ships on `gcr.io/distroless/static:nonroot`.

```bash
docker build -t meilisearch-operator:dev .
# Optional: run against your kubeconfig
docker run --rm -e OPERATOR_NAMESPACE=meilisearch-operator \
  -v $HOME/.kube/config:/home/nonroot/.kube/config:ro \
  meilisearch-operator:dev
```

## CRDs at a glance

- Server (v1beta1): image?, replicas (default 1), storage?, service_type (ClusterIP), port (7700)
- Index (v1alpha1): server_ref, uid, primary_key?, delete_on_finalize (false), admin_key?
- Key (v1alpha1): server_ref, name?, description?, actions[], indexes[], expires_at?, secret_namespace, secret_name
- Policy (v1alpha1): reserved for future use

Generate CRDs:

```bash
cargo run --bin crdgen > manifests/crds.yaml
```

## Behavior overview

- Server
  - Generates a 64-char master key and stores it in the Server namespace and in the operator namespace.
  - Waits for `/health` before marking ready.
  - On deletion: removes operator copy Secret and fast-deletes related Index/Key CRs (removes their finalizers and deletes the CRs).

- Index
  - Creates the index; optionally creates or adopts an admin key scoped to the index (`<uid>-admin`).
  - On deletion: if the Server is not deleting and `delete_on_finalize=true`, deletes the Meili index; otherwise just removes finalizer.

- Key
  - Creates Meili keys and writes them into the configured Secret (defaults name to CR name if `spec.name` is omitted).
  - Adoption logic: prefers existing Secret value if valid; otherwise adopts exact or relaxed matches from Meili to avoid duplicates.
  - On deletion: if the Server is not deleting and we own a `uid`, deletes the Meili key; otherwise just removes finalizer.

## Troubleshooting

- API connect refused (10.43.0.1:443 or similar): ensure the operator NetworkPolicy permits egress to the Kubernetes API (TCP/443). Set `networkPolicy.egress.kubeApi.cidrs` for your cluster.
- Cross-namespace access blocked: confirm NetworkPolicies are enabled for operator egress and sample chart ingress.
- Key duplication: verify adoption paths—existing Secrets, exact match (name/description/actions/indexes/expiry), and relaxed match (actions/indexes/expiry) are in effect.

## Repository layout

- `crates/meilisearch-operator/` — Rust operator code (controllers, CRDs, tests)
- `charts/meilisearch-operator/` — Helm chart for the operator (RBAC, Deployment, NetworkPolicy)
- `charts/meilisearch-samples/` — Sample Server/Index/Key resources
- `manifests/` — Raw manifests for CRDs, RBAC, Deployment, Samples
