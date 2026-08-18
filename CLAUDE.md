# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repository is

The **Rust arm** of a port evaluation. A sibling Python repository (`../kerbside`)
holds the reference implementation, its `PORTING.md` (the task), its
`compare_runs.py` (the correctness oracle) and `docs/re_scoring.md` (the
reverse-engineering targets). This crate reproduces that program module-for-module
so the two can be read side by side and measured against each other.

Two consequences that shape almost every judgement call here:

* **Bit-exact output against the Python is the contract.** Anything that changes
  the arithmetic — a different matrix inverse, a different RNG consumption
  pattern, a different ONNX Runtime build — breaks the comparison.
* **Performance changes that only one arm gets are not improvements, they are
  measurement errors.** Notably, the evidence record and evidence ring allocate
  at the same rate and retain to the same depth as the Python's, deliberately.
  Do not "optimise" them; `tests/churn.rs` pins this.

## Commands

Build (Raspberry Pi / Linux). `scripts/build.sh` sources the environment, builds,
checks the binary for build-machine paths, and confirms it starts:

```bash
scripts/build.sh
```

```bash
scripts/build.sh release
```

Never invoke bare `cargo build` for anything shipped or measured — the
`--remap-path-prefix` flags live in `scripts/env-linux.sh` / `env-windows.ps1`,
not in `.cargo/config.toml`, so an unsourced build leaks the builder's home
directory into ~90 strings in the binary. `build.sh` refuses to proceed if the
flags are absent. See `BUILD.md` for Windows and for ONNX Runtime placement.

Test — 45 tests, ~25 s. Use `--release`: `tests/stage_budget.rs` asserts on real
timings and a debug build fails it:

```bash
cargo test --release
```

```bash
cargo test --release --test determinism replay_is_reproducible
```

Run:

```bash
cargo run --release --bin kerbside -- --replay --frames 1500 --out telemetry/candidate.csv
```

Cross-check the scene generator against the Python **first** — nothing
downstream is worth debugging until the per-frame hashes match:

```bash
cargo run --release --bin make_clip -- --frames 20 --hash
```

Benchmark (gated on governor, thermals and load; refuses to record otherwise):

```bash
tools/bench.sh --frames 3000 --baseline ../kerbside/telemetry/perf_realtime.csv
```

Before shipping:

```bash
python3 tools/check_binary.py target/dist/kerbside --strict
```

Profiles: `dev` → `target/debug`, `release` → benchmarking, `dist` → shipped.
`dist` is the hardened, **obfuscated** artefact and carries six layers at once:
`strip`; `--no-default-features` (drops the settings-schema field-name strings);
`-Zbuild-std` + `panic_immediate_abort` (drops the residual `/rustc/…` std paths
and the panic-machinery strings); an **OLLVM pass plugin** (Pluto) that flattens
the control flow of the kerbside crate; **numeric-constant encryption**
(`crate::crypt` — the tuning values and the calibration survey are XORed at rest
and decoded at run time, so a float scan of `.rodata` no longer finds them); and
**string encryption** (`obfstr`, the diagnostic message text). The plugin is
passed via `RUSTFLAGS` with `cargo build` and scoped to this crate by `policy.json`
(`func: ".*kerbside.*"`) — *not* `cargo rustc`, which self-deadlocks with
`-Zbuild-std`. See BUILD.md → "The dist profile", and respect the **LLVM-version
coupling** invariant below when bumping the toolchain. `release`/`dev` use the
default toolchain: they get the constant and string encryption (both source-level,
so they are in every build) but not strip, the schema drop, build-std or the
control-flow obfuscation. `bench.sh` honours `BINARY=target/dist/kerbside` —
benchmark whatever you ship.

## Architecture

`src/main.rs` builds settings, then a `RoadScene`, then a `ConsumerChain`, then a
`Pipeline`, and drives frames through it. `--replay` (default) runs inline and
deterministically; `--realtime` paces the source and drops frames.

One frame, in `pipeline/pipeline.rs::process_frame`, is a single synchronous call
stack — that stack *is* the frame budget:

```
pre → [submit inference] → background → blobs → [join inference]
    → score → associate → measure → gate → consumers
```

The inference submit/join straddles the background model on purpose: per-frame
cost is `max(background, inference)`, not the sum. `tests/stage_budget.rs::inference_overlaps_the_background_model`
is what proves it still happens — serialising the detector barely moves the frame
time, so this is the easiest thing to silently break.

Module map: `source/` (deterministic scene + ground truth) → `pipeline/`
(one-slot mailbox, background model, blob extraction) → `detect/` (ONNX session on
its own thread) → `track/` (IoU association) → `measure/` (homography, speed fit) →
`enforce/` (violation gate) → `consumers.rs` (evidence record, ring) →
`output.rs` (result CSV — the oracle — and overlay).

### Invariants that are easy to break

