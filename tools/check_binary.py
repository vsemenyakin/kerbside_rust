#!/usr/bin/env python3
"""Static-analysis defensiveness audit of a built binary.

Runs, against a built binary, the checks a reverse engineer would run *statically*
-- across every anti-static-analysis layer this project applies -- and reports
where the binary still leaks. It is deliberately shaped like an attacker's triage
tool: string clusters, float-immediate scans, symbol/RTTI metadata, toolchain
fingerprints, and the build-machine paths the original version of this script
checked.

    python3 tools/check_binary.py target/dist/kerbside --profile dist
    python3 tools/check_binary.py target/release/kerbside --profile release
    python3 tools/check_binary.py target/dist/kerbside --profile dist --strict

Profile-aware. `dist` is held to the full hardened standard, so every category
below FAILs the audit if it finds anything. `dev`/`release` deliberately keep
symbols, panic-site paths and the introspection strings (field names, CLI, the
result schema), so for them those categories are NOTES, not failures -- only the
build-machine paths (which must never appear anywhere) still FAIL.

Exit code 1 if any category FAILs for the given profile; 0 otherwise. `--quiet`
prints only the verdict.
"""

from __future__ import annotations

import argparse
import re
import struct
import sys

# --------------------------------------------------------------------------
# Category 1 (build-machine paths) and 2 (first-party module paths): unchanged
# from the original script -- they are the reason it runs on every build.
# --------------------------------------------------------------------------

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

#: A first-party source path, e.g. `src/detect/detector.rs` (relative -- see the
#: negative lookbehind, which is what distinguishes it from an absolute std/dep
#: path). These panic-site locations hand over this project's module tree.
FIRST_PARTY = re.compile(rb"(?<![\x21-\x7e])(src/[A-Za-z0-9_./-]{1,100}\.rs)\b")

#: Printable-ASCII runs, the same way `strings` finds them.
STRINGS = re.compile(rb"[\x20-\x7e]{6,}")

# --------------------------------------------------------------------------
# Category 3 (/rustc + std paths) and 4 (panic messages).
# --------------------------------------------------------------------------

RUSTC_STD = [
    re.compile(rb"/rustc/[0-9a-f]{6,}"),
    re.compile(rb"library[\\/](?:std|core|alloc)[\\/]src[\\/]"),
]

#: Signature fragments Rust's panic machinery embeds. `-Zbuild-std` with
#: panic_immediate_abort removes them; their presence means a dist build did not.
PANIC_SIGNATURES = [
    b"called `Option::unwrap()` on a `None` value",
    b"called `Result::unwrap()` on an `Err` value",
    b"index out of bounds: the len is",
    b"attempt to add with overflow",
    b"attempt to subtract with overflow",
    b"attempt to multiply with overflow",
    b"attempt to divide by zero",
    b"slice index starts at",
    b"internal error: entered unreachable code",
    b"already borrowed",
]

# --------------------------------------------------------------------------
# Category 7: sensitive string clusters. Each sub-list is one kind of tell.
# In `dist` every one of these must be gone (obfstr-encrypted, feature-gated, or
# scrubbed). In dev/release the domain/schema/CLI ones are present by design.
# --------------------------------------------------------------------------

#: Unambiguously ours (never from libc/std, which live in their own .so): if these
#: appear in plaintext the anti-tamper obfstr! did not take.
CLUSTER_ANTI_TAMPER = [
    b"LD_PRELOAD", b"LD_AUDIT", b"TracerPid", b"/proc/self/status", b"PR_SET_DUMPABLE",
]
#: The purpose of the program -- what the whole project denies a reverse engineer.
CLUSTER_DOMAIN = [
    b"kerbside", b"km/h", b"kph", b"speed_kph", b"violation", b"enforce",
    b"homograph", b"calibrat", b"radar", b"roadside", b"vehicle", b"licence plate",
    b"number plate",
]
#: The result schema, the settings schema, and the CLI -- gated out of dist.
CLUSTER_SCHEMA_CLI = [
    b"frame_id", b"lead_speed", b"n_blobs", b"foreground_ratio", b"FRAME_WIDTH",
    b"IMAGE_POINTS", b"SPEED_LIMIT", b"--replay", b"--frames", b"--realtime",
    b"--dump-settings", b"--overlay",
]
#: OpenCV's own build banner / version -- scrubbed post-build in dist.
CLUSTER_OPENCV_BANNER = [
    b"General configuration for OpenCV", b"To be built:", b"4.10.0",
]

