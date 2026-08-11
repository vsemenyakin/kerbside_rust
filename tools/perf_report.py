#!/usr/bin/env python3
"""Turn a per-frame perf CSV into the stage table. Rust arm.

    ./target/release/kerbside --realtime --perf
    python3 tools/perf_report.py telemetry/perf_realtime.csv

This is the Rust arm's copy of the Python project's `tools/perf_report.py`. The
table, the columns and the arithmetic are identical on purpose: the deliverable
of this exercise is two tables read side by side, and a report that reformatted
its output would make that comparison harder for no benefit.

Read it in this order:

1.  **The `total` percentiles**, not the mean. A mean hides the tail, and the
    tail is where a garbage-collected runtime differs from a compiled one.
2.  **The over-budget count.** Frames missed, not milliseconds lost.
3.  **The native/host split at the bottom.** Stages marked `native` are
    dominated by a single OpenCV or ONNX Runtime call and cost the same in any
    language; stages marked `rust` are the ones that were interpreter-bound in
    the Python, and are what this port actually removed.

What changed from the Python's copy
-----------------------------------
One label. Where that file says `python` this says `rust`, because the bucket
means "time in the host language, not in a native library" and the host
language is different. The stage *membership* of each bucket is unchanged --
they are the same stages doing the same work -- which is what makes the two
tables comparable line by line.

`--baseline` is new and optional; it prints the Python's table beside this one.
Everything else behaves exactly as the original.
"""

from __future__ import annotations

import argparse
import csv
import sys

#: How each stage's time is actually spent. The classification is a claim about
#: the code, and it is the most important content of this file -- see the module
#: documentation of the stages themselves for the justification.
ATTRIBUTION = {
    "pre": "native",      # one cv::resize
    "bg": "native",       # one MOG2 apply -- the dominant call
    "morph": "native",    # threshold + open + close
    "bl_find": "native",  # cv::findContours
    "bl_filter": "rust",  # loop per contour
    # NOTE: "infer" is deliberately absent. It runs on the worker thread,
    # concurrently with the background model, so adding it to the frame's cost
    # would double-count time that never elapsed on the pipeline thread -- and
    # would report a native share above 100%, which is how this was noticed.
    # What the pipeline thread actually pays is the residual wait, infer_join.
    "infer_join": "native",
    "sc_sample": "rust",   # loop per blob over the likelihood map
    "as_score": "rust",    # nested loop over (blob, vehicle)
    "as_pick": "rust",     # greedy assignment
    "as_life": "rust",     # confirm / coast / retire / spawn
    "sp_project": "rust",  # loop, per-vehicle homography
    "sp_fit": "rust",      # least-squares fit per vehicle
    "gate": "rust",        # per-vehicle preconditions
    "em_record": "rust",   # nested record construction
}

#: The label the Python's report uses for the same bucket. Kept so `--baseline`
#: can read that arm's numbers without a second copy of this table.
BASELINE_HOST_LABEL = "python"
HOST_LABEL = "rust"

#: Stages that contain other stages; excluded from the attribution sum so their
#: children are not counted twice.
CONTAINERS = {"blobs", "score", "assoc", "speed", "emit", "total"}

#: Measured but not part of the frame's serial cost: it happens on another
#: thread. Shown in the table, excluded from the attribution.
OVERLAPPED = {"infer"}


def percentile(ordered: list[float], p: float) -> float:
    if not ordered:
        return 0.0
    index = min(len(ordered) - 1, max(0, int(round(p / 100.0 * len(ordered))) - 1))
    return ordered[index]


def load(path: str) -> tuple[dict[str, list[float]], dict[str, float], int]:
    """Sorted series and means per column."""
    with open(path, newline="", encoding="utf-8") as fh:
        rows = list(csv.DictReader(fh))
    if not rows:
        raise SystemExit(f"{path}: no rows")
    columns = [c for c in rows[0] if c != "frame_id"]
    series = {c: sorted(float(r[c]) for r in rows) for c in columns}
    means = {c: sum(float(r[c]) for r in rows) / len(rows) for c in columns}
    return series, means, len(rows)


