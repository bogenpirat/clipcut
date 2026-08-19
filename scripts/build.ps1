<#
.SYNOPSIS
    Runs cargo with a working MSVC environment.

.DESCRIPTION
    Some Visual Studio Build Tools installations register the C++ compiler files
    without registering the workload, so `vcvarsall.bat` silently adds nothing to
    PATH and rustc cannot find link.exe. This locates the newest usable MSVC
    toolchain and Windows SDK and assembles PATH and LIB directly.

    Only the *libraries* are needed - missing C headers do not matter, because
    nothing in the dependency tree compiles C.

    Where the toolchain is registered properly, plain `cargo build` works and this
    script is optional; it still does the right thing either way.

.EXAMPLE
    .\scripts\build.ps1                 # cargo build
    .\scripts\build.ps1 run -- video.mkv
    .\scripts\build.ps1 test
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs = @("build")
)

$ErrorActionPreference = "Stop"

# ---- MSVC toolchain (newest complete one wins) -----------------------------
$vc = Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*" -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName "lib\x64\msvcrt.lib") } |
    Sort-Object Name | Select-Object -Last 1

# ---- Windows SDK -----------------------------------------------------------
$sdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10"
$sdk = Get-ChildItem "$sdkRoot\Lib\*" -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName "um\x64\kernel32.Lib") } |
    Sort-Object Name | Select-Object -Last 1

# Where either is missing, leave the environment alone: cargo is very likely
# already able to build, and a broken guess is worse than no guess.
if ($vc -and $sdk) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$($vc.FullName)\bin\Hostx64\x64;$sdkRoot\bin\$($sdk.Name)\x64;$env:PATH"
    $env:LIB = "$($vc.FullName)\lib\x64;$($sdk.FullName)\ucrt\x64;$($sdk.FullName)\um\x64"
    Write-Host "MSVC $($vc.Name) | SDK $($sdk.Name)" -ForegroundColor DarkGray
}
else {
    Write-Host "No MSVC toolchain located; using the environment as-is." -ForegroundColor DarkGray
}

# ---- libmpv present? -------------------------------------------------------
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path (Join-Path $repoRoot "third_party\mpv-dev\64\mpv.lib"))) {
    throw "libmpv not set up. Run .\scripts\setup-mpv.ps1 first."
}

Push-Location $repoRoot
try {
    & cargo @CargoArgs
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}