**Settings are pulled exactly once per frame.** `config::current()` is called at
pipeline ingress; everything below receives `&Settings`. Never call `current()`
deeper in the frame path — a mid-frame `apply()` would then be half-visible.
Runtime-mutable names are the two in `config::VOLATILE`; adding a name there is a
claim that no component caches it at construction time, and `tests/settings.rs`
enforces it.

**Adding a setting is one line** in the relevant `config/*.rs` group, via the
`settings_group!` macro. It generates name-based access, so `--dump-settings` and
the override machinery cannot go stale. Do not add hand-kept name lists.

**`numpy_rng.rs` is bit-exact by requirement.** It reproduces numpy's
`SeedSequence`, PCG64, *and* the way each distribution consumes the stream
(`integers(dtype=uint8)` draws 32 bits and dispenses four bytes with Lemire
rejection). Get the buffering wrong and output is still uniform, still
deterministic, and completely different.

**`geometry.rs` uses an LU solve with partial pivoting for the matrix inverse**,
not an adjugate — the survey marks project to exactly integer coordinates and the
scene generator truncates, so a last-bit disagreement with `np.linalg.inv` moves a
painted mark one pixel and breaks every frame hash. There is a bitwise test.

**`perf.rs`: the hot path deposits numbers and nothing else.** Formatting,
aggregation and I/O belong on the `perf-writer` thread. Never time a stage by
logging it. The tier-2 gate is an `AtomicBool` read through `is_on()` and is never
handed out by value.

**Native libraries are load-bearing, not incidental.** OpenCV and ONNX Runtime
versions must match the Python's (`opencv-python-headless==4.13.0.92`,
`onnxruntime==1.26.0`); a different build dispatches to different kernels. Do not
substitute pure-Rust reimplementations — three quarters of the frame is inside
them and that share is meant to cost the same from both languages.

**The obfuscated `dist` pins its toolchain to the plugin's LLVM.** The OLLVM
(Pluto) pass plugin loads into the LLVM that rustc itself carries, so it must be
built against that exact LLVM major. `scripts/build.sh` pins `OBF_TOOLCHAIN`
(currently `nightly-2025-06-15`, LLVM 20 — also the oldest nightly that still
satisfies the deps' MSRV of rustc ≥ 1.88) to the committed `OBF_PLUGIN`
(`vendor/Pluto-llvm20.so`), and **refuses** to build `dist` if the toolchain, its
`rust-src`, the plugin, or `policy.json` is missing — it does not silently fall
back. Bumping the nightly means rebuilding the plugin against the new LLVM (needs
that LLVM's `-dev` headers, from apt.llvm.org since Debian lags) and updating both
variables together; a plugin built against a different LLVM will not load.

The plugin is passed via **`RUSTFLAGS` with `cargo build`**, *not* `cargo rustc
-- -Zllvm-plugins`: `cargo rustc` + `-Zbuild-std` self-deadlocks on the target
lock (it opens `target/<triple>/dist/.cargo-lock` twice, exclusively). Because
RUSTFLAGS reaches every crate, the selective `policy.json` — in the package root,
where Pluto reads it from the CWD — scopes obfuscation to this crate with
`func: ".*kerbside.*"` (flatten the crown functions, `bcf`+`sub` on the rest of
kerbside). std and the dependencies compile with the plugin loaded but unmatched,
so they are left untransformed; the "conf not found" lines in the build log are
that no-op, not an error. Do not "simplify" this back to `cargo rustc` or the
default nightly.

### Deliberate non-goals

`--gc-stats` reports that there is no tracing collector, rather than being
removed — "the pauses are gone" is the headline result. `tuning.rs` values and
`config/calibration.rs`'s survey numbers are **encrypted at rest** (`crate::crypt`,
`encf!`/`enci!`): stored XORed and decoded at run time behind a `black_box`
barrier, so a float scan of `.rodata` no longer finds them and the contiguous
survey table is gone. Diagnostic message text is likewise encrypted with `obfstr`.
Both defeat a *static* read of the binary, **not a RAM dump** — the decoded values
and strings sit in memory while the process runs. Neither is total: a value that
recurs as an unrelated literal elsewhere still leaks, and strings in `const`/`static`
arrays (`perf::STAGES`, the CSV columns, `USAGE`) or inside `format!`/`println!`/
`panic!` literals cannot be wrapped. OLLVM, on top, obscures the *logic* (A1); the
port changes the *names*. `model/vehicle.onnx` is copied next to the binary by
`build.rs`, not embedded.

## Conventions

Divergences from the Python are commented **at the point they happen**, in module
docs, rather than collected into a document that would drift. When you introduce
one, follow that pattern. Prose is British English and explains *why*, not what.

`tools/perf_report.py`, `check_binary.py` and `bench.sh` are Python/shell on
purpose — the reporter must stay identical to the Python arm's so the two stage
tables are comparable line by line. `compare_runs.py`, `make_clip.py` and
`export_model.py` are deliberately not duplicated here.
