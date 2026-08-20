# Building

The port deliberately links the **same** native libraries the Python calls —
OpenCV and ONNX Runtime — because roughly three quarters of the frame is already
inside them and that share costs the same from either language. The price is
that both have to be present at build time, and neither is discoverable the same
way on Windows and on Raspberry Pi OS.

Two targets are supported and the code is identical on both. Nothing below
changes any source file.

| | Windows (x86-64, MSVC) | Raspberry Pi OS 64-bit (aarch64) |
| --- | --- | --- |
| Rust | 1.88+ | 1.88+ |
| OpenCV | prebuilt release, found via `OPENCV_*` | `libopencv-dev`, found via pkg-config |
| libclang | required, must also be on `PATH` | `libclang-dev` |
| ONNX Runtime | shared library, `ORT_DYLIB_PATH` | shared library, `ORT_DYLIB_PATH` |

---

## Raspberry Pi OS (64-bit) — the target board

```bash
sudo apt install build-essential pkg-config libopencv-dev clang libclang-dev
```

`pkg-config` finds OpenCV, so no `OPENCV_*` variables are needed.

```bash
scripts/build.sh
```

That is the one-command path: it sources the environment, builds the **obfuscated
`dist`** profile (see "The dist profile" below), checks the binary for
build-machine paths, and confirms it actually starts. `scripts/build.sh release`
builds the profile `tools/bench.sh` measures, and `scripts/build.sh dev` a debug
build.

The obfuscated `dist` needs a one-time set-up — a pinned nightly whose LLVM
matches the committed obfuscation plugin, that toolchain's `rust-src`, and the
matching LLVM `-dev` headers — all documented in "The dist profile". `release`
and `dev` need none of it, and their long form is just:

```bash
source scripts/env-linux.sh && cargo build --profile release
```

### ONNX Runtime on the Pi

`apt` has no ONNX Runtime package, so this is the one piece you fetch by hand.
It is loaded at run time rather than linked in, and the binary looks for
`libonnxruntime.so` next to itself before anything else — so the least
error-prone thing is to put it there:

```bash
curl -L -o ort.tgz https://github.com/microsoft/onnxruntime/releases/download/v1.26.0/onnxruntime-linux-aarch64-1.26.0.tgz
```

```bash
tar xf ort.tgz && cp onnxruntime-linux-aarch64-1.26.0/lib/libonnxruntime.so target/release/
```

After that the binary runs with no environment set, exactly as on Windows. Two
alternatives, both fine:

* `export ORT_DYLIB_PATH=/path/to/libonnxruntime.so` — takes priority over the
  copy next to the binary, which makes it the right way to pin a *specific*
  runtime for a comparison run.
* Install it system-wide (`sudo cp … /usr/local/lib && sudo ldconfig`) — the
  binary falls back to the platform loader, so this is found too.

If you set `ORT_DYLIB_PATH` **before** `cargo build`, the build script copies
that library next to the binary for you, and pip's versioned filename
(`libonnxruntime.so.1.26.0`) is handled.

**Match the ONNX Runtime version to the Python's.** `requirements.txt` pins
`onnxruntime==1.26.0`, and a different build dispatches to different kernels —
which is a legitimate source of `lead_coverage` differences and an illegitimate
source of confusion. If the Python virtualenv is on the same board, point
`ORT_DYLIB_PATH` at the copy inside it and both arms run the identical library:

```bash
export ORT_DYLIB_PATH="$PWD/../kerbside/venv/lib/python3.11/site-packages/onnxruntime/capi/libonnxruntime.so.1.26.0"
```

### Before benchmarking on the board

The Python's `tools/bench.sh` refuses to record a run on a non-`performance`
governor, a hot SoC, a throttled board, or a loaded machine. Those gates apply
to this arm too and for the same reason: on a small ARM board, thermal
throttling and an on-demand governor each move the frame time by more than the
entire interpreter share the exercise is trying to measure.

Run every arm on the same board, in the same session, in the same thermal state.

```bash
sudo cpupower frequency-set -g performance
```

---

## Fresh-machine setup — the obfuscated `dist` from a clean checkout

