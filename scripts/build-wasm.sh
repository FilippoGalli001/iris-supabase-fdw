#!/usr/bin/env bash
# Build the Wasm FDW inside Docker (no local Rust toolchain needed).
# Output: dist/iris_fdw.wasm and dist/checksum.txt
set -euo pipefail
cd "$(dirname "$0")/.."
# Git Bash on Windows: use a Windows path for the bind mount and disable path mangling
SRC="$(pwd -W 2>/dev/null || pwd)"
export MSYS_NO_PATHCONV=1
docker build -q -t iris-fdw-builder -f docker/builder.Dockerfile docker >/dev/null
docker run --rm -v "$SRC":/src -v iris-fdw-cargo:/usr/local/cargo/registry -w /src/fdw iris-fdw-builder \
  sh -c "cargo test --lib --quiet && cargo component build --release --target wasm32-unknown-unknown"
mkdir -p dist
cp fdw/target/wasm32-unknown-unknown/release/iris_fdw.wasm dist/
(cd dist && sha256sum iris_fdw.wasm | tee checksum.txt)
