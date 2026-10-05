<#
.SYNOPSIS
  Builds clip4 and publishes it as a GitHub release (zip + SHA-256) with the GitHub CLI.

.DESCRIPTION
  The tag (or version) is the first parameter: v1.2.0 and 1.2.0 mean the same. Without it, the
  version in Cargo.toml is used.

  1. checks the tools and the repository state
  2. if the tag's version differs from Cargo.toml, bumps Cargo.toml + Cargo.lock so the binary
     reports the version it is released as (restored on -DryRun, on cancel and on failure;
     committed and pushed only after you confirm)
  3. builds the release binary (build.ps1) unless -SkipBuild
  4. packs dist\clip4-v<version>-windows-x64.zip (clip4.exe + README.md) and its .sha256
  5. creates the release on the pushed commit (GitHub creates the tag)

  Nothing is published or pushed without a confirmation (-Yes skips it); -DryRun does
  everything except committing, pushing and publishing.

.EXAMPLE
  .\publish_github.ps1 v1.2.0 -DryRun               # build + package + checks, nothing is published
  .\publish_github.ps1 v1.2.0                       # bump if needed, release v1.2.0 with generated notes
  .\publish_github.ps1 -Tag 2.0.0-beta.1            # a version with a suffix is a pre-release automatically
  .\publish_github.ps1                              # release the version Cargo.toml already has
  .\publish_github.ps1 v1.2.0 -NotesFile CHANGES.md -Draft
  .\publish_github.ps1 v1.2.0 -Update               # replace the assets of an existing release
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [Alias('Version')]
    [string]$Tag,            # v1.2.0 or 1.2.0; default: the version in Cargo.toml
    [string]$Repo,           # owner/name; default: the repository 'origin' points to
    [string]$Notes,          # release notes text
    [string]$NotesFile,      # release notes file (wins over -Notes)
    [switch]$Draft,
    [switch]$Prerelease,     # implied by a version with a suffix such as 1.2.0-rc.1
    [switch]$NoBump,         # refuse (instead of bumping) when the tag differs from Cargo.toml
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

$cargoToml = Join-Path $PSScriptRoot 'Cargo.toml'
$cargoLock = Join-Path $PSScriptRoot 'Cargo.lock'
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Get-CargoVersion {
    $m = [regex]::Match([IO.File]::ReadAllText($cargoToml), '(?m)^\s*version\s*=\s*"([^"]+)"')
    if (-not $m.Success) { throw 'Could not read the package version from Cargo.toml.' }
    $m.Groups[1].Value
}

# The package's own `version = ` is the first one in Cargo.toml; in Cargo.lock it is the entry
# named "clip4". Line endings are preserved.
function Set-CargoVersion([string]$New) {
    $t = [IO.File]::ReadAllText($cargoToml)
    $t = ([regex]'(?m)^(\s*version\s*=\s*")[^"]+(")').Replace($t, "`${1}$New`${2}", 1)
    [IO.File]::WriteAllText($cargoToml, $t, $utf8)
    if (Test-Path $cargoLock) {
        $l = [IO.File]::ReadAllText($cargoLock)
        $l = ([regex]'(\[\[package\]\]\r?\nname = "clip4"\r?\nversion = ")[^"]+(")').Replace($l, "`${1}$New`${2}", 1)
        [IO.File]::WriteAllText($cargoLock, $l, $utf8)
    }
    if ((Get-CargoVersion) -ne $New) { throw "Could not set the version in Cargo.toml to $New." }
}

# ---- tools -------------------------------------------------------------------------------
foreach ($tool in 'git', 'gh') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "'$tool' was not found on PATH. Install it (gh: https://cli.github.com) and rerun."
    }
}
if (-not (Probe gh @('auth', 'status')).Ok) { throw "The GitHub CLI is not logged in. Run 'gh auth login' first." }

