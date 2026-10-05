<#
.SYNOPSIS
  Builds the release clip4.exe and copies it next to this script (the repo root).

.PARAMETER TargetDir
  Cargo build directory. Defaults to %TEMP%\clip4-target so build output stays out of the
  OneDrive-synced repo folder. Pass -TargetDir .\target to build inside the repo instead.

.PARAMETER OutDir
  Where clip4.exe is written. Defaults to the repo root.

.PARAMETER Rebuild
  Recompile the clip4 crate from scratch first (dependencies stay cached).

  The build is always checked: the binary must carry the version in Cargo.toml. Cargo can reuse an
  older artifact after the version was changed back, so a mismatch triggers one automatic rebuild
  of the crate.
#>
[CmdletBinding()]
param(
    [string]$TargetDir = (Join-Path $env:TEMP 'clip4-target'),
    [string]$OutDir,
    [switch]$Rebuild
)

$ErrorActionPreference = 'Stop'
if (-not $OutDir) { $OutDir = $PSScriptRoot }   # ($PSScriptRoot is not set yet inside param())
Push-Location $PSScriptRoot
try {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo was not found. Install the stable Rust toolchain (MSVC target) from https://rustup.rs'
    }
    $env:CARGO_TARGET_DIR = $TargetDir

    function Invoke-Cargo([string[]]$Arguments) {
        & cargo @Arguments
        if ($LASTEXITCODE -ne 0) { throw "cargo $($Arguments -join ' ') failed (exit code $LASTEXITCODE)" }
    }

    $built = Join-Path $TargetDir 'release\clip4.exe'
    $version = [regex]::Match([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'Cargo.toml')), '(?m)^\s*version\s*=\s*"([^"]+)"').Groups[1].Value
    # src\lib.rs: VERSION_STRING = "clip4 <version>", logged at startup, so it is in the binary.
    function Test-BinaryVersion {
        [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($built)).Contains("clip4 $version")
    }

    if ($Rebuild) { Invoke-Cargo @('clean', '--release', '-p', 'clip4') }
    Invoke-Cargo @('build', '--release')
    if (-not (Test-BinaryVersion)) {
        Write-Host "The binary does not carry version $version (cargo reused an older build); recompiling clip4..."
        Invoke-Cargo @('clean', '--release', '-p', 'clip4')
        Invoke-Cargo @('build', '--release')
        if (-not (Test-BinaryVersion)) { Write-Warning "Could not confirm that the binary reports version $version." }
    }

    New-Item -ItemType Directory -Force $OutDir | Out-Null
    $dest = Join-Path $OutDir 'clip4.exe'
    # A running clip4 keeps its exe locked; say so instead of failing with a cryptic copy error.
    try { Copy-Item $built $dest -Force }
    catch { throw "Could not write $dest - is clip4 still running? Exit it from the tray menu and rerun. ($($_.Exception.Message))" }

    $mb = [math]::Round((Get-Item $dest).Length / 1MB, 2)
    Write-Host "Built $dest ($mb MB, version $version)"
}
finally {
    Pop-Location
}
