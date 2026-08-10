# Build environment for Windows (MSVC).
#
# Dot-source it before building:
#
#     . .\scripts\env-windows.ps1
#     cargo build --release
#
# Three things have to be found, and none of them is discoverable on Windows
# the way pkg-config makes them on Raspberry Pi OS:
#
#   libclang   the `opencv` crate generates its bindings with it at build time,
#              and the generated build script *links* against libclang.dll, so
#              the DLL has to be on PATH as well as in LIBCLANG_PATH.
#   OpenCV     headers and the import library. The prebuilt release ships one
#              `opencv_world` module rather than the per-module libraries a
#              Linux distribution installs, hence OPENCV_LINK_LIBS below.
#   the DLLs   opencv_world4130.dll at run time. onnxruntime.dll is copied next
#              to the binary by the `ort` crate's copy-dylibs feature, so it
#              needs nothing here.
#
# Override any of these before dot-sourcing if your install lives elsewhere.

if (-not $env:LLVM_HOME)   { $env:LLVM_HOME   = "C:\Program Files\LLVM" }
if (-not $env:OPENCV_HOME) { $env:OPENCV_HOME = "C:\opencv\opencv\build" }
# vc16 is the toolset the prebuilt release ships; MSVC 2022 (vc17) links
# against it without trouble -- the C++ ABI has been stable since VS 2015.
if (-not $env:OPENCV_MSVC) { $env:OPENCV_MSVC = "vc16" }

if (-not (Test-Path "$env:LLVM_HOME\bin\libclang.dll")) {
    throw "libclang.dll not found under $env:LLVM_HOME. Install LLVM (winget install LLVM.LLVM) or set LLVM_HOME."
}
if (-not (Test-Path "$env:OPENCV_HOME\include\opencv2")) {
    throw "OpenCV headers not found under $env:OPENCV_HOME. See BUILD.md, or set OPENCV_HOME."
}

$libDir = "$env:OPENCV_HOME\x64\$env:OPENCV_MSVC\lib"
$binDir = "$env:OPENCV_HOME\x64\$env:OPENCV_MSVC\bin"

# The import library is named for the version: opencv_world4130.lib is 4.13.0.
# Derive it rather than hardcoding, so a different prebuilt release still works.
$world = Get-ChildItem -Path $libDir -Filter "opencv_world*.lib" -ErrorAction SilentlyContinue |
         Where-Object { $_.BaseName -notmatch 'd$' } |
         Select-Object -First 1
if (-not $world) {
    throw "No opencv_world*.lib under $libDir. Check OPENCV_MSVC (currently '$env:OPENCV_MSVC')."
}

$env:LIBCLANG_PATH        = "$env:LLVM_HOME\bin"
$env:OPENCV_LINK_LIBS     = $world.BaseName
$env:OPENCV_LINK_PATHS    = $libDir
$env:OPENCV_INCLUDE_PATHS = "$env:OPENCV_HOME\include"

foreach ($p in @("$env:LLVM_HOME\bin", $binDir)) {
    if ($env:PATH -notlike "*$p*") { $env:PATH = "$p;$env:PATH" }
}

# ONNX Runtime is opened at run time, so the binary needs a path to it. Point at
# any 1.26-compatible build; the copy inside the Python project's virtualenv is
# the best default, because then both implementations run the identical
# inference library and a CSV difference cannot be blamed on the runtime.
if (-not $env:ORT_DYLIB_PATH) {
    $candidates = @(
        (Join-Path (Split-Path -Parent $PSScriptRoot | Split-Path -Parent) `
                   "kerbside\venv\Lib\site-packages\onnxruntime\capi\onnxruntime.dll"),
        (Join-Path (Split-Path -Parent $PSScriptRoot) "onnxruntime.dll")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path $candidate) { $env:ORT_DYLIB_PATH = $candidate; break }
    }
}

Write-Host "libclang : $env:LIBCLANG_PATH"
Write-Host "opencv   : $env:OPENCV_LINK_LIBS in $env:OPENCV_LINK_PATHS"
if ($env:ORT_DYLIB_PATH) {
    Write-Host "onnxrt   : $env:ORT_DYLIB_PATH"
} else {
    Write-Warning "ORT_DYLIB_PATH is not set -- the binary will fail to start. See BUILD.md."
}
