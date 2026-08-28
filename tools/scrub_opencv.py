#!/usr/bin/env python3
"""Scrub OpenCV's identifying strings from a shipped binary, in place.

Two OpenCV tells survive `strip` because they are ordinary `.rodata` data, not
symbols:

1. The ``getBuildInformation()`` banner -- one big embedded string ("General
   configuration for OpenCV X.Y.Z ...": version, platform, CPU features, the full
   C/C++ compiler command lines, the module list, FFMPEG/GStreamer flags).
   kerbside never calls it, so it is dead data.

2. The exact OpenCV version ("X.Y.Z"), which also appears in a couple of plugin
   version-mismatch error messages and in the bare ``getVersionString()`` value.
   In this static, plugin-free build those paths are display-only -- verified by
   blanking them and re-running the oracle (the result digest is unchanged).

Both are neutralised **in place, preserving file size** (so every ELF offset --
and the result oracle -- stays valid): the version digits are masked (digits ->
'0', punctuation kept, same length, no NUL inserted so format strings stay
intact) and the banner is overwritten with NUL. Version-agnostic: the version is
read from the banner, not hard-coded. Idempotent: once the banner is gone, a
second run is a no-op.

Usage:  scrub_opencv.py <binary>
"""

import re
import sys

BANNER = b"General configuration for OpenCV"


def _mask_version(tok: bytes) -> bytes:
    """Same-length mask: each digit -> '0', punctuation kept ('4.10.0' -> '0.00.0')."""
    return bytes(ord("0") if 0x30 <= b <= 0x39 else b for b in tok)


def scrub(path: str) -> int:
    with open(path, "rb") as f:
        data = bytearray(f.read())
    size = len(data)

    i = data.find(BANNER)
    if i < 0:
        print(f"scrub_opencv: no OpenCV banner in {path} (already clean)")
        return 0

    # 1. Read the version token that follows the banner marker, e.g. b"4.10.0".
    m = re.match(rb"\s+(\d+(?:\.\d+)+)", data[i + len(BANNER):])
    if m:
        version = m.group(1)
        masked = _mask_version(version)
        hits = 0
        start = 0
        while True:
            j = data.find(version, start)
            if j < 0:
                break
            data[j:j + len(version)] = masked
            start = j + len(version)
            hits += 1
        print(f"scrub_opencv: masked OpenCV version {version.decode()!r} "
              f"-> {masked.decode()!r} at {hits} site(s)")
    else:
        print("scrub_opencv: could not read version from banner; leaving it", file=sys.stderr)

    # 2. Blank the whole banner string (start-of-string .. terminating NUL).
    s = data.rfind(b"\x00", 0, i) + 1
    e = data.find(b"\x00", i)
    if e < 0:
        print(f"scrub_opencv: banner at {hex(i)} is not NUL-terminated?", file=sys.stderr)
        return 1
    data[s:e] = b"\x00" * (e - s)
    print(f"scrub_opencv: blanked {e - s} bytes of getBuildInformation banner at {hex(s)}")

    assert len(data) == size, "scrub must preserve file size"
    with open(path, "wb") as f:
        f.write(data)
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: scrub_opencv.py <binary>", file=sys.stderr)
        sys.exit(2)
    sys.exit(scrub(sys.argv[1]))
