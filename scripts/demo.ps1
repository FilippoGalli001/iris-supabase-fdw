# End-to-end demo for Windows (PowerShell 5.1+ / PowerShell 7): Supabase (local) + IRIS + iris_fdw.
#   powershell -ExecutionPolicy Bypass -File scripts\demo.ps1
# Requirements: Docker Desktop (running) and Node.js. Uses the published release of iris_fdw.wasm,
# so no Rust toolchain or build is needed. (To build from source, use WSL/Git Bash: scripts/demo.sh --build)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$Version  = '0.1.0'
$Checksum = 'c39c575db2def2168c927ace03ce4c1923ca6e4c8f1d269b0496300f399c78e5'
$Url      = "https://github.com/FilippoGalli001/iris-supabase-fdw/releases/download/v$Version/iris_fdw.wasm"
$Db       = 'supabase_db_iris-fdw-demo'

function Step($msg) { Write-Host ""; Write-Host ("==> [{0}] {1}" -f (Get-Date -Format HH:mm:ss), $msg) -ForegroundColor Cyan }
function Run($exe, [string[]]$argList) {
    & $exe @argList
    if ($LASTEXITCODE -ne 0) { throw "$exe $($argList -join ' ') failed (exit $LASTEXITCODE)" }
}

Step "Downloading iris_fdw.wasm v$Version from the GitHub release"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
New-Item -ItemType Directory -Force dist | Out-Null
$ProgressPreference = 'SilentlyContinue'
Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile dist\iris_fdw.wasm
$actual = (Get-FileHash dist\iris_fdw.wasm -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $Checksum) { throw "checksum mismatch: $actual" }
Write-Host "checksum OK"

Step "Starting Supabase (first run downloads the Supabase images)"
Push-Location demo
try { Run 'npx' @('-y', 'supabase', 'start') } finally { Pop-Location }

Step "Starting IRIS with the iris-fdw-gateway package and demo data"
Run 'docker' @('compose', 'up', '-d', '--build')
$auth = 'Basic ' + [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes('fdw_user:fdw_password'))
$up = $false
for ($i = 0; $i -lt 120 -and -not $up; $i++) {
    try {
        Invoke-WebRequest -UseBasicParsing -Uri 'http://localhost:32773/iris-fdw/v1/ping' -Headers @{ Authorization = $auth } | Out-Null
        $up = $true
    } catch { Start-Sleep -Seconds 3 }
}
if (-not $up) { throw "IRIS gateway did not come up; see: docker logs iris-fdw" }

Step "Loading the FDW into Postgres and creating the foreign tables"
Run 'docker' @('cp', 'dist/iris_fdw.wasm', "${Db}:/tmp/iris_fdw.wasm")
foreach ($f in '01_setup.sql', '02_tables.sql', '03_patient_portal.sql') {
    Run 'docker' @('cp', "demo/sql/$f", "${Db}:/tmp/$f")
    Run 'docker' @('exec', $Db, 'psql', '-U', 'postgres', '-v', 'ON_ERROR_STOP=1', '-q', '-f', "/tmp/$f")
}

Step "First query through the wrapper"
Run 'docker' @('exec', $Db, 'psql', '-U', 'postgres', '-c', "select id, name, city from iris.patient where city = 'Roma' order by id limit 5")

Step "Ready"
Write-Host "    SQL:    docker exec -it $Db psql -U postgres"
Write-Host "    Studio: http://127.0.0.1:54323  (SQL editor -> select * from iris.patient limit 10)"
Write-Host "    Tests:  docker cp tests/e2e.sql ${Db}:/tmp/e2e.sql; docker exec $Db psql -U postgres -v ON_ERROR_STOP=1 -f /tmp/e2e.sql"
