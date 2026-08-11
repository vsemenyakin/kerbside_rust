#!/usr/bin/env bash
# Run the benchmark, refusing to record a number that would be meaningless.
#
# The Rust arm's copy of the Python project's tools/bench.sh. Same gates, same
# refusals, same output files -- because the two arms have to be measured under
# conditions that are identical in every respect except the one under test.
#
# The gates below are not fussiness. On a small ARM board, thermal throttling
# and an on-demand governor each move the frame time by more than the entire
# interpreter share this benchmark exists to measure. A run taken on a warm
# board does not produce a noisy answer, it produces a confident wrong one, and
# there is no way to tell from the output file which one you have.
#
#     tools/bench.sh [--frames N] [--out DIR] [--baseline PYTHON_PERF_CSV]
#
# BINARY=path/to/kerbside overrides which build is measured, for a copied-out
# release directory that is not under target/.
#
# Override a gate with FORCE=1 if you know why you are doing it. The report
# records that you did.

set -euo pipefail

cd "$(dirname "$0")/.."

FRAMES=3000
OUT="telemetry"
BASELINE=""
FORCE="${FORCE:-0}"
MAX_TEMP_C=65
BINARY="${BINARY:-target/release/kerbside}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --frames) FRAMES="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --baseline) BASELINE="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

fail() {
    if [[ "$FORCE" == "1" ]]; then
        echo "WARNING (forced): $1" >&2
    else
        echo "REFUSING TO BENCHMARK: $1" >&2
        echo "  Fix it, or re-run with FORCE=1 to record it anyway." >&2
        exit 1
    fi
}

# --- the binary ----------------------------------------------------------
# Not a gate the Python needs: it has no build step, so it cannot be run in a
# slow configuration by accident. A debug build here is several times slower
# than a release one and would be a spectacularly wrong number to publish.
if [[ ! -x "$BINARY" ]]; then
    echo "REFUSING TO BENCHMARK: $BINARY not found." >&2
    echo "  Build it first:  cargo build --release" >&2
    exit 1
fi

# A binary that does not understand --version is one built before this script
# existed, which means it was built from different source than the tree you are
# standing in. That is not an environment problem to be forced past -- it is a
# stale artefact, and benchmarking it would attribute its numbers to code that
# is not in it. Hence a hard refusal rather than a FORCE-able gate.
if ! VERSION_BLOCK="$("$BINARY" --version 2>&1)"; then
    echo "REFUSING TO BENCHMARK: $BINARY does not understand --version." >&2
    echo "  It predates this script, so it is built from older source than this" >&2
    echo "  checkout. Rebuild it:" >&2
    echo "    cargo build --release" >&2
    echo >&2
    echo "  What it said:" >&2
    sed 's/^/    /' <<<"$VERSION_BLOCK" >&2
    exit 1
fi

if grep -q "NOT valid for benchmarking" <<<"$VERSION_BLOCK"; then
    fail "$BINARY is a debug build. Timings from it mean nothing.
    cargo build --release"
fi

echo "== environment =="

# --- CPU governor --------------------------------------------------------
GOVERNOR="unknown"
if [[ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor ]]; then
    GOVERNOR="$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
    echo "governor:      $GOVERNOR"
    if [[ "$GOVERNOR" != "performance" ]]; then
        fail "governor is '$GOVERNOR', not 'performance'. An on-demand governor
  ramps the clock *during* the run, so early frames are slow and late ones are
  fast, and the percentiles are a mixture of two machines.
    sudo cpupower frequency-set -g performance"
    fi
else
    echo "governor:      not exposed (not a Linux cpufreq system)"
fi

# --- temperature ---------------------------------------------------------
TEMP_C="unknown"
for zone in /sys/class/thermal/thermal_zone*/temp; do
    [[ -r "$zone" ]] || continue
    raw="$(cat "$zone")"
    TEMP_C=$((raw / 1000))
    break
done
if [[ "$TEMP_C" != "unknown" ]]; then
    echo "temperature:   ${TEMP_C} C"
    if (( TEMP_C > MAX_TEMP_C )); then
        fail "SoC is at ${TEMP_C} C (limit ${MAX_TEMP_C} C). Let it cool.
  Thermal derate moves the frame time by more than the effect you are trying to
  measure."
    fi
else
    echo "temperature:   not exposed"
fi

# --- clock ---------------------------------------------------------------
if command -v vcgencmd >/dev/null 2>&1; then
    echo "clock:         $(vcgencmd measure_clock arm | cut -d= -f2) Hz"
    THROTTLED="$(vcgencmd get_throttled | cut -d= -f2)"
    echo "throttled:     $THROTTLED"
    if [[ "$THROTTLED" != "0x0" ]]; then
        fail "the board reports throttling ($THROTTLED). Check power and cooling."
    fi
elif [[ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq ]]; then
    echo "clock:         $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq) kHz"
fi

echo "cores:         $(nproc)"
# The binary reports its own build profile and the native libraries it will
# actually call into -- including which libonnxruntime it resolved, which is
# the thing most likely to differ between two boards that look identical.
sed 's/^/               /' <<<"$VERSION_BLOCK" | sed '1s/^ *//;1s/^/binary:        /'
if command -v rustc >/dev/null 2>&1; then
    echo "rustc:         $(rustc --version)"
fi

# --- load ----------------------------------------------------------------
LOAD="$(cut -d' ' -f1 /proc/loadavg 2>/dev/null || echo 0)"
echo "load average:  $LOAD"
if [[ "$(echo "$LOAD > 1.0" | bc -l 2>/dev/null || echo 0)" == "1" ]]; then
    fail "load average is $LOAD before the run started. Something else is using
  this machine, and it will show up as tail latency attributed to this program."
fi

echo
echo "== realtime run: $FRAMES frames =="
mkdir -p "$OUT"

"$BINARY" \
    --realtime \
    --profile bench \
    --frames "$FRAMES" \
    --gc-stats \
    --out "$OUT/results_realtime.csv" \
    --perf-dir "$OUT" \
    | tee "$OUT/bench_summary.txt"

echo
echo "== stage report =="

# The report tool is Python, deliberately: it is the *same* analysis the Python
# arm runs, so the two tables cannot differ because of the reporting. Raspberry
# Pi OS ships python3; nothing beyond the standard library is needed.
PYTHON="$(command -v python3 || command -v python || true)"
if [[ -z "$PYTHON" ]]; then
    echo "python3 not found -- skipping the stage table." >&2
    echo "The raw per-frame CSV is at $OUT/perf_realtime.csv; run" >&2
    echo "  python3 tools/perf_report.py $OUT/perf_realtime.csv" >&2
    echo "wherever you do have it." >&2
else
    REPORT_ARGS=("$OUT/perf_realtime.csv")
    if [[ -n "$BASELINE" ]]; then
        REPORT_ARGS+=(--baseline "$BASELINE")
    fi
    "$PYTHON" tools/perf_report.py "${REPORT_ARGS[@]}" | tee -a "$OUT/bench_summary.txt"
fi

echo
echo "Recorded to $OUT/. Environment: governor=$GOVERNOR temp=${TEMP_C}C forced=$FORCE"
