@echo off
REM Build from cmd.exe.
REM
REM     scripts\build.cmd            dist profile -- what you ship
REM     scripts\build.cmd release    for benchmarking; tools\bench.sh looks here
REM
REM A shim onto scripts\build.ps1, which holds the actual logic. The setup has
REM to resolve libclang, OpenCV and the ONNX Runtime and export half a dozen
REM variables, and cmd.exe cannot dot-source a PowerShell script -- so the whole
REM job goes to PowerShell in one child process.
REM
REM Nothing here is required: from a PowerShell prompt, `.\scripts\build.ps1`
REM does exactly the same thing.

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
exit /b %errorlevel%
