#!/usr/bin/env bash
# RUSTC_WORKSPACE_WRAPPER for scripts/build.sh's dist profile: load the OLLVM
# (Pluto) obfuscation plugin ONLY when compiling the kerbside crate itself.
#
# cargo invokes this as:   obf-rustc-wrapper.sh <rustc> <rustc-args...>
#
# Why a wrapper instead of -Zllvm-plugins in RUSTFLAGS: a flag in RUSTFLAGS loads
# the plugin into EVERY crate, including the std crates that -Zbuild-std
# recompiles. Those compile with a working directory inside the rustup std source
# tree, where the plugin cannot find policy.json and prints "Error: conf not
# found" for each of them. cargo calls RUSTC_WORKSPACE_WRAPPER only for workspace
# members -- never for std or the third-party dependencies -- so std and the deps
# stay plugin-free (they are not meant to be obfuscated), and the plugin runs only
# where policy.json is actually reachable: the kerbside crate compiles with the
# repo root as its CWD, which is where policy.json lives.
#
# The extra --crate-name kerbside guard keeps the plugin off any other workspace
# target (e.g. the make_clip helper bin or a build script), so only the shipped
# crate pays the obfuscation.
set -euo pipefail

rustc="$1"
shift

is_kerbside=0
prev=""
for arg in "$@"; do
    case "$arg" in
        --crate-name=kerbside) is_kerbside=1 ;;
    esac
    if [[ "$prev" == "--crate-name" && "$arg" == "kerbside" ]]; then
        is_kerbside=1
    fi
    prev="$arg"
done

if [[ "$is_kerbside" == "1" && -n "${OBF_PLUGIN:-}" ]]; then
    exec "$rustc" "$@" -Zllvm-plugins="$OBF_PLUGIN"
fi
exec "$rustc" "$@"
