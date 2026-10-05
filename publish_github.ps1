<#
.SYNOPSIS
  Builds clip4 and publishes it as a GitHub release (zip + SHA-256) with the GitHub CLI.

.DESCRIPTION
  1. checks the tools, the repository state and that the version matches Cargo.toml
  2. builds the release binary (build.ps1) unless -SkipBuild
  3. packs dist\clip4-v<version>-windows-x64.zip (clip4.exe + README.md) and its .sha256
  4. creates the release v<version> on the commit you are on (the tag is created by GitHub)

  The commit must already be pushed. Nothing is published without a confirmation (-Yes skips it);
  -DryRun does everything except the publishing.

.EXAMPLE
  .\publish_github.ps1 -DryRun                      # build + package + checks, publish nothing
  .\publish_github.ps1                              # release v<Cargo.toml version>, generated notes
  .\publish_github.ps1 -NotesFile CHANGES.md -Draft # review the draft on GitHub before releasing
  .\publish_github.ps1 -Update                      # replace the assets of an existing release
#>
[CmdletBinding()]
param(
    [string]$Version,        # default: the version in Cargo.toml
    [string]$Repo,           # owner/name; default: the repository 'origin' points to
    [string]$Notes,          # release notes text
    [string]$NotesFile,      # release notes file (wins over -Notes)
    [switch]$Draft,
    [switch]$Prerelease,
    [switch]$SkipBuild,      # package the clip4.exe that is already in the repo root
    [switch]$AllowDirty,     # allow uncommitted changes (the release still tags the pushed commit)
    [switch]$Update,         # replace the assets of an existing release instead of failing
    [switch]$DryRun,
    [switch]$Yes             # do not ask for confirmation
)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Exec {
    param([string]$Exe, [string[]]$Arguments)
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "'$Exe $($Arguments -join ' ')' failed (exit code $LASTEXITCODE)" }
}

# Runs a command whose failure is an ANSWER ("is there such a release?"), not an error. Windows
# PowerShell 5.1 turns a native command's stderr into a terminating error under -Stop, so the
# preference is relaxed for the call and the exit code decides.
function Probe {
    param([string]$Exe, [string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $out = & $Exe @Arguments 2>$null
        [pscustomobject]@{ Ok = ($LASTEXITCODE -eq 0); Out = @($out) }
    }
    finally { $ErrorActionPreference = $previous }
}

# ---- tools -------------------------------------------------------------------------------
foreach ($tool in 'git', 'gh') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "'$tool' was not found on PATH. Install it (gh: https://cli.github.com) and rerun."
    }
}
if (-not (Probe gh @('auth', 'status')).Ok) { throw "The GitHub CLI is not logged in. Run 'gh auth login' first." }

# ---- version ------------------------------------------------------------------------------
$m = Select-String -Path (Join-Path $PSScriptRoot 'Cargo.toml') -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m) { throw 'Could not read the package version from Cargo.toml.' }
$cargoVersion = $m.Matches[0].Groups[1].Value
if (-not $Version) { $Version = $cargoVersion }
$Version = $Version.TrimStart('v')
if ($Version -ne $cargoVersion) {
    throw "Version $Version does not match Cargo.toml ($cargoVersion). The binary reports Cargo's version: bump Cargo.toml (and commit) first."
}
$tag = "v$Version"

# ---- repository state ---------------------------------------------------------------------
if (-not (Probe git @('rev-parse', '--verify', 'HEAD')).Ok) { throw 'This repository has no commits yet. Commit and push first.' }
$sha = (& git rev-parse HEAD).Trim()
$dirty = (& git status --porcelain) -join "`n"
if ($dirty -and -not $AllowDirty) {
    throw "There are uncommitted changes:`n$dirty`nCommit them (the release is built from your working tree but tags the pushed commit), or pass -AllowDirty."
}
if (-not (Probe git @('branch', '-r', '--contains', $sha)).Out) {
    throw "Commit $($sha.Substring(0, 7)) is not on the remote yet. Run 'git push' first: GitHub creates the tag on a pushed commit."
}
if (-not $Repo) {
    $r = Probe gh @('repo', 'view', '--json', 'nameWithOwner', '-q', '.nameWithOwner')
    if ($r.Ok) { $Repo = ($r.Out -join '').Trim() }
}
if (-not $Repo) { throw "Could not work out the GitHub repository. Pass -Repo owner/name." }

$exists = (Probe gh @('release', 'view', $tag, '--repo', $Repo)).Ok
if ($exists -and -not $Update) {
    throw "Release $tag already exists in $Repo. Bump the version in Cargo.toml, or pass -Update to replace its assets."
}

# ---- build + package ----------------------------------------------------------------------
if (-not $SkipBuild) {
    & (Join-Path $PSScriptRoot 'build.ps1')
    if (-not $?) { throw 'build.ps1 failed.' }
}
$exe = Join-Path $PSScriptRoot 'clip4.exe'
if (-not (Test-Path $exe)) { throw "clip4.exe is not in the repo root. Run .\build.ps1 (or drop -SkipBuild)." }

$dist = Join-Path $PSScriptRoot 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
$base = "clip4-v$Version-windows-x64"
$zip = Join-Path $dist "$base.zip"
$sum = "$zip.sha256"
Remove-Item $zip, $sum -ErrorAction SilentlyContinue
Compress-Archive -Path $exe, (Join-Path $PSScriptRoot 'README.md') -DestinationPath $zip
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
"$hash  $base.zip" | Set-Content -Encoding ascii $sum
$mb = [math]::Round((Get-Item $zip).Length / 1MB, 2)

Write-Host ''
Write-Host "Repository : $Repo"
Write-Host "Release    : $tag$(if ($Draft) { ' (draft)' })$(if ($Prerelease) { ' (pre-release)' })$(if ($exists) { '  [replacing assets]' })"
Write-Host "Commit     : $sha"
Write-Host "Assets     : $base.zip ($mb MB), $base.zip.sha256"
Write-Host "SHA-256    : $hash"
if ($dirty) { Write-Warning 'Uncommitted changes are in the binary but not in the tagged commit.' }

if ($DryRun) {
    Write-Host "`nDry run: nothing was published. Assets are in $dist"
    return
}
if (-not $Yes) {
    $answer = Read-Host "`nPublish this release to GitHub? (y/N)"
    if ($answer -notmatch '^(y|yes)$') { Write-Host 'Cancelled.'; return }
}

# ---- publish ------------------------------------------------------------------------------
if ($exists) {
    Exec gh @('release', 'upload', $tag, $zip, $sum, '--repo', $Repo, '--clobber')
}
else {
    $ghArgs = @('release', 'create', $tag, $zip, $sum, '--repo', $Repo, '--target', $sha, '--title', "clip4 $tag")
    if ($NotesFile) { $ghArgs += @('--notes-file', $NotesFile) }
    elseif ($Notes) { $ghArgs += @('--notes', $Notes) }
    else { $ghArgs += '--generate-notes' }
    if ($Draft) { $ghArgs += '--draft' }
    if ($Prerelease) { $ghArgs += '--prerelease' }
    Exec gh $ghArgs
}
Write-Host "`nPublished: $(((Probe gh @('release', 'view', $tag, '--repo', $Repo, '--json', 'url', '-q', '.url')).Out -join '').Trim())"
