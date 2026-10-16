# Starts the local crates.io HTTP bridge (see registry-proxy.mjs).
#
#   .\scripts\registry-proxy.ps1          # foreground
#   .\scripts\registry-proxy.ps1 -Stop    # stop a previously started instance
param(
    [switch] $Stop,
    [int] $Port = 8799,
    [string] $Cache = ''
)

$RepoRoot = Split-Path -Parent $PSScriptRoot
$script = Join-Path $PSScriptRoot 'registry-proxy.mjs'

if ($Stop) {
    $conn = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
    if ($conn) {
        $conn | Select-Object -ExpandProperty OwningProcess -Unique | ForEach-Object {
            Stop-Process -Id $_ -Force -ErrorAction SilentlyContinue
            Write-Host "stopped pid $_" -ForegroundColor Yellow
        }
    }
    else { Write-Host "nothing listening on $Port" }
    return
}

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    throw 'node.exe not found on PATH; the bridge requires Node.js 18+.'
}

$args = @($script, '--port', "$Port")
if ($Cache) { $args += @('--cache', $Cache) }
else { $args += @('--cache', (Join-Path $RepoRoot '.toolchain\proxy-cache')) }

Push-Location $RepoRoot
try { & node @args } finally { Pop-Location }
