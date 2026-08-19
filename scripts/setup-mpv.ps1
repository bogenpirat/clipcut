<#
.SYNOPSIS
    Fetches the libmpv development files and builds an MSVC import library.

.DESCRIPTION
    zhongfly's mpv-winbuild ships a MinGW import library (libmpv.dll.a), which the
    MSVC toolchain cannot link against. This script downloads the LGPL dev build and
    synthesises a COFF import library (mpv.lib) from the DLL's export table.

    The LGPL build is used deliberately: the GPL variant would make the whole binary
    GPL on distribution, and nothing we need (H.264 / AV1 decode, the render API)
    lives in the GPL-only parts.

    Idempotent - re-running only redoes missing pieces unless -Force is passed.
#>
[CmdletBinding()]
param(
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$mpvDir = Join-Path $repoRoot "third_party\mpv-dev\64"
$dll = Join-Path $mpvDir "libmpv-2.dll"
$lib = Join-Path $mpvDir "mpv.lib"

# ---- 1. download + extract -------------------------------------------------
if ($Force -or -not (Test-Path $dll)) {
    Write-Host "Resolving latest mpv-winbuild release..."
    $release = Invoke-RestMethod "https://api.github.com/repos/zhongfly/mpv-winbuild/releases/latest" `
        -Headers @{ "User-Agent" = "clipcut-setup" }

    # Plain x86_64 (not -v3) for broader CPU compatibility.
    $asset = $release.assets |
        Where-Object { $_.name -like "mpv-dev-lgpl-x86_64-2*" -and $_.name -notlike "*-v3-*" } |
        Select-Object -First 1
    if (-not $asset) { throw "no mpv-dev-lgpl-x86_64 asset in release $($release.tag_name)" }

    Write-Host "Downloading $($asset.name) ($([math]::Round($asset.size / 1MB, 1)) MB)..."
    $tmp = Join-Path $env:TEMP $asset.name
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmp -UseBasicParsing

    $sevenZip = "C:\Program Files\7-Zip\7z.exe"
    if (-not (Test-Path $sevenZip)) {
        throw "7-Zip not found at $sevenZip. Install it (winget install 7zip.7zip) and re-run."
    }

    New-Item -ItemType Directory -Force -Path $mpvDir | Out-Null
    & $sevenZip x $tmp "-o$mpvDir" -y | Out-Null
    Remove-Item $tmp -Force
    Write-Host "Extracted to $mpvDir"
} else {
    Write-Host "libmpv-2.dll already present; skipping download."
}

# ---- 2. locate MSVC tooling ------------------------------------------------
if ($Force -or -not (Test-Path $lib)) {
    $msvcRoot = Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\lib.exe" `
        -ErrorAction SilentlyContinue | Sort-Object FullName | Select-Object -Last 1
    if (-not $msvcRoot) { throw "could not locate MSVC lib.exe" }
    $binDir = Split-Path -Parent $msvcRoot.FullName

    # ---- 3. DLL exports -> .def -> .lib ------------------------------------
    Write-Host "Generating import library from $dll ..."
    $env:PATH = "$binDir;$env:PATH"     # dumpbin needs its sibling DLLs
    $raw = & "$binDir\dumpbin.exe" /EXPORTS $dll

    $names = $raw | ForEach-Object {
        if ($_ -match '^\s+\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]{8}\s+(\S+)') { $Matches[1] }
    }
    if ($names.Count -lt 10) { throw "parsed only $($names.Count) exports - dumpbin output format changed?" }

    $mpvCount = ($names | Where-Object { $_ -like 'mpv_*' }).Count
    Write-Host "  $($names.Count) exports ($mpvCount mpv_*)"

    # LIBRARY must name the real DLL, otherwise the linker records an import
    # against "mpv.dll" (derived from the .def filename) and the exe won't start.
    $def = Join-Path $mpvDir "mpv.def"
    $content = "LIBRARY libmpv-2.dll`r`nEXPORTS`r`n" + (($names | ForEach-Object { "    $_" }) -join "`r`n") + "`r`n"
    Set-Content -Path $def -Value $content -Encoding ascii

    & "$binDir\lib.exe" /def:$def /out:$lib /machine:x64 | Out-Null
    if (-not (Test-Path $lib)) { throw "lib.exe did not produce $lib" }
    Write-Host "Created $lib"
} else {
    Write-Host "mpv.lib already present; skipping generation."
}

Write-Host ""
Write-Host "libmpv ready. Build with:  .\scripts\build.ps1" -ForegroundColor Green
