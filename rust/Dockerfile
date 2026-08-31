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
RUN cd rust && cargo build --release --locked -p md-server -p md-hook 2>/dev/null || true

# Real sources.
COPY contract/ ./contract/
COPY rust/ ./rust/
# Cargo skips a rebuild when mtimes look unchanged; touching forces it.
RUN find rust/crates -name '*.rs' -exec touch {} + \
 && cd rust && cargo build --release --locked -p md-server -p md-hook

FROM debian:bookworm-slim AS runtime
# ca-certificates for outbound HTTPS; procps so process inspection works when
# debugging a wedged agent inside the container; git because the whole git plane
# shells out to it — without it those channels fail at runtime, not at build.
#
# nodejs and npm because with MD_SANDBOX=passthrough this container IS the agent
# environment: every agent CLI worth running is an npm package, and so is every
# server in the MCP bundle (all seven launch through `npx`). Without a runtime
# here the floor starts, shows a roster, and can spawn precisely nothing.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates procps git nodejs npm \
 && rm -rf /var/lib/apt/lists/*

# The server spawns agent processes, so it does not run as root.
RUN useradd --create-home --uid 10001 --shell /bin/bash md
COPY --from=builder /build/rust/target/release/md-server /usr/local/bin/md-server
# The hook shim runs inside the agent's own environment on every lifecycle hook.
# It ships as a static binary so an agent container needs no language runtime
# just to report that a tool ran.
COPY --from=builder /build/rust/target/release/md-hook /usr/local/bin/md-hook

USER md
WORKDIR /home/md
RUN mkdir -p /home/md/data

# Inside a container 127.0.0.1 is unreachable from the host, so bind all
# interfaces. The container boundary is what limits exposure; the published
# port decides who can reach it.
# Engines and tools install into the DATA VOLUME, not into an image layer.
#
# The layer is the obvious place and the wrong one: `up --build` recreates the
# container, and everything anyone had installed would go with it. Putting the
# prefix under the volume means an engine installed once stays installed across
# rebuilds — which is what makes "install" a button worth pressing rather than a
# thing you do again every deploy.
#
# `MD_AGENT_PATH` is what agents actually inherit, and what the engines panel
# checks when it says whether a command is installed. Both have to name this
# directory or the two answers disagree.
ENV MD_BIND=0.0.0.0:7777 \
    MD_DATA_ROOT=/home/md/data \
    MD_SANDBOX=passthrough \
    NPM_CONFIG_PREFIX=/home/md/data/.tools \
    MD_AGENT_PATH=/home/md/data/.tools/bin:/usr/local/bin:/usr/bin:/bin \
    RUST_LOG=md_server=info

EXPOSE 7777
HEALTHCHECK --interval=10s --timeout=3s --start-period=3s --retries=3 \
  CMD ["/usr/local/bin/md-server", "--health-check"]

CMD ["md-server"]
