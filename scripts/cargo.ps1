# 使用项目本地 CARGO_HOME 的 cargo 包装器。
# 原因：本机全局 ~/.cargo/config.toml 指向的 USTC 镜像当前 403/429，下载极慢；
# 项目本地 CARGO_HOME 没有全局镜像配置，直接走 crates.io 官方源。
#
# 用法（PowerShell 5.1 与 PowerShell 7 均可用）：
#   powershell -File scripts/cargo.ps1 check -p derive-core
#   pwsh -File scripts/cargo.ps1 test -p derive-relay
#
# 注意：这里故意不声明 param 块，统一用 $args 透传，
# 避免 PowerShell 把 cargo 的 `-p` 误当成脚本自己的参数。
$root = Split-Path -Parent $PSScriptRoot
$env:CARGO_HOME = Join-Path $root '.cargo-home'
if (-not (Test-Path $env:CARGO_HOME)) {
    New-Item -ItemType Directory -Path $env:CARGO_HOME -Force | Out-Null
}
& cargo @args
exit $LASTEXITCODE