def buckets_of(means: dict[str, float], host_label: str) -> dict[str, float]:
    out = {"native": 0.0, host_label: 0.0}
    for column, kind in ATTRIBUTION.items():
        if column in means:
            out["native" if kind == "native" else host_label] += means[column]
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csv_path")
    parser.add_argument("--budget", type=float, default=20.0, help="frame budget in ms")
    parser.add_argument(
        "--baseline",
        default=None,
        help="the Python arm's perf CSV, to print a tail comparison against",
    )
    args = parser.parse_args()

    series, means, frames = load(args.csv_path)
    columns = list(series)
    total_mean = means.get("total", 0.0) or 1.0

    print(f"{args.csv_path}  --  {frames} frames, budget {args.budget:.1f} ms\n")
    header = f"{'stage':<12}{'mean':>8}{'p50':>8}{'p95':>8}{'p99':>8}{'max':>9}{'share':>8}  where"
    print(header)
    print("-" * len(header))
    for column in columns:
        values = series[column]
        share = 100.0 * means[column] / total_mean
        if column in CONTAINERS:
            where = ""
        elif column in OVERLAPPED:
            where = "overlapped"
        else:
            where = ATTRIBUTION.get(column, "?")
        marker = ">>" if column == "total" else "  "
        print(
            f"{marker}{column:<10}{means[column]:>8.3f}{percentile(values, 50):>8.3f}"
            f"{percentile(values, 95):>8.3f}{percentile(values, 99):>8.3f}"
            f"{values[-1]:>9.3f}{share:>7.1f}%  {where}"
        )

    totals = series.get("total", [])
    over = sum(1 for v in totals if v > args.budget)
    print(
        f"\nover budget: {over} / {len(totals)} frames "
        f"({100.0 * over / max(1, len(totals)):.2f}%)"
    )

    buckets = buckets_of(means, HOST_LABEL)
    accounted = sum(buckets.values())
    print("\nwhere the mean frame goes:")
    for kind in ("native", HOST_LABEL):
        pct = 100.0 * buckets[kind] / total_mean
        print(f"  {kind:<8} {buckets[kind]:6.3f} ms  {pct:5.1f}% of the frame")
    other = total_mean - accounted
    print(
        f"  {'other':<8} {other:6.3f} ms  {100.0 * other / total_mean:5.1f}%"
        f"  (uninstrumented glue -- also host code)"
    )
    print(
        "\n'native' is the floor no rewrite goes below: the same OpenCV and ONNX\n"
        "Runtime calls run in any language. 'rust' is the part that was Python\n"
        "here -- compare it against the same bucket in the baseline table, and\n"
        "against the effort it took to move it.\n"
        "Neither number says anything about the tail; for that, compare the p99\n"
        "and max rows above, and the collector pauses from --gc-stats."
    )

    if args.baseline:
        report_baseline(args.baseline, args.budget, series, means, total_mean)
    return 0


def report_baseline(
    path: str,
    budget: float,
    series: dict[str, list[float]],
    means: dict[str, float],
    total_mean: float,
) -> None:
    """The tail comparison, which is the actual deliverable.

    Deliberately only the four numbers that decide the question: the tail
    percentiles, the worst frame, the frames missed, and the split. A full
    side-by-side of every stage invites reading per-stage differences that are
    only meaningful on the target board.
    """
    base_series, base_means, base_frames = load(path)
    base_total = base_means.get("total", 0.0) or 1.0

    print(f"\n\nbaseline: {path}  --  {base_frames} frames\n")
    row = f"{'':<12}{'baseline':>12}{'this build':>12}{'change':>12}"
    print(row)
    print("-" * len(row))

    def line(label: str, base: float, here: float, unit: str = "ms") -> None:
        if base:
            change = f"{100.0 * (here - base) / base:+.1f}%"
        else:
            change = "-"
        print(f"{label:<12}{base:>10.3f} {unit:<1}{here:>10.3f} {unit:<1}{change:>12}")

    here_total = series.get("total", [])
    base_totals = base_series.get("total", [])
    line("mean", base_total, total_mean)
    line("p50", percentile(base_totals, 50), percentile(here_total, 50))
    line("p95", percentile(base_totals, 95), percentile(here_total, 95))
    line("p99", percentile(base_totals, 99), percentile(here_total, 99))
    line("max", base_totals[-1] if base_totals else 0.0,
         here_total[-1] if here_total else 0.0)

    base_over = sum(1 for v in base_totals if v > budget)
    here_over = sum(1 for v in here_total if v > budget)
    print(
        f"{'over budget':<12}{base_over:>10d}  {here_over:>10d}  "
        f"{'':>12}"
    )

    base_buckets = buckets_of(base_means, BASELINE_HOST_LABEL)
    here_buckets = buckets_of(means, HOST_LABEL)
    print()
    line("native", base_buckets["native"], here_buckets["native"])
    line("host", base_buckets[BASELINE_HOST_LABEL], here_buckets[HOST_LABEL])
    print(
        "\nThe 'host' row is the one the port was supposed to move. The 'native'\n"
        "row should barely move at all -- if it did, the two runs are not\n"
        "comparable and something other than the language changed."
    )


if __name__ == "__main__":
    sys.exit(main())
