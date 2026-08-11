# tools

The Rust arm's benchmarking scripts, ported from the Python project's `tools/`.

| | |
| --- | --- |
| `bench.sh` | The gated benchmark. **This is the one that produces report numbers.** Linux/Raspberry Pi OS. |
| `bench.ps1` | Windows equivalent, for development signal only — see the warning it prints. |
| `perf_report.py` | Turns a per-frame perf CSV into the stage table. Called by both. |

```bash
tools/bench.sh --frames 3000
```

```bash
tools/bench.sh --frames 3000 --baseline ../kerbside/telemetry/perf_realtime.csv
```

`--baseline` is the only behaviour these add over the originals: given the
Python arm's perf CSV it prints the tail comparison — p50/p95/p99/max,
over-budget count, and the native-versus-host split — which is item 2 of what
`PORTING.md` asks to hand back. Everything else behaves as the Python's copies
do, including refusing to record a run on a machine whose state would make the
number meaningless.

## Running both arms

The gates only mean something if both arms are measured under identical
conditions. Same board, same session, same thermal state:

```bash
cd ../kerbside && tools/bench.sh --frames 3000 --out telemetry
```

```bash
cd ../kerbside_rust && tools/bench.sh --frames 3000 --baseline ../kerbside/telemetry/perf_realtime.csv
```

## What is deliberately not here

`compare_runs.py`, `make_clip.py` and `export_model.py` are not duplicated.

* **`compare_runs.py`** is the correctness oracle, and running the *same*
  comparator over both arms is precisely what makes its verdict trustworthy.
  Call the Python project's copy with this arm's CSV as the candidate.
* **`make_clip`** is a Rust binary in this crate (`cargo run --bin make_clip`),
  because a port needs its own scene generator — that is the thing under test,
  not a tool.
* **`export_model.py`** builds the `.onnx` this arm consumes. There is one
  model and it must stay reproducible from one place.

## Why the reporter is Python

`perf_report.py` is Python in a Rust project on purpose. The deliverable is two
stage tables read side by side, and a reimplementation would eventually differ
in a percentile rule or a rounding step and quietly become the thing the two
arms disagree about. It uses only the standard library, and Raspberry Pi OS
ships `python3`. If Python is genuinely absent, `bench.sh` says so, tells you
where the raw CSV is, and still leaves you the run.

One label differs from the Python's copy: that file's `python` bucket is
`rust` here, because the bucket means "time in the host language rather than in
a native library". The stage membership is unchanged, which is what keeps the
two tables comparable line by line.