CLUSTERS = [
    ("anti-tamper literals", CLUSTER_ANTI_TAMPER),
    ("domain / purpose", CLUSTER_DOMAIN),
    ("result schema / settings / CLI", CLUSTER_SCHEMA_CLI),
    ("OpenCV build banner / version", CLUSTER_OPENCV_BANNER),
]

# --------------------------------------------------------------------------
# Category 9: the integrity seal (positive check). The sentinel must be GONE in a
# sealed dist binary -- if it survives, patch_integrity.py did not run and the
# binary would itself decode its constants to garbage. Must match
# crates/crypt/src/lib.rs (crypt::SALT2_SENTINEL).
# --------------------------------------------------------------------------
SALT2_SENTINEL = 0xA1B2C3D4E5F60718

# --------------------------------------------------------------------------
# Category 10 (informational): RTTI + OpenCV toolkit residue.
# --------------------------------------------------------------------------
RTTI_TYPEINFO = re.compile(rb"_ZT[SIV][A-Za-z0-9_]{2,}")
RTTI_CV_NAMES = re.compile(rb"N\d+cv[0-9A-Za-z]+E")
CV_FUNC = re.compile(rb"cv::")
OPENCL_KERNEL = re.compile(rb"__kernel |get_global_id|__global ")
CPP_PATH = re.compile(rb"modules/[A-Za-z0-9_/]+\.(?:cpp|hpp)")


# --------------------------------------------------------------------------
# Minimal ELF section reader (for symbols / debug / .comment / data sections).
# --------------------------------------------------------------------------

class Sections:
    """name -> (sh_type, sh_offset, sh_size). Empty if not a parseable ELF64."""

    def __init__(self, data: bytes):
        self.by_name: dict[str, tuple[int, int, int]] = {}
        self.ok = False
        if data[:4] != b"\x7fELF" or len(data) < 0x40 or data[4] != 2:
            return
        try:
            e_shoff = struct.unpack_from("<Q", data, 0x28)[0]
            e_shentsize = struct.unpack_from("<H", data, 0x3A)[0]
            e_shnum = struct.unpack_from("<H", data, 0x3C)[0]
            e_shstrndx = struct.unpack_from("<H", data, 0x3E)[0]
            if e_shoff == 0 or e_shnum == 0:
                return
            # section-header string table
            strtab_hdr = e_shoff + e_shstrndx * e_shentsize
            str_off = struct.unpack_from("<Q", data, strtab_hdr + 24)[0]
            str_size = struct.unpack_from("<Q", data, strtab_hdr + 32)[0]
            strtab = data[str_off:str_off + str_size]
            for i in range(e_shnum):
                base = e_shoff + i * e_shentsize
                sh_name, sh_type = struct.unpack_from("<II", data, base)
                sh_offset = struct.unpack_from("<Q", data, base + 24)[0]
                sh_size = struct.unpack_from("<Q", data, base + 32)[0]
                end = strtab.find(b"\x00", sh_name)
                name = strtab[sh_name:end].decode("ascii", "replace")
                self.by_name[name] = (sh_type, sh_offset, sh_size)
            self.ok = True
        except Exception:
            self.by_name = {}
            self.ok = False


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------

def containing_string(data: bytes, off: int, maxlen: int = 120) -> str:
    start = data.rfind(b"\x00", 0, off) + 1
    end = data.find(b"\x00", off)
    if end < 0:
        end = off + 40
    return data[start:end][:maxlen].decode("ascii", "replace")


def _sig_digits(x: float) -> int:
    s = repr(abs(x))
    s = re.split("[eE]", s)[0].replace(".", "").strip("0")
    return len(s) if s else 1


