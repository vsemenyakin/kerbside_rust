#!/usr/bin/env python3
"""Seal a shipped binary's decode key to its own code (`.text`), in place.

The anti-tamper decode key is ``probe ^ text_hash() ^ SALT2`` (see
``src/crypt.rs`` :mod:`keying`). This tool computes ``text_hash()`` over the final
executable segment and patches the embedded ``SALT2`` placeholder to

    SALT2 = C_REAL ^ text_hash ^ K

so that at run time the key resolves to ``K`` **only** for this exact code on the
real hardware. Patch any instruction byte afterwards (to splice in an argument
logger, a breakpoint, a detour) and ``text_hash()`` moves, the key is wrong, and
every ``encf!``/``enci!`` constant and every ``obfstr!`` string decodes to
garbage -- with no comparison to NOP, because the key *is* a function of the code.

Must run **last** in the build (after .comment removal and scrub_opencv), because
it hashes the final ``.text``. It only writes the 8-byte ``SALT2`` word, which
lives in ``.data`` (not in the hashed segment), so the patch does not disturb the
hash it just computed. Size-preserving.

Usage:  patch_integrity.py <binary>
"""

import struct
import sys

# Must match src/crypt.rs exactly.
C_REAL = 16384
K = 0x9E3779B97F4A7C15
SALT2_SENTINEL = 0xA1B2C3D4E5F60718
MASK64 = 0xFFFFFFFFFFFFFFFF

PT_LOAD = 1
PF_X = 1


def fnv1a(buf: bytes) -> int:
    h = 0xCBF29CE484222325
    prime = 0x100000001B3
    for x in buf:
        h = ((h ^ x) * prime) & MASK64
    return h


def exec_segment(data: bytes):
    """Return (p_offset, p_filesz) of the executable PT_LOAD, matching the range
    keying::text_hash() hashes at run time."""
    if data[:4] != b"\x7fELF" or data[4] != 2:  # ELFCLASS64
        raise SystemExit("patch_integrity: not a 64-bit ELF")
    if data[5] != 1:  # ELFDATA2LSB
        raise SystemExit("patch_integrity: not little-endian ELF")
    e_phoff = struct.unpack_from("<Q", data, 0x20)[0]
    e_phentsize = struct.unpack_from("<H", data, 0x36)[0]
    e_phnum = struct.unpack_from("<H", data, 0x38)[0]
    for i in range(e_phnum):
        base = e_phoff + i * e_phentsize
        p_type, p_flags = struct.unpack_from("<II", data, base)
        p_offset, _p_vaddr, _p_paddr, p_filesz = struct.unpack_from("<QQQQ", data, base + 8)
        if p_type == PT_LOAD and (p_flags & PF_X):
            return p_offset, p_filesz
    raise SystemExit("patch_integrity: no executable PT_LOAD segment found")


def patch(path: str) -> int:
    with open(path, "rb") as f:
        data = bytearray(f.read())

    p_offset, p_filesz = exec_segment(bytes(data))
    text = bytes(data[p_offset:p_offset + p_filesz])
    if len(text) != p_filesz:
        raise SystemExit("patch_integrity: executable segment truncated in file")
    th = fnv1a(text)
    salt2 = (C_REAL ^ th ^ K) & MASK64

    sentinel = struct.pack("<Q", SALT2_SENTINEL)
    hits = []
    start = 0
    while True:
        j = data.find(sentinel, start)
        if j < 0:
            break
        hits.append(j)
        start = j + 8
    if len(hits) == 0:
        print("patch_integrity: SALT2 sentinel not found -- already sealed, or the "
              "anti-tamper keying is not compiled in", file=sys.stderr)
        return 1
    if len(hits) != 1:
        print(f"patch_integrity: sentinel appears {len(hits)} times, expected 1 -- "
              "choose a different SALT2_SENTINEL", file=sys.stderr)
        return 1

    struct.pack_into("<Q", data, hits[0], salt2)
    with open(path, "wb") as f:
        f.write(data)
    print(f"patch_integrity: text_hash={th:#018x} over {p_filesz} bytes at file "
          f"{p_offset:#x}; sealed SALT2={salt2:#018x} at {hits[0]:#x}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: patch_integrity.py <binary>", file=sys.stderr)
        sys.exit(2)
    sys.exit(patch(sys.argv[1]))
