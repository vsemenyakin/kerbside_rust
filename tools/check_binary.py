#!/usr/bin/env python3
"""Check a built binary for absolute paths from the build machine.

    python3 tools/check_binary.py target/dist/kerbside
    python3 tools/check_binary.py target/dist/kerbside --strict

Exits non-zero if it finds any. Run it before shipping, and after any change to
how the binary is built.

Why this is needed at all
-------------------------
Rust embeds the source file name of every panic site -- `unwrap`, `expect`,
slice indexing, integer overflow -- so that a panic can say where it happened.
For code in a dependency, that file name is the *absolute* path into the build
machine's cargo registry, and it ends up in `.rodata` whether or not the panic
ever fires.

An untreated release build of this project carried around ninety strings naming
the developer's home directory. That is an information leak in a binary meant to
be installed on a device someone else may hold, and it is also the cheapest
possible win for a reverse engineer: the paths spell out the module tree, which
is target T2 of the assessment in docs/re_scoring.md.

Two things address it, and this script checks both:

* `--remap-path-prefix`, set by the environment scripts, rewrites the paths to
  anonymous forms. That is what the default check below tests for, and it is
  the one that matters -- it is what removes your name from the artefact.
* `-Zlocation-detail=none`, used by the `dist` profile on nightly, removes the
  panic-site paths outright rather than rewriting them. `--strict` reports what
  survives that, which is the standard library's own paths and nothing else.

A build-time flag that silently stops being passed is exactly the kind of thing
nobody notices, which is why this runs on every build rather than on request.
"""

from __future__ import annotations

import argparse
import re
import sys

#: Patterns that indicate a path from the machine that did the build.
LEAKS = [
    (re.compile(rb"[A-Za-z]:[\\/](?:Users|Documents and Settings)[\\/][^\x00-\x1f\"<>|]{1,120}"),
     "a Windows user profile path"),
    (re.compile(rb"/(?:home|Users)/[A-Za-z0-9._-]{1,32}/[^\x00-\x1f\"]{1,120}"),
     "a Unix home directory path"),
    (re.compile(rb"[A-Za-z]:[\\/][^\x00-\x1f\"<>|]{0,60}\.cargo[\\/]registry"),
     "a cargo registry path"),
    (re.compile(rb"/[^\x00-\x1f\"]{0,60}\.cargo/registry"),
     "a cargo registry path"),
    (re.compile(rb"[A-Za-z]:[\\/][^\x00-\x1f\"<>|]{0,60}\.rustup[\\/]"),
     "a rustup toolchain path"),
    (re.compile(rb"/[^\x00-\x1f\"]{0,60}\.rustup/"),
     "a rustup toolchain path"),
]

#: A first-party source path, e.g. `src/detect/detector.rs`.
#:
#: These are panic-site locations in *this project's* own code. They are a leak
#: in their own right -- they hand a reverse engineer the module tree (target T2
#: of docs/re_scoring.md) -- and, unlike the absolute paths above, they name no
#: home directory, so the LEAKS checks never saw them and the binary read as
#: "clean" while advertising its own layout.
#:
#: They are stored *relative* (`src/...`, no leading slash) because cargo
#: compiles the local crate by its relative path, whereas std
#: (`/rustc/.../library/std/src/...`) and dependencies (`/cargo/registry/src/...`)
#: are always absolute. The negative lookbehind is what tells them apart: it
#: fires only when the `src` is *not* preceded by another printable character,
#: which for an absolute path is always the separator before it.
FIRST_PARTY = re.compile(rb"(?<![\x21-\x7e])(src/[A-Za-z0-9_./-]{1,100}\.rs)\b")

#: Printable-ASCII runs, the same way `strings` finds them.
STRINGS = re.compile(rb"[\x20-\x7e]{6,}")


