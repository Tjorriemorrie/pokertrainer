# Local PostgreSQL instance for pokertrainer.
# All data lives in <project>\data, so deleting that folder removes everything.
#
# Usage:
#   .\pg.ps1          # init (first time), start, create db/user
#   .\pg.ps1 stop     # stop the server
#   .\pg.ps1 status   # show server status
#   .\pg.ps1 reset    # stop server and DELETE all data

param(
    [string]$Command = "up"
)

# ====== CONFIG ======
$pgBin      = "C:\Program Files\PostgreSQL\18\bin"
$dataDir    = Join-Path $PSScriptRoot "data"
$logFile    = Join-Path $dataDir "server.log"

$dbName     = "pokertrainer"
$dbUser     = "pokertrainer"
$dbPassword = "pokertrainer"
$dbPort     = 5433

# ====== FUNCTIONS ======

# Initialize PostgreSQL data directory (trust auth => non-interactive, localhost-only)
function Init-DB {
    if (-not (Test-Path $dataDir)) {
        Write-Host "Initializing PostgreSQL data directory at $dataDir..."
        & "$pgBin\initdb.exe" -D "$dataDir" -U postgres -A trust --encoding=UTF8 --locale=C --no-instructions
    } else {
        Write-Host "Data directory already exists at $dataDir"
    }
}

# Start PostgreSQL and wait until it accepts connections
function Start-DB {
    $running = & "$pgBin\pg_ctl.exe" -D "$dataDir" status 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "PostgreSQL already running."
        return
    }
    Write-Host "Starting PostgreSQL on port $dbPort..."
    & "$pgBin\pg_ctl.exe" -D "$dataDir" -l "$logFile" -o "-p $dbPort" start

    $maxAttempts = 30
    $attempt = 0
    while ($attempt -lt $maxAttempts) {
        $attempt++
        & "$pgBin\pg_isready.exe" -h localhost -p $dbPort 2>$null | Out-Null
        if ($LASTEXITCODE -eq 0) {
            Write-Host "PostgreSQL is ready (waited $attempt seconds)."
            return
        }
        Start-Sleep -Seconds 1
    }
    Write-Host "WARNING: PostgreSQL did not become ready after $maxAttempts seconds. Check $logFile"
}

# Stop PostgreSQL
function Stop-DB {
    & "$pgBin\pg_ctl.exe" -D "$dataDir" status 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "PostgreSQL is not running."
        return
    }
    Write-Host "Stopping PostgreSQL..."
    & "$pgBin\pg_ctl.exe" -D "$dataDir" -m fast stop
    Write-Host "PostgreSQL stopped."
}

# Show server status
function Get-PgStatus {
    & "$pgBin\pg_ctl.exe" -D "$dataDir" status
    & "$pgBin\pg_isready.exe" -h localhost -p $dbPort
}

# Stop server and delete all data (fresh start next run)
function Reset-DB {
    Stop-DB
    if (Test-Path $dataDir) {
        $answer = Read-Host "Delete ALL database data in $dataDir? (y/N)"
        if ($answer -eq "y") {
            Remove-Item "$dataDir" -Recurse -Force
            Write-Host "Data directory deleted. Run .\pg.ps1 to re-initialize."
        } else {
            Write-Host "Aborted."
        }
    } else {
        Write-Host "Nothing to delete."
    }
}

# Ensure app role and database exist
function Create-DB-And-User {
    Write-Host "Ensuring user '$dbUser' exists..."
    $roleExists = & "$pgBin\psql.exe" -U postgres -h localhost -p $dbPort -t -c "SELECT 1 FROM pg_roles WHERE rolname='$dbUser';" 2>&1
    if (-not $roleExists -or -not "$roleExists".Trim()) {
        & "$pgBin\psql.exe" -U postgres -h localhost -p $dbPort -c "CREATE ROLE $dbUser LOGIN PASSWORD '$dbPassword';"
        Write-Host "User '$dbUser' created."
    } else {
        Write-Host "User '$dbUser' already exists."
    }

    Write-Host "Ensuring database '$dbName' exists..."
    $dbExists = & "$pgBin\psql.exe" -U postgres -h localhost -p $dbPort -t -c "SELECT 1 FROM pg_database WHERE datname='$dbName';" 2>&1
    if (-not $dbExists -or -not "$dbExists".Trim()) {
        & "$pgBin\psql.exe" -U postgres -h localhost -p $dbPort -c "CREATE DATABASE $dbName OWNER $dbUser ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0;"
        Write-Host "Database '$dbName' created."
    } else {
        Write-Host "Database '$dbName' already exists."
    }

    Write-Host "Database and user are ready."
}

# ====== MAIN ======

switch ($Command.ToLower()) {
    "up"     { Init-DB; Start-DB; Create-DB-And-User; Write-Host "`nPostgreSQL ready on port $dbPort." }
    "stop"   { Stop-DB }
    "status" { Get-PgStatus }
    "reset"  { Reset-DB }
    default  { Write-Host "Unknown command '$Command'. Use: up | stop | status | reset" }
}
