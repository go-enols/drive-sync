# drive-sync 自动更新本地端到端自测（Windows / PowerShell 5.1+ 或 pwsh）
#
# 验证内容（不需要联网，全部走 file:// 清单与产物）：
#   1. gen-manifest asset/merge 生成未签名清单；
#   2. 未签名清单在 update check 时被拒绝（安全默认）；
#   3. sign-manifest 用临时生成的 Ed25519 密钥签名，check 通过（--json 可解析）；
#   4. update apply 下载 → BLAKE3 校验 → 替换当前 exe（保留 .old-* 备份）→ 写 update-state.json；
#   5. 替换后的 exe 内容 = 测试“新版本”内容；备份内容 = 旧版本内容。
#
# 全过程只操作仓库 tmp/update-e2e 下的 exe 副本与临时密钥，不会碰真实安装的 drive-sync。
# 用法：
#   pwsh -File scripts/update-e2e.ps1
#   pwsh -File scripts/update-e2e.ps1 -KeepWork   # 保留临时目录便于排查

param(
    [switch]$KeepWork
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$env:CARGO_HOME = Join-Path $root '.cargo-home'
Set-Location $root
$target = 'x86_64-pc-windows-msvc'
$binExt = '.exe'

Write-Host '==> 构建 drive-cli / drive-updater（debug）' -ForegroundColor Cyan
& cargo build -p drive-cli -p drive-updater
if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败' }

$work = Join-Path $root 'tmp\update-e2e'
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
foreach ($dir in @('bin', 'assets', 'data', 'downloads')) {
    New-Item -ItemType Directory -Force -Path (Join-Path $work $dir) | Out-Null
}

$cli = Join-Path $work 'bin\drive-sync.exe'
Copy-Item (Join-Path $root 'target\debug\drive-sync.exe') $cli
$newBinary = Join-Path $work 'bin\drive-sync-new.exe'
Set-Content -Path $newBinary -Value 'drive-sync NEW BINARY (update-e2e)' -NoNewline

function Convert-ToFileUrl([string]$Path) {
    return 'file:///' + ($Path -replace '\\', '/')
}

Write-Host '==> 生成临时签名密钥对' -ForegroundColor Cyan
$keygen = & (Join-Path $root 'target\debug\sign-manifest.exe') --keygen
$pubkey = (($keygen | Select-String '^pubkey').Line -split ':', 2)[1].Trim()
$seed = (($keygen | Select-String '^seed').Line -split ':', 2)[1].Trim()
if ($pubkey.Length -ne 64 -or $seed.Length -ne 64) { throw 'keygen 输出解析失败' }
$env:DRIVE_UPDATE_PUBKEY = $pubkey
$env:UPDATE_SIGNING_KEY = $seed

Write-Host '==> 生成 asset + 未签名清单（v0.9.9）' -ForegroundColor Cyan
$assetJson = Join-Path $work 'assets\asset-windows.json'
& (Join-Path $root 'target\debug\gen-manifest.exe') asset `
    --target $target `
    --url (Convert-ToFileUrl $newBinary) `
    --file $newBinary `
    --out $assetJson
if ($LASTEXITCODE -ne 0) { throw 'gen-manifest asset 失败' }

$notesFile = Join-Path $work 'assets\notes.md'
[System.IO.File]::WriteAllText($notesFile, 'update-e2e test version', (New-Object System.Text.UTF8Encoding($false)))
$unsignedManifest = Join-Path $work 'latest.unsigned.json'
& (Join-Path $root 'target\debug\gen-manifest.exe') merge `
    --version '0.9.9' `
    --notes-file $notesFile `
    --assets (Join-Path $work 'assets') `
    --out $unsignedManifest
if ($LASTEXITCODE -ne 0) { throw 'gen-manifest merge 失败' }

Write-Host '==> 未签名清单应被拒绝（check 退出码非 0）' -ForegroundColor Cyan
& $cli --data-dir (Join-Path $work 'data') update check --url (Convert-ToFileUrl $unsignedManifest)
if ($LASTEXITCODE -eq 0) { throw '未签名清单不应通过 update check' }

Write-Host '==> 签名生成 latest.json' -ForegroundColor Cyan
$signedManifest = Join-Path $work 'latest.json'
& (Join-Path $root 'target\debug\sign-manifest.exe') --in $unsignedManifest --out $signedManifest
if ($LASTEXITCODE -ne 0) { throw 'sign-manifest 失败' }

Write-Host '==> update check --json（应显示 0.9.9 可更新）' -ForegroundColor Cyan
$checkJson = & $cli --data-dir (Join-Path $work 'data') update check --url (Convert-ToFileUrl $signedManifest) --json
if ($LASTEXITCODE -ne 0) { throw '签名清单 check 失败' }
$check = $checkJson | ConvertFrom-Json
if (-not $check.update_available) { throw '应检测到新版本 0.9.9' }
if ($check.latest.version -ne '0.9.9') { throw '清单版本解析不符' }
Write-Host "    current=$($check.current_version) latest=$($check.latest.version) target=$($check.target)"

Write-Host '==> update apply --yes（下载/校验/替换 exe）' -ForegroundColor Cyan
$applyJson = & $cli --data-dir (Join-Path $work 'data') update apply --url (Convert-ToFileUrl $signedManifest) --yes --json
if ($LASTEXITCODE -ne 0) { throw 'update apply 失败' }
$apply = $applyJson | ConvertFrom-Json
if (-not $apply.installed) { throw 'apply 未报告安装成功' }
if ($apply.installed_version -ne '0.9.9') { throw '安装版本不符' }
Write-Host "    installed=$($apply.installed_version) backup=$($apply.backup)"

Write-Host '==> 校验安装结果' -ForegroundColor Cyan
$currentContent = Get-Content -Raw -Path $cli
if ($currentContent.Trim() -ne 'drive-sync NEW BINARY (update-e2e)') {
    throw "替换后的 exe 内容不符: $currentContent"
}
if (-not (Test-Path $apply.backup)) { throw "未找到旧版本备份: $($apply.backup)" }
$backupContent = Get-Content -Raw -Path $apply.backup
if ($backupContent.Trim() -eq $currentContent.Trim()) { throw '备份内容不应等于新内容' }
$statePath = Join-Path $work 'data\update-state.json'
if (-not (Test-Path $statePath)) { throw '未写入 update-state.json' }
$state = Get-Content -Raw -Path $statePath | ConvertFrom-Json
if ($state.last_installed_version -ne '0.9.9') { throw 'update-state.json 版本不符' }

Write-Host '==> 用新的 CLI 副本继续测试（安装后的副本已是测试文本，不能执行）' -ForegroundColor Cyan
$freshCli = Join-Path $work 'bin\drive-sync-fresh.exe'
Copy-Item (Join-Path $root 'target\debug\drive-sync.exe') $freshCli

Write-Host '==> 未签名清单 apply 应被拒绝' -ForegroundColor Cyan
& $freshCli --data-dir (Join-Path $work 'data2') update apply --url (Convert-ToFileUrl $unsignedManifest) --yes
if ($LASTEXITCODE -eq 0) { throw '未签名清单不应通过 update apply' }

Write-Host '==> --allow-unsigned 的 apply 可用（仅本地测试路径）' -ForegroundColor Cyan
$allowJson = & $freshCli --data-dir (Join-Path $work 'data2') update apply --url (Convert-ToFileUrl $unsignedManifest) --yes --allow-unsigned --json
if ($LASTEXITCODE -ne 0) { throw '--allow-unsigned apply 失败' }
$allow = $allowJson | ConvertFrom-Json
if (-not $allow.installed) { throw '--allow-unsigned 未安装成功' }
if ((Get-Content -Raw -Path $freshCli).Trim() -ne 'drive-sync NEW BINARY (update-e2e)') {
    throw '--allow-unsigned 安装内容不符'
}

Write-Host '==> 篡改清单应被拒绝' -ForegroundColor Cyan
$tampered = Join-Path $work 'latest.tampered.json'
[System.IO.File]::WriteAllText(
    $tampered,
    ((Get-Content -Raw -Path $signedManifest) -replace '0.9.9', '9.9.9'),
    (New-Object System.Text.UTF8Encoding($false)))
$tamperedCli = Join-Path $work 'bin\drive-sync-tampered.exe'
Copy-Item (Join-Path $root 'target\debug\drive-sync.exe') $tamperedCli
& $tamperedCli --data-dir (Join-Path $work 'data3') update check --url (Convert-ToFileUrl $tampered)
if ($LASTEXITCODE -eq 0) { throw '篡改清单不应通过验签' }

Write-Host '==> config.json 覆盖项（update_manifest_url / update_pubkey）' -ForegroundColor Cyan
$configDir = Join-Path $work 'data4'
New-Item -ItemType Directory -Force -Path $configDir | Out-Null
$config = @{
    update_manifest_url = (Convert-ToFileUrl $signedManifest)
    update_pubkey       = $pubkey
} | ConvertTo-Json
[System.IO.File]::WriteAllText((Join-Path $configDir 'config.json'), $config, (New-Object System.Text.UTF8Encoding($false)))
$configCli = Join-Path $work 'bin\drive-sync-config.exe'
Copy-Item (Join-Path $root 'target\debug\drive-sync.exe') $configCli
$configCheck = & $configCli --data-dir $configDir update check --json
if ($LASTEXITCODE -ne 0) { throw 'config.json 覆盖项 check 失败' }
if (-not ($configCheck | ConvertFrom-Json).update_available) { throw 'config.json 覆盖项未生效' }

if (-not $KeepWork) { Remove-Item -Recurse -Force $work }
Write-Host ''
Write-Host '自动更新端到端自测通过 ✓' -ForegroundColor Green
