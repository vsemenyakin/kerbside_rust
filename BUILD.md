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

That is the one-command path: it sources the environment, builds the `dist`
profile, checks the binary for build-machine paths, and confirms it actually
starts. `scripts/build.sh release` builds the profile `tools/bench.sh` measures,
and `scripts/build.sh dev` a debug build.

The long form is the same thing:

```bash
source scripts/env-linux.sh && cargo build --profile dist
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

Identical code generation to `release` -- same `opt-level`, same LTO, so timings
carry over -- with the symbol table stripped, **and built on nightly for one
flag**: `-Zlocation-detail=none`.

That flag is the only way to *remove* panic-site source paths rather than
rewrite them. `--remap-path-prefix` turns a path under your home directory into
`/cargo\registry\...\ndarray-0.17.2\src\impl_constructors.rs`; the flag makes it
nothing at all. Panics still report their message, they just no longer name the
line that raised them.

Measured on this project:

| build | source paths in the binary | size |
| --- | --- | --- |
| untreated | 167, about 92 naming the builder's home directory | 929 KB |
| `release` (stable, remap only) | 133, none naming a person or machine | 894 KB |
| `dist` (nightly) | 53, all from the precompiled standard library | 822 KB |

The 53 survivors are `/rustc/<commit>/library/...` and `/rust/deps/...`, which
ship inside the precompiled `std` with their paths already anonymised by the
Rust project. Removing those too needs `-Zbuild-std` with
`panic_immediate_abort`, which costs every panic message in the program; that
was judged not worth it, and the decision belongs in the report rather than
buried in a build flag.

Verify with `--strict`, which counts what is left and says where each kind comes
from:

```bash
python3 tools/check_binary.py target/dist/kerbside --strict
```

`scripts/build.sh` falls back to the default toolchain with a warning if nightly
is absent, so the build never breaks -- it just produces the `release`-grade
result. Install nightly with:

```bash
rustup toolchain install nightly --profile minimal
```

Because `dist` is built by a different compiler than `release`, **benchmark
whatever you ship**. `bench.sh` takes an override:

```bash
BINARY=target/dist/kerbside tools/bench.sh --frames 3000
``` It is a separate profile rather
than the default because stripping is one of the variables the
reverse-engineering assessment exists to *measure*: build both, record how long
each takes to reverse, and report the difference.

Stripping does much more on the Pi than on Windows. An ELF binary carries its
symbol table in the file; MSVC keeps names in a separate `.pdb` that is simply
not shipped, which is why `target/dist/kerbside.exe` is the same size as the
release build.

### What is still in there, and what is not

Checked on the current build:

| | |
| --- | --- |
| Build-machine paths | gone |
| Tuning constant *names* (`GATE_COAST_MARGIN_KPH`) | gone |
| Tuning constant *values* (`1.85`) | present, as immediates -- see `tuning.rs` |
| The survey (`calibration.rs`) | present, as `f64` literals in `.rodata` |
| `vehicle.onnx` | a plain file next to the binary; the port does not address this |
| Panic and usage message text | present, and deliberately: a device that cannot explain a failure is worse |

The last three are on the assessment's list and are not solved by any language
port. Say so in the report rather than letting a clean `strings` output imply
otherwise.

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
