# syntax=docker/dockerfile:1.19
# Multi-stage build for a small final image

FROM rust:1.91 AS builder
WORKDIR /app
# Cache dependencies
COPY Cargo.toml Cargo.lock ./
COPY crates/meilisearch-operator/Cargo.toml crates/meilisearch-operator/Cargo.toml
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/* \
	&& rustup target add x86_64-unknown-linux-musl \
	&& mkdir -p crates/meilisearch-operator/src \
	&& echo "fn main(){}" > crates/meilisearch-operator/src/main.rs \
	&& cargo build --release -p meilisearch-operator --target x86_64-unknown-linux-musl
# Build real binary
COPY . .
RUN cargo build --release -p meilisearch-operator --target x86_64-unknown-linux-musl

FROM gcr.io/distroless/static:nonroot
WORKDIR /
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/meilisearch-operator /usr/local/bin/meilisearch-operator
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/meilisearch-operator"]
