/// Creates demo data and a least-privilege SQL user for the Supabase FDW.
Class Demo.Setup
{

ClassMethod Run(patients As %Integer = 200) As %Status
{
    do ..CreateData(patients)
    do ..CreateProfiles()
    do ..CreateUser()
    quit $$$OK
}

/// Compute a small "clinical profile" vector for each patient
ClassMethod CreateProfiles()
{
    set sql = "SELECT p.ID, DATEDIFF('yy', p.BirthDate, CURRENT_DATE) AS Age, p.RiskScore, "
        _ "COUNT(a.ID) AS Visits, SUM(CASE WHEN a.Specialty = 'Cardiology' THEN 1 ELSE 0 END) AS Cardio "
        _ "FROM Demo.Patient p LEFT JOIN Demo.Appointment a ON a.Patient = p.ID GROUP BY p.ID"
    set rs = ##class(%SQL.Statement).%ExecDirect(, sql)
    set upd = ##class(%SQL.Statement).%New()
    $$$ThrowOnError(upd.%Prepare("UPDATE Demo.Patient SET Profile = TO_VECTOR(?, DOUBLE) WHERE ID = ?"))
    while rs.%Next() {
        set visits = rs.Visits
        set vec = $fnumber(rs.Age / 100, "", 3) _ "," _ $fnumber(rs.RiskScore / 100, "", 3) _ ","
            _ $fnumber(visits / 4, "", 3) _ "," _ $fnumber($select(visits: rs.Cardio / visits, 1: 0), "", 3)
        set r = upd.%Execute(vec, rs.ID)
        if r.%SQLCODE < 0 write !, "Profile update failed: ", r.%Message, !
    }
    write !, "Clinical profile vectors created", !
}

ClassMethod CreateData(patients As %Integer = 200)
{
    do ##class(Demo.Appointment).%KillExtent()
    do ##class(Demo.Patient).%KillExtent()

    set first = $listbuild("Anna","Marco","Giulia","Luca","Sofia","Matteo","Chiara","Davide","Elena","Paolo","Zoë","José")
    set last = $listbuild("Rossi","Bianchi","Ferrari","Esposito","Romano","Colombo","Ricci","Marino","Greco","Bruno")
    set cities = $listbuild("Milano","Roma","Torino","Napoli","Bologna","Firenze","")
    set doctors = $listbuild("Dr. House","Dr. Grey","Dr. Strange","Dr. Watson","Dr. Quinn")
    set specs = $listbuild("Cardiology","Neurology","Dermatology","Pediatrics","Oncology")
    set statuses = $listbuild("scheduled","completed","cancelled")
    set base = $zdateh("1950-01-01", 3)

    for i = 1:1:patients {
        set p = ##class(Demo.Patient).%New()
        set p.Name = $list(first, i # $listlength(first) + 1) _ " " _ $list(last, i * 7 # $listlength(last) + 1)
        set p.BirthDate = base + (i * 9973 # 25000)
        set p.City = $list(cities, i # $listlength(cities) + 1)
        set p.Email = $select(i # 10 = 0: "", 1: "patient" _ i _ "@example.com")
        set p.Active = (i # 6 '= 0)
        set p.RiskScore = $justify((i * 37 # 1000) / 10, 0, 2)
        if i # 25 = 0 do p.Notes.Write("Follow-up required. Patient #" _ i _ " reported symptoms during the last visit.")
        $$$ThrowOnError(p.%Save())

        for j = 1:1:(i # 4) + 1 {
            set a = ##class(Demo.Appointment).%New()
            set a.Patient = p
            set a.Doctor = $list(doctors, (i + j) # $listlength(doctors) + 1)
            set a.Specialty = $list(specs, (i * j) # $listlength(specs) + 1)
            set day = $zdateh("2026-09-01", 3) + ((i * 13 + j * 5) # 120)
            set a.StartsAt = $zdate(day, 3) _ " " _ $translate($justify(8 + ((i + j) # 10), 2), " ", "0") _ ":" _ $select(j # 2: "00", 1: "30") _ ":00"
            set a.Status = $list(statuses, (i + j) # 3 + 1)
            set a.Cost = 50 + ((i * j * 17) # 200) + 0.5
            set a.Paid = (a.Status = "completed")
            $$$ThrowOnError(a.%Save())
        }
    }
    write !, "Demo data created: ", patients, " patients", !
}

/// Create the IRIS user used by Supabase, with SQL privileges limited to the Demo schema
ClassMethod CreateUser(user As %String = "fdw_user", password As %String = "fdw_password")
{
    new $namespace
    set $namespace = "%SYS"
    if '##class(Security.Users).Exists(user) {
        $$$ThrowOnError(##class(Security.Users).Create(user, "", password, "Supabase FDW user"))
    }
    set $namespace = "USER"
    set rs = ##class(%SQL.Statement).%ExecDirect(, "GRANT SELECT, INSERT, UPDATE, DELETE ON SCHEMA Demo TO " _ user)
    if rs.%SQLCODE < 0 write !, "GRANT failed: ", rs.%Message, !
    write !, "User ", user, " ready", !
}

}
