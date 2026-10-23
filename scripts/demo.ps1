# derive-sync Windows one-click demo (ASCII-only for Windows PowerShell 5.1).
#
# Flow (all CLI processes run SERIALLY; a relay only allows one connection per device,
# so a daemon must be stopped before running one-shot commands like catalog add/fetch
# or vault put/get):
#   1) start relay on 127.0.0.1:<port>
#   2) init two data dirs (demo-A / demo-B)
#   3) pair A (initiator, background) with B (joins using the 8-digit code)
#   4) daemon A + daemon B with --clipboard file:<path>; A -> B and B -> A text sync
#   5) stop daemons; A `catalog add` a file
#   6) start A daemon (serves the file); B `catalog fetch` -> verify SHA256
#   7) stop A daemon; A `vault put` -> `vault get` -> verify SHA256
#
# Usage:
#   powershell -NoProfile -File scripts/demo.ps1
#   powershell -NoProfile -File scripts/demo.ps1 -RelayPort 18888 -WorkRoot target/demo
#
# NOTE: this script is intentionally ASCII-only so Windows PowerShell 5.1 parses it.
param(
    [int]$RelayPort = 18787,
    [string]$WorkRoot = "target/demo"
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$sync = Join-Path $repo "target/release/derive-sync.exe"
$relayExe = Join-Path $repo "target/release/derive-relay.exe"
foreach ($exe in @($sync, $relayExe)) {
    if (-not (Test-Path $exe)) {
        throw "missing binary: $exe ; run: cargo build --release -p derive-cli -p derive-relay"
    }
}
$work = Join-Path $repo $WorkRoot
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Force -Path $work | Out-Null

$token = "demo-access-token"
$url = "ws://127.0.0.1:$RelayPort/ws"
$stepLog = Join-Path $work "demo-steps.log"
$script:children = @()
$script:failures = 0

function Log([string]$message) {
    $line = "[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $message
    Write-Host $line
    Add-Content -Path $stepLog -Value $line
}
function Log-Fail([string]$message) {
    $script:failures += 1
    Log "FAIL: $message"
}
function Invoke-Cli([string[]]$arguments, [string]$label) {
    Log "RUN $label"
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $output = & $sync @arguments 2>&1 | Out-String
    $exitCode = $LASTEXITCODE
    $ErrorActionPreference = $previous
    if ($exitCode -ne 0) {
        Log-Fail "$label exit=$exitCode"
        Log ($output.Trim())
        return $null
    }
    Log ($output.Trim())
    return $output
}
function Start-Daemon([string]$dataDir, [string]$clipFile, [string]$logName) {
    $out = Join-Path $work "$logName.out.log"
    $err = Join-Path $work "$logName.err.log"
    $proc = Start-Process -FilePath $sync `
        -ArgumentList @("--data-dir", $dataDir, "--relay", $url, "--token", $token,
                        "daemon", "--clipboard", "file:$clipFile", "--poll-ms", "100") `
        -RedirectStandardOutput $out -RedirectStandardError $err -PassThru -NoNewWindow
    $script:children += $proc
    return @{ Process = $proc; Stdout = $out; Stderr = $err }
}
function Wait-LogMatch([string]$path, [string]$pattern, [int]$timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $path) {
            # UTF8: daemon logs are UTF-8; PS 5.1 Get-Content defaults to ANSI and would garble CJK lines, breaking pattern matches
            $content = Get-Content -Path $path -Raw -Encoding UTF8 -ErrorAction SilentlyContinue
            if ($content -and ($content -match $pattern)) { return $true }
        }
        Start-Sleep -Milliseconds 250
    }
    return $false
}
function Wait-FileContent([string]$path, [string]$expected, [int]$timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $path) {
            $content = Get-Content -Path $path -Raw -Encoding UTF8 -ErrorAction SilentlyContinue
            if ($content -and $content.Contains($expected)) { return $true }
        }
        Start-Sleep -Milliseconds 250
    }
    return $false
}
function Wait-FileContains([string]$path, [string]$expected, [int]$timeoutSec) {
    return Wait-FileContent $path $expected $timeoutSec
}
function Stop-Daemon($daemon) {
    if ($daemon -and $daemon.Process -and -not $daemon.Process.HasExited) {
        Stop-Process -Id $daemon.Process.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 400
    }
}
function Get-Sha256Hex([string]$path) {
    return (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLowerInvariant()
}

try {
    # ------------------------------------------------------------------
    Log "STEP 1/7: start relay on 127.0.0.1:$RelayPort"
    $relayLog = Join-Path $work "relay.out.log"
    $relayErr = Join-Path $work "relay.err.log"
    $relayProc = Start-Process -FilePath $relayExe `
        -ArgumentList @("--bind", "127.0.0.1:$RelayPort", "--token", $token,
                        "--vault-dir", (Join-Path $work "relay-vault")) `
        -RedirectStandardOutput $relayLog -RedirectStandardError $relayErr -PassThru -NoNewWindow
    $script:children += $relayProc
    $healthy = $false
    for ($i = 0; $i -lt 40; $i++) {
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$RelayPort/healthz" -TimeoutSec 1
            if ($response.StatusCode -eq 200) { $healthy = $true; break }
        } catch { }
        Start-Sleep -Milliseconds 150
    }
    if (-not $healthy) { throw "relay did not become healthy on port $RelayPort" }
    Log "relay healthy (http://127.0.0.1:$RelayPort/healthz -> 200 ok)"

    # ------------------------------------------------------------------
    Log "STEP 2/7: init two devices"
    $dirA = Join-Path $work "data-a"
    $dirB = Join-Path $work "data-b"
    $initA = Invoke-Cli @("--data-dir", $dirA, "--relay", $url, "--token", $token, "init", "--name", "demo-A") "init demo-A"
    $initB = Invoke-Cli @("--data-dir", $dirB, "--relay", $url, "--token", $token, "init", "--name", "demo-B") "init demo-B"
    if (-not $initA -or -not $initB) { throw "init failed" }

    # ------------------------------------------------------------------
    Log "STEP 3/7: pair A (initiator) with B (joins by 8-digit code)"
    $pairAOut = Join-Path $work "pair-a.out.log"
    $pairAErr = Join-Path $work "pair-a.err.log"
    $pairA = Start-Process -FilePath $sync `
        -ArgumentList @("--data-dir", $dirA, "--relay", $url, "--token", $token, "pair", "--yes") `
        -RedirectStandardOutput $pairAOut -RedirectStandardError $pairAErr -PassThru -NoNewWindow
    $script:children += $pairA
    if (-not (Wait-LogMatch $pairAOut '([0-9]{8})' 20)) { throw "pair code not found in A output" }
    $pairText = Get-Content -Path $pairAOut -Raw
    $codeMatch = [regex]::Match($pairText, '([0-9]{8})')
    $code = $codeMatch.Groups[1].Value
    Log "pair code from A: $code"
    $pairB = Invoke-Cli @("--data-dir", $dirB, "--relay", $url, "--token", $token, "pair", "--code", $code, "--yes") "pair demo-B"
    $pairA.WaitForExit(15000) | Out-Null
    $pairA.Refresh()
    if ($pairA.HasExited -and ($null -ne $pairA.ExitCode) -and $pairA.ExitCode -ne 0) {
        Log "warn: pair demo-A exit=$($pairA.ExitCode) (judged by peers.json instead)"
    }
    $peersA = Join-Path $dirA "peers.json"
    $paired = ((Test-Path $peersA) -and ((Get-Content $peersA -Raw -Encoding UTF8) -match 'dev-'))
    if (-not $pairB -or -not $paired) { $script:failures += 1; Log-Fail "pair demo-B failed" } else { Log "pair OK" }

    # ------------------------------------------------------------------
    Log "STEP 4/7: daemons (file clipboard) + bidirectional clipboard sync"
    $clipA = Join-Path $work "clip-a.txt"
    $clipB = Join-Path $work "clip-b.txt"
    Set-Content -Path $clipA -Value "" -NoNewline
    Set-Content -Path $clipB -Value "" -NoNewline
    $daemonA = Start-Daemon $dirA $clipA "daemon-a"
    $daemonB = Start-Daemon $dirB $clipB "daemon-b"
    $sessionSeen = (Wait-LogMatch $daemonA.Stdout "E2E" 10) -or (Wait-LogMatch $daemonA.Stderr "E2E|session" 1)
    if (-not $sessionSeen) { Log "WARN: A daemon session line not matched (clipboard sync below is the real check)" } else { Log "session A<->B established" }
    $clipText = "hello from demo-A $(Get-Date -Format 'HH:mm:ss')"
    Set-Content -Path $clipA -Value $clipText -NoNewline
    if (Wait-FileContent $clipB $clipText 15) {
        Log "clipboard A -> B OK: $clipText"
    } else {
        $script:failures += 1
        Log-Fail "clipboard A -> B did not arrive (B file: $(Get-Content $clipB -Raw -ErrorAction SilentlyContinue))"
    }
    $replyText = "reply from demo-B $(Get-Date -Format 'HH:mm:ss')"
    Set-Content -Path $clipB -Value $replyText -NoNewline
    if (Wait-FileContent $clipA $replyText 15) {
        Log "clipboard B -> A OK: $replyText"
    } else {
        $script:failures += 1
        Log-Fail "clipboard B -> A did not arrive (A file: $(Get-Content $clipA -Raw -ErrorAction SilentlyContinue))"
    }

    # ------------------------------------------------------------------
    Log "STEP 5/7: catalog add on A (daemons stopped: relay allows one connection per device)"
    Stop-Daemon $daemonA
    Stop-Daemon $daemonB
    $payload = Join-Path $work "installer.bin"
    $bytes = New-Object byte[] (512 * 1024)
    (New-Object System.Random 42).NextBytes($bytes)
    [System.IO.File]::WriteAllBytes($payload, $bytes)
    $payloadHash = Get-Sha256Hex $payload
    $addOut = Invoke-Cli @("--data-dir", $dirA, "--relay", $url, "--token", $token, "catalog", "add", "--path", $payload) "catalog add"
    $entryId = $null
    if ($addOut -and ($addOut -match '(ent-[0-9a-f]{16})')) { $entryId = $Matches[1] }
    if (-not $entryId) { throw "catalog add did not return an entry id" }
    Log "catalog entry = $entryId (sha256=$payloadHash)"

    # ------------------------------------------------------------------
    Log "STEP 6/7: let B sync catalog, then B fetches file from A (verify SHA256)"
    $daemonA2 = Start-Daemon $dirA $clipA "daemon-a2"
    $daemonB2 = Start-Daemon $dirB $clipB "daemon-b2"
    # Wait until B's local catalog contains the entry (synced via snapshot from A).
    $bCatalogSeen = $false
    for ($i = 0; $i -lt 40; $i++) {
        $bList = & $sync --data-dir $dirB "catalog" "list" 2>&1 | Out-String
        if ($bList -match [regex]::Escape($entryId)) { $bCatalogSeen = $true; break }
        Start-Sleep -Milliseconds 250
    }
    if (-not $bCatalogSeen) { $script:failures += 1; Log-Fail "B did not receive catalog entry $entryId" }
    # One-shot commands contend with a daemon for the same device connection: stop B daemon, keep A daemon serving the file.
    Stop-Daemon $daemonB2
    Start-Sleep -Milliseconds 500
    $fetchOut = Invoke-Cli @("--data-dir", $dirB, "--relay", $url, "--token", $token, "catalog", "fetch", $entryId, "--out", (Join-Path $work "fetched")) "catalog fetch"
    Stop-Daemon $daemonA2
    $fetched = Get-ChildItem -Path (Join-Path $work "fetched") -File -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $fetched) {
        $script:failures += 1
        Log-Fail "catalog fetch produced no file (output: $fetchOut)"
    } else {
        $fetchedHash = Get-Sha256Hex $fetched.FullName
        if ($fetchedHash -eq $payloadHash) {
            Log "file transfer OK: $($fetched.Name) sha256=$fetchedHash"
        } else {
            $script:failures += 1
            Log-Fail "file SHA256 mismatch: $fetchedHash vs $payloadHash"
        }
    }

    # ------------------------------------------------------------------
    Log "STEP 7/7: vault put on A, share key to B via E2E, B decrypts (TTL 1 day)"
    $putOut = Invoke-Cli @("--data-dir", $dirA, "--relay", $url, "--token", $token, "vault", "put", "--path", $payload, "--ttl-days", "1") "vault put"
    $objectId = $null
    if ($putOut -and ($putOut -match '(obj-[0-9a-f]{16})')) { $objectId = $Matches[1] }
    if (-not $objectId) {
        $script:failures += 1
        Log-Fail "vault put did not return an object id"
    } else {
        Log "vault object = $objectId"
        # Start both daemons so A can share the vault object key to B via E2E.
        $daemonA3 = Start-Daemon $dirA $clipA "daemon-a3"
        $daemonB3 = Start-Daemon $dirB $clipB "daemon-b3"
        $bKeyFile = Join-Path $dirB "vault_keys.json"
        $keyShared = $false
        for ($i = 0; $i -lt 40; $i++) {
            if (Test-Path $bKeyFile) {
                $keyContent = Get-Content $bKeyFile -Raw -Encoding UTF8 -ErrorAction SilentlyContinue
                if ($keyContent -and ($keyContent -match [regex]::Escape($objectId))) { $keyShared = $true; break }
            }
            Start-Sleep -Milliseconds 250
        }
        if (-not $keyShared) { Log "warn: B did not receive vault key in 10s (continue with A-only get fallback)" }
        Stop-Daemon $daemonB3
        Stop-Daemon $daemonA3
        $getDir = if ($keyShared) { $dirB } else { $dirA }
        $restored = Join-Path $work "vault-restored.bin"
        $getOut = Invoke-Cli @("--data-dir", $getDir, "--relay", $url, "--token", $token, "vault", "get", $objectId, "--out", $restored) "vault get"
        if (Test-Path $restored) {
            $restoredHash = Get-Sha256Hex $restored
            if ($restoredHash -eq $payloadHash) {
                if ($keyShared) {
                    Log "vault share roundtrip OK (B decrypted): sha256=$restoredHash"
                } else {
                    Log "vault A-only roundtrip OK: sha256=$restoredHash"
                }
            } else {
                $script:failures += 1
                Log-Fail "vault decrypted SHA256 mismatch: $restoredHash vs $payloadHash"
            }
        } else {
            $script:failures += 1
            Log-Fail "vault get produced no file (output: $getOut)"
        }
    }

    # ------------------------------------------------------------------
}
catch {
    Log-Fail $_.Exception.Message
}
finally {
    Log "cleanup: stopping child processes"
    foreach ($proc in $script:children) {
        if ($proc -and -not $proc.HasExited) {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        }
    }
    Log "demo finished: failures=$($script:failures) (work dir: $work)"
    Log "step log: $stepLog"
    if ($script:failures -gt 0) { exit 1 } else { exit 0 }
}
