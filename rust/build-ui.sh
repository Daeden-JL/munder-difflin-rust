#!/usr/bin/env sh
# Build the WASM client into rust/web/dist, which compose mounts as MD_STATIC_DIR.
#
# Not part of the Docker build: trunk pulls a wasm toolchain and wasm-opt, which
# would add minutes to every server image rebuild for something that changes far
# less often. Run this when the client changes.
set -e
cd "$(dirname "$0")/crates/md-ui"
trunk build --release
# The dev console stays reachable at /console.html — it speaks the raw contract
# and is the fastest way to poke a channel the client does not use yet.
cp ../../web/index.html ../../web/dist/console.html
echo "built -> rust/web/dist"
