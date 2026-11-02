-- "Patient portal": clinical data stays in IRIS, the app is built on Supabase.
-- Authenticated users call /rest/v1/rpc/my_appointments and only see their own
-- appointments, read live from IRIS. The iris schema itself is NOT exposed by the API.

create or replace function public.my_appointments()
returns table (
  appointment_id bigint,
  starts_at timestamp,
  doctor text,
  specialty text,
  status text,
  cost numeric,
  paid boolean
)
language plpgsql
stable
security definer
set search_path = ''
as $$
declare
  v_email text := auth.jwt() ->> 'email';
  v_patient_id bigint;
begin
  if v_email is null then
    raise exception 'not authenticated';
  end if;

  -- pushed down to IRIS as: SELECT id, email FROM Demo.Patient WHERE email = ?
  select p.id into v_patient_id from iris.patient p where p.email = v_email;
  if v_patient_id is null then
    return;
  end if;

  -- pushed down to IRIS as: ... FROM Demo.Appointment WHERE patient = ?
  return query
    select a.id, a.startsat, a.doctor, a.specialty, a.status, a.cost, a.paid
    from iris.appointment a
    where a.patient = v_patient_id
    order by a.startsat;
end;
$$;

revoke all on function public.my_appointments() from public, anon;
grant execute on function public.my_appointments() to authenticated;
