# Kerbside — Rust

A Rust port of [kerbside](../kerbside), the roadside speed-enforcement camera
built as a stand-in for a real 50 fps computer-vision application being
evaluated for migration off Python.

This is the **Rust arm** of that evaluation. Read the Python repository's
`README.md` for what the application does and `PORTING.md` for the task this
port exists to answer.

## Status against the acceptance criteria

`PORTING.md` asks a port to reproduce the reference CSV within per-column
tolerances, and says exact equality is not required and should not be attempted.

On one machine, with both implementations calling the same OpenCV 4.13.0 and
ONNX Runtime 1.26 binaries, the output is **byte-identical**:

```
                                    Python                          Rust
1500-frame replay, sha256   c63ad198648bd137…      c63ad198648bd137…
violations                                721                            721
frame hashes (400 frames)                        identical on all 400
--dump-settings                                  identical, all 59 settings
--replay vs --replay --threaded                  identical
```

That is stronger than required and should not be read as a general guarantee:
on a different architecture the vectorised kernels differ and the low bits with
them, which is exactly why `tools/compare_runs.py` exists and why the tolerance
table is the real contract. Verify with it on the target board:

```bash
venv/bin/python -m kerbside --replay --frames 1500 --out reference.csv
./kerbside --replay --frames 1500 --out candidate.csv
venv/bin/python tools/compare_runs.py reference.csv candidate.csv
```

## Quick start

Build setup differs per platform and is in [BUILD.md](BUILD.md). Once built,
`target/<profile>/` is self-contained — binary, model and native runtimes — so
running it needs no environment at all.

```bash
cargo run --release --bin kerbside -- --replay --frames 700
```

```bash
cargo test --release
```

```bash
cargo run --release --bin make_clip -- --frames 20 --hash
```

45 tests, about 25 s. `make_clip --hash` is the first thing to run after
touching the scene generator: it prints a per-frame SHA-256 that must match the
Python's `tools/make_clip.py --frames 20 --hash` before any downstream
comparison means anything.

## What is the same, and what could not be

The module layout mirrors the Python one-for-one, so the two can be read side by
side. Four things did not survive the crossing intact, and each is the sort of
finding the port evaluation is meant to produce.

### The inference overlap had to be built

In Python the classifier hides behind the background model because
`session.run` releases the GIL. Nothing was designed; it fell out of a C
extension releasing a lock. There is no GIL here, so `detect/detector.rs` runs a
real worker thread with an explicit handoff and an explicit join, and the frame
buffer needed its own safety argument — `SharedMat` in `pipeline/types.rs`, a
fifteen-line wrapper and a paragraph justifying `unsafe impl Sync`.

The `inference_overlaps_the_background_model` test is what proves the overlap is
actually happening. Without a GIL to make it accidental, silently serialising
the detector is the most likely way to get this wrong, and the frame time barely
moves when you do.

### The settings rebind became an explicit atomic swap

`apply()` in Python assigns one module global, and the interpreter lock makes it
atomic for free. Here it is an `ArcSwap`, and every per-frame consumer holds its
snapshot for the whole frame. The Python enforces "pull once per frame" by
convention; here the pipeline takes the `Arc` at ingress and passes `&Settings`
down, so nothing deeper *can* re-pull.

Settings also lost their reflection. Python gets name-based access from
dataclass introspection; `config/mod.rs` has a `settings_group!` macro that
generates the same access from the same declaration, so adding a setting is
still one line and `--dump-settings` still cannot go stale.

### numpy's random number generator had to be reimplemented exactly

The scene is generated from `np.random.default_rng(seed)`, and the whole
comparison rests on the *input* being identical. `numpy_rng.rs` reproduces
numpy's `SeedSequence`, its PCG64 variant, and — the part that is easy to miss —
the way each distribution consumes the stream. `integers(..., dtype=uint8)` does
not draw a 64-bit word per value; it draws 32 bits, dispenses four bytes, and
applies Lemire's debiasing with rejection. Get the buffering wrong and the
values are still uniform, still deterministic, and completely different.

It is checked against the real interpreter in six tests.

One related trap cost an afternoon and is worth stating: the survey marks
project to *exactly* integer image coordinates, and the scene generator
truncates. `np.linalg.inv` and a textbook adjugate inverse disagree in the last
bit there, which moved a painted mark one pixel and broke every frame hash. The
matrix inverse in `geometry.rs` is an LU solve with partial pivoting for that
reason, and there is a bitwise test pinning it.

### The allocation is still here; the collector is not

The evidence record and the evidence ring are the measured cause of the Python's
latency tail: hundreds of tracked containers per frame, retained hundreds of
frames deep, walked in full by every major collection. **This port allocates the
same data at the same rate and retains it to the same depth.** That is
deliberate and the churn tests pin it. Making the record cheaper here would
compare a heavy Python against a light Rust rather than two implementations of
the same program.

What changed is that nobody walks it. `--gc-stats` reports exactly that rather
than being quietly dropped, because "the pauses are gone" is the headline result
and a missing flag would look like an oversight.

## What this port does *not* address

**No language port protects the model weights.** `model/vehicle.onnx` is opened
the same way from Rust as from Python, and the build script copies it next to
the binary rather than embedding it. Embedding, encrypting, and decrypting to
memory is real work and belongs in the report as a separate line item — doing it
silently here would make the reverse-engineering assessment read better than the
shipped artefact deserves.

The **survey** is in the same position. `config/calibration.rs` holds eight
numbers that are the difference between a device that measures metres and one
that measures pixels. In a release build they are `f64` literals in `.rodata`:
harder to find than a Python source file, no more encrypted.

What the port does change is `tuning.rs` — target T1 of the RE assessment. The
*names* are gone from a release build; the values are not. That distinction is
what the RE table should record, in time-to-recover rather than in
possibility.

## Layout

```
src/
  config/        Settings, 7 group modules, the volatility contract
  source/        deterministic road scene, with ground truth
  pipeline/      mailbox, worker thread, background model, blob extraction
  detect/        ONNX session on its own thread, blob scoring
  track/         IoU association, lifecycle, published state snapshot
  measure/       ground-plane homography, least-squares speed fit
  enforce/       the violation gate
  consumers.rs   evidence record, evidence ring
  output.rs      result CSV (the oracle), overlay renderer
  perf.rs        two-tier instrumentation, writer thread
  geometry.rs    Point, Box, Homography
  numpy_rng.rs   bit-exact numpy PCG64
  tuning.rs      empirical constants; also the reverse-engineering target
  bin/make_clip  scene renderer and frame hasher
model/           vehicle.onnx, copied next to the binary at build time
tools/           bench.sh, bench.ps1, perf_report.py -- see tools/README.md
tests/           45 tests
```

`tools/bench.sh`, `tools/bench.ps1` and `tools/perf_report.py` are this arm's
copies of the Python's benchmarking scripts -- same gates, same table, plus an
optional `--baseline` that prints the tail comparison against the Python run.
See [tools/README.md](tools/README.md).

`compare_runs.py`, `make_clip.py` and `export_model.py` are deliberately not
duplicated: the comparator has to be the *same* program for both arms to make
its verdict trustworthy, `make_clip` is a Rust binary here because the scene
generator is the thing under test, and there is one model, built from one
place.

## Licence

MIT, as the original.
