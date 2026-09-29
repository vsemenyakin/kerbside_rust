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
#     tools/bench.sh [--build release|dist | --path FILE [--dist]] \
#                    [--clip FILE] [--frames N] [--out DIR] [--baseline PYTHON_PERF_CSV]
#
# --build selects which artefact is measured, by name: 'release'
# (target/release/kerbside, the default) or 'dist' (the shipped, obfuscated
# build at target/dist/kerbside -- see scripts/build.sh).
#
# --path measures the binary at an explicit path instead -- for an artefact that
# does not live at a --build location (a copied-out build, or one built
# elsewhere). --path and --build are mutually exclusive. A relative --path is
# taken relative to the directory you ran the script from, not the repo root.
# BINARY=path/to/kerbside still overrides the path directly and wins over both.
#
# There are two measurements, picked by whether --clip is given:
#
#   * realtime (no --clip): the paced run over the self-generated scene, with the
#     per-frame perf CSV and the stage table -- the latency benchmark. Introspection
#     builds only. This is the default and is unchanged.
#   * throughput (--clip FILE): time a full *replay* of the clip and report fps
#     (frames / wall). Both kinds do this, so a protected (dist) and an unprotected
#     (release) build can be compared head-to-head on the SAME clip -- that is how
#     you measure what the hardening costs. Both replay under the default profile
#     (no --profile bench) with no perf instrumentation, so the numbers are
#     comparable. The dist build has this as its only mode (it takes one positional
#     clip); the introspection build is driven with `--replay --input FILE`.
#
#     A clip is ~2.76 MB/frame and the pipeline caps replay at SCENE_FRAMES
#     (default 1500), so make a clip of <= that many frames from a release build:
#         target/release/kerbside --generate clip.krw --frames 1500
#     A dist binary REQUIRES --clip (it cannot generate a scene).
#
# --build dist selects the dist kind; for a dist binary passed via --path, add
# --dist so the script uses the dist protocol rather than trying --version.
#
# Override a gate with FORCE=1 if you know why you are doing it. The report
# records that you did.

set -euo pipefail

# Remember where we were invoked from before cd'ing to the repo root, so a
# relative --path can be resolved against it rather than against the root.
ORIG_PWD="$PWD"
cd "$(dirname "$0")/.."

FRAMES=3000
OUT="telemetry"
BASELINE=""
FORCE="${FORCE:-0}"
MAX_TEMP_C=65
BUILD="release"
BUILD_SET=0
PATH_ARG=""
DIST_FLAG=0
CLIP=""
BINARY_ENV="${BINARY:-}"
# The default-profile SCENE_FRAMES (config/video.rs): the throughput replay runs
# under the default profile, which caps replay at this many frames, for both kinds.
# Update if that default changes. A clip longer than this is only partly replayed.
SCENE_CAP=1500

while [[ $# -gt 0 ]]; do
    case "$1" in
        --build) BUILD="$2"; BUILD_SET=1; shift 2 ;;
        --path) PATH_ARG="$2"; shift 2 ;;
        --dist) DIST_FLAG=1; shift ;;
        --clip) CLIP="$2"; shift 2 ;;
        --frames) FRAMES="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --baseline) BASELINE="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# --path and --build name the binary two different ways; giving both is a
# contradiction, not a refinement -- reject it rather than silently pick one.
if [[ -n "$PATH_ARG" && "$BUILD_SET" == "1" ]]; then
    echo "--path and --build are mutually exclusive." >&2
    exit 2
fi
# --dist marks a --path binary as a shipped build; it is meaningless on its own
# (use --build dist to select the dist artefact by profile).
if [[ "$DIST_FLAG" == "1" && -z "$PATH_ARG" ]]; then
    echo "--dist only applies with --path (use --build dist otherwise)." >&2
    exit 2
