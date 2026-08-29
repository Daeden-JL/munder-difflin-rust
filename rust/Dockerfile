# Build context is the REPO ROOT, not rust/: md-contract's build script reads
# ../../../contract/manifest.json, so the manifest has to sit beside the
# workspace in the image exactly as it does in the repo.
#
#   docker build -f rust/Dockerfile -t munder-difflin/server:dev .

FROM rust:slim-bookworm AS builder
WORKDIR /build

# Dependencies first, against a stub main, so edits to our own sources do not
# invalidate the (slow) dependency layer.
COPY rust/Cargo.toml rust/Cargo.lock ./rust/
COPY rust/crates/md-contract/Cargo.toml ./rust/crates/md-contract/
COPY rust/crates/md-tenant/Cargo.toml   ./rust/crates/md-tenant/
COPY rust/crates/md-pty/Cargo.toml      ./rust/crates/md-pty/
COPY rust/crates/md-server/Cargo.toml   ./rust/crates/md-server/
COPY contract/manifest.json ./contract/manifest.json
RUN mkdir -p rust/crates/md-contract/src rust/crates/md-tenant/src \
             rust/crates/md-pty/src rust/crates/md-server/src \
 && echo 'fn main(){}' > rust/crates/md-server/src/main.rs \
 && touch rust/crates/md-contract/src/lib.rs rust/crates/md-tenant/src/lib.rs \
          rust/crates/md-pty/src/lib.rs rust/crates/md-server/src/lib.rs \
 # build.rs is required for md-contract to resolve at all.
 && printf 'fn main(){}' > rust/crates/md-contract/build.rs
RUN cd rust && cargo build --release --locked -p md-server 2>/dev/null || true

# Real sources.
COPY contract/ ./contract/
COPY rust/ ./rust/
# Cargo skips a rebuild when mtimes look unchanged; touching forces it.
RUN find rust/crates -name '*.rs' -exec touch {} + \
 && cd rust && cargo build --release --locked -p md-server

FROM debian:bookworm-slim AS runtime
# ca-certificates for outbound HTTPS; procps so process inspection works when
# debugging a wedged agent inside the container; git because the whole git plane
# shells out to it — without it those channels fail at runtime, not at build.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates procps git \
 && rm -rf /var/lib/apt/lists/*

# The server spawns agent processes, so it does not run as root.
RUN useradd --create-home --uid 10001 --shell /bin/bash md
COPY --from=builder /build/rust/target/release/md-server /usr/local/bin/md-server

USER md
WORKDIR /home/md
RUN mkdir -p /home/md/data

# Inside a container 127.0.0.1 is unreachable from the host, so bind all
# interfaces. The container boundary is what limits exposure; the published
# port decides who can reach it.
ENV MD_BIND=0.0.0.0:7777 \
    MD_DATA_ROOT=/home/md/data \
    MD_SANDBOX=passthrough \
    RUST_LOG=md_server=info

EXPOSE 7777
HEALTHCHECK --interval=10s --timeout=3s --start-period=3s --retries=3 \
  CMD ["/usr/local/bin/md-server", "--health-check"]

CMD ["md-server"]
