<#
.SYNOPSIS
    Run the benchmark on Windows, refusing to record a meaningless number.

.DESCRIPTION
    The Windows counterpart of tools/bench.sh. Same structure, same run, same
    output files -- but read the warning below before using its numbers for
    anything.

    THIS IS NOT THE TARGET PLATFORM. The evaluation this port belongs to is
    about a Raspberry Pi 5, and a desktop x86-64 machine differs from it in
    every way that matters to the result: different vectorised kernels, a
    different memory system, far more thermal headroom, and a scheduler with
    other work to do. Per-stage shares in particular do not transfer.

    Worse, Windows exposes much less of the machine state than Linux does.
    bench.sh can refuse a run because the governor is wrong or the SoC is at
    70 C; here the CPU temperature is usually not readable at all, and the
    closest thing to a governor is the power scheme. The gates below are
    therefore weaker, and a run that passes them is *not* the same evidence a
    run that passes bench.sh is.

    Use this to catch regressions during development. Produce the numbers that
    go in the report on the board, with bench.sh.

.EXAMPLE
    .\tools\bench.ps1 -Frames 3000

.EXAMPLE
    .\tools\bench.ps1 -Frames 3000 -Baseline ..\kerbside\telemetry\perf_realtime.csv
#>

[CmdletBinding()]
param(
    [int]$Frames = 3000,
    [string]$Out = "telemetry",
    # The Python arm's per-frame perf CSV, for the tail comparison.
    [string]$Baseline = "",
    # Record the run even if a gate fails. The report records that you did.
    [switch]$Force
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

if (-not $Force -and $env:FORCE -eq "1") { $Force = $true }
$binary = "target\release\kerbside.exe"

function Fail([string]$message) {
    if ($Force) {
        Write-Warning "(forced): $message"
    } else {
        Write-Host "REFUSING TO BENCHMARK: $message" -ForegroundColor Red
        Write-Host "  Fix it, or re-run with -Force to record it anyway." -ForegroundColor Red
        exit 1
    }
}

# --- the binary ----------------------------------------------------------
# A debug build is several times slower than a release one and would be a
# spectacularly wrong number to publish.
if (-not (Test-Path $binary)) {
    Write-Host "REFUSING TO BENCHMARK: $binary not found." -ForegroundColor Red
    Write-Host "  Build it first:" -ForegroundColor Red
    Write-Host "    . .\scripts\env-windows.ps1; cargo build --release" -ForegroundColor Red
    exit 1
}
# A binary that does not understand --version predates this script, so it is
# built from different source than the tree you are standing in. That is a stale
# artefact rather than an environment condition, so it is a hard refusal and not
# a -Force-able gate: benchmarking it would attribute its numbers to code that
# is not in it.
$versionBlock = & $binary --version 2>&1
if ($LASTEXITCODE -ne 0) {
    # Two very different faults land here, and conflating them sends people to
    # the wrong fix. A binary that *ran* and rejected the argument is stale
    # source; a binary that never started is a missing DLL.
    if ("$versionBlock" -match "unknown argument") {
        Write-Host "REFUSING TO BENCHMARK: $binary does not understand --version." -ForegroundColor Red
        Write-Host "  It predates this script, so it is built from older source than" -ForegroundColor Red
        Write-Host "  this checkout. Rebuild it:" -ForegroundColor Red
    } else {
        Write-Host "REFUSING TO BENCHMARK: $binary would not start." -ForegroundColor Red
        Write-Host "  Usually a DLL it links against is missing. A build done without" -ForegroundColor Red
        Write-Host "  the environment script links the per-module OpenCV DLLs" -ForegroundColor Red
        Write-Host "  (opencv_core4.dll and friends) instead of the single opencv_world" -ForegroundColor Red
        Write-Host "  DLL shipped next to the binary, and the loader then fails before" -ForegroundColor Red
        Write-Host "  main() runs. Rebuild through the script:" -ForegroundColor Red
    }
    Write-Host "    . .\scripts\env-windows.ps1; cargo build --release" -ForegroundColor Red
    Write-Host ""
    Write-Host "  What it said:"
    if ("$versionBlock".Trim()) {
        $versionBlock | ForEach-Object { Write-Host "    $_" }
    } else {
        Write-Host "    (nothing -- it died before it could print anything)"
    }
    exit 1
}
if ($versionBlock -match "NOT valid for benchmarking") {
    Fail "$binary is a debug build. Timings from it mean nothing.`n    cargo build --release"
}

Write-Host "== environment =="
Write-Host "platform:      NOT the target board -- see the note in this script"

# --- power scheme --------------------------------------------------------
# The nearest Windows equivalent of the cpufreq governor. A balanced scheme
# ramps the clock during the run, so early frames are slow and late ones are
# fast, and the percentiles are a mixture of two machines.
#
# Matched by GUID, not by name: scheme names are localised, and OEM images ship
# their own schemes with their own names and GUIDs. Only the two schemes that
# are known to derate are refused. An unrecognised scheme is *warned* about
# rather than refused, because a gate that cannot be satisfied teaches people to
# pass -Force, which disables every other gate as well.
$schemeBalanced = "381b4222-f694-41f0-9685-ff5bb260df2e"
$schemePowerSaver = "a1841308-3541-4fab-bc81-f71556f20b4a"
$schemeHigh = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c"
$schemeUltimate = "e9a42b02-d5df-448d-aa00-03f14749eb61"

$scheme = "unknown"
$schemeGuid = ""
try {
    $active = powercfg /getactivescheme 2>$null
    if ($active -match '([0-9a-fA-F-]{36})') { $schemeGuid = $Matches[1].ToLower() }
    if ($active -match '\(([^)]+)\)') { $scheme = $Matches[1] }
    Write-Host "power scheme:  $scheme  [$schemeGuid]"
    if ($schemeGuid -eq $schemeBalanced -or $schemeGuid -eq $schemePowerSaver) {
        Fail "power scheme is '$scheme', which derates the CPU during the run.
    powercfg /setactive SCHEME_MIN"
    } elseif ($schemeGuid -ne $schemeHigh -and $schemeGuid -ne $schemeUltimate) {
        Write-Warning "power scheme '$scheme' is not one this script recognises."
        Write-Warning "Confirm yourself that it does not scale the clock down, or run:"
        Write-Warning "  powercfg /setactive SCHEME_MIN"
    }
} catch {
    Write-Host "power scheme:  not readable"
}

# --- mains power ---------------------------------------------------------
# On battery, Windows derates aggressively and silently.
try {
    $battery = Get-CimInstance -ClassName Win32_Battery -ErrorAction Stop
    if ($battery) {
        # BatteryStatus 2 == on AC.
        $onAc = ($battery | Where-Object { $_.BatteryStatus -eq 2 }).Count -gt 0
        Write-Host ("power source:  " + $(if ($onAc) { "mains" } else { "BATTERY" }))
        if (-not $onAc) {
            Fail "running on battery. Windows derates the CPU on battery, by an
  amount it does not report. Plug in."
        }
    } else {
        Write-Host "power source:  mains (no battery present)"
    }
} catch {
    Write-Host "power source:  not readable"
}

# --- temperature ---------------------------------------------------------
# Almost never available without vendor drivers. Reported honestly rather than
# guessed at, because a missing gate you know about is survivable and one you
# do not is not.
$tempC = "unknown"
try {
    $thermal = Get-CimInstance -Namespace "root/WMI" -ClassName MSAcpi_ThermalZoneTemperature -ErrorAction Stop
    if ($thermal) {
        $tempC = [int](($thermal | Select-Object -First 1).CurrentTemperature / 10 - 273.15)
        Write-Host "temperature:   $tempC C"
        if ($tempC -gt 65) {
            Fail "CPU is at $tempC C. Let it cool."
        }
    }
} catch {
    Write-Host "temperature:   not exposed (normal on Windows -- this gate is absent)"
}

Write-Host "cores:         $env:NUMBER_OF_PROCESSORS"
$versionBlock | ForEach-Object -Begin { $first = $true } -Process {
    if ($first) { Write-Host "binary:        $_"; $first = $false }
    else { Write-Host "               $_" }
}
if (Get-Command rustc -ErrorAction SilentlyContinue) {
    Write-Host "rustc:         $((rustc --version))"
}

# --- load ----------------------------------------------------------------
# Win32_Processor.LoadPercentage rather than a performance counter, because
# counter names are localised and this is not.
$load = -1
try {
    $load = (Get-CimInstance -ClassName Win32_Processor -ErrorAction Stop |
             Measure-Object -Property LoadPercentage -Average).Average
    Write-Host "cpu load:      $load %"
    if ($load -gt 25) {
        Fail "CPU is $load% busy before the run started. Something else is using
  this machine, and it will show up as tail latency attributed to this program."
    }
} catch {
    Write-Host "cpu load:      not readable"
}

Write-Host ""
Write-Host "== realtime run: $Frames frames =="
New-Item -ItemType Directory -Force $Out | Out-Null

$summary = Join-Path $Out "bench_summary.txt"

# Captured and re-emitted rather than piped through Tee-Object: pipeline output
# and Write-Host reach the console by different routes, and interleaving them
# puts the section headings in the wrong place in both the terminal and the
# saved summary. A benchmark log that reads out of order is a benchmark log
# nobody trusts.
function Emit([string[]]$lines, [switch]$Append) {
    $lines | ForEach-Object { Write-Host $_ }
    if ($Append) { $lines | Out-File -FilePath $summary -Encoding utf8 -Append }
    else { $lines | Out-File -FilePath $summary -Encoding utf8 }
}

$runOutput = & $binary --realtime --profile bench --frames $Frames --gc-stats `
    --out (Join-Path $Out "results_realtime.csv") `
    --perf-dir $Out 2>&1 | ForEach-Object { "$_" }
Emit $runOutput

Write-Host ""
Write-Host "== stage report =="

# The report tool is Python, deliberately: it is the *same* analysis the Python
# arm runs, so the two tables cannot differ because of the reporting.
$python = $null
foreach ($candidate in @("python3", "python", "py")) {
    if (Get-Command $candidate -ErrorAction SilentlyContinue) { $python = $candidate; break }
}
$perfCsv = Join-Path $Out "perf_realtime.csv"
if (-not $python) {
    Write-Warning "python was not found -- skipping the stage table."
    Write-Warning "The raw per-frame CSV is at $perfCsv; run"
    Write-Warning "  python3 tools/perf_report.py $perfCsv"
    Write-Warning "wherever you do have it."
} else {
    $reportArgs = @("tools/perf_report.py", $perfCsv)
    if ($Baseline) { $reportArgs += @("--baseline", $Baseline) }
    $reportOutput = & $python @reportArgs 2>&1 | ForEach-Object { "$_" }
    Emit $reportOutput -Append
}

Write-Host ""
Write-Host "Recorded to $Out\. Environment: scheme=$scheme temp=${tempC}C forced=$Force"
Write-Host "Reminder: these numbers are development signal, not report material." -ForegroundColor Yellow
