# iris-supabase-fdw — InterSystems IRIS wrapper for Supabase

Query **InterSystems IRIS** tables from **Supabase** as if they were local Postgres tables:

```sql
select p.name, a.starts_at, a.doctor
from iris.appointment a
join iris.patient p on p.id = a.patient
where p.city = 'Roma' and a.status = 'scheduled';
```

This project implements the Ideas Portal idea **"Add InterSystems wrapper for Supabase"**
(Community Bounty Program *Idea to Application*, Round 3). It is a
[Supabase Wasm Foreign Data Wrapper](https://supabase.com/blog/postgres-foreign-data-wrappers-with-wasm)
written in Rust, plus a small ObjectScript REST package for IRIS.

Once IRIS tables are available in Supabase, they work with the rest of the Supabase stack:
joins with Postgres tables, the auto-generated REST/GraphQL APIs, Auth, Edge Functions and Studio.
The data stays in IRIS, so nothing is copied or migrated.

## Features

- **Read and write**: `SELECT`, `INSERT`, `UPDATE`, `DELETE` on IRIS tables.
- **`IMPORT FOREIGN SCHEMA`**: one statement creates a foreign table for every table of an
  IRIS schema, mapping the IRIS types to Postgres types.
- **Filter pushdown**: `=`, `<>`, `<`, `<=`, `>`, `>=`, `LIKE`, `NOT LIKE`, `IN`, `NOT IN`,
  `IS [NOT] NULL` are executed by IRIS, always with bound `?` parameters.
- **Lazy paging**: rows are fetched in batches (`batch_size`). A `LIMIT` sizes the first page,
  so `select ... limit 10` transfers 10 rows.
- **Opt-in `TOP`/`ORDER BY` pushdown** (`limit_pushdown 'true'`), applied only when it is safe.
- **Custom SQL** as a foreign table (`query` option), for example IRIS aggregations, views and
  **IRIS Vector Search** (`VECTOR_COSINE`). The demo includes a "similar patients" table.
- **`_attrs jsonb`** column with the whole IRIS row as JSON.
- **Two connection modes**:
  - `gateway` (recommended): the `iris-fdw-gateway` IPM package. It is a stateless `%CSP.REST` API
    that runs only SELECT on `/query` and INSERT/UPDATE/DELETE on `/exec`, under the IRIS user's
    SQL privileges.
  - `atelier`: zero install. It uses the Atelier REST API that ships with IRIS. Use it to try the
    wrapper quickly against an existing instance.
- Credentials can be stored in the **Supabase Vault**.

## Architecture

```
┌──────────────── Supabase (Postgres) ────────────────┐         ┌──────────── InterSystems IRIS ────────────┐
│ SQL · PostgREST · GraphQL · Studio · Auth            │  HTTP   │  /iris-fdw/v1   (iris-fdw-gateway, IPM)    │
│        │                                             │  JSON   │    GET  /ping                              │
│ wrappers ext ── Wasm runtime ── iris_fdw.wasm  ──────┼────────►│    POST /query  SELECT, paged, typed JSON  │
│                                (Rust)                │         │    POST /exec   INSERT / UPDATE / DELETE   │
└──────────────────────────────────────────────────────┘         └────────────────────────────────────────────┘
```

Supabase Wasm FDWs run sandboxed and **can only make HTTP(S) requests**: they cannot open the
IRIS SuperServer (TCP) connection that JDBC/ODBC use. For that reason the wrapper sends
parameterized SQL over HTTP, and the gateway runs it with IRIS Dynamic SQL in ODBC mode, so
dates are exchanged in ISO format.

## Quick start (local demo)

Requirements: Docker, Node.js (for `npx supabase`), Bash (Git Bash on Windows works).
You don't need a Rust toolchain: the wrapper is compiled inside a Docker container.

```bash
git clone https://github.com/FilippoGalli001/iris-supabase-fdw.git
cd iris-supabase-fdw
./scripts/demo.sh
```

The script:
1. builds `dist/iris_fdw.wasm` (Rust → WebAssembly component) and runs the unit tests,
2. starts Supabase locally (`demo/supabase`),
3. builds and starts IRIS (`intersystemsdc/iris-community`). This installs the
   `iris-fdw-gateway` package, creates demo data (`Demo.Patient`, `Demo.Appointment`) and creates
   a least-privilege user `fdw_user`,
4. loads the wrapper into Postgres and runs `demo/sql/*.sql`.

Then open Supabase Studio at http://127.0.0.1:54323 → SQL editor:

```sql
select * from iris.patient where city = 'Roma' limit 10;
select specialty, visits, total from iris.spend_by_specialty order by total desc;
insert into iris.patient (name, birthdate, city) values ('Ada Lovelace', '1815-12-10', 'London');
```

With `debug 'true'` on the server, every scan prints the SQL sent to IRIS:

```
INFO:  iris_fdw: SELECT id, name FROM Demo.Patient WHERE city = ? params=[String("Roma")]
```

### IRIS Vector Search from Supabase

Each demo patient has a clinical-profile embedding (`Profile As %Library.Vector`).
`iris.similar_patients` is backed by an IRIS query that computes `VECTOR_COSINE` inside IRIS:

```sql
select similar_name, similarity
from iris.similar_patients
where patient_id = 7
order by similarity desc
limit 3;
```

```
INFO:  iris_fdw: SELECT TOP 3 "similar_name", "similarity", "patient_id" FROM (SELECT ... VECTOR_COSINE(a.Profile, b.Profile) AS similarity ...) fdw_q
       WHERE "patient_id" = ? ORDER BY CASE WHEN "similarity" IS NULL THEN 0 ELSE 1 END, "similarity" DESC
 similar_name  | similarity
---------------+------------
 Marco Bianchi |     0.9990
 Luca Bruno    |     0.9977
 José Bruno    |     0.9909
```

The patient filter, the ordering and the `LIMIT` all run in IRIS. Postgres receives only 3 rows.

### Patient portal demo (Supabase Auth + IRIS)

`demo/sql/03_patient_portal.sql` creates the RPC `my_appointments()`. Each user signed in with
Supabase Auth gets only their own appointments, read live from IRIS. The `iris` schema itself is
not exposed by the API.

```bash
# sign in as a patient (create the user first in Studio → Authentication, e.g. patient7@example.com)
curl -X POST "http://127.0.0.1:54321/rest/v1/rpc/my_appointments" \
  -H "apikey: $ANON_KEY" -H "Authorization: Bearer $USER_JWT" -H "content-type: application/json" -d '{}'
```

Inside the function the patient filter is pushed down to IRIS: `WHERE email = ?`, then
`WHERE patient = ?`.

## Installation on your own IRIS + Supabase

### 1. IRIS side: install the gateway

Clone this repository on the IRIS host, then load it with IPM:

```objectscript
zpm "load /path/to/iris-supabase-fdw"
```

Once the package is published to the IPM registry, `zpm "install iris-fdw-gateway"` also works.
Either way, this creates the web application `/iris-fdw/v1` (password authentication).

Create a dedicated user and grant only the SQL privileges you want to expose, for example:

```sql
GRANT SELECT ON SCHEMA Demo TO fdw_user
```

To make the gateway read-only: `set ^FDW.Gateway("readOnly") = 1`.

The gateway must be reachable from your Supabase database. For Supabase Cloud, that means a
public HTTPS endpoint, for example IRIS behind a web server or reverse proxy with TLS.

### 2. Supabase side: create the server

```sql
create extension if not exists wrappers with schema extensions;

create foreign data wrapper wasm_wrapper
  handler extensions.wasm_fdw_handler
  validator extensions.wasm_fdw_validator;

-- store the IRIS password in Vault
select vault.create_secret('<iris password>', 'iris_password');  -- returns the secret id

create server iris_server
  foreign data wrapper wasm_wrapper
  options (
    fdw_package_url 'https://github.com/FilippoGalli001/iris-supabase-fdw/releases/download/v0.1.0/iris_fdw.wasm',
    fdw_package_name 'fgalli:iris-fdw',
    fdw_package_version '0.1.0',
    fdw_package_checksum '<sha256 from the release>',
    api_url 'https://iris.example.com',
    username 'fdw_user',
    password_id '<vault secret id>'
  );

create schema if not exists iris;
import foreign schema "Demo" from server iris_server into iris;
```

### Server options

| Option | Default | Description |
|---|---|---|
| `api_url` | required | Base URL of the IRIS web server, e.g. `http://iris:52773` |
| `api_mode` | `gateway` | `gateway` or `atelier` |
| `gateway_path` | `/iris-fdw/v1` | Path of the gateway web application |
| `namespace` | `USER` | IRIS namespace (Atelier mode; in gateway mode the web app defines it) |
| `username` | required | IRIS user |
| `password` / `password_id` | required | Plain password or Supabase Vault secret id |
| `batch_size` | `1000` | Rows per HTTP request |
| `limit_pushdown` | `false` | Default for the table option below |
| `debug` | `false` | Print the generated IRIS SQL as `INFO` messages |

### Foreign table options

| Option | Description |
|---|---|
| `table` | IRIS table, e.g. `Demo.Patient` (read/write) |
| `query` | Any IRIS `SELECT` (read-only), used as a subquery |
| `rowid_column` | Column that identifies a row, required for UPDATE/DELETE (usually `id`) |
| `limit_pushdown` | `true` to send `TOP n` + `ORDER BY` to IRIS (see below) |

### Type mapping

| IRIS | Postgres |
|---|---|
| `BIT` / `%Boolean` | `boolean` |
| `TINYINT`, `SMALLINT`, `INTEGER`, `BIGINT` | `smallint`, `integer`, `bigint` |
| `NUMERIC`, `DECIMAL`, `MONEY` | `numeric` (transferred as double precision) |
| `DOUBLE`, `FLOAT`, `REAL` | `double precision` |
| `DATE` | `date` |
| `TIMESTAMP`, `DATETIME`, `POSIXTIME` | `timestamp` (or `timestamptz`, read as UTC) |
| `VARCHAR`, `LONGVARCHAR` (streams), `VECTOR`, others | `text` (`jsonb` also accepted) |

## How correctness is preserved

- **Every pushed-down filter is re-checked by Postgres.** Pushed-down conditions only need to
  return a superset of the matching rows. This matters because IRIS's default collation
  (SQLUPPER) compares strings case-insensitively. For example, `city = 'roma'` returns no rows,
  as in Postgres. Conditions that cannot be translated, such as `ILIKE` or complex expressions,
  are evaluated by Postgres.
- **`LIMIT` never truncates results by default.** The wrappers host passes the query's `LIMIT`
  to the wrapper even when the table is joined or other filters are still applied locally, so
  truncating on IRIS could lose rows. By default the `LIMIT` only sizes the first page. If
  Postgres needs more rows, the next pages are fetched.
- **`limit_pushdown 'true'`** sends `TOP n` + `ORDER BY` to IRIS (top-N queries such as the
  vector search above). It applies only when all of these hold:
  - every condition was pushed down;
  - the query is sorted by at least one column that the host passed to the wrapper;
  - no text column is sorted (IRIS and Postgres collations differ).

  The host silently drops `ORDER BY` items that are expressions (`order by round(x)`, or an
  alias of an expression). Because of this the wrapper never uses `TOP` without a sort key, so
  `limit_pushdown` cannot return the wrong top-N rows in that case. Enable it only on tables you
  don't join.
- **Identifiers** are always sent as delimited identifiers (`"name"`). IRIS still resolves them
  case-insensitively, and any column name works, including the ~250 IRIS SQL reserved words
  (`role`, `start`, ...). Table names from the options are validated part by part (`Schema.Table`,
  `"My Schema"."My Table"`), so a table option cannot inject SQL.
- **IRIS parameter limit**: IRIS accepts at most 380 `?` per statement. `IN` lists longer than 100
  values, and conditions beyond a budget of 300 parameters, are not sent to IRIS: Postgres
  evaluates them instead.
- **NULL vs empty string**: IRIS stores `''` as `$c(0)`. The gateway converts it both ways, so
  `NULL` and `''` keep their meaning.

## Security notes

- The gateway runs each statement through IRIS Dynamic SQL **as the authenticated IRIS user**,
  so IRIS enforces that user's SQL privileges (`SQLCODE -99` otherwise). Use a dedicated user
  that has only the `GRANT`s you want Supabase to see.
- `/query` accepts only `SELECT`, and `/exec` only `INSERT`/`UPDATE`/`DELETE`. A `SELECT` can still
  call SQL functions (`[SqlProc]`) that the user may `EXECUTE`, so don't grant `EXECUTE` on
  procedures with side effects to the FDW user.
- `set ^FDW.Gateway("readOnly") = 1` disables `/exec` entirely.
- Store the password in the Supabase Vault (`password_id`), not in plain server options. Use
  HTTPS between Supabase and IRIS outside a local network.
- In Supabase, keep the foreign tables in a schema that the Data API does not expose (`iris`).
  Expose data through views or functions that check `auth.uid()` / `auth.jwt()`, as in the
  patient portal demo. Postgres RLS cannot be enabled on foreign tables.

## Development

```bash
./scripts/build-wasm.sh          # unit tests + dist/iris_fdw.wasm + checksum
./scripts/load-wasm.sh           # copy into the local Supabase db container
```

During development use `fdw_package_url 'file:///tmp/iris_fdw.wasm'`. Packages downloaded from a
URL are cached by name and version.

IRIS unit tests:

```objectscript
zpm "iris-fdw-gateway test -only"
```

End-to-end regression suite against the running demo. It covers paging across many pages,
pushdown versus local filtering, writes, `''` vs `NULL`, vector top-N, Atelier mode, error
reporting and table-name validation:

```bash
docker exec -i supabase_db_iris-fdw-demo psql -U postgres -v ON_ERROR_STOP=1 < tests/e2e.sql
```

GitHub Actions runs everything on every push: Rust unit tests, the Wasm build, Supabase + IRIS,
the IRIS unit tests and the e2e suite.

Releases: pushing a tag `vX.Y.Z` makes GitHub Actions build the component, compute the SHA-256
checksum and publish a release with a ready-to-use `create server` snippet.

## Repository layout

```
fdw/                 Rust Wasm FDW (src/core.rs = SQL generation & conversions, src/lib.rs = host routines)
wit/v2/              Supabase Wrappers WIT interfaces (v2, from supabase/wrappers)
iris/src/            ObjectScript gateway (FDW.Gateway.REST)
iris/tests/          %UnitTest tests
module.xml           IPM package iris-fdw-gateway
demo/                Supabase project, demo IRIS classes and SQL scripts
scripts/             build / load / demo scripts
```

## Limitations

- `numeric` values travel as 64-bit floats (a limit of the Wasm FDW interface). To keep more
  than 15 significant digits, declare the column as `text`.
- Atelier mode returns the whole result set in a single response and does not read streams.
  Stream columns come back as internal stream ids. Use the gateway for large tables.
- Paging is stateless: each page re-runs the query and skips the previous rows. That is fine up
  to tens of thousands of rows. For bigger scans raise `batch_size` (up to 50,000). If the table
  changes while a multi-page scan is running, pages may overlap or miss rows, as with any
  offset-based paging.
- `VECTOR` columns are read as text (`"0.1,0.2,..."`). To write them, use an IRIS SQL function
  with `TO_VECTOR()`: a plain text value is not converted.
- `TIME` columns are mapped to `text`. The Wasm FDW interface has no time type.
- InterSystems IRIS Cloud SQL only exposes SQL drivers (no web applications), so it is not
  supported.

## License

Apache-2.0. The WIT files in `wit/v2` come from [supabase/wrappers](https://github.com/supabase/wrappers) (Apache-2.0).
