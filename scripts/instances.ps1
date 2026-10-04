# 多实例管理：一台物理机、多个工作目录、多个相互隔离的 Agent OS 节点。
#
#   pwsh -File scripts/instances.ps1 list
#   pwsh -File scripts/instances.ps1 new   -Name a            # 分配端口、复制二进制、建目录
#   pwsh -File scripts/instances.ps1 start -Name a
#   pwsh -File scripts/instances.ps1 status
#   pwsh -File scripts/instances.ps1 stop  -Name a
#   pwsh -File scripts/instances.ps1 remove -Name a -Force
#
# 每个实例的目录布局（全部在 instances/<name>/ 下，互不可见）：
#
#   instances/<name>/
#     instance.json      身份与端口（node_id / node_name / http / grpc）
#     config.json        可选：存在时作为 AGENTOS_CONFIG，覆盖默认配置
#     bin/               该实例独占的 agentos-server 副本
#     data/              State Store / 事件日志 / Artifact
#     workspace/         文件系统能力的唯一可访问目录
#     run/               pid、stdout、stderr
#
# 为什么每个实例要复制一份二进制：Windows 会锁定正在运行的 exe，共用一份二进制时只要有任何
# 实例在跑，cargo build 就会失败（os error 5）。复制副本同时让各实例可以停在**不同版本**上，
# 便于灰度升级。
#
# 环境变量是配置的唯一入口（见 crates/core/src/config.rs）。脚本显式设置自己管理的变量，
# 并清掉会从父 shell 泄漏进来的 AGENTOS_CONFIG，避免一个实例误读别的实例配置。

param(
    [Parameter(Position = 0)]
    [ValidateSet('list', 'new', 'start', 'stop', 'restart', 'status', 'remove', 'logs')]
    [string]$Action = 'list',

    [Parameter(Position = 1)]
    [string]$Name,

    [int]$Http = 0,
    [int]$Grpc = 0,
    [string]$NodeId,
    [string]$NodeName,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$instancesRoot = Join-Path $repo 'instances'
$sourceBinary = Join-Path $repo 'target\debug\agentos-server.exe'

function Get-InstanceDir([string]$n) { Join-Path $instancesRoot $n }
function Get-InstanceFile([string]$n) { Join-Path (Get-InstanceDir $n) 'instance.json' }
function Require-Name { if (-not $Name) { throw 'this action needs -Name <instance>' } }
function Ensure-Root { if (-not (Test-Path $instancesRoot)) { New-Item -ItemType Directory -Force -Path $instancesRoot | Out-Null } }

function Read-Instance([string]$n) {
    $file = Get-InstanceFile $n
    if (-not (Test-Path $file)) { throw "instance '$n' does not exist (run: instances.ps1 new -Name $n)" }
    Get-Content $file -Raw | ConvertFrom-Json
}

function Get-Instances {
    Ensure-Root
    Get-ChildItem $instancesRoot -Directory | ForEach-Object {
        $file = Join-Path $_.FullName 'instance.json'
        if (Test-Path $file) { Get-Content $file -Raw | ConvertFrom-Json }
    }
}

function Test-PortFree([int]$port) {
    if ($port -le 0) { return $false }
    $used = Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue
    return ($null -eq $used)
}

function Find-PortPair([int]$startAt = 8788) {
    $http = $startAt
    while ($true) {
        # 注意：PowerShell 里逗号比 + 结合更紧，@($http, $http + 1) 会得到三个元素，必须加括号。
        if ((Test-PortFree $http) -and (Test-PortFree ($http + 1))) { return @($http, ($http + 1)) }
        $http += 2
        if ($http -gt 65000) { throw 'no free port pair found' }
    }
}

function Get-InstanceProcess($inst) {
    $pidFile = Join-Path (Get-InstanceDir $inst.name) 'run\pid.txt'
    if (-not (Test-Path $pidFile)) { return $null }
    $procId = (Get-Content $pidFile -Raw).Trim()
    if (-not $procId) { return $null }
    $proc = Get-Process -Id ([int]$procId) -ErrorAction SilentlyContinue
    if ($proc -and $proc.ProcessName -like 'agentos*') { return $proc }
    return $null
}

function Wait-Healthy([int]$port, [int]$seconds = 20) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        try {
            $r = Invoke-WebRequest -Uri "http://127.0.0.1:$port/healthz" -UseBasicParsing -TimeoutSec 2
            if ($r.StatusCode -eq 200) { return $true }
        } catch { Start-Sleep -Milliseconds 300 }
    }
    return $false
}

