# Reproducible build environment for the IRIS Wasm FDW
# (same toolchain versions used by supabase/wrappers release workflow)
FROM rust:1.97.1-slim
RUN rustup target add wasm32-unknown-unknown wasm32-wasip1 \
 && cargo install --locked cargo-component --version 0.21.1
WORKDIR /src
