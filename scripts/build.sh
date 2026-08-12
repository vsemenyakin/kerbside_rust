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
# The dist profile builds with nightly, purely for one flag:
# -Zlocation-detail=none. Rust embeds the source file and line of every panic
# site, and nothing on stable removes them -- --remap-path-prefix can only
# rewrite them to something anonymous. That flag makes them empty instead.
#
# Panics still report their message; they just no longer say which line raised
# them.
#
# Other profiles use whatever toolchain is default, because they are not what
# gets shipped. If you ship dist, benchmark dist:
#     BINARY=target/dist/kerbside tools/bench.sh
CARGO_ARGS=()
if [[ "$PROFILE" == "dist" ]]; then
    if rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
        CARGO_ARGS+=("+nightly")
        export RUSTFLAGS="$RUSTFLAGS -Zlocation-detail=none"
        echo "toolchain: nightly (-Zlocation-detail=none)"
    else
        echo "WARNING: nightly is not installed, so panic-site source paths will" >&2
        echo "  remain in the binary (anonymised, but present). Install it:" >&2
        echo "    rustup toolchain install nightly --profile minimal" >&2
    fi
fi

echo
echo "== building profile '$PROFILE' -> target/$OUT_DIR =="
if ! cargo "${CARGO_ARGS[@]}" build --profile "$PROFILE"; then
    echo
    echo "BUILD FAILED" >&2
    exit 1
fi

BINARY="target/$OUT_DIR/kerbside"

# --- the leaked-path check -----------------------------------------------
echo
PYTHON="$(command -v python3 || command -v python || true)"
if [[ -z "$PYTHON" ]]; then
    echo "python3 not found -- skipping the leaked-path check." >&2
    echo "Run it wherever you do have python:" >&2
    echo "  python3 tools/check_binary.py $BINARY" >&2
else
    "$PYTHON" tools/check_binary.py "$BINARY"
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