# ---- tag / version ------------------------------------------------------------------------
$cargoVersion = Get-CargoVersion
if (-not $Tag) { $Tag = $cargoVersion }
$Version = $Tag.Trim() -replace '^[vV]', ''
if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$') {
    throw "'$Tag' is not a version like v1.2.0, 1.2.0 or 1.2.0-rc.1."
}
$tag = "v$Version"
$bump = ($Version -ne $cargoVersion)
if ($bump -and $NoBump) {
    throw "Version $Version does not match Cargo.toml ($cargoVersion) and -NoBump was given."
}
if ($Version -match '-') { $Prerelease = [switch]$true }

# ---- repository state ---------------------------------------------------------------------
if (-not (Probe git @('rev-parse', '--verify', 'HEAD')).Ok) { throw 'This repository has no commits yet. Commit and push first.' }
$sha = (& git rev-parse HEAD).Trim()
$dirty = (& git status --porcelain) -join "`n"
if ($dirty -and -not $AllowDirty) {
    throw "There are uncommitted changes:`n$dirty`nCommit them (the release is built from your working tree but tags the pushed commit), or pass -AllowDirty."
}
if ($bump -and (& git status --porcelain -- Cargo.toml Cargo.lock)) {
    throw 'Cargo.toml / Cargo.lock have uncommitted changes; commit or revert them before a version bump.'
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
    throw "Release $tag already exists in $Repo. Pick another tag, or pass -Update to replace its assets."
}

# ---- bump, build, package, publish --------------------------------------------------------
$bumped = $false
try {
    if ($bump) {
        Write-Host "Bumping Cargo.toml / Cargo.lock: $cargoVersion -> $Version"
        Set-CargoVersion $Version
        $bumped = $true
    }
    $dist = Join-Path $PSScriptRoot 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    if (-not $SkipBuild) {
        # Built into dist\stage, so the clip4.exe in the repo root is left alone (a dry run with a
        # version bump must not leave a binary of the wrong version there).
        $stage = Join-Path $dist 'stage'
        & (Join-Path $PSScriptRoot 'build.ps1') -OutDir $stage
        if (-not $?) { throw 'build.ps1 failed.' }
        $exe = Join-Path $stage 'clip4.exe'
    }
    else {
        if ($bump) { Write-Warning "-SkipBuild: the packaged clip4.exe was not built with version $Version." }
        $exe = Join-Path $PSScriptRoot 'clip4.exe'
    }
    if (-not (Test-Path $exe)) { throw "clip4.exe was not found at $exe. Run .\build.ps1 (or drop -SkipBuild)." }

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
    if ($bump) { Write-Host "Version    : Cargo.toml $cargoVersion -> $Version (committed and pushed only when you confirm)" }
    else { Write-Host "Commit     : $sha" }
    Write-Host "Assets     : $base.zip ($mb MB), $base.zip.sha256"
    Write-Host "SHA-256    : $hash"
    if ($dirty) { Write-Warning 'Uncommitted changes are in the binary but not in the tagged commit.' }

    if ($DryRun) {
        Write-Host "`nDry run: nothing was committed, pushed or published. Assets are in $dist"
        return
    }
    if (-not $Yes) {
        $what = if ($bump) { "Commit the version bump, push it and publish $tag" } else { "Publish $tag" }
        $answer = Read-Host "`n$what to GitHub? (y/N)"
        if ($answer -notmatch '^(y|yes)$') { Write-Host 'Cancelled.'; return }
    }

    if ($bump) {
        Exec git @('commit', '-m', "Release $tag", '--', 'Cargo.toml', 'Cargo.lock')
        $bumped = $false   # committed: nothing to restore any more
        if ((Probe git @('rev-parse', '--abbrev-ref', '@{u}')).Ok) { Exec git @('push') }
        else { Exec git @('push', '-u', 'origin', 'HEAD') }
        $sha = (& git rev-parse HEAD).Trim()
    }

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
}
finally {
    if ($bumped) {
        # Dry run, cancelled, or failed before the commit: leave Cargo.toml / Cargo.lock as they were.
        $null = Probe git @('checkout', '--', 'Cargo.toml', 'Cargo.lock')
        Write-Host "Cargo.toml / Cargo.lock restored to version $cargoVersion."
    }
}
