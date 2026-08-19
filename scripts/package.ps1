<#
.SYNOPSIS
    Builds and assembles a portable distribution.

.DESCRIPTION
    Produces dist\clipcut-<version>-<profile>\ containing the executable,
    libmpv-2.dll and the licence notices, plus a zip alongside it.

    Portable rather than an installer, deliberately: the app needs no registry
    entries or file associations, ffmpeg comes from the user's PATH, and settings
    live in %APPDATA%. Unzip-and-run is the whole install.

.PARAMETER Profile
    release (default) or debug. Debug packages keep the console window and the
    headless self-check hooks, which is what makes them worth shipping alongside
    a release for bug reports.

.PARAMETER Version
    Overrides the version in the artifact name. Defaults to the Cargo.toml value.

.EXAMPLE
    .\scripts\package.ps1
    .\scripts\package.ps1 -Profile debug
#>
[CmdletBinding()]
param(
    [ValidateSet('release', 'debug')]
    [string]$Profile = 'release',
    [string]$Version,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $SkipBuild) {
    Write-Host "Building $Profile..." -ForegroundColor Cyan
    if ($Profile -eq 'release') {
        & "$PSScriptRoot\build.ps1" build --release
    }
    else {
        & "$PSScriptRoot\build.ps1" build
    }
    if ($LASTEXITCODE -ne 0) { throw "$Profile build failed" }
}

$exe = Join-Path $repoRoot "target\$Profile\clipcut.exe"
if (-not (Test-Path $exe)) { throw "no $Profile binary at $exe" }

if (-not $Version) {
    # Straight from the manifest, so the package cannot drift from the About box.
    $Version = (Select-String -Path (Join-Path $repoRoot "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1).Matches[0].Groups[1].Value
}

$stage = Join-Path $repoRoot "dist\clipcut-$Version-$Profile"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item $exe $stage
$dll = Join-Path $repoRoot "third_party\mpv-dev\64\libmpv-2.dll"
if (-not (Test-Path $dll)) { throw "libmpv-2.dll missing - run .\scripts\setup-mpv.ps1" }
Copy-Item $dll $stage
Copy-Item (Join-Path $repoRoot "README.md") $stage

# Licence notices. Slint's royalty-free licence requires attribution and libmpv
# is LGPL, so shipping without these would be non-compliant.
@"
ClipCut $Version - third-party notices
======================================

Slint (https://slint.dev)
    Used under the Slint Royalty-free License, which requires attribution.
    Attribution is shown in the application's About dialog.

libmpv / mpv (https://mpv.io)
    Distributed here as libmpv-2.dll, an LGPL-2.1 build, dynamically linked.
    Source: https://github.com/zhongfly/mpv-winbuild
    The LGPL build is used deliberately so this application is not required to
    be GPL. You may replace libmpv-2.dll with your own compatible build.

FFmpeg (https://ffmpeg.org)
    NOT distributed with this application. ClipCut invokes the ffmpeg and
    ffprobe executables found on your PATH as separate processes.
"@ | Set-Content (Join-Path $stage "THIRD-PARTY-NOTICES.txt") -Encoding utf8

$profileNote = if ($Profile -eq 'debug') {
    @"

THIS IS A DEBUG BUILD
    Slower, larger, and it opens a console window showing diagnostics. Useful
    for reporting a problem; use the release build for normal work.
"@
}
else { "" }

@"
ClipCut $Version ($Profile)
================================

Unzip anywhere and run clipcut.exe. Settings are stored in
%APPDATA%\clipcut\config\config.toml.

REQUIREMENT: ffmpeg and ffprobe must be on your PATH.
    winget install Gyan.FFmpeg
$profileNote
Keyboard
    Space        play / pause
    Left/Right   seek 5 seconds
    , / .        step one frame
    I / O        set the in / out point
    Home / End   jump to the in / out point
    M            mute
"@ | Set-Content (Join-Path $stage "READ-ME-FIRST.txt") -Encoding utf8

$zip = Join-Path $repoRoot "dist\clipcut-$Version-win64-$Profile.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path "$stage\*" -DestinationPath $zip

Write-Host ""
Write-Host "Packaged clipcut $Version ($Profile)" -ForegroundColor Green
Get-ChildItem $stage | Select-Object Name, @{n = 'MB'; e = { [math]::Round($_.Length / 1MB, 2) } } | Format-Table
Write-Host "  folder: $stage"
Write-Host "  zip:    $zip  ($([math]::Round((Get-Item $zip).Length / 1MB, 1)) MB)"

# Emit the zip path for CI to pick up.
if ($env:GITHUB_OUTPUT) {
    "zip=$zip" | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8
}
