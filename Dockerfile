# Arena game server (blades-server-rewritten) — multi-stage build.
#
# NOTE: the release build is memory-hungry (actix + diesel + tokio); build it on
# a machine with >= ~4 GB RAM (NOT the 1.9 GB prod box — it will OOM). The
# resulting runtime image is small. See deploy/README-arena-deploy.md.
FROM rust:1-bookworm AS build
WORKDIR /src
# build-essential/pkg-config: zstd-sys (a transitive dep) compiles a C library.
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*
# Copy the whole workspace (blades_lib + server + arena_proto + Cargo.lock).
COPY . .
RUN cargo build --release -p server \
 && strip target/release/server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/server /usr/local/bin/blades-server
# HTTP REST (blades.bgs.services API) + the arena UDP/ENet host.
EXPOSE 8080
EXPOSE 7777/udp
ENTRYPOINT ["blades-server"]
