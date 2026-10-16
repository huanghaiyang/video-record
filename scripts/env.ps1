# Dot-source this file to get a working Rust build environment for this repo:
#
#     . .\scripts\env.ps1
#
# It sets a repository-local toolchain (RUSTUP_HOME/CARGO_HOME under .toolchain)
# and loads the MSVC 2022 x64 developer environment (link.exe / cl.exe) so that
# `cargo build` works without a globally installed Rust.

$RepoRoot = Split-Path -Parent $PSScriptRoot
$env:RUSTUP_HOME = Join-Path $RepoRoot '.toolchain\rustup'
$env:CARGO_HOME = Join-Path $RepoRoot '.toolchain\cargo'

$cargoBin = Join-Path $env:CARGO_HOME 'bin'
if ($env:PATH -notlike "*$cargoBin*") {
    $env:PATH = "$cargoBin;$env:PATH"
}

# Load MSVC environment only once per shell.
if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
    $vcvarsCandidates = @(
        'D:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat',
        'D:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat',
        'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat',
        'C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Auxiliary\Build\vcvars64.bat'
    )
    $vcvars = $vcvarsCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($vcvars) {
        foreach ($line in (cmd /c "`"$vcvars`" >nul 2>&1 && set")) {
            if ($line -match '^([^=]+)=(.*)$') {
                Set-Item -Path "env:$($matches[1])" -Value $matches[2] -ErrorAction SilentlyContinue
            }
        }
    }
    else {
        Write-Warning 'vcvars64.bat not found - MSVC linking will fail. Install VS 2022 Build Tools.'
    }
}

Write-Host "Rust env ready: $(if (Get-Command rustc -ErrorAction SilentlyContinue) { (& rustc --version) } else { 'rustc NOT found' })" -ForegroundColor DarkGray
Write-Host "CARGO_HOME=$env:CARGO_HOME" -ForegroundColor DarkGray
