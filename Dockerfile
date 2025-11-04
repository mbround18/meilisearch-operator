# syntax=docker/dockerfile:1.19
# Multi-stage build using cargo-chef for optimal dependency caching and a small final image

ARG RUST_VERSION=1.91
ARG TARGET_TRIPLE=x86_64-unknown-linux-musl

# Base with Rust toolchain and cargo-chef installed
FROM rust:${RUST_VERSION} AS chef
WORKDIR /app
ARG TARGET_TRIPLE
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add ${TARGET_TRIPLE} \
    && cargo install cargo-chef --version 0.1.73

# Compute dependency graph
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build dependency layers
FROM chef AS builder
WORKDIR /app
ARG TARGET_TRIPLE
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --target ${TARGET_TRIPLE} --recipe-path recipe.json --locked

# Build the actual binary
COPY . .
ENV RUSTFLAGS="-C strip=symbols"
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release -p meilisearch-operator --target ${TARGET_TRIPLE} --locked

# Final minimal image
FROM gcr.io/distroless/static:nonroot
WORKDIR /
ARG TARGET_TRIPLE
COPY --from=builder /app/target/${TARGET_TRIPLE}/release/meilisearch-operator /usr/local/bin/meilisearch-operator
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/meilisearch-operator"]
