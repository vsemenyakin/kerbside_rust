<#
.SYNOPSIS
    Build on Windows, and prove the result carries no build-machine paths.

.DESCRIPTION
    The Windows counterpart of scripts/build.sh. scripts/build.cmd is a thin
    shim onto this, for cmd.exe users.

    It sets the environment, picks the toolchain, builds, checks the binary for
    absolute paths, and confirms it actually starts.

    Toolchain
    ---------
    The `dist` profile builds with **nightly**, purely for one flag:
    `-Zlocation-detail=none`. Rust embeds the source file and line of every
    panic site -- `unwrap`, `expect`, indexing, overflow -- and nothing on
    stable removes them; `--remap-path-prefix` can only rewrite them to
    something anonymous. That flag makes them empty instead, which takes the
    binary from 167 embedded source paths to 53, all of the survivors coming
    from the precompiled standard library.

    Panics still report their message, they just no longer say which line
    raised them.

    Other profiles build with whatever toolchain is default, because they are
    not what gets shipped. If you ship `dist`, benchmark `dist`:

        BINARY=target/dist/kerbside tools/bench.sh

.EXAMPLE
    .\scripts\build.ps1

.EXAMPLE
    .\scripts\build.ps1 release
#>

[CmdletBinding()]
param(
    [string]$Profile = "dist"
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

# Cargo's profile names and its output directories do not match for the one
# built-in debug profile: `--profile dev` writes to target\debug. Everything
# else, including custom profiles like `dist`, uses its own name.
if ($Profile -eq "debug") { $Profile = "dev" }
if ($Profile -eq "dev") { $outDir = "debug" } else { $outDir = $Profile }

. (Join-Path $PSScriptRoot "env-windows.ps1")

if ($env:RUSTFLAGS -notlike "*remap-path-prefix*") {
    Write-Host "REFUSING TO BUILD: RUSTFLAGS carries no --remap-path-prefix flags." -ForegroundColor Red
    Write-Host "  scripts\env-windows.ps1 did not take effect, so the binary would" -ForegroundColor Red
    Write-Host "  embed this machine's paths. Check that the script is intact." -ForegroundColor Red
    exit 1
}

# --- toolchain -----------------------------------------------------------
$cargoArgs = @()
if ($Profile -eq "dist") {
    $haveNightly = (rustup toolchain list) -match "^nightly"
    if ($haveNightly) {
        $cargoArgs += "+nightly"
        $env:RUSTFLAGS = "$env:RUSTFLAGS -Zlocation-detail=none"
        Write-Host "toolchain: nightly (-Zlocation-detail=none)"
    } else {
        Write-Warning "nightly is not installed, so panic-site source paths will"
        Write-Warning "remain in the binary (anonymised, but present). Install it:"
        Write-Warning "  rustup toolchain install nightly --profile minimal"
    }
}

Write-Host ""
Write-Host "== building profile '$Profile' -> target\$outDir =="
$cargoArgs += @("build", "--profile", $Profile)
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) {
    Write-Host ""
    Write-Host "BUILD FAILED" -ForegroundColor Red
    exit 1
}

$binary = "target\$outDir\kerbside.exe"

# --- the leaked-path check -----------------------------------------------
Write-Host ""
$python = $null
foreach ($candidate in @("python3", "python", "py")) {
    if (Get-Command $candidate -ErrorAction SilentlyContinue) { $python = $candidate; break }
}
if (-not $python) {
    Write-Warning "python not found -- skipping the leaked-path check."
    Write-Warning "Run it wherever you do have python:"
    Write-Warning "  python tools\check_binary.py $binary"
} else {
    & $python tools\check_binary.py $binary
    if ($LASTEXITCODE -ne 0) { exit 1 }
}

# --- will it actually start? ---------------------------------------------
# A build can succeed and still produce something that dies in the loader --
# most often a missing DLL. Better to find that out here than in the middle of
# a benchmark run.
Write-Host ""
& ".\$binary" --version
if ($LASTEXITCODE -ne 0) {
    Write-Host ""
    Write-Host "WARNING: $binary was built but would not start." -ForegroundColor Red
    Write-Host "  Usually a DLL it links against is missing. See BUILD.md." -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "Built $binary"
