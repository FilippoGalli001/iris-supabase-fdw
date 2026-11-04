#!/usr/bin/env bash
# End-to-end demo: Supabase (local) + IRIS + the iris_fdw wrapper.
#   ./scripts/demo.sh           use the published release of iris_fdw.wasm (fast, no Rust build)
#   ./scripts/demo.sh --build   build iris_fdw.wasm from source in Docker first
# On Windows without Bash, use scripts/demo.ps1 instead.
set -euo pipefail
cd "$(dirname "$0")/.."
export MSYS_NO_PATHCONV=1

VERSION="0.1.0"
CHECKSUM="c39c575db2def2168c927ace03ce4c1923ca6e4c8f1d269b0496300f399c78e5"
URL="https://github.com/FilippoGalli001/iris-supabase-fdw/releases/download/v${VERSION}/iris_fdw.wasm"
DB_CONTAINER="supabase_db_iris-fdw-demo"

step() { printf '\n==> [%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

if [[ "${1:-}" == "--build" ]]; then
  step "Building the Wasm FDW from source (Docker, first run takes several minutes)"
  ./scripts/build-wasm.sh
else
  step "Downloading iris_fdw.wasm v${VERSION} from the GitHub release"
  mkdir -p dist
  curl -fsSL -o dist/iris_fdw.wasm "$URL"
  actual=$( (sha256sum dist/iris_fdw.wasm 2>/dev/null || shasum -a 256 dist/iris_fdw.wasm) | cut -d' ' -f1)
  [[ "$actual" == "$CHECKSUM" ]] || { echo "checksum mismatch: $actual" >&2; exit 1; }
  echo "checksum OK"
fi

step "Starting Supabase (first run downloads the Supabase images)"
(cd demo && npx -y supabase start)

step "Starting IRIS with the iris-fdw-gateway package and demo data"
docker compose up -d --build
for i in $(seq 1 120); do
  curl -sf -u fdw_user:fdw_password http://localhost:32773/iris-fdw/v1/ping >/dev/null && break
  [[ $i == 120 ]] && { echo "IRIS gateway did not come up; see: docker logs iris-fdw" >&2; exit 1; }
  sleep 3
done

step "Loading the FDW into Postgres and creating the foreign tables"
./scripts/load-wasm.sh
for f in demo/sql/01_setup.sql demo/sql/02_tables.sql demo/sql/03_patient_portal.sql; do
  docker exec -i "$DB_CONTAINER" psql -U postgres -v ON_ERROR_STOP=1 -q < "$f"
done

step "First query through the wrapper"
docker exec -i "$DB_CONTAINER" psql -U postgres -c "select id, name, city from iris.patient where city = 'Roma' order by id limit 5"

step "Ready"
echo "    SQL:    docker exec -it $DB_CONTAINER psql -U postgres"
echo "    Studio: http://127.0.0.1:54323  (SQL editor -> select * from iris.patient limit 10)"
echo "    Tests:  docker exec -i $DB_CONTAINER psql -U postgres -v ON_ERROR_STOP=1 < tests/e2e.sql"
