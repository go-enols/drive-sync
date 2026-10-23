# Run derive-sync end-to-end tests + latency/throughput benchmark (release build).
#
# Usage (works with Windows PowerShell 5.1 and PowerShell 7+):
#   powershell -NoProfile -File scripts/bench.ps1
#   pwsh -NoProfile -File scripts/bench.ps1
#   powershell -NoProfile -File scripts/bench.ps1 -Quick       # N=100, 5MiB, 20 conns
#   powershell -NoProfile -File scripts/bench.ps1 -SkipTests   # benchmark only
#   powershell -NoProfile -File scripts/bench.ps1 -JsonOut target/bench/my.json
#
# NOTE: do not modify scripts/cargo.ps1; this script only replicates its
# project-local CARGO_HOME setup so it can invoke cargo directly.
# This script is intentionally ASCII-only: Windows PowerShell 5.1 parses .ps1 files
# without a BOM as ANSI, which can corrupt UTF-8 non-ASCII literals.
param(
    [switch]$SkipTests,
    [switch]$Quick,
    [string]$JsonOut = "target/bench/derive-bench.json"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$env:CARGO_HOME = Join-Path $root ".cargo-home"
if (-not (Test-Path $env:CARGO_HOME)) {
    New-Item -ItemType Directory -Path $env:CARGO_HOME -Force | Out-Null
}

Push-Location $root
try {
    if (-not $SkipTests) {
        Write-Host "==> cargo test -p derive-e2e"
        & cargo test -p derive-e2e --offline
        if ($LASTEXITCODE -ne 0) { throw "cargo test -p derive-e2e failed (exit=$LASTEXITCODE)" }
    }

    $benchArgs = @(
        "run", "--release", "-p", "derive-e2e", "--bin", "derive-bench",
        "--offline", "--", "--json-out", $JsonOut
    )
    if ($Quick) { $benchArgs += "--quick" }
    Write-Host "==> derive-bench (release)"
    & cargo @benchArgs
    if ($LASTEXITCODE -ne 0) { throw "derive-bench failed (exit=$LASTEXITCODE)" }

    $resolved = [System.IO.Path]::GetFullPath((Join-Path $root $JsonOut))
    Write-Host ""
    Write-Host "benchmark JSON: $resolved"
}
finally {
    Pop-Location
}
