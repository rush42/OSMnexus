# Self-contained demo image: builds the osmnexus Rust pipeline and bundles a Berlin OSM extract as
# the live editor's default base file, so someone can `docker run -p 5173:5173 <image>` and start
# picking a bbox/editing categories immediately — no local Rust toolchain, no separate PBF download.

FROM rust:1-bookworm AS rust-builder
WORKDIR /repo
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin osmnexus

FROM node:20-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends osmium-tool curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# berlin.osm.pbf is gitignored (not committed — too large), so fetch the same extract fresh at
# build time instead of relying on it being present in the build context.
RUN curl -fL -o /data/base.osm.pbf https://download.geofabrik.de/europe/germany/berlin-latest.osm.pbf

WORKDIR /repo
COPY configs ./configs
COPY editor ./editor
COPY --from=rust-builder /repo/target/release/osmnexus ./target/release/osmnexus

ENV BASE_PBF_PATH=/data/base.osm.pbf
WORKDIR /repo/editor
RUN npm install

EXPOSE 5173
CMD ["npm", "run", "dev"]
