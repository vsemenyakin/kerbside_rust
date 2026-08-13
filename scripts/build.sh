#!/usr/bin/env bash
# Build on Linux / Raspberry Pi OS, and prove the result carries no leaks.
#
#     scripts/build.sh              dist profile -- what you ship
#     scripts/build.sh release      for benchmarking; tools/bench.sh looks here
#     scripts/build.sh dev          debug build, for development only
#
# The Linux counterpart of scripts/build.cmd. It sets the environment, builds,
# and then checks the binary for absolute paths from this machine -- because a
# `--remap-path-prefix` flag that silently stops being passed is exactly the
# kind of regression nobody notices until the artefact is already distributed.
#
# Nothing here is required: `source scripts/env-linux.sh` followed by
# `cargo build --profile dist` does the same thing. This exists so it is one
# command and the check cannot be forgotten.

set -euo pipefail

cd "$(dirname "$0")/.."

PROFILE="${1:-dist}"

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
# The dist profile builds with nightly for two things that stable cannot do,
# and both are the difference between anonymising a leak and removing it:
#
#   * -Zlocation-detail=none              drops the source file+line of every
#                                         panic site (src/detect/detector.rs and
#                                         friends). --remap-path-prefix can only
#                                         rewrite those, not delete them.
#   * -Zbuild-std + panic_immediate_abort rebuilds std without its panic
#                                         machinery, which is where the residual
#                                         /rustc/... paths and the panic *message
#                                         strings* come from. Needs the rust-src
#                                         component and forces --target.
#
# There is no stable fallback for either, so when nightly (or rust-src) is
# missing we REFUSE rather than quietly shipping the leaky binary. Build the
# un-hardened release profile deliberately if that is what you want.
#
# Other profiles use whatever toolchain is default, because they are not what
# gets shipped. dist's codegen now diverges from release (panic = "abort"), so
# if you ship dist, benchmark dist:
#     BINARY=target/dist/kerbside tools/bench.sh
CARGO_ARGS=()
BUILD_STD_ARGS=()
HOST_TRIPLE=""
if [[ "$PROFILE" == "dist" ]]; then
    if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
        echo "REFUSING TO BUILD: the dist profile requires the nightly toolchain." >&2
        echo "  It removes the first-party source paths (src/detect/detector.rs and" >&2
        echo "  friends) and the panic message strings; stable can only anonymise" >&2
        echo "  them. Install it:" >&2
        echo "    rustup toolchain install nightly --profile minimal" >&2
        echo "    rustup component add rust-src --toolchain nightly" >&2
        echo "  Or build the un-hardened profile for development or benchmarking:" >&2
        echo "    scripts/build.sh release" >&2
        exit 1
    fi
    if ! rustup component list --toolchain nightly 2>/dev/null \
            | grep -q '^rust-src .*(installed)'; then
        echo "REFUSING TO BUILD: -Zbuild-std needs the rust-src component on nightly." >&2
        echo "    rustup component add rust-src --toolchain nightly" >&2
        exit 1
    fi
    # build-std must know the concrete target; there is no host default for it.
    HOST_TRIPLE="$(rustc +nightly -vV | awk '/^host: /{print $2}')"
    if [[ -z "$HOST_TRIPLE" ]]; then
        echo "REFUSING TO BUILD: could not determine the host target triple from" >&2
        echo "  'rustc +nightly -vV'." >&2
        exit 1
    fi
    CARGO_ARGS+=("+nightly")
    # -Cpanic=immediate-abort is the current spelling (nightly 1.99): the old
    # -Zbuild-std-features=panic_immediate_abort was promoted to a real panic
    # strategy. It needs -Zunstable-options, and it needs core rebuilt, which is
    # what -Zbuild-std does. Set here rather than in Cargo.toml because the
    # manifest opt-in (cargo-features = ["panic-immediate-abort"]) breaks stable
    # cargo -- and hence every `cargo test --release`. It overrides the dist
    # profile's stable-safe panic = "abort".
    export RUSTFLAGS="$RUSTFLAGS -Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort"
    BUILD_STD_ARGS+=(
        "--target" "$HOST_TRIPLE"
        "-Zbuild-std=std,panic_abort"
    )
    echo "toolchain: nightly"
    echo "  -Zlocation-detail=none               (drop panic-site source paths)"
    echo "  -Cpanic=immediate-abort + build-std  (drop std paths and panic strings)"
    echo "  --target $HOST_TRIPLE"
fi

echo
echo "== building profile '$PROFILE' -> target/$OUT_DIR =="
if ! cargo "${CARGO_ARGS[@]}" build --profile "$PROFILE" "${BUILD_STD_ARGS[@]}"; then
    echo
    echo "BUILD FAILED" >&2
    exit 1
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