fi
# Resolve relative --path / --clip against the caller's directory (we cd'd to the
# repo root above, so a bare relative path would otherwise be looked up there).
if [[ -n "$PATH_ARG" && "$PATH_ARG" != /* ]]; then
    PATH_ARG="$ORIG_PWD/$PATH_ARG"
fi
if [[ -n "$CLIP" && "$CLIP" != /* ]]; then
    CLIP="$ORIG_PWD/$CLIP"
fi

# --- which binary to measure --------------------------------------------
# --build names the artefact (fixed path per profile); --path gives one
# explicitly. An explicit BINARY=... still wins over both, for a directory
# copied out from under target/.
if [[ -n "$PATH_ARG" ]]; then
    # Explicit path: measure exactly this file. There is no profile to rebuild,
    # so the "rebuild it" hints below name the path rather than a build command.
    BUILD_BINARY="$PATH_ARG"
    REBUILD_CMD="rebuild the binary you passed to --path"
    BUILD="path"
else
    case "$BUILD" in
        release)   BUILD_BINARY="target/release/kerbside" ; REBUILD_CMD="scripts/build.sh release" ;;
        dist)      BUILD_BINARY="target/dist/kerbside"    ; REBUILD_CMD="scripts/build.sh dist" ;;
        dev|debug) BUILD_BINARY="target/debug/kerbside"   ; REBUILD_CMD="scripts/build.sh dev" ;;
        *) echo "unknown --build '$BUILD' (expected: release, dist)" >&2; exit 2 ;;
    esac
    # The dist artefact is built with -Zbuild-std, which nests it under
    # target/<triple>/dist/. scripts/build.sh symlinks target/dist -> there; if
    # that link is absent (built elsewhere, or an older build.sh) resolve the
    # triple dir.
    if [[ "$BUILD" == "dist" && ! -e "$BUILD_BINARY" ]]; then
        for cand in target/*/dist/kerbside; do
            [[ -x "$cand" ]] && BUILD_BINARY="$cand" && break
        done
    fi
fi
BINARY="${BINARY_ENV:-$BUILD_BINARY}"

# The binary's CLI "kind" decides how it is validated and measured: the shipped
# 'dist' build has no --version and no realtime/perf flags (it replays one clip
# and prints the oracle hash), so it is throughput-benchmarked; everything else
# is the introspection build with the realtime/stage flow.
if [[ "$BUILD" == "dist" || "$DIST_FLAG" == "1" ]]; then
    KIND="dist"
else
    KIND="introspection"
fi

# A clip switches on the throughput measurement (timed replay); with no clip the
# introspection build does its realtime/stage benchmark. dist always has a clip.
if [[ -n "$CLIP" ]]; then
    MODE="throughput"
else
    MODE="realtime"
fi

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
    echo "  Build it first:  $REBUILD_CMD" >&2
    exit 1
fi

# Validate the input clip whenever one is given -- both the dist kind and an
# introspection throughput run replay it. n_frames is the little-endian i64 at
# byte offset 16 of the KRW1 header; the throughput measurement caps at SCENE_CAP.
if [[ -n "$CLIP" ]]; then
    if [[ ! -f "$CLIP" ]]; then
        echo "REFUSING TO BENCHMARK: --clip $CLIP not found." >&2
        exit 1
    fi
    if [[ "$(od -An -c -N4 "$CLIP" 2>/dev/null | tr -d ' ')" != "KRW1" ]]; then
        echo "REFUSING TO BENCHMARK: $CLIP is not a KRW1 clip (bad magic)." >&2
        exit 1
    fi
    CLIP_FRAMES="$(od -An -j16 -N8 -tu8 "$CLIP" 2>/dev/null | tr -d ' ')"
    [[ "$CLIP_FRAMES" =~ ^[0-9]+$ ]] || CLIP_FRAMES=0
fi

# The dist build takes a clip positionally and has no --version, so it requires a
# clip and skips the version gate -- its functional/staleness check is the timed
# replay itself (a stale/wrong build will not print a 64-hex oracle).
if [[ "$KIND" == "dist" && -z "$CLIP" ]]; then
    echo "REFUSING TO BENCHMARK: the dist build needs an input clip." >&2
    echo "  It takes one positional argument and replays it; pass --clip FILE." >&2
    echo "  Make one from a release build (a clip is ~2.76 MB/frame; keep it" >&2
    echo "  <= SCENE_FRAMES = $SCENE_CAP frames):" >&2
    echo "    target/release/kerbside --generate clip.krw --frames $SCENE_CAP" >&2
    exit 1
fi
[[ "$KIND" == "dist" ]] && VERSION_BLOCK=""

# A binary that does not understand --version is one built before this script
# existed, which means it was built from different source than the tree you are
# standing in. That is not an environment problem to be forced past -- it is a
# stale artefact, and benchmarking it would attribute its numbers to code that
# is not in it. Hence a hard refusal rather than a FORCE-able gate.
if [[ "$KIND" == "introspection" ]] && ! VERSION_BLOCK="$("$BINARY" --version 2>&1)"; then
    # Two very different faults land here, and conflating them sends people to
    # the wrong fix. A binary that *ran* and rejected the argument is stale
    # source; a binary that never started is a missing shared library.
    if grep -qi "unknown argument" <<<"$VERSION_BLOCK"; then
        echo "REFUSING TO BENCHMARK: $BINARY does not understand --version." >&2
        echo "  It predates this script, so it is built from older source than" >&2
        echo "  this checkout. Rebuild it:" >&2
        echo "    $REBUILD_CMD" >&2
    else
        echo "REFUSING TO BENCHMARK: $BINARY would not start." >&2
        echo "  Usually a shared library it links against is missing. On Windows" >&2
        echo "  a build done without the environment script links the per-module" >&2
        echo "  OpenCV DLLs (opencv_core4.dll and friends) instead of the single" >&2
        echo "  opencv_world DLL that is shipped next to the binary, and the" >&2
        echo "  loader then fails before main() runs. Rebuild through the script:" >&2
        echo "    . ./scripts/env-windows.ps1   # or source scripts/env-linux.sh" >&2
        echo "    $REBUILD_CMD" >&2
    fi
    echo >&2
    echo "  What it said:" >&2
    if [[ -n "${VERSION_BLOCK//[[:space:]]/}" ]]; then
        sed 's/^/    /' <<<"$VERSION_BLOCK" >&2
    else
        echo "    (nothing -- it died before it could print anything)" >&2
    fi
    exit 1
fi

if grep -q "NOT valid for benchmarking" <<<"$VERSION_BLOCK"; then
    fail "$BINARY is a debug build. Timings from it mean nothing.
    $REBUILD_CMD"
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
# the thing most likely to differ between two boards that look identical. The
# dist build says none of this (no --version), so name it and its input clip.
if [[ "$KIND" == "introspection" ]]; then
    sed 's/^/               /' <<<"$VERSION_BLOCK" | sed '1s/^ *//;1s/^/binary:        /'
else
    echo "binary:        $BINARY (dist build; no --version)"
fi
if [[ -n "$CLIP" ]]; then
    echo "clip:          $CLIP ($CLIP_FRAMES frames)"
fi
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

mkdir -p "$OUT"

if [[ "$MODE" == "realtime" ]]; then
    echo
    echo "== realtime run: $FRAMES frames  (build: $BUILD, binary: $BINARY) =="

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

    # The report tool is Python, deliberately: it is the *same* analysis the
    # Python arm runs, so the two tables cannot differ because of the reporting.
    # Raspberry Pi OS ships python3; nothing beyond the standard library is needed.
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
else
    # Throughput: time a full replay of the clip and report fps (frames / wall).
    # The measurement is the same for both kinds -- only the invocation differs --
    # so a protected (dist) and an unprotected (release) build are directly
    # comparable on the SAME clip. Both replay under the default profile with no
    # perf instrumentation. The run doubles as the functional/staleness check: a
    # stale/wrong build (or a bad clip, or a missing shared library) will not print
    # a `sha256 <64 hex>` line, and we refuse without recording.
    if [[ "$KIND" == "dist" ]]; then
        RUN_CMD=("$BINARY" "$CLIP")
    else
        # introspection replay on the clip: default profile (no --profile bench),
        # no --perf/--gc-stats, and --out "" to suppress the results CSV -- as close
        # to the dist invocation's work as the introspection CLI gets.
        RUN_CMD=("$BINARY" --replay --input "$CLIP" --out "")
    fi

    # Frames actually replayed: the pipeline caps at SCENE_CAP for both kinds.
    if [[ "$CLIP_FRAMES" -gt "$SCENE_CAP" ]]; then
        PROCESSED="$SCENE_CAP"
        echo "NOTE: clip has $CLIP_FRAMES frames but replay caps at SCENE_FRAMES=$SCENE_CAP;" >&2
        echo "      fps is computed on $SCENE_CAP frames." >&2
    else
        PROCESSED="$CLIP_FRAMES"
    fi

    echo
    echo "== $KIND throughput: $PROCESSED frames  (binary: $BINARY, clip: $CLIP) =="

    set +e
    T0="$(date +%s.%N)"
    RAW_OUT="$("${RUN_CMD[@]}" 2>/dev/null)"
    RC=$?
    T1="$(date +%s.%N)"
    set -e

    # dist prints only the oracle; the introspection build prints extra diagnostic
    # lines too, so pull the sha256 line out rather than matching the whole output.
    HASH_LINE="$(grep -E '^sha256 [0-9a-f]{64}$' <<<"$RAW_OUT" | head -1)"
    if [[ $RC -ne 0 || -z "$HASH_LINE" ]]; then
        echo "REFUSING TO BENCHMARK: the replay did not produce an oracle hash." >&2
        echo "  exit=$RC  output: ${RAW_OUT:-(none)}" >&2
        echo "  A stale/wrong build, a bad clip, or a missing shared library." >&2
        echo "    $REBUILD_CMD" >&2
        exit 1
    fi

    WALL="$(echo "$T1 - $T0" | bc -l)"
    if [[ "$PROCESSED" -gt 0 ]]; then
        FPS="$(echo "scale=1; $PROCESSED / $WALL" | bc -l 2>/dev/null || echo 'n/a')"
    else
        FPS="n/a"
    fi
    {
        echo "kind:          $KIND"
        echo "frames:        $PROCESSED"
        printf 'wall:          %.2f s\n' "$WALL"
        echo "throughput:    $FPS fps"
        echo "oracle:        $HASH_LINE"
    } | tee "$OUT/bench_summary.txt"
fi

echo
echo "Recorded to $OUT/. Environment: kind=$KIND build=$BUILD mode=$MODE governor=$GOVERNOR temp=${TEMP_C}C forced=$FORCE"
