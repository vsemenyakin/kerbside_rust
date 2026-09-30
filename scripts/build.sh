#!/usr/bin/env bash
# Build on Linux / Raspberry Pi OS, and prove the result carries no leaks.
#
#     scripts/build.sh              dist profile -- what you ship (OBFUSCATED)
#     scripts/build.sh release      for benchmarking; tools/bench.sh looks here
#     scripts/build.sh dev          debug build, for development only
#     scripts/build.sh --no-defense UNPROTECTED twin of dist (dist-nodef profile):
#                                   identical codegen but no anti-tamper feature and
#                                   no OLLVM, for measuring the protection's cost.
#                                   Cleartext constants -- benchmark only, NEVER ship.
#
# The Linux counterpart of scripts/build.cmd. It sets the environment, builds,
# and then checks the binary for absolute paths from this machine -- because a
# `--remap-path-prefix` flag that silently stops being passed is exactly the
# kind of regression nobody notices until the artefact is already distributed.
#
# The dist profile is the shipped artefact and now carries THREE hardening
# layers at once:
#   * strip + --no-default-features + build-std   -- removes names, the settings
#       schema, the source paths and the panic message strings (T2 leak);
#   * -Zllvm-plugins=<Pluto>                      -- OLLVM control-flow obfuscation
#       (bogus flow + flattening + substitution) applied to the kerbside crate,
#       so the algorithm/stage order (A1) is expensive to recover even from a
#       memory dump. Applied via `cargo rustc -- ...` so ONLY the kerbside crate
#       pays it; std and the dependencies are not obfuscated.
#
# LLVM-version coupling (important): -Zllvm-plugins loads a pass plugin into the
# LLVM that rustc itself carries, so the plugin MUST be built against that exact
# LLVM major. The obfuscated dist therefore pins its own toolchain (OBF_TOOLCHAIN)
# whose LLVM matches the committed plugin (OBF_PLUGIN). Bump both together.

set -euo pipefail

cd "$(dirname "$0")/.."

# Make the rust toolchain reachable even from a non-login shell (cron/CI/ssh
# exec), where ~/.cargo/bin may not be on PATH yet.
if ! command -v rustup >/dev/null 2>&1 && [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
fi

# --- arguments -----------------------------------------------------------
#   [dist|release|dev]   which profile (default dist)
#   --no-defense         build the UNPROTECTED benchmark twin of dist: same code
#                        generation, but without the anti-tamper feature and
#                        without the OLLVM plugin, written to the dist-nodef
#                        profile so it cannot overwrite the real dist. dist only.
PROFILE="dist"
NO_DEFENSE=0
for arg in "$@"; do
    case "$arg" in
        --no-defense)          NO_DEFENSE=1 ;;
        dist|release|dev|debug) PROFILE="$arg" ;;
        *) echo "REFUSING TO BUILD: unknown argument '$arg'" >&2
           echo "  usage: scripts/build.sh [dist|release|dev] [--no-defense]" >&2
           exit 2 ;;
    esac
done
if [[ "$NO_DEFENSE" == 1 && "$PROFILE" != "dist" ]]; then
    echo "REFUSING TO BUILD: --no-defense only applies to the dist profile." >&2
    exit 2
fi

# --- obfuscation configuration (dist only) -------------------------------
# Overridable from the environment. Defaults match what was built on this board.
#   OBF_TOOLCHAIN  a rustup toolchain whose bundled LLVM == the plugin's LLVM,
#                  and which satisfies the deps' MSRV (opencv/ort need >= 1.88).
#   OBF_PLUGIN     the Pluto pass-plugin .so, built against that same LLVM.
#   OBF_POLICY     Pluto reads "policy.json" from the working directory; this is
#                  the checked-in selective policy (flatten crown functions,
#                  bcf+sub elsewhere, gle on the module).
OBF_TOOLCHAIN="${OBF_TOOLCHAIN:-nightly-2025-06-15}"
# The plugin is vendored in-repo (stripped, ~1.3 MB) so a fresh checkout needs no
# plugin build. It is aarch64 + LLVM 20 specific and loads libLLVM.so.20.1 +
# libz3.so.4 at run time -- see BUILD.md "Fresh-machine setup". $PWD is the repo
# root here (we cd'd to it above).
OBF_PLUGIN="${OBF_PLUGIN:-$PWD/vendor/Pluto-llvm20.so}"
OBF_POLICY="${OBF_POLICY:-policy.json}"

# A `dist` build is "dist-like": build-std, static OpenCV, the pinned toolchain.
# The unprotected benchmark twin (--no-defense) is dist-like too, but skips the
# protection steps (OLLVM, the anti-tamper feature, CRYPT_K, seal, the strict
# audit) and builds the `dist-nodef` profile so its artefact goes to its own
# directory and can never overwrite or be mistaken for the real shipped dist.
DIST_LIKE=0
DEFENSE=1
CARGO_PROFILE="$PROFILE"
if [[ "$PROFILE" == "dist" ]]; then
    DIST_LIKE=1
    if [[ "$NO_DEFENSE" == 1 ]]; then
        CARGO_PROFILE="dist-nodef"
        DEFENSE=0
    fi
fi

# Cargo's profile names and its output directories do not match for the one
# built-in debug profile: `--profile dev` writes to target/debug. Everything
# else, including custom profiles like `dist`/`dist-nodef`, uses its own name.
case "$CARGO_PROFILE" in
    dev|debug) CARGO_PROFILE="dev"; OUT_DIR="debug" ;;
    *)         OUT_DIR="$CARGO_PROFILE" ;;
esac

if [[ "$NO_DEFENSE" == 1 ]]; then
    echo "##############################################################"
    echo "##  UNPROTECTED BUILD (--no-defense)                        ##"
    echo "##  NO obfuscation, NO constant encryption, NO anti-tamper. ##"
    echo "##  Cleartext constants. For benchmarking ONLY -- DO NOT SHIP.##"
    echo "##############################################################"
fi

# --- environment ---------------------------------------------------------
# Sets RUSTFLAGS with the --remap-path-prefix flags, and points at the ONNX
# Runtime if it can find one. Sourced rather than executed: the variables have
# to survive into the cargo invocation below.
# shellcheck source=scripts/env-linux.sh
source scripts/env-linux.sh

if [[ "${RUSTFLAGS:-}" != *remap-path-prefix* ]]; then
    echo "REFUSING TO BUILD: RUSTFLAGS carries no --remap-path-prefix flags." >&2
    echo "  scripts/env-linux.sh did not take effect, so the binary would embed" >&2
    echo "  this machine's paths. Check that the script is intact." >&2
    exit 1
fi

# --- toolchain -----------------------------------------------------------
# The dist profile builds with a pinned nightly for three things stable cannot
# do, and each is the difference between anonymising a leak and removing it:
#
#   * -Zlocation-detail=none              drops the source file+line of every
#                                         panic site.
#   * -Zbuild-std + panic_immediate_abort rebuilds std without its panic
#                                         machinery -- removes /rustc/... paths
#                                         and the panic *message strings*.
#   * -Zllvm-plugins=<Pluto>              OLLVM obfuscation of the kerbside crate.
#
# There is no stable fallback, so when the pinned toolchain / rust-src / plugin /
# policy are missing we REFUSE rather than quietly shipping an un-hardened binary.
CARGO_ARGS=()
BUILD_STD_ARGS=()
RUSTC_PLUGIN_ARGS=()
HOST_TRIPLE=""
if [[ "$DIST_LIKE" == 1 ]]; then
    if ! rustup toolchain list 2>/dev/null | grep -q "^${OBF_TOOLCHAIN}"; then
        echo "REFUSING TO BUILD: the dist profile needs the pinned toolchain '${OBF_TOOLCHAIN}'." >&2
        echo "  Its LLVM must match the obfuscation plugin's LLVM. Install it:" >&2
        echo "    rustup toolchain install ${OBF_TOOLCHAIN} --profile minimal" >&2
        echo "    rustup component add rust-src --toolchain ${OBF_TOOLCHAIN}" >&2
        echo "  Or build the un-hardened profile:  scripts/build.sh release" >&2
        exit 1
    fi
    if ! rustup component list --toolchain "${OBF_TOOLCHAIN}" 2>/dev/null \
            | grep -q '^rust-src .*(installed)'; then
        echo "REFUSING TO BUILD: -Zbuild-std needs rust-src on ${OBF_TOOLCHAIN}." >&2
        echo "    rustup component add rust-src --toolchain ${OBF_TOOLCHAIN}" >&2
        exit 1
    fi
    # The OLLVM plugin / policy / wrapper are only needed for the protected build.
    if [[ "$DEFENSE" == 1 ]]; then
        if [[ ! -f "$OBF_PLUGIN" ]]; then
            echo "REFUSING TO BUILD: obfuscation plugin not found: $OBF_PLUGIN" >&2
            echo "  Build it (Pluto backend of lich4/ollvm-pass) against the LLVM that" >&2
            echo "  ${OBF_TOOLCHAIN} carries, or point OBF_PLUGIN at it. See hardening/." >&2
            exit 1
        fi
        if [[ ! -f "$OBF_POLICY" ]]; then
            echo "REFUSING TO BUILD: obfuscation policy not found: $OBF_POLICY" >&2
            echo "  Pluto reads it from the working directory. Restore policy.json." >&2
            exit 1
        fi
        if [[ ! -f "$PWD/scripts/obf-rustc-wrapper.sh" ]]; then
            echo "REFUSING TO BUILD: obfuscation rustc wrapper not found:" >&2
            echo "  $PWD/scripts/obf-rustc-wrapper.sh" >&2
            echo "  It scopes -Zllvm-plugins to the kerbside crate. Restore it." >&2
            exit 1
        fi
    fi
    # build-std must know the concrete target; there is no host default for it.
    HOST_TRIPLE="$(rustc "+${OBF_TOOLCHAIN}" -vV | awk '/^host: /{print $2}')"
    if [[ -z "$HOST_TRIPLE" ]]; then
        echo "REFUSING TO BUILD: could not determine the host target triple." >&2
        exit 1
    fi
    CARGO_ARGS+=("+${OBF_TOOLCHAIN}")
    # The obfuscation plugin goes in RUSTFLAGS, NOT `cargo rustc -- ...`: the
    # `cargo rustc` + `-Zbuild-std` combination self-deadlocks on the target lock
    # (cargo opens target/<triple>/dist/.cargo-lock twice, exclusively). With the
    # plugin in RUSTFLAGS a plain `cargo build` is used instead, and policy.json's
    # func filter (".*kerbside.*") scopes obfuscation to this crate, so std and the
    # dependencies are compiled with the plugin loaded but left untransformed.
    export RUSTFLAGS="$RUSTFLAGS -Zlocation-detail=none"
    # The obfuscation plugin is loaded ONLY for the kerbside crate, through a
    # RUSTC_WORKSPACE_WRAPPER -- not globally via RUSTFLAGS. A global
    # -Zllvm-plugins also loads the plugin into every std crate that -Zbuild-std
    # recompiles; those compile with a CWD inside the rustup std source tree,
    # where the plugin cannot find policy.json and prints "Error: conf not found"
    # for each. cargo calls the workspace wrapper only for workspace members
    # (never for std or the dependencies), so std and the deps stay plugin-free
    # (they must not be obfuscated) and the plugin reads policy.json from the repo
    # root, which is the kerbside crate's own compile CWD.
    if [[ "$DEFENSE" == 1 ]]; then
        export OBF_PLUGIN
        export RUSTC_WORKSPACE_WRAPPER="$PWD/scripts/obf-rustc-wrapper.sh"
    fi

    # --- static OpenCV (shipped binary only) ---------------------------------
    # Link core/imgproc/video from the system static archives (.a) so their cv::
    # functions become internal symbols instead of dynamic imports resolved from
    # libopencv_*.so. That removes the LD_PRELOAD interposition surface a reverse
    # engineer uses on the running device to read the exact arguments of
    # createBackgroundSubtractorMOG2 / getPerspectiveTransform /
    # getStructuringElement. videoio and imgcodecs -- which would drag OpenCV's
    # FFmpeg/GStreamer/codec backends and cannot be linked statically without ~30
    # more libraries -- are already excluded here by --no-default-features (the
    # `overlay` feature is off in dist). Debian ships no -ldev symlinks for
    # BLAS/LAPACK, so make build-local ones for the three modules' small dep set.
    OCV_LINKS="$PWD/target/.static-opencv-links"
    mkdir -p "$OCV_LINKS"
    for pair in "libblas.so:libblas.so.3" "liblapack.so:liblapack.so.3"; do
        link="${pair%%:*}"; soname="${pair##*:}"
        real="$(ls /usr/lib/*/"$soname" 2>/dev/null | head -1)"
        if [[ -z "$real" ]]; then
            echo "REFUSING TO BUILD: $soname not found, needed to static-link OpenCV." >&2
            echo "  Install the runtime (libblas3 / liblapack3) or adjust scripts/build.sh." >&2
            exit 1
        fi
        ln -sf "$real" "$OCV_LINKS/$link"
    done
    export OPENCV_INCLUDE_PATHS="/usr/include/opencv4"
    export OPENCV_LINK_PATHS="/usr/lib/aarch64-linux-gnu,$OCV_LINKS"
    export OPENCV_LINK_LIBS="static=opencv_video,static=opencv_imgproc,static=opencv_core,lapack,blas,tbb,z,GLX"
    BUILD_STD_ARGS+=(
        "--target" "$HOST_TRIPLE"
        "-Zbuild-std=std,panic_abort"
        # Old spelling of immediate-abort: rebuilds std without panic strings.
        # (The newer -Cpanic=immediate-abort only exists on later nightlies.)
        "-Zbuild-std-features=panic_immediate_abort"
        # Drop the introspection feature so the settings field-name strings
        # (IMAGE_POINTS, FRAME_WIDTH, ...) and the derived Debug labels never
        # reach the shipped binary. dev/release keep it (and --dump-settings).
        "--no-default-features"
    )
    # Anti-tamper umbrella (protected build only): the run-time gates
    # (main::harden -- non-dumpable, TracerPid refusal, self-ptrace, mseal), the
    # code-integrity decode key and constant encryption (crypt). The --no-defense
    # twin omits it, leaving cleartext constants and no runtime hardening -- which
    # is the whole point of that build.
    if [[ "$DEFENSE" == 1 ]]; then
        BUILD_STD_ARGS+=("--features" "anti-tamper")
    fi
    echo "toolchain: ${OBF_TOOLCHAIN}  (LLVM matched to plugin)"
    echo "  -Zlocation-detail=none                    (drop panic-site source paths)"
    echo "  -Zbuild-std + panic_immediate_abort       (drop std paths and panic strings)"
    echo "  --no-default-features                     (drop settings field-name strings + overlay)"
    echo "  static OpenCV core/imgproc/video          (cv:: calls not LD_PRELOAD-interposable)"
    if [[ "$DEFENSE" == 1 ]]; then
        echo "  --features anti-tamper                    (non-dumpable + TracerPid + self-ptrace + anti-emulation key + constant encryption)"
        echo "  -Zllvm-plugins=$(basename "$OBF_PLUGIN")   (OLLVM obfuscation via workspace wrapper, kerbside crate only, policy: $OBF_POLICY)"
    else
        echo "  NO --features anti-tamper                 (UNPROTECTED: cleartext constants, no runtime hardening)"
        echo "  NO -Zllvm-plugins                         (UNPROTECTED: no OLLVM obfuscation)"
    fi
    echo "  --target $HOST_TRIPLE"

    # Fresh random obfuscation key K for the protected build (crypt reads CRYPT_K
    # at compile time). Kills the recognisable magic number and makes two shipped
    # copies use different keys. Set once, BEFORE both the program build and the
    # seal-tool build, so the two agree. Not needed without constant encryption.
    if [[ "$DEFENSE" == 1 ]]; then
        if [[ -z "${CRYPT_K:-}" ]]; then
            CRYPT_K="0x$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
            export CRYPT_K
        fi
        echo "  CRYPT_K set (random per build)"
    fi
fi

echo
echo "== building profile '$PROFILE' -> target/$OUT_DIR =="

# `cargo build` (not `cargo rustc`): the plugin is already in RUSTFLAGS, and
# `cargo rustc` + `-Zbuild-std` self-deadlocks on the target lock. Obfuscation is
# scoped to this crate by policy.json, not by which crate cargo passes args to.
#
# There is a *second* -Zbuild-std deadlock, independent of `cargo rustc`: cargo
# opens target/<triple>/dist/.cargo-lock twice and flock() blocks on the lock this
# same process already holds. It only triggers when an existing std cache is in a
# stale, partially built state -- after a reboot, or after a previous build was
# interrupted; a clean std build takes the single-lock path and does not deadlock.
# The symptom is the line "Blocking waiting for file lock on build directory" with
# no "Compiling" ever following. `build_dist_once` runs the build under a watchdog
# that detects exactly that hang (rc 42) so the caller can wipe the per-target tree
# and retry from clean. Grace window overridable via BUILD_STD_GRACE (seconds).
build_dist_once() {
    local log; log="$(mktemp)"
    cargo "${CARGO_ARGS[@]}" build --profile "$CARGO_PROFILE" "${BUILD_STD_ARGS[@]}" --bin kerbside >"$log" 2>&1 &
    local cpid=$!
    tail -n +1 -f "$log" 2>/dev/null & local tpid=$!
    # The "Blocking waiting for file lock" line is printed only on the self-
    # deadlock here (build.sh is the sole builder, so there is no real contender
    # for the lock); a healthy build reaches "Compiling" within seconds. So a
    # short grace is enough to tell the two apart and recover quickly.
    local waited=0 grace="${BUILD_STD_GRACE:-45}" deadlock=0
    while kill -0 "$cpid" 2>/dev/null; do
        if grep -q "Compiling" "$log" 2>/dev/null; then break; fi   # compilation started -> healthy
        if (( waited >= grace )) && grep -q "Blocking waiting for file lock" "$log" 2>/dev/null; then
            deadlock=1
            pkill -9 -P "$cpid" 2>/dev/null || true
            kill -9 "$cpid" 2>/dev/null || true
            break
        fi
        sleep 5; waited=$(( waited + 5 ))
    done
    local rc=0
    wait "$cpid" 2>/dev/null || rc=$?
    kill "$tpid" 2>/dev/null || true
    rm -f "$log"
    if (( deadlock )); then return 42; fi
    return "$rc"
}

if [[ "$DIST_LIKE" == 1 ]]; then
    if build_dist_once; then rc=0; else rc=$?; fi
    if (( rc == 42 )); then
        echo >&2
        echo "note: -Zbuild-std lock deadlock detected -- wiping target/$HOST_TRIPLE" >&2
        echo "      for a clean std build and retrying once." >&2
        rm -rf "target/$HOST_TRIPLE" "target/$CARGO_PROFILE"
        if build_dist_once; then rc=0; else rc=$?; fi
    fi
    if (( rc != 0 )); then echo; echo "BUILD FAILED" >&2; exit 1; fi
else
    if ! cargo "${CARGO_ARGS[@]}" build --profile "$CARGO_PROFILE" "${BUILD_STD_ARGS[@]}"; then
        echo; echo "BUILD FAILED" >&2; exit 1
    fi
fi

# build-std forces --target, which nests the output under target/<triple>/.
# Re-point the documented target/<profile>/ path at it so every downstream
# reference (check_binary below, BUILD.md, BINARY=target/dist/kerbside for
# bench.sh) keeps working regardless of the host triple. The --no-defense twin
# points its OWN target/dist-nodef -> ... and never touches target/dist.
if [[ "$DIST_LIKE" == 1 ]]; then
    if [[ -d "target/$CARGO_PROFILE" && ! -L "target/$CARGO_PROFILE" ]]; then
        rm -rf "target/$CARGO_PROFILE"   # stale real directory from a pre-build-std build
    fi
    ln -sfn "$HOST_TRIPLE/$CARGO_PROFILE" "target/$CARGO_PROFILE"
fi

BINARY="target/$OUT_DIR/kerbside"

# --- strip the toolchain fingerprint (dist only) -------------------------
# `.comment` carries the GCC and rustc version strings (e.g. "GCC: (Debian
# 14.2.0-19) 14.2.0" / "rustc version 1.89.0-nightly (...)"). It is a
# non-allocated, informational section -- dropping it is behaviour-neutral (the
# oracle digest is unchanged) but denies a reverse engineer the exact compiler
# and toolchain versions they use to match a disassembler/plugin build. `strip =
# "symbols"` does not remove it, so do it explicitly. Not done for dev/release,
# which stay diagnosable.
if [[ "$DIST_LIKE" == 1 ]]; then
    if command -v objcopy >/dev/null 2>&1; then
        objcopy --remove-section .comment "$BINARY" 2>/dev/null \
            || echo "note: could not remove .comment from $BINARY" >&2
    else
        echo "note: objcopy not found -- .comment left in $BINARY" >&2
    fi
fi

# --- scrub the OpenCV identifying strings (dist only) --------------------
# cv::getBuildInformation() returns one big embedded string (OpenCV version,
# platform, CPU features, the full C/C++ compiler command lines, the module list,
# FFMPEG/GStreamer flags), and the exact version also survives in a couple of
# plugin version-mismatch messages and the bare getVersionString() value. kerbside
# never calls getBuildInformation and this is a static, plugin-free build, so all
# of it is dead/display-only data (verified: blanking it leaves the oracle digest
# unchanged). Neutralise it in place, size-preserving, so ELF offsets and the
# oracle are untouched.
if [[ "$DIST_LIKE" == 1 ]]; then
    _scrub_py="$(command -v python3 || command -v python || true)"
    if [[ -n "$_scrub_py" ]]; then
        "$_scrub_py" tools/scrub_opencv.py "$BINARY" \
            || echo "note: scrub_opencv failed on $BINARY" >&2
    else
        echo "note: python not found -- OpenCV banner/version left in $BINARY" >&2
    fi
fi

# --- seal the decode key to the code (dist only) -------------------------
# MUST be the last step that could touch the binary: it hashes the final .text
# and patches SALT2 so the anti-tamper decode key resolves to K only for exactly
# this code (see src/crypt.rs::keying and tools/patch_integrity.py). Any later
# byte change to .text -- a spliced-in argument logger, a breakpoint -- moves the
# hash, so every encrypted constant/string decodes to garbage. Fail the build if
# it cannot run, rather than shipping an unsealed (sentinel-keyed) binary that
# would itself produce garbage. Skipped for --no-defense: with no anti-tamper
# there is no SALT2 sentinel to patch (and nothing to seal).
if [[ "$DEFENSE" == 1 ]]; then
    # The sealer is the crypt crate's `seal` bin (single source of truth for the
    # constants and the .text hash -- it can never drift from the runtime). Build
    # it in a CLEAN environment: it is a host build-tool and must NOT inherit the
    # dist RUSTFLAGS (OLLVM plugin via the workspace wrapper, -Zlocation-detail),
    # which would obfuscate/slow it for no reason. Page size is the deployment
    # target's (RPi5 = 16384); override with SEAL_PAGE_SIZE.
    SEAL_PAGE_SIZE="${SEAL_PAGE_SIZE:-16384}"
    if ! env -u RUSTFLAGS -u RUSTC_WORKSPACE_WRAPPER cargo build -q -p crypt --bin seal; then
        echo "REFUSING TO SHIP: could not build the crypt seal tool." >&2
        exit 1
    fi
    if ! ./target/debug/seal "$BINARY" --page-size "$SEAL_PAGE_SIZE"; then
        echo "REFUSING TO SHIP: could not seal $BINARY." >&2
        exit 1
    fi
fi

# --- the leaked-path check -----------------------------------------------
# dev and release deliberately do not harden away the first-party module tree;
# only dist does. So allow first-party paths for those (the check still fails on
# any absolute build-machine path), and demand them gone for dist. The
# --no-defense twin is deliberately unprotected (cleartext constants), so the
# strict audit would fail by design -- skip it (this artefact is never shipped).
echo
if [[ "$NO_DEFENSE" == 1 ]]; then
    echo "note: skipping the leak audit for the UNPROTECTED --no-defense build." >&2
    echo "      It has cleartext constants by design and must never be shipped." >&2
else
    CHECK_ARGS=("--profile" "$PROFILE")
    if [[ "$PROFILE" != "dist" ]]; then
        CHECK_ARGS+=("--allow-first-party")
    fi
    PYTHON="$(command -v python3 || command -v python || true)"
    if [[ -z "$PYTHON" ]]; then
        echo "python3 not found -- skipping the leaked-path check." >&2
        echo "Run it wherever you do have python:" >&2
        echo "  python3 tools/check_binary.py $BINARY" >&2
    else
        "$PYTHON" tools/check_binary.py "$BINARY" "${CHECK_ARGS[@]}"
    fi
fi

# --- will it actually start? ---------------------------------------------
# A build can succeed and still produce something that dies in the loader --
# most often because libonnxruntime.so is neither beside the binary nor on the
# library search path. Better to find that out here than in the middle of a
# benchmark run.
echo
# The dist build takes a positional clip path and needs a clip to run, so it is
# smoke-tested by resolving its dynamic libraries (`ldd`): if they all resolve it
# will load. Every other profile keeps the flag CLI, so a one-frame replay is
# used there -- it also exercises the OpenCV/onnxruntime load. (libonnxruntime is
# `dlopen`ed, not an `ldd` entry, so the dist check does not cover it; a missing
# runtime shows up on the first real analysis run instead.)
if [[ "$DIST_LIKE" == 1 ]]; then
    if ldd "$BINARY" 2>&1 | grep -q "not found"; then
        echo
        echo "WARNING: $BINARY was built but has unresolved libraries:" >&2
        ldd "$BINARY" 2>&1 | grep "not found" >&2
        exit 1
    fi
    echo
    echo "Built $BINARY"
    if [[ "$NO_DEFENSE" == 1 ]]; then
        echo "  ^ UNPROTECTED (--no-defense): benchmark twin of dist, cleartext"
        echo "    constants, no anti-tamper, no OLLVM. DO NOT SHIP THIS."
    fi
elif "$BINARY" --replay --frames 1 >/dev/null 2>&1; then
    echo
    echo "Built $BINARY"
else
    echo
    echo "WARNING: $BINARY was built but would not start." >&2
    echo "  Usually libonnxruntime.so is missing. Put it next to the binary:" >&2
    echo "    cp /path/to/libonnxruntime.so target/$OUT_DIR/" >&2
    echo "  or export ORT_DYLIB_PATH and build again, and build.rs will copy it." >&2
    echo "  See BUILD.md." >&2
    exit 1
fi
