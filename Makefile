# Configurable variables (override via `make VAR=value`)
# Read version from the operator crate Cargo.toml
CARGO_TOML := crates/meilisearch-operator/Cargo.toml
VERSION ?= $(shell sed -n 's/^version\s*=\s*"\(.*\)"/\1/p' $(CARGO_TOML) | head -n1)
VVERSION := v$(VERSION)

# Derive image repository from compose.yml if present, else fallback
IMAGE_REPOSITORY_FROM_COMPOSE := $(shell sed -n 's/^\s*image:\s*\([^:]*\):.*/\1/p' compose.yml | head -n1)
IMAGE_REPOSITORY ?= $(if $(IMAGE_REPOSITORY_FROM_COMPOSE),$(IMAGE_REPOSITORY_FROM_COMPOSE),mbround18/meilisearch-operator)
IMAGE_TAG ?= $(VVERSION)
IMAGE := $(IMAGE_REPOSITORY):$(IMAGE_TAG)

OPERATOR_NAMESPACE ?= meilisearch-operator
SAMPLES_NAMESPACE ?= meilisearch-testing
CREATE_NAMESPACE ?= false

OPERATOR_RELEASE ?= meili-operator
SAMPLES_RELEASE ?= meili-samples

OPERATOR_CHART_DIR := charts/meilisearch-operator
SAMPLES_CHART_DIR := charts/meilisearch-samples

CARGO ?= cargo
DOCKER ?= docker
HELM ?= helm
COMPOSE ?= docker compose
KUBECTL ?= kubectl

.PHONY: all build deploy crds docker-build docker-push compose-build compose-push helm-lint helm-package helm-clean helm-install-operator helm-install-samples undeploy print-vars version kget kdescribe klogs ensure-namespaces

all: build

# Regenerate CRDs from Rust types and sync into operator chart
crds:
	@echo "==> Generating CRDs from Rust types"
	$(CARGO) run --manifest-path crates/meilisearch-operator/Cargo.toml --bin crdgen > manifests/crds.yaml
	@echo "==> Syncing CRDs into operator chart"
	cp manifests/crds.yaml $(OPERATOR_CHART_DIR)/crds/crds.yaml

# Build the Docker image for the operator
docker-build:
	@echo "==> Building Docker image $(IMAGE)"
	$(DOCKER) build -t $(IMAGE) .

# Push the Docker image to the registry
docker-push:
	@echo "==> Pushing Docker image $(IMAGE)"
	$(DOCKER) push $(IMAGE)

# Build and push using Docker Compose (uses compose.yml image and VERSION tag)
compose-build:
	@echo "==> docker compose build (VERSION=$(VVERSION))"
	VERSION=$(VVERSION) $(COMPOSE) build

compose-push:
	@echo "==> docker compose push (VERSION=$(VVERSION))"
	VERSION=$(VVERSION) $(COMPOSE) push

# Lint and package Helm charts
helm-lint:
	@echo "==> Helm lint operator chart"
	$(HELM) lint $(OPERATOR_CHART_DIR)
	@echo "==> Helm lint samples chart"
	$(HELM) lint $(SAMPLES_CHART_DIR)

helm-package: helm-clean
	@echo "==> Packaging Helm charts into dist/"
	mkdir -p dist
	$(HELM) package $(OPERATOR_CHART_DIR) --destination dist
	$(HELM) package $(SAMPLES_CHART_DIR) --destination dist

helm-clean:
	@rm -rf dist

# Complete build: docker image, CRDs generation, and Helm packaging
build: docker-build crds helm-lint helm-package
	@echo "==> Build complete"

