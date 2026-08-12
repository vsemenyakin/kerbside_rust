# Build environment for Linux and Raspberry Pi OS.
#
#     source scripts/env-linux.sh
#     cargo build --release
#
# OpenCV comes from `libopencv-dev` and is found by pkg-config, so unlike the
# Windows scripts this sets no OPENCV_* variables. It exists for two other jobs.
#
# 1. Strip build-machine paths out of the binary.
#
#    Rust embeds the source file name of every panic site, and for a dependency
#    that is the absolute path into this machine's cargo registry. An untreated
#    build ships around ninety strings naming your home directory. They are an
#    information leak, and they hand a reverse engineer the module tree for
#    free -- target T2 of docs/re_scoring.md.
#
#    `--remap-path-prefix` rewrites them to anonymous, stable forms. Verify with
#    `python3 tools/check_binary.py target/release/kerbside`.
#
#    Note that changing RUSTFLAGS invalidates the build cache, so the first
#    build after sourcing this rebuilds every dependency. That is expected, and
#    it is why this is a deliberate step rather than something buried in
#    .cargo/config.toml -- which could not be written portably anyway, since the
#    paths differ per machine.
#
# 2. Point at the ONNX Runtime, if it is somewhere findable.

: "${CARGO_HOME:=$HOME/.cargo}"
: "${RUSTUP_HOME:=$HOME/.rustup}"

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"

# Order matters: rustc applies the first matching prefix, so the most specific
# has to come first.
remap="--remap-path-prefix=$CARGO_HOME=/cargo"
remap="$remap --remap-path-prefix=$RUSTUP_HOME=/rustup"
remap="$remap --remap-path-prefix=$project_dir=/kerbside"

case "${RUSTFLAGS:-}" in
    *remap-path-prefix*) : ;;                       # already set; leave it alone
    "")  export RUSTFLAGS="$remap" ;;
    *)   export RUSTFLAGS="$RUSTFLAGS $remap" ;;
esac

# ONNX Runtime is opened at run time. If it is already next to the binary --
# build.rs puts it there when ORT_DYLIB_PATH was set at build time -- nothing
# needs doing. Otherwise try the usual places.
if [ -z "${ORT_DYLIB_PATH:-}" ]; then
    for candidate in \
        "$project_dir/target/release/libonnxruntime.so" \
        "$project_dir/onnxruntime-linux-aarch64-1.26.0/lib/libonnxruntime.so" \
        "/usr/local/lib/libonnxruntime.so"
    do
        if [ -f "$candidate" ]; then
            export ORT_DYLIB_PATH="$candidate"
            break
        fi
    done
fi

echo "rustflags: $RUSTFLAGS"
echo "onnxrt   : ${ORT_DYLIB_PATH:-<not set -- see BUILD.md>}"
if ! pkg-config --exists opencv4 2>/dev/null; then
    echo "warning  : pkg-config cannot find opencv4 -- install libopencv-dev" >&2
fi
