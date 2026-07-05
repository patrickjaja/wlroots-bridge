#!/usr/bin/env bash
# Reproduce the CI cross-build pipeline against the local source tree.
#
# wlroots-bridge is pure Rust with zero C dependencies (wayland-client's
# client_rust backend, no libwayland; no libxkbcommon), so both static-musl
# targets build in a single docker run with nothing but a Rust toolchain + the
# two musl targets. rust-lld (from .cargo/config.toml) links both, including the
# aarch64 cross build, so no external cross-linker is needed.
#
# NOTE: the headless-sway smoke test needs the CI job (sway from apt), NOT this
# docker build - this script only produces + static-verifies the binaries.
#
# Output binaries land in ./dist/ (override with OUTPUT_DIR=...):
#   dist/wlroots-bridge          (x86_64, statically linked)
#   dist/wlroots-bridge-aarch64  (aarch64, statically linked)
# A persistent build cache lives in ./.pipeline-cache/ so re-runs are fast.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${OUTPUT_DIR:-${REPO_ROOT}/dist}"
CACHE_DIR="${CACHE_DIR:-${REPO_ROOT}/.pipeline-cache}"
IMAGE="${IMAGE:-rust:1-bookworm}"

mkdir -p "${OUTPUT_DIR}"
mkdir -p "${CACHE_DIR}/target" "${CACHE_DIR}/cargo-registry" "${CACHE_DIR}/cargo-git" \
         "${CACHE_DIR}/rustup"

echo ">>> Building wlroots-bridge from ${REPO_ROOT}"
echo ">>> Base:    ${IMAGE}"
echo ">>> Output:  ${OUTPUT_DIR}"
echo ">>> Cache:   ${CACHE_DIR}"

docker run --rm \
  -v "${REPO_ROOT}:/src:ro" \
  -v "${OUTPUT_DIR}:/output" \
  -v "${CACHE_DIR}/target:/build/target" \
  -v "${CACHE_DIR}/cargo-registry:/usr/local/cargo/registry" \
  -v "${CACHE_DIR}/cargo-git:/usr/local/cargo/git" \
  -v "${CACHE_DIR}/rustup:/usr/local/rustup" \
  -e CARGO_TARGET_DIR=/build/target \
  "${IMAGE}" \
  bash -c '
    set -eo pipefail

    export CARGO_HOME=/usr/local/cargo
    export RUSTUP_HOME=/usr/local/rustup
    export PATH="$CARGO_HOME/bin:$PATH"

    # rust-lld (used as the musl linker via .cargo/config.toml) ships with the
    # toolchain component; add it plus the two static-musl targets.
    rustup component add rust-src rustc-dev >/dev/null 2>&1 || true
    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

    # --- writable copy of source so cargo can touch Cargo.lock if needed ---
    mkdir -p /build/src
    cp -a /src/. /build/src/
    cd /build/src

    # --- x86_64 static-musl build ---
    cargo build --release --locked --target x86_64-unknown-linux-musl
    cp /build/target/x86_64-unknown-linux-musl/release/wlroots-bridge /output/wlroots-bridge
    echo "wlroots-bridge x86_64 built successfully"

    # --- aarch64 static-musl build (rust-lld cross-links, no external gcc) ---
    cargo build --release --locked --target aarch64-unknown-linux-musl
    cp /build/target/aarch64-unknown-linux-musl/release/wlroots-bridge /output/wlroots-bridge-aarch64
    echo "wlroots-bridge aarch64 built successfully"

    # --- verify both are fully static (newer file(1) says "static-pie linked") ---
    for f in /output/wlroots-bridge /output/wlroots-bridge-aarch64; do
      file "$f"
      file "$f" | grep -qE "statically linked|static-pie linked" \
        || { echo "FAIL: $f is not static" >&2; exit 1; }
    done
    echo "both binaries are fully static"
  '

echo
echo ">>> Artifacts:"
ls -lh "${OUTPUT_DIR}"
