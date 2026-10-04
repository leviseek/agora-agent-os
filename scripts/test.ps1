# Full verification: Rust workspace tests plus the TypeScript builds.
# Usage: pwsh -File scripts/test.ps1 [-SkipWeb]
param(
    [switch]$SkipWeb
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Host "=== cargo test --workspace ===" -ForegroundColor Cyan
cargo test --workspace --no-fail-fast
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "=== cargo build --release ===" -ForegroundColor Cyan
cargo build --release -p agentos-server -p agentos-cli
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if (-not $SkipWeb) {
    Write-Host "=== pnpm install ===" -ForegroundColor Cyan
    pnpm install
    Write-Host "=== web build ===" -ForegroundColor Cyan
    pnpm --filter @agent-os/web build
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

Write-Host "all checks passed" -ForegroundColor Green
