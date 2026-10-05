<#
.SYNOPSIS
  Builds the release clip4.exe and copies it next to this script (the repo root).

.PARAMETER TargetDir
  Cargo build directory. Defaults to %TEMP%\clip4-target so build output stays out of the
  OneDrive-synced repo folder. Pass -TargetDir .\target to build inside the repo instead.
#>
[CmdletBinding()]
param(
    [string]$TargetDir = (Join-Path $env:TEMP 'clip4-target')
)

$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo was not found. Install the stable Rust toolchain (MSVC target) from https://rustup.rs'
    }

    $env:CARGO_TARGET_DIR = $TargetDir
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE)" }

    $built = Join-Path $TargetDir 'release\clip4.exe'
    $dest  = Join-Path $PSScriptRoot 'clip4.exe'
    # A running clip4 keeps its exe locked; say so instead of failing with a cryptic copy error.
    try { Copy-Item $built $dest -Force }
    catch { throw "Could not write $dest - is clip4 still running? Exit it from the tray menu and rerun. ($($_.Exception.Message))" }

    $mb = [math]::Round((Get-Item $dest).Length / 1MB, 2)
    Write-Host "Built $dest ($mb MB)"
}
finally {
    Pop-Location
}