def report_residual(data: bytes) -> None:
    """Count the source paths that survive, and say where each kind comes from.

    Paths from the precompiled standard library are not leaks in the sense the
    checks above test for: they name no person, machine or module of this
    project, and the Rust project has already rewritten them to
    `/rustc/<commit>/...`. Nothing short of rebuilding std removes them.

    Counted anyway, because "what is still in the binary" is a question the
    reverse-engineering assessment has to answer with a number rather than an
    impression.
    """
    residual: dict[str, int] = {}
    for match in STRINGS.findall(data):
        value = match.decode("ascii", "replace")
        if not re.search(r"\.rs\b", value):
            continue
        if "/cargo" in value:
            origin = "a dependency compiled here"
        elif "/kerbside" in value:
            origin = "this project"
        elif re.search(r"/rustc/|/rust/deps|library[\\/]", value):
            origin = "the precompiled standard library"
        else:
            origin = "unclassified"
        residual[origin] = residual.get(origin, 0) + 1

    print(f"residual source paths: {sum(residual.values())}")
    for origin, count in sorted(residual.items(), key=lambda kv: -kv[1]):
        print(f"  {count:>4}  {origin}")
    removable = residual.get("a dependency compiled here", 0) + residual.get("this project", 0)
    if removable:
        print("  -- those two are removable: build the dist profile, which uses")
        print("     nightly and -Zlocation-detail=none.")
        print("         scripts/build.sh          (or scripts\\build.cmd)")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    parser.add_argument(
        "--quiet", action="store_true", help="only report the verdict"
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help="also count every remaining source path, whoever it belongs to",
    )
    parser.add_argument(
        "--allow-first-party",
        action="store_true",
        help="downgrade first-party src/*.rs paths to a note rather than a "
        "failure. dev and release builds do not harden these away -- only the "
        "dist profile does -- so the build script passes this for them. A dist "
        "binary must never need it.",
    )
    args = parser.parse_args()

    try:
        with open(args.binary, "rb") as fh:
            data = fh.read()
    except OSError as e:
        print(f"cannot read {args.binary}: {e}", file=sys.stderr)
        return 2

    findings: list[tuple[str, str]] = []
    for pattern, description in LEAKS:
        for match in pattern.findall(data):
            findings.append((description, match.decode("ascii", "replace")))

    # De-duplicate while keeping the first sighting of each string.
    seen: set[str] = set()
    unique = []
    for description, value in findings:
        if value not in seen:
            seen.add(value)
            unique.append((description, value))

    # First-party module paths, deduplicated and sorted for a stable report.
    first_party = sorted(
        {m.decode("ascii", "replace") for m in FIRST_PARTY.findall(data)}
    )

    print(f"{args.binary}: {len(data):,} bytes")
    if args.strict:
        report_residual(data)

    absolute_leaked = bool(unique)
    first_party_leaked = bool(first_party) and not args.allow_first_party

    if unique:
        print(f"LEAKED: {len(unique)} distinct build-machine path(s)\n")
        if not args.quiet:
            for description, value in unique[:20]:
                print(f"  [{description}]")
                print(f"    {value[:140]}")
            if len(unique) > 20:
                print(f"  ... and {len(unique) - 20} more")
        print(
            "\nBuild through one of the build scripts, which set the\n"
            "--remap-path-prefix flags that strip these:\n"
            "    scripts/build.sh                 # Linux / Raspberry Pi OS\n"
            "    scripts\\build.cmd                # Windows, cmd.exe\n"
            "    .\\scripts\\build.ps1              # Windows, PowerShell\n"
            "Changing RUSTFLAGS forces a full rebuild; that is expected."
        )

    if first_party:
        label = "note" if args.allow_first_party else "LEAKED"
        print(
            f"\n{label}: {len(first_party)} first-party source path(s) "
            f"naming this project's module tree"
        )
        if not args.quiet:
            for value in first_party[:20]:
                print(f"    {value[:140]}")
            if len(first_party) > 20:
                print(f"  ... and {len(first_party) - 20} more")
        if not args.allow_first_party:
            print(
                "\nThese are panic-site locations in first-party code, and only\n"
                "the dist profile removes them (nightly: -Zlocation-detail=none\n"
                "and -Zbuild-std with panic_immediate_abort):\n"
                "    scripts/build.sh\n"
                "dev and release keep them by design; pass --allow-first-party\n"
                "to check those without failing."
            )

    if absolute_leaked or first_party_leaked:
        return 1
    if first_party:
        print(
            f"\nok -- no build-machine paths; {len(first_party)} first-party "
            f"path(s) present but allowed"
        )
    else:
        print("clean -- no build-machine paths found")
    return 0


if __name__ == "__main__":
    sys.exit(main())