def round_float_candidates(data: bytes, sections: Sections):
    """8-aligned f64 in the data sections that look like intentional constants:
    finite, human-magnitude, few significant digits. Heuristic and noisy
    (library constants count too) -- reported, never failed on."""
    ranges = []
    if sections.ok:
        for name in (".rodata", ".data.rel.ro", ".data"):
            if name in sections.by_name:
                _t, off, size = sections.by_name[name]
                ranges.append((off, size))
    if not ranges:
        ranges = [(0, len(data))]
    seen: dict[float, int] = {}
    for off, size in ranges:
        end = off + size - 8
        pos = off + (-off % 8)  # 8-aligned start
        while pos <= end:
            (x,) = struct.unpack_from("<d", data, pos)
            pos += 8
            if x != x or x in (float("inf"), float("-inf")):
                continue
            ax = abs(x)
            if ax == 0.0 or ax < 1e-6 or ax > 1e9:
                continue
            if _sig_digits(x) <= 6:
                seen[x] = seen.get(x, 0) + 1
    return seen


def count_matches(data: bytes, patterns) -> list[str]:
    out: list[str] = []
    for pat in patterns:
        for m in pat.findall(data):
            out.append(m.decode("ascii", "replace") if isinstance(m, bytes) else str(m))
    return out


def find_terms(data: bytes, terms) -> list[tuple[str, str]]:
    hits = []
    for term in terms:
        i = data.find(term)
        if i >= 0:
            hits.append((term.decode("ascii", "replace"), containing_string(data, i)))
    return hits


# --------------------------------------------------------------------------
# Reporting
# --------------------------------------------------------------------------

class Report:
    def __init__(self, quiet: bool):
        self.quiet = quiet
        self.failed = False

    def category(self, verdict: str, title: str, count, samples=None, extra: str = ""):
        if verdict == "FAIL":
            self.failed = True
        mark = {"PASS": "PASS", "FAIL": "FAIL", "NOTE": "note", "INFO": "info"}[verdict]
        n = "none" if not count else f"{count} hit(s)"
        line = f"  [{mark:4}] {title:<34} {n}"
        if extra:
            line += f"   {extra}"
        print(line)
        if samples and not self.quiet and verdict in ("FAIL", "NOTE", "INFO"):
            for s in samples[:6]:
                print(f"            - {s[:120]}")


