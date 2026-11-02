#!/usr/bin/env bash
# Copy the locally built FDW into the Supabase Postgres container (development mode,
# used with fdw_package_url 'file:///tmp/iris_fdw.wasm').
set -euo pipefail
cd "$(dirname "$0")/.."
export MSYS_NO_PATHCONV=1
DB_CONTAINER="${DB_CONTAINER:-supabase_db_iris-fdw-demo}"
docker cp dist/iris_fdw.wasm "$DB_CONTAINER":/tmp/iris_fdw.wasm
echo "iris_fdw.wasm copied into $DB_CONTAINER:/tmp/iris_fdw.wasm"