function Build-Binary {
    if (Test-Path $sourceBinary) { return }
    Write-Host 'building agentos-server (first run)...'
    Push-Location $repo
    try { cargo build -p agentos-server | Out-Null } finally { Pop-Location }
}

switch ($Action) {
    'new' {
        Require-Name
        Ensure-Root
        $dir = Get-InstanceDir $Name
        if ((Test-Path $dir) -and -not $Force) { throw "instance '$Name' already exists (use -Force to recreate)" }
        if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }

        $existing = @(Get-Instances)
        $startAt = 8788
        if ($existing.Count -gt 0) { $startAt = (($existing | Measure-Object -Property http -Maximum).Maximum) + 2 }
        $pair = Find-PortPair $startAt
        if ($Http -gt 0) { $pair[0] = $Http }
        if ($Grpc -gt 0) { $pair[1] = $Grpc }
        if (-not (Test-PortFree $pair[0])) { throw "http port $($pair[0]) is in use" }
        if (-not (Test-PortFree $pair[1])) { throw "grpc port $($pair[1]) is in use" }
        if ($pair[0] -eq $pair[1]) { throw "http and grpc resolved to the same port ($($pair[0]))" }

        New-Item -ItemType Directory -Force -Path (Join-Path $dir 'bin'), (Join-Path $dir 'data'), (Join-Path $dir 'workspace'), (Join-Path $dir 'run') | Out-Null
        Build-Binary
        Copy-Item $sourceBinary (Join-Path $dir 'bin\agentos-server.exe') -Force
        Set-Content (Join-Path $dir 'workspace\README.txt') "Files here are the only ones filesystem capabilities may touch for instance '$Name'."

        $record = [ordered]@{
            name       = $Name
            nodeId     = if ($NodeId) { $NodeId } else { "agora-$Name" }
            nodeName   = if ($NodeName) { $NodeName } else { "agora-$Name" }
            http       = $pair[0]
            grpc       = $pair[1]
            createdAt  = (Get-Date).ToString('s')
            binaryHash = (Get-FileHash (Join-Path $dir 'bin\agentos-server.exe') -Algorithm SHA256).Hash.Substring(0, 12)
        }
        $record | ConvertTo-Json | Set-Content (Get-InstanceFile $Name)
        Write-Host "created instance '$Name'  http=$($pair[0])  grpc=$($pair[1])  node_id=$($record.nodeId)"
        Write-Host "workspace: $(Join-Path $dir 'workspace')"
        Write-Host "start it : pwsh -File scripts/instances.ps1 start -Name $Name"
    }

    'start' {
        Require-Name
        $inst = Read-Instance $Name
        if (Get-InstanceProcess $inst) { Write-Host "instance '$Name' is already running (pid $((Get-InstanceProcess $inst).Id))"; break }
        $dir = Get-InstanceDir $Name
        $binary = Join-Path $dir 'bin\agentos-server.exe'
        if (-not (Test-Path $binary)) { throw "binary missing for '$Name'; run: instances.ps1 new -Name $Name -Force" }
        if (-not (Test-PortFree $inst.http)) { throw "http port $($inst.http) is already in use" }

        # 每个实例自己的身份与路径：显式设置，绝不依赖父 shell 的残留值。
        $env:AGENTOS_NODE_NAME = $inst.nodeName
        $env:AGENTOS_NODE_ID = $inst.nodeId
        $env:AGENTOS_HTTP_ADDR = "127.0.0.1:$($inst.http)"
        $env:AGENTOS_GRPC_ADDR = "127.0.0.1:$($inst.grpc)"
        $env:AGENTOS_DATA_DIR = './data'
        $env:AGENTOS_WORKSPACE_ROOT = './workspace'
        if (-not $env:AGENTOS_LOG) { $env:AGENTOS_LOG = 'info' }
        $configFile = Join-Path $dir 'config.json'
        if (Test-Path $configFile) { $env:AGENTOS_CONFIG = $configFile } else { Remove-Item Env:AGENTOS_CONFIG -ErrorAction SilentlyContinue }

        $out = Join-Path $dir 'run\out.log'
        $err = Join-Path $dir 'run\err.log'
        # stdin 也必须重定向：否则子进程会继承调用方的标准输入，在非交互环境（CI、上层脚本、
        # 管道）里父进程会一直等到子进程结束才返回——实例是长驻进程，于是 start 永不返回。
        $nul = Join-Path $dir 'run\stdin.nul'
        if (-not (Test-Path $nul)) { New-Item -ItemType File -Force -Path $nul | Out-Null }
        $proc = Start-Process -FilePath $binary -WorkingDirectory $dir -PassThru -WindowStyle Hidden -RedirectStandardInput $nul -RedirectStandardOutput $out -RedirectStandardError $err
        Set-Content (Join-Path $dir 'run\pid.txt') $proc.Id

        if (Wait-Healthy $inst.http) {
            Write-Host "instance '$Name' up  pid=$($proc.Id)  http://127.0.0.1:$($inst.http)  ws=http://127.0.0.1:$($inst.http)/v1/ws  grpc=$($inst.grpc)"
        } else {
            Write-Host "instance '$Name' started (pid $($proc.Id)) but health did not answer; see $err"
        }
    }

    'stop' {
        Require-Name
        $inst = Read-Instance $Name
        $proc = Get-InstanceProcess $inst
        if (-not $proc) { Write-Host "instance '$Name' is not running"; break }
        Stop-Process -Id $proc.Id -Force
        Start-Sleep -Milliseconds 500
        Remove-Item (Join-Path (Get-InstanceDir $Name) 'run\pid.txt') -ErrorAction SilentlyContinue
        Write-Host "stopped instance '$Name' (pid $($proc.Id))"
    }

    'restart' { & $PSCommandPath stop -Name $Name; Start-Sleep -Seconds 1; & $PSCommandPath start -Name $Name }

    'status' {
        $all = @(Get-Instances)
        if ($all.Count -eq 0) { Write-Host 'no instances yet: pwsh -File scripts/instances.ps1 new -Name a'; break }
        $all | ForEach-Object {
            $proc = Get-InstanceProcess $_
            $state = if ($proc) { 'running' } else { 'stopped' }
            $health = if ($proc) { if (Wait-Healthy $_.http 2) { 'healthy' } else { 'no health' } } else { '-' }
            [pscustomobject]@{
                name = $_.name; node_id = $_.nodeId; http = $_.http; grpc = $_.grpc
                state = $state; health = $health; pid = if ($proc) { $proc.Id } else { '' }
            }
        } | Format-Table -AutoSize | Out-String | Write-Host
    }

    'logs' {
        Require-Name
        $dir = Get-InstanceDir $Name
        Get-Content (Join-Path $dir 'run\out.log') -Tail 40 -ErrorAction SilentlyContinue
        Get-Content (Join-Path $dir 'run\err.log') -Tail 20 -ErrorAction SilentlyContinue
    }

    'remove' {
        Require-Name
        $inst = Read-Instance $Name
        if (-not $Force) { throw "refusing to delete '$Name' without -Force" }
        $proc = Get-InstanceProcess $inst
        if ($proc) { Stop-Process -Id $proc.Id -Force; Start-Sleep -Milliseconds 500 }
        Remove-Item -Recurse -Force (Get-InstanceDir $Name)
        Write-Host "removed instance '$Name'"
    }

    'list' {
        $all = @(Get-Instances)
        if ($all.Count -eq 0) { Write-Host 'no instances yet: pwsh -File scripts/instances.ps1 new -Name a'; break }
        $all | Format-Table name, nodeId, http, grpc, createdAt -AutoSize | Out-String | Write-Host
    }
}
