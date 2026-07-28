# Multi-stage: compiles the osmnexus release binary at image-build time and bakes it into
# /opt/osmnexus/osmnexus, so the container never needs a host-built target/release/osmnexus. The
# repo itself is still bind-mounted at runtime (see docker-compose.yml) for live config/frontend
# edits — override PIPELINE_BIN_PATH to point at a host-built binary instead when iterating on Rust
# code without waiting on a full image rebuild.
FROM rust:1-bookworm AS rust-builder
WORKDIR /repo
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
RUN cargo build --release --bin osmnexus --bin dag_json

FROM node:20-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends osmium-tool \
    && rm -rf /var/lib/apt/lists/*

COPY --from=rust-builder /repo/target/release/osmnexus /opt/osmnexus/osmnexus
COPY --from=rust-builder /repo/target/release/dag_json /opt/osmnexus/dag_json
ENV PIPELINE_BIN_PATH=/opt/osmnexus/osmnexus
ENV DAG_JSON_BIN_PATH=/opt/osmnexus/dag_json

WORKDIR /repo/editor
# Installed at image-build time so `node_modules` lands in the anonymous volume that
# docker-compose.yml mounts over `/repo/editor/node_modules` (see comment there) — the repo bind
# mount shadows everything else under /repo/editor, but not that one path, so this survives.
COPY editor/package.json editor/package-lock.json ./
RUN npm install

CMD ["npm", "run", "dev"]
