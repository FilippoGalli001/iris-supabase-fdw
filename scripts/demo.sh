#!/usr/bin/env bash
# End-to-end demo: Supabase (local) + IRIS + the iris_fdw wrapper.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> Building the Wasm FDW"
./scripts/build-wasm.sh

echo "==> Starting Supabase (local)"
(cd demo && npx -y supabase start)

echo "==> Starting IRIS with the iris-fdw-gateway package and demo data"
docker compose up -d --build
until curl -sf -u fdw_user:fdw_password http://localhost:32773/iris-fdw/v1/ping >/dev/null; do sleep 3; done

echo "==> Loading the FDW into Postgres and creating the foreign tables"
./scripts/load-wasm.sh
for f in demo/sql/01_setup.sql demo/sql/02_tables.sql demo/sql/03_patient_portal.sql; do
  docker exec -i supabase_db_iris-fdw-demo psql -U postgres -v ON_ERROR_STOP=1 -q < "$f"
done

echo "==> Ready. Try:"
echo "    docker exec -it supabase_db_iris-fdw-demo psql -U postgres -c 'select * from iris.patient limit 5'"
echo "    Supabase Studio: http://127.0.0.1:54323"
