-- Create one foreign table per IRIS table of the "Demo" schema, with the right types
import foreign schema "Demo" from server iris_server into iris;

-- A foreign table can also be backed by any IRIS SELECT (read-only).
-- %EXACT keeps the original case: IRIS GROUP BY uses the SQLUPPER collation by default.
create foreign table if not exists iris.spend_by_specialty (
  specialty text,
  visits bigint,
  total numeric,
  _attrs jsonb          -- optional: the whole IRIS row as JSON
)
server iris_server
options (
  query 'select %EXACT(Specialty) as Specialty, count(*) as visits, sum(Cost) as total from Demo.Appointment group by %EXACT(Specialty)'
);

-- IRIS Vector Search from Supabase: patients with the most similar clinical profile.
-- VECTOR_COSINE runs inside IRIS; the patient filter, ORDER BY and LIMIT are pushed down
-- (limit_pushdown is safe here: we never join this table).
create foreign table if not exists iris.similar_patients (
  patient_id bigint,
  similar_id bigint,
  similar_name text,
  similarity double precision
)
server iris_server
options (
  query 'SELECT a.ID AS patient_id, b.ID AS similar_id, b.Name AS similar_name,
                VECTOR_COSINE(a.Profile, b.Profile) AS similarity
         FROM Demo.Patient a JOIN Demo.Patient b ON a.ID <> b.ID',
  limit_pushdown 'true'
);
