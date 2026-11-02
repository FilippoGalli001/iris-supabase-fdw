-- End-to-end regression tests: run after demo/sql/01..03 on a fresh demo database.
--   docker exec -i supabase_db_iris-fdw-demo psql -U postgres -v ON_ERROR_STOP=1 -f - < tests/e2e.sql
-- Every block raises an exception (and stops psql) when a check fails.

\set QUIET on
set client_min_messages = warning;

-- a second server with tiny pages to exercise multi-page scans
drop server if exists iris_small_pages cascade;
create server iris_small_pages foreign data wrapper wasm_wrapper options (
  fdw_package_url 'file:///tmp/iris_fdw.wasm', fdw_package_name 'fgalli:iris-fdw', fdw_package_version '0.1.0',
  api_url 'http://iris-fdw:52773', username 'fdw_user', password 'fdw_password', batch_size '7');
create schema if not exists e2e;
import foreign schema "Demo" limit to (patient, appointment) from server iris_small_pages into e2e;

-- an Atelier-mode server (logical mode: $HOROLOG dates)
drop server if exists iris_atelier_e2e cascade;
create server iris_atelier_e2e foreign data wrapper wasm_wrapper options (
  fdw_package_url 'file:///tmp/iris_fdw.wasm', fdw_package_name 'fgalli:iris-fdw', fdw_package_version '0.1.0',
  api_url 'http://iris-fdw:52773', api_mode 'atelier', username '_SYSTEM', password 'SYS');
create schema if not exists e2e_atelier;
import foreign schema "Demo" limit to (patient) from server iris_atelier_e2e into e2e_atelier;

do $$
declare n bigint; s numeric; t text;
begin
  -- counts and paging
  select count(*) into n from iris.patient;               assert n = 200, format('patients: %s', n);
  select count(*) into n from iris.appointment;           assert n = 500, format('appointments: %s', n);
  select count(*) into n from e2e.patient;                assert n = 200, format('patients (7-row pages): %s', n);
  select count(distinct id) into n from e2e.appointment;  assert n = 500, format('distinct ids across pages: %s', n);
  select count(*) into n from (select id from e2e.patient limit 20) x; assert n = 20, 'limit across pages';

  -- pushed-down filters give the same result as local filtering
  select count(*) into n from iris.patient where city = 'Roma';
  assert n = (select count(*) from (select city from iris.patient) x where city = 'Roma'), 'city = pushdown';
  select count(*) into n from iris.patient where city = 'roma';            assert n = 0, 'case-sensitive equality';
  select count(*) into n from iris.patient where name like 'Zo%';
  assert n = (select count(*) from (select name from iris.patient) x where name like 'Zo%'), 'LIKE pushdown';
  select count(*) into n from iris.patient where name ilike 'zo%';         assert n > 0, 'ILIKE evaluated locally';
  select count(*) into n from iris.patient where id in (1, 2, 3, 999);     assert n = 3, 'IN pushdown';
  select count(*) into n from iris.patient where id not in (1, 2, 3);      assert n = 197, 'NOT IN pushdown';
  -- constant array of 150 values (dynamic SQL makes it a literal): over the IN budget
  execute format('select count(*) from iris.patient where id = any(%L::bigint[])',
                 (select array_agg(g)::text from generate_series(1, 150) g)) into n;
  assert n = 150, 'IN list longer than the parameter budget';
  select count(*) into n from iris.patient where city is null;
  assert n = (select count(*) from (select city from iris.patient) x where city is null), 'IS NULL';
  select count(*) into n from iris.patient where birthdate >= '2000-01-01';
  assert n = (select count(*) from (select birthdate from iris.patient) x where birthdate >= '2000-01-01'), 'date pushdown';

  -- types
  select name into t from iris.patient where id = 22;                       assert t = 'Zoë Romano', format('unicode: %s', t);
  select sum(cost) into s from iris.appointment;
  assert s = (select sum(cost) from e2e.appointment), 'numeric across pages';

  -- writes: '' vs NULL, update to NULL, delete
  insert into iris.patient (name, city, email, birthdate, active) values ('E2E Test', 'Pisa', '', '1999-12-31', true);
  select count(*) into n from iris.patient where name = 'E2E Test' and email = '' and city = 'Pisa'; assert n = 1, 'insert';
  update iris.patient set city = null, riskscore = 1.5 where name = 'E2E Test';
  select count(*) into n from iris.patient where name = 'E2E Test' and city is null and riskscore = 1.5; assert n = 1, 'update';
  delete from iris.patient where name = 'E2E Test';
  select count(*) into n from iris.patient where name = 'E2E Test';         assert n = 0, 'delete';

  -- custom query, _attrs, vector search
  select count(*) into n from iris.spend_by_specialty;                      assert n = 5, 'query option';
  select _attrs ->> 'Specialty' into t from iris.spend_by_specialty order by specialty limit 1;
  assert t = 'Cardiology', format('_attrs: %s', t);
  -- ORDER BY an expression alias: TOP must not be pushed (regression)
  select string_agg(similar_id::text, ',') into t from (
    select similar_id, round(similarity::numeric, 4) as similarity
    from iris.similar_patients where patient_id = 7 order by similarity desc limit 3) x;
  assert t = (select string_agg(similar_id::text, ',') from (
    select similar_id from (select * from iris.similar_patients where patient_id = 7) y
    order by similarity desc limit 3) z), format('vector top-3 with expression sort: %s', t);

  -- Atelier mode: logical dates
  select count(*) into n from e2e_atelier.patient where birthdate < '1980-01-01';
  assert n = (select count(*) from iris.patient where birthdate < '1980-01-01'), 'atelier date filter';

  raise notice 'all e2e checks passed';
end $$;

-- joins with local tables
create temp table vip (email text);
insert into vip values ('patient7@example.com'), ('patient22@example.com');
do $$ declare n bigint; begin
  select count(*) into n from vip v join iris.patient p on p.email = v.email; assert n = 2, 'join';
end $$;

-- errors from IRIS are reported
do $$ begin
  create foreign table pg_temp.missing (id bigint) server iris_server options (table 'Demo.DoesNotExist');
  perform * from pg_temp.missing;
  raise exception 'expected an IRIS error';
exception when others then
  assert sqlerrm like '%SQLCODE -30%', format('error message: %s', sqlerrm);
end $$;

-- invalid table names are rejected before reaching IRIS
do $$ begin
  create foreign table pg_temp.inj (id bigint) server iris_server options (table 'Demo.Patient WHERE 1=1 OR id');
  perform * from pg_temp.inj;
  raise exception 'expected a validation error';
exception when others then
  assert sqlerrm like '%invalid IRIS table name%', format('error message: %s', sqlerrm);
end $$;

drop schema e2e cascade;
drop schema e2e_atelier cascade;
drop server iris_small_pages cascade;
drop server iris_atelier_e2e cascade;
\echo 'E2E: OK'