# Deploy both charts with configurable namespaces and image settings
helm-install-operator:
	@echo "==> Installing/upgrading operator: $(OPERATOR_RELEASE) in namespace $(OPERATOR_NAMESPACE)"
			$(HELM) upgrade --install $(OPERATOR_RELEASE) $(OPERATOR_CHART_DIR) \
				--namespace $(OPERATOR_NAMESPACE) $(if $(filter true,$(CREATE_NAMESPACE)),--create-namespace,) \
			--set namespace=$(OPERATOR_NAMESPACE) \
			--set createNamespace=$(CREATE_NAMESPACE) \
	  --set image.repository=$(IMAGE_REPOSITORY) \
	  --set image.tag=$(IMAGE_TAG)

helm-install-samples:
	@echo "==> Installing/upgrading samples: $(SAMPLES_RELEASE) in namespace $(SAMPLES_NAMESPACE)"
			$(HELM) upgrade --install $(SAMPLES_RELEASE) $(SAMPLES_CHART_DIR) \
				--namespace $(SAMPLES_NAMESPACE) $(if $(filter true,$(CREATE_NAMESPACE)),--create-namespace,) \
			--set namespace=$(SAMPLES_NAMESPACE) \
			--set createNamespace=$(CREATE_NAMESPACE)

deploy: compose-build compose-push crds ensure-namespaces helm-install-operator helm-install-samples
	@echo "==> Deploy complete"

undeploy:
	@echo "==> Uninstalling Helm releases"
	-$(HELM) uninstall $(SAMPLES_RELEASE) --namespace $(SAMPLES_NAMESPACE)
	-$(HELM) uninstall $(OPERATOR_RELEASE) --namespace $(OPERATOR_NAMESPACE)

print-vars:
	@echo IMAGE=$(IMAGE)
	@echo VERSION=$(VERSION)
	@echo VVERSION=$(VVERSION)
	@echo OPERATOR_NAMESPACE=$(OPERATOR_NAMESPACE)
	@echo SAMPLES_NAMESPACE=$(SAMPLES_NAMESPACE)
	@echo OPERATOR_RELEASE=$(OPERATOR_RELEASE)
	@echo SAMPLES_RELEASE=$(SAMPLES_RELEASE)

version:
	@echo $(VERSION)

# Kubernetes helpers (diagnostics)
kget:
	@echo "==> Resources in $(OPERATOR_NAMESPACE)"
	$(KUBECTL) -n $(OPERATOR_NAMESPACE) get deploy,po,svc,sa,role,rolebinding,events --show-kind

kdescribe:
	@echo "==> Describe operator pod"
	POD=$$($(KUBECTL) -n $(OPERATOR_NAMESPACE) get pods -l app=meilisearch-operator -o jsonpath='{.items[0].metadata.name}'); \
	if [ -n "$$POD" ]; then $(KUBECTL) -n $(OPERATOR_NAMESPACE) describe pod $$POD; else echo "No operator pod found"; fi

klogs:
	@echo "==> Tail operator logs"
	POD=$$($(KUBECTL) -n $(OPERATOR_NAMESPACE) get pods -l app=meilisearch-operator -o jsonpath='{.items[0].metadata.name}'); \
	if [ -n "$$POD" ]; then $(KUBECTL) -n $(OPERATOR_NAMESPACE) logs $$POD -c operator --tail=200 -f; else echo "No operator pod found"; fi

# Ensure namespaces exist when CREATE_NAMESPACE is false
ensure-namespaces:
		@if [ "$(CREATE_NAMESPACE)" != "true" ]; then \
			echo "==> Ensuring namespaces exist (CREATE_NAMESPACE=false)"; \
			$(KUBECTL) get namespace $(OPERATOR_NAMESPACE) >/dev/null 2>&1 || $(KUBECTL) create namespace $(OPERATOR_NAMESPACE); \
			$(KUBECTL) get namespace $(SAMPLES_NAMESPACE) >/dev/null 2>&1 || $(KUBECTL) create namespace $(SAMPLES_NAMESPACE); \
		else \
			echo "==> Skipping namespace ensure (CREATE_NAMESPACE=true)"; \
		fi