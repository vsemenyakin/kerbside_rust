#!/usr/bin/env bash
# Build on Linux / Raspberry Pi OS, and prove the result carries no leaks.
#
#     scripts/build.sh              dist profile -- what you ship (OBFUSCATED)
#     scripts/build.sh release      for benchmarking; tools/bench.sh looks here
#     scripts/build.sh dev          debug build, for development only
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

PROFILE="${1:-dist}"

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

# Cargo's profile names and its output directories do not match for the one
# built-in debug profile: `--profile dev` writes to target/debug. Everything
# else, including custom profiles like `dist`, uses its own name.
case "$PROFILE" in
    dev|debug) PROFILE="dev"; OUT_DIR="debug" ;;
    *)         OUT_DIR="$PROFILE" ;;
esac

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
if [[ "$PROFILE" == "dist" ]]; then
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
    export OBF_PLUGIN
    export RUSTC_WORKSPACE_WRAPPER="$PWD/scripts/obf-rustc-wrapper.sh"

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
    echo "toolchain: ${OBF_TOOLCHAIN}  (LLVM matched to plugin)"
    echo "  -Zlocation-detail=none                    (drop panic-site source paths)"
    echo "  -Zbuild-std + panic_immediate_abort       (drop std paths and panic strings)"
    echo "  --no-default-features                     (drop settings field-name strings + overlay)"
    echo "  static OpenCV core/imgproc/video          (cv:: calls not LD_PRELOAD-interposable)"
    echo "  -Zllvm-plugins=$(basename "$OBF_PLUGIN")   (OLLVM obfuscation via workspace wrapper, kerbside crate only, policy: $OBF_POLICY)"
    echo "  --target $HOST_TRIPLE"
fi

echo
echo "== building profile '$PROFILE' -> target/$OUT_DIR =="
if [[ "$PROFILE" == "dist" ]]; then
    # `cargo build` (not `cargo rustc`): the plugin is already in RUSTFLAGS, and
    # `cargo rustc` + `-Zbuild-std` self-deadlocks on the target lock. Obfuscation
    # is scoped to this crate by policy.json, not by which crate cargo passes args to.
    if ! cargo "${CARGO_ARGS[@]}" build --profile dist "${BUILD_STD_ARGS[@]}" --bin kerbside; then
        echo; echo "BUILD FAILED" >&2; exit 1
    fi
else
    if ! cargo "${CARGO_ARGS[@]}" build --profile "$PROFILE" "${BUILD_STD_ARGS[@]}"; then
        echo; echo "BUILD FAILED" >&2; exit 1
    fi
fi

# build-std forces --target, which nests the output under target/<triple>/.
# Re-point the documented target/dist/ path at it so every downstream reference
# (check_binary below, BUILD.md, BINARY=target/dist/kerbside for bench.sh) keeps
# working regardless of the host triple.
if [[ "$PROFILE" == "dist" ]]; then
    if [[ -d "target/dist" && ! -L "target/dist" ]]; then
        rm -rf "target/dist"   # stale real directory from a pre-build-std build
    fi
    ln -sfn "$HOST_TRIPLE/dist" "target/dist"
fi

BINARY="target/$OUT_DIR/kerbside"

# --- the leaked-path check -----------------------------------------------
# dev and release deliberately do not harden away the first-party module tree;
# only dist does. So allow first-party paths for those (the check still fails on
# any absolute build-machine path), and demand them gone for dist.
CHECK_ARGS=()
if [[ "$PROFILE" != "dist" ]]; then
    CHECK_ARGS+=("--allow-first-party")
fi
echo
PYTHON="$(command -v python3 || command -v python || true)"
if [[ -z "$PYTHON" ]]; then
    echo "python3 not found -- skipping the leaked-path check." >&2
    echo "Run it wherever you do have python:" >&2
    echo "  python3 tools/check_binary.py $BINARY" >&2
else
    "$PYTHON" tools/check_binary.py "$BINARY" "${CHECK_ARGS[@]}"
fi

# --- will it actually start? ---------------------------------------------
# A build can succeed and still produce something that dies in the loader --
# most often because libonnxruntime.so is neither beside the binary nor on the
# library search path. Better to find that out here than in the middle of a
# benchmark run.
echo
if "$BINARY" --version; then
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