def report_residual(data: bytes) -> None:
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
    print(f"\nresidual source paths: {sum(residual.values())}")
    for origin, count in sorted(residual.items(), key=lambda kv: -kv[1]):
        print(f"  {count:>4}  {origin}")


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binary")
    parser.add_argument("--profile", choices=["dev", "debug", "release", "dist"],
                        help="hold the binary to this profile's standard; only "
                             "'dist' is audited strictly")
    parser.add_argument("--quiet", action="store_true", help="only report the verdict")
    parser.add_argument("--strict", action="store_true",
                        help="also count every remaining source path, whoever it owns")
    parser.add_argument("--allow-first-party", action="store_true",
                        help="downgrade first-party src/*.rs paths to a note (dev/release)")
    args = parser.parse_args()

    try:
        with open(args.binary, "rb") as fh:
            data = fh.read()
    except OSError as e:
        print(f"cannot read {args.binary}: {e}", file=sys.stderr)
        return 2

    strict = args.profile == "dist"
    sections = Sections(data)
    rep = Report(args.quiet)

    print(f"{args.binary}: {len(data):,} bytes   profile={args.profile or 'unset'}"
          f"   {'STRICT (dist)' if strict else 'lenient'}")
    print("static-analysis audit:")

    # 1. build-machine paths -- always FAIL.
    machine = []
    for pat, desc in LEAKS:
        for m in pat.findall(data):
            machine.append(f"[{desc}] {m.decode('ascii', 'replace')}")
    machine = sorted(set(machine))
    rep.category("FAIL" if machine else "PASS", "build-machine paths", len(machine), machine)

    # 2. first-party module paths -- FAIL for dist (or when not allowed).
    fp = sorted({m.decode("ascii", "replace") for m in FIRST_PARTY.findall(data)})
    fp_fail = bool(fp) and (strict or not args.allow_first_party)
    rep.category("FAIL" if fp_fail else ("NOTE" if fp else "PASS"),
                 "first-party module paths", len(fp), fp)

    # 3. /rustc + std paths -- FAIL for dist.
    rustc = sorted(set(count_matches(data, RUSTC_STD)))
    rep.category("FAIL" if (rustc and strict) else ("NOTE" if rustc else "PASS"),
                 "std / rustc paths", len(rustc), rustc)

    # 4. panic messages -- FAIL for dist.
    panics = find_terms(data, PANIC_SIGNATURES)
    rep.category("FAIL" if (panics and strict) else ("NOTE" if panics else "PASS"),
                 "panic messages", len(panics), [f"{t}" for t, _ in panics])

    # 5. symbol table + debug sections -- FAIL for dist.
    sym_bits = []
    if sections.ok:
        st = sections.by_name.get(".symtab")
        if st and st[2] > 0:
            sym_bits.append(f".symtab present ({st[2]:,} bytes)")
        dbg = [n for n in sections.by_name if n.startswith(".debug")]
        if dbg:
            sym_bits.append(f"debug sections: {', '.join(dbg)}")
    else:
        sym_bits.append("(section headers unreadable -- cannot check)")
    real = [s for s in sym_bits if not s.startswith("(")]
    rep.category("FAIL" if (real and strict) else ("NOTE" if real else "PASS"),
                 "symbol table / debug info", len(real), sym_bits)

    # 6. toolchain fingerprint -- FAIL for dist.
    fp_bits = []
    if sections.ok and ".comment" in sections.by_name and sections.by_name[".comment"][2] > 0:
        fp_bits.append(".comment section present")
    for term in (b"rustc version", b"GCC: ("):
        if data.find(term) >= 0:
            fp_bits.append(containing_string(data, data.find(term)))
    rep.category("FAIL" if (fp_bits and strict) else ("NOTE" if fp_bits else "PASS"),
                 "toolchain fingerprint", len(fp_bits), fp_bits)

    # 7. sensitive string clusters -- FAIL for dist.
    for label, terms in CLUSTERS:
        hits = find_terms(data, terms)
        samples = [f"{t}   in: {ctx}" for t, ctx in hits]
        rep.category("FAIL" if (hits and strict) else ("NOTE" if hits else "PASS"),
                     f"cluster: {label}", len(hits), samples)

    # 8. round float-immediates -- INFO always (library constants inflate it).
    floats = round_float_candidates(data, sections)
    top = sorted(floats.items(), key=lambda kv: (_sig_digits(kv[0]), -kv[1]))
    fsamples = [f"{x!r}  (x{c})" for x, c in top]
    rep.category("INFO", "round float-immediates", len(floats), fsamples,
                 extra="(heuristic; incl. library constants)")

    # 9. integrity seal (dist only) -- sentinel must be gone.
    if strict:
        sentinel = struct.pack("<Q", SALT2_SENTINEL)
        if data.find(sentinel) >= 0:
            rep.category("FAIL", "integrity seal (SALT2)", 1,
                         ["SALT2 sentinel still present -- patch_integrity.py did not run"])
        else:
            rep.category("PASS", "integrity seal (SALT2)", 0)

    # 10. RTTI / OpenCV toolkit residue -- INFO always (known residual).
    residue = {
        "RTTI typeinfo (_ZTS/TI/TV)": len(set(count_matches(data, [RTTI_TYPEINFO]))),
        "RTTI cv names (N..cv..E)": len(set(count_matches(data, [RTTI_CV_NAMES]))),
        "cv:: strings": len(CV_FUNC.findall(data)),
        "OpenCL kernel source": len(OPENCL_KERNEL.findall(data)),
        "OpenCV .cpp/.hpp paths": len(set(count_matches(data, [CPP_PATH]))),
    }
    total = sum(residue.values())
    rep.category("INFO", "RTTI / OpenCV residue", total,
                 [f"{k}: {v}" for k, v in residue.items() if v])

    if args.strict:
        report_residual(data)

    print()
    if rep.failed:
        print(f"AUDIT FAILED for profile '{args.profile or 'unset'}': "
              f"one or more categories leaked.")
        return 1
    print(f"AUDIT PASSED for profile '{args.profile or 'unset'}'.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