From a freshly installed **Raspberry Pi OS 64-bit (trixie)** and a clean checkout,
here is the whole path to `scripts/build.sh` producing the obfuscated `dist`.

**The checkout already ships** everything version-controlled: the source,
`model/vehicle.onnx`, the selective `policy.json`, and the prebuilt obfuscation
plugin `vendor/Pluto-llvm20.so` (stripped, ~1.3 MB, aarch64 + LLVM 20). You do
**not** build the plugin. Two things are deliberately *not* in the repo and must
be fetched: the Rust toolchain and the ONNX Runtime shared library.

**1. System packages.** Build tools + OpenCV (+ its bindgen's libclang) + the two
shared libraries the vendored plugin loads at run time (`libz3.so.4`; the LLVM
runtime comes in step 2):

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libopencv-dev clang libclang-dev \
                    libz3-4 curl wget
```

Note this is lighter than a plugin build: no `cmake`, `ninja`, `libz3-dev` or
`llvm-*-dev` — those are only needed to *rebuild* the plugin (see "The dist
profile" → the LLVM-version coupling).

**2. The LLVM 20 runtime** the plugin links (`libLLVM.so.20.1`). Debian's newest
packaged LLVM is 19, so take the *runtime* package (not `-dev`) from apt.llvm.org:

```bash
wget -qO- https://apt.llvm.org/llvm-snapshot.gpg.key \
  | sudo tee /etc/apt/trusted.gpg.d/apt.llvm.org.asc >/dev/null
echo 'deb http://apt.llvm.org/trixie/ llvm-toolchain-trixie-20 main' \
  | sudo tee /etc/apt/sources.list.d/llvm20.list
sudo apt update && sudo apt install -y libllvm20
```

**3. Rust: rustup + the pinned nightly + `rust-src`** (for `-Zbuild-std`):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
rustup toolchain install nightly-2025-06-15 --profile minimal
rustup component add rust-src --toolchain nightly-2025-06-15
```

**4. ONNX Runtime** — loaded at run time, not linked, and not in `apt`. Extract the
official aarch64 build into the package root; `scripts/env-linux.sh` finds
`onnxruntime-linux-aarch64-1.26.0/lib/libonnxruntime.so` there automatically:

```bash
cd path/to/kerbside_rust
wget -q https://github.com/microsoft/onnxruntime/releases/download/v1.26.0/onnxruntime-linux-aarch64-1.26.0.tgz
tar xzf onnxruntime-linux-aarch64-1.26.0.tgz
```

**5. Build:**

```bash
scripts/build.sh
```

That produces `target/dist/kerbside` — stripped, introspection removed, std paths
and panic strings gone, and control-flow obfuscated. Verify:

```bash
python3 tools/check_binary.py target/dist/kerbside --strict
target/dist/kerbside --replay --frames 1500 --out /tmp/c.csv   # reproduces the CSV
```

`scripts/build.sh release` and `scripts/build.sh dev` need only steps 1, 3 (any
toolchain) and 4 — none of the obfuscation set-up.

## Windows (x86-64, MSVC)

You need three things the toolchain will not find on its own.

**1. Visual Studio Build Tools** with the C++ workload, for `link.exe`.

**2. LLVM**, for `libclang.dll` — the `opencv` crate generates its bindings with
it. Note that the generated build script *links* against `libclang.dll`, so the
DLL has to be on `PATH` and not merely named in `LIBCLANG_PATH`; a missing
`PATH` entry shows up as an opaque `STATUS_DLL_NOT_FOUND` (exit code
`0xc0000135`) from the build script.

```powershell
winget install LLVM.LLVM
```

**3. OpenCV**, prebuilt. Download `opencv-4.13.0-windows.exe` from
<https://github.com/opencv/opencv/releases/tag/4.13.0> and extract it — it is a
self-extracting archive, not an installer:

```powershell
.\opencv-4.13.0-windows.exe -o"C:\opencv" -y
```

Match 4.13.0 to the Python's `opencv-python-headless==4.13.0.92` for the same
reason as ONNX Runtime above.

Then dot-source the environment script and build:

```powershell
. .\scripts\env-windows.ps1
cargo build --release
```

From `cmd.exe`, which cannot dot-source a PowerShell script, use the wrapper
instead — it does the same thing in one child process:

```
scriptsuild.cmd
```

The script sets `LIBCLANG_PATH`, the three `OPENCV_*` variables, the
`--remap-path-prefix` flags, and prepends the LLVM and OpenCV `bin` directories
to `PATH`. Override `LLVM_HOME`, `OPENCV_HOME` or `OPENCV_MSVC` beforehand if
your install lives elsewhere.

There is deliberately only one Windows environment script. An earlier Git Bash
copy of it drifted out of step with this one — it set `ORT_DYLIB_PATH` and the
PowerShell version did not — which produced a binary that built cleanly and then
died at startup. To build from a bash prompt on Windows, call PowerShell:

```bash
powershell -NoProfile -Command ". .\scripts\env-windows.ps1; cargo build --release"
```

### ONNX Runtime on Windows

Same as on the Pi: a DLL and a path to it. The bash script will point
`ORT_DYLIB_PATH` at the copy inside the Python project's virtualenv if it finds
one, which is the best option for a comparison run — both arms then load the
identical library. Otherwise download `onnxruntime-win-x64-1.26.0.zip` from the
ONNX Runtime releases and set the variable by hand.

### Why the runtime is loaded rather than linked

`ort`'s prebuilt **static** libraries are compiled against a newer MSVC standard
library than some VS 2022 installs ship, and linking them fails with an
unresolved `__std_find_last_of_trivial_pos_1`. Loading the shared library
sidesteps that, and it also matches how the Pi image is built — so both targets
ship the same kind of artefact and the reverse-engineering assessment does not
have to caveat that one build inlined the runtime and the other did not.

---

## Running

**The environment scripts are for *building*. Running needs nothing.**

`build.rs` copies everything the binary needs into `target/<profile>/` beside
it, so the output directory is self-contained:

```
target/release/
  kerbside.exe
  make_clip.exe
  vehicle.onnx                       the model
  onnxruntime.dll                    the inference runtime, loaded at startup
  onnxruntime_providers_shared.dll
  opencv_world4130.dll               Windows only; apt provides this on the Pi
```

```bash
.\target\release\kerbside.exe --replay --frames 1500 --out telemetry\candidate.csv
```

from any shell, with no variables set. Copying that directory to another machine
of the same architecture works as-is.

Three overrides exist if you need them, and each is searched before the copy
next to the binary:

* `KERBSIDE_MODEL_DIR` — where `vehicle.onnx` lives.
* `ORT_DYLIB_PATH` — which ONNX Runtime to load. Worth setting deliberately for
  a comparison run, so both arms load the identical library.
* `PATH` — for `opencv_world*.dll`, if you would rather not have a copy in
  `target/`.

### If it fails to start

| Symptom | Cause |
| --- | --- |
| Exit code 53, no output at all | The loader cannot find `opencv_world*.dll`. It happens before `main`, so nothing can report it. Rebuild with the env script set, or put the DLL on `PATH`. |
| `Failed to load ONNX Runtime dylib` | Only possible if the copy next to the binary is missing *and* `ORT_DYLIB_PATH` is unset or wrong. The error names every path that was tried. |
| `vehicle.onnx was not found` | Same, for the model. The error names the search path. |

## Shipping a binary

An untreated Rust release build embeds the **absolute source path of every panic
site** -- `unwrap`, `expect`, slice indexing, integer overflow -- and for a
dependency that is the full path into the build machine's cargo registry. This
project's binary carried about ninety strings naming the builder's home
directory before that was dealt with.

Two reasons that matters here. It is an information leak in an artefact meant to
be installed on a device someone else may hold; and the paths spell out the
module tree, which is target T2 of `docs/re_scoring.md` handed over for free.

The environment scripts fix it, on every platform, by setting
`--remap-path-prefix` for the cargo registry, the rustup toolchain and the
project directory. **Build through them and it is handled.**

From PowerShell -- note the leading `. `, which dot-sources the script so its
variables land in your session rather than in a child process that immediately
exits:

```powershell
. .\scripts\env-windows.ps1
cargo build --profile dist
```

From `cmd.exe`, which cannot dot-source a PowerShell script:

```
scriptsuild.cmd
```

That wrapper does the same thing in one child process and then runs the check
below. `scriptsuild.cmd release` builds the benchmarking profile instead.

Verify:

```bash
python3 tools/check_binary.py target/release/kerbside
```

```
target/release/kerbside: 927,744 bytes
clean -- no build-machine paths found
```

Two things to know:

* Changing `RUSTFLAGS` invalidates the build cache, so the first build after
  sourcing an environment script rebuilds every dependency. That is expected.
* This could not go in `.cargo/config.toml`: the paths differ per machine and
  that file cannot expand environment variables. Cargo's purpose-built
  `trim-paths` profile option would be the tidier answer, but it is still
  nightly-only as of Cargo 1.91.

### The dist profile

```bash
scripts/build.sh
```

Same `opt-level` and LTO as `release`, plus **seven hardening layers applied
together** — this is the shipped, obfuscated artefact:

1. **`strip`** — the symbol table (an ELF carries it in the file; there is no
   `.pdb` to leave behind, unlike MSVC).
2. **`--no-default-features`** — drops the `introspection` feature, so the
   settings *field-name* strings (`IMAGE_POINTS`, `FRAME_WIDTH`, …) and the
   derived `Debug` labels — the schema a reverse engineer would otherwise get
   for free — never reach the binary.
3. **`-Zbuild-std` + `panic_immediate_abort` + `-Zlocation-detail=none`** —
   rebuilds `std` without its panic machinery and drops panic-site source
   paths. This *removes* the residual `/rustc/<commit>/library/...` paths that
   `--remap-path-prefix` can only anonymise, and the panic-message formatting
   strings with them.
4. **OLLVM obfuscation** — the Pluto New-PM pass plugin (bogus control flow +
   control-flow flattening + instruction substitution) obscures the algorithm
   and stage order (target A1), so recovering intent is expensive even from a
   memory dump. Passed via `RUSTFLAGS` and scoped to the **kerbside crate** by
   `policy.json` (`func: ".*kerbside.*"`); `std` and the dependencies are not
   obfuscated. See the coupling note below for why it is `cargo build`, not
   `cargo rustc`.
5. **Numeric-constant encryption** (`crate::crypt`, source-level — in every
   build) — the `tuning.rs` values and the `calibration.rs` survey are stored
   XORed on their bit pattern and decoded at run time behind a `black_box`
   barrier, so a float scan of `.rodata` no longer finds them and the contiguous
   survey table is gone (targets A2/A4). Bit-exact, so the CSV oracle still holds.
6. **String encryption** (`obfstr`, source-level) — the diagnostic message text
   (`"cannot draw the verge"`, `"homography element"`, …) is XOR-encrypted at
   compile time and decoded on the stack at use.
7. **Static OpenCV** (`dist` only, configured in `scripts/build.sh`) — the
   `core`/`imgproc`/`video` modules are linked from the system static archives
   (`/usr/lib/*/libopencv_{core,imgproc,video}.a`, provided by `libopencv-dev`)
   instead of the shared `libopencv_*.so`. Their `cv::` functions become internal
   symbols rather than dynamic imports, which removes the `LD_PRELOAD`
   interposition surface a reverse engineer uses on the running board to read the
   exact arguments of `createBackgroundSubtractorMOG2`, `getPerspectiveTransform`
   and `getStructuringElement`. `videoio`/`imgcodecs` — which would drag OpenCV's
   FFmpeg/GStreamer/codec backends and cannot be linked statically without ~30
   more libraries — are excluded by `--no-default-features`: the `overlay`
   feature (which gates `--overlay` mp4 output and make_clip's PNG export) is off
   in `dist`. The wiring is base `opencv` features `imgproc`+`video` in
   `Cargo.toml`, `overlay = ["opencv/videoio", "opencv/imgcodecs"]` default-on for
   `dev`/`release`, and `OPENCV_LINK_LIBS="static=opencv_video,…"` exported for
   `dist` in `scripts/build.sh` (which also makes build-local `-ldev` symlinks for
   BLAS/LAPACK under `target/`, since Debian ships none).

Layers 5 and 6 defeat a *static* read of the medium, not a **RAM dump** — the
decoded values and strings are in memory while the process runs. Layer 7 goes
after a *different* attack: `LD_PRELOAD` interposition of OpenCV on the running
board, which reads library-call arguments directly and is unaffected by 4–6. It
does not stop `ptrace`/GDB/Frida hooking internal functions, nor differential
probing of the binary's own output.

**Build-time requirement (`dist` only).** The static link consumes the OpenCV
*static archives* `libopencv_{core,imgproc,video}.a` and the OpenCV headers —
both from `libopencv-dev` (already in the apt line above) — and links the
BLAS/LAPACK runtime (`libblas3` / `liblapack3` / `libgfortran5`), which
`libopencv-dev` already pulls in as dependencies. `release`/`dev` are unaffected:
they link OpenCV dynamically via pkg-config, as before.

**Runtime dependencies change with it.** Because OpenCV is now inside the binary,
the shipped `dist` no longer loads `libopencv_*.so`; instead it needs `liblapack3`,
`libblas3`, `libtbb12`, `libgfortran5` and `libGLX`/`libX11` present on the
*deployment* board. Confirm what remains dynamic with `ldd target/dist/kerbside`
(there should be no `libopencv_*` line).

Measured on this project (obfuscated `dist`):

| property | value |
| --- | --- |
| residual build-machine / std source paths | **0** (was 53 before `-Zbuild-std`) |
| settings field-name strings | gone |
| conditional branches in `.text` | **31 131** vs 8 336 un-obfuscated |
| contiguous survey table in `.rodata` | **gone**; float scan drops 10/16 → 4/16 (residuals are unrelated literals) |
| diagnostic strings | encrypted with `obfstr` — absent from the binary |
| size | ~1.64 MB (obfuscation + the inlined constant/string decoders) |
| result-CSV reproduction | **byte-identical `sha256`** to the clean build — passes the oracle |
| frame-time cost of obfuscation | **≈ +0.08 %** (the obfuscatable code is ~0.3 % of the frame; 99 % is native OpenCV/ONNX) |

Verify the path check with `--strict`:

```bash
python3 tools/check_binary.py target/dist/kerbside --strict
```

#### The LLVM-version coupling (and rebuilding the plugin)

`-Zllvm-plugins` loads the plugin into the LLVM **that rustc itself carries**, so
the plugin must match that LLVM major exactly. The plugin is therefore **vendored
in the repo** — `vendor/Pluto-llvm20.so`, stripped to ~1.3 MB, built against the
LLVM of the pinned toolchain — so a normal checkout builds `dist` without a plugin
build. `scripts/build.sh` pins both (overridable via `OBF_TOOLCHAIN` /
`OBF_PLUGIN` / `OBF_POLICY`) and **refuses to build — it does not silently fall
back** — if the toolchain, its `rust-src`, the plugin, or `policy.json` is missing.
Current pin: `nightly-2025-06-15` (rustc 1.89, **LLVM 20** — also the oldest
nightly that still satisfies the deps' MSRV of rustc ≥ 1.88).

The plugin is applied through **`RUSTFLAGS` with `cargo build`**, not `cargo rustc
-- -Zllvm-plugins`: the latter self-deadlocks with `-Zbuild-std` — it opens
`target/<triple>/dist/.cargo-lock` twice, exclusively, and blocks on itself.
Because RUSTFLAGS reaches every crate, `policy.json` does the scoping: its
`func: ".*kerbside.*"` filter matches only this crate's mangled symbols, so `std`
and the dependencies compile with the plugin loaded but leave it a no-op (the
`conf not found: …/policy.json` lines in the build log are that no-op — Pluto
looks for the policy in each crate's working directory and finds none).

For a from-nothing install, see **"Fresh-machine setup"** above; it does not build
the plugin. You only rebuild it when you **bump the toolchain to a different LLVM**,
and then you must update `OBF_TOOLCHAIN` / `OBF_PLUGIN` together — a plugin built
against a different LLVM will not load. The coupling is deliberate and is called
out in `CLAUDE.md` as an invariant. To rebuild:

```bash
# the new LLVM's dev headers (Debian lags -> apt.llvm.org), plus cmake/ninja/z3
sudo apt-get install -y --no-install-recommends llvm-20-dev cmake ninja-build libz3-dev
git clone --depth 1 https://github.com/lich4/ollvm-pass && cd ollvm-pass
# three fixes the upstream needs to compile with Linux/gcc:
sed -i 's|#include "CryptoUtils.h"|#include "CryptoUtils.h"\n#include <queue>|' pluto/MBAUtils.cpp
sed -i 's|#include <fstream>|#include <fstream>\n#include <filesystem>|' common.h
sed -i 's|match_path = weakly_canonical(match_path);|match_path = filesystem::weakly_canonical(match_path);|' PassUtils.h
export LLVM_DIR=/usr/lib/llvm-20/lib/cmake/llvm
cmake -S pluto -G Ninja -B build && cmake --build build
strip --strip-unneeded build/Pluto.so                       # 22 MB -> ~1.3 MB
cp build/Pluto.so /path/to/repo/vendor/Pluto-llvm20.so      # rename to the new LLVM
```

Because `dist` is built by a different compiler than `release`, **benchmark
whatever you ship**. `bench.sh` takes an override:

```bash
BINARY=target/dist/kerbside tools/bench.sh --frames 3000
```

It is a separate profile rather than the default because stripping and
obfuscation are among the variables the reverse-engineering assessment exists to
*measure*: build both, record how long each takes to reverse, and report the
difference.

Stripping does much more on the Pi than on Windows. An ELF binary carries its
symbol table in the file; MSVC keeps names in a separate `.pdb` that is simply
not shipped, which is why `target/dist/kerbside.exe` is the same size as the
release build.

### What is still in there, and what is not

Checked on the current build:

| | |
| --- | --- |
| Build-machine paths and std `/rustc/…` paths | gone (`--remap-path-prefix` + `-Zbuild-std`) |
| Symbols | gone (`strip`) |
| Settings schema — field *names* (`IMAGE_POINTS`, `GATE_COAST_MARGIN_KPH`) | gone (`--no-default-features` + strip) |
| Algorithm / stage order (control flow) | **obscured** — OLLVM flattening |
| Tuning values / calibration survey | **encrypted at rest** (`crate::crypt`) — contiguous survey table gone; only values that recur as unrelated literals elsewhere remain |
| Diagnostic message strings | **encrypted** (`obfstr`) |
| Stage names (`perf::STAGES`), CSV columns, `USAGE` help | present — `const`/`static` string arrays, which `obfstr` cannot wrap |
| `panic!` / `println!` text | `panic!` gone (`panic_immediate_abort`); `println!` / usage text present |
| `vehicle.onnx` | a plain file next to the binary; the port does not address this |

The obfuscation moves the **control flow** (the algorithm and stage order): it is
flattened and padded with bogus paths, so recovering *intent* is expensive even
from a memory dump. On top of the language port, `crate::crypt` encrypts the
numeric **data** (the tuning values and the survey) and `obfstr` encrypts the
diagnostic **strings** — both source-level and bit-exact. They shrink what a
static `strings`/float scan of the medium returns, but not to zero and not against
a **RAM dump**: the decoded values and text sit in memory at run time. Two
categories resist encryption entirely and still ship — `const`/`static` string
arrays (`perf::STAGES`, the CSV columns, `USAGE`) and `format!`/`println!` format
literals, because neither can be moved into an `obfstr!`/`encf!` call — and
`vehicle.onnx` is still a plain file. Say so in the report rather than letting a
shrunken `strings` output imply the data is gone.

## Cross-checking the two arms

```bash
cargo run --release --bin make_clip -- --frames 20 --hash
```

against the Python's

```bash
venv/bin/python tools/make_clip.py --frames 20 --hash
```

Compare the hash column only — this binary terminates lines with `LF`, and
Python's `print` on Windows writes `CRLF`, so `diff --strip-trailing-cr` or an
`awk '{print $2}'` is the honest comparison. If the hashes differ, stop: nothing
downstream is worth debugging until the input matches.
