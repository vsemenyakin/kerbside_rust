# Build environment for Windows, for Git Bash / MSYS shells.
#
#     source scripts/env-windows.sh
#     cargo build --release
#
# The PowerShell equivalent is scripts/env-windows.ps1; see BUILD.md for what
# each variable is for. On Raspberry Pi OS none of this is needed -- pkg-config
# finds OpenCV and the ONNX Runtime shared object is installed system-wide.

: "${LLVM_HOME:=/c/Program Files/LLVM}"
: "${OPENCV_HOME:=/c/opencv/opencv/build}"
: "${OPENCV_MSVC:=vc16}"

if [ ! -f "$LLVM_HOME/bin/libclang.dll" ]; then
    echo "libclang.dll not found under $LLVM_HOME -- install LLVM or set LLVM_HOME" >&2
    return 1 2>/dev/null || exit 1
fi
if [ ! -d "$OPENCV_HOME/include/opencv2" ]; then
    echo "OpenCV headers not found under $OPENCV_HOME -- see BUILD.md or set OPENCV_HOME" >&2
    return 1 2>/dev/null || exit 1
fi

lib_dir="$OPENCV_HOME/x64/$OPENCV_MSVC/lib"
bin_dir="$OPENCV_HOME/x64/$OPENCV_MSVC/bin"

# opencv_world4130.lib is 4.13.0; derive it so a different prebuilt still works.
world="$(ls "$lib_dir" 2>/dev/null | grep -E '^opencv_world[0-9]+\.lib$' | head -1)"
if [ -z "$world" ]; then
    echo "no opencv_world*.lib under $lib_dir (check OPENCV_MSVC='$OPENCV_MSVC')" >&2
    return 1 2>/dev/null || exit 1
fi

# Windows-style paths: the crates' build scripts pass these to MSVC tooling.
export LIBCLANG_PATH="$(cygpath -w "$LLVM_HOME/bin")"
export OPENCV_LINK_LIBS="${world%.lib}"
export OPENCV_LINK_PATHS="$(cygpath -w "$lib_dir")"
export OPENCV_INCLUDE_PATHS="$(cygpath -w "$OPENCV_HOME/include")"
export PATH="$LLVM_HOME/bin:$bin_dir:$PATH"

# ONNX Runtime is opened at run time. Point at any 1.26-compatible build; the
# one inside the Python project's virtualenv is a good default, because then
# both implementations run the identical inference library and a CSV difference
# cannot be blamed on the runtime.
if [ -z "$ORT_DYLIB_PATH" ]; then
    for candidate in \
        "$(dirname "$PWD")/kerbside/venv/Lib/site-packages/onnxruntime/capi/onnxruntime.dll" \
        "$PWD/onnxruntime.dll"
    do
        if [ -f "$candidate" ]; then
            export ORT_DYLIB_PATH="$(cygpath -w "$candidate")"
            break
        fi
    done
fi

echo "libclang : $LIBCLANG_PATH"
echo "opencv   : $OPENCV_LINK_LIBS in $OPENCV_LINK_PATHS"
echo "onnxrt   : ${ORT_DYLIB_PATH:-<not set -- see BUILD.md>}"
