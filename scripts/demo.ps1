# End-to-end demo: agent loop, parallel sessions, snapshot/restore, migration, events.
# Usage: pwsh -File scripts/demo.ps1 [-Goal "what is 12*12"]
param(
    [string]$Goal = "what is 21*2 and then read notes.txt"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$env:AGENTOS_DATA_DIR = Join-Path $env:TEMP ("agentos-demo-" + (Get-Random))
$env:AGENTOS_WORKSPACE_ROOT = Join-Path $env:TEMP ("agentos-demo-ws-" + (Get-Random))
New-Item -ItemType Directory -Force -Path $env:AGENTOS_WORKSPACE_ROOT | Out-Null
"hello from the workspace" | Set-Content (Join-Path $env:AGENTOS_WORKSPACE_ROOT "notes.txt")

cargo run -q -p agentos-cli -- demo --goal $Goal
