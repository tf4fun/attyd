#!/usr/bin/env python3
"""Compare complete Feed publication costs before and after removing the R3 publisher.

Run in CI, on one runner, sequentially. Only the fixture's Feed constructor is adapted to
the old API; the baseline production code is unchanged. Timings are evidence, not gates.
"""
import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[1]
BASELINE = "ab4f0c5"
FIXTURE = Path("src/protocol/feed/perf_tests.rs")
ROW = re.compile(
    r"feed_single_chunk entries=2000 turns=500 output=(\d+) chunks=(\d+) mode=(\w+) "
    r"median_us=(\d+) p95_us=(\d+) allocation_bytes=(\d+) allocation_calls=(\d+)"
)


def run(command, cwd, env, log):
    with subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, text=True) as process:
        for line in process.stdout:
            print(line, end="", flush=True)
            log.write(line)
            log.flush()
        if process.wait() != 0:
            raise RuntimeError(f"command failed: {command!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", default=BASELINE)
    args = parser.parse_args()
    baseline = subprocess.check_output(["git", "rev-parse", args.baseline], cwd=ROOT, text=True).strip()
    current = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    output = ROOT / "artifacts/protocol-benchmark"
    output.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "ATTYD_SKIP_WEB_BUILD": "1", "CARGO_TARGET_DIR": str(ROOT / "target")}
    results = {}
    with tempfile.TemporaryDirectory(prefix="attyd-protocol-baseline-") as temporary:
        before = Path(temporary)
        archive = before / "source.tar"
        with archive.open("wb") as target:
            subprocess.run(["git", "archive", baseline], cwd=ROOT, stdout=target, check=True)
        with tarfile.open(archive) as source:
            source.extractall(before, filter="data")
        archive.unlink()
        fixture = (ROOT / FIXTURE).read_text()
        module_path = before / "src/protocol/feed.rs"
        module = module_path.read_text()
        if "pub(crate) fn new(process_epoch:" in module:
            start = fixture.index("// BEGIN BENCH FEED ADAPTER")
            end = fixture.index("// END BENCH FEED ADAPTER") + len("// END BENCH FEED ADAPTER")
            fixture = fixture[:start] + 'fn new_feed() -> Feed { Feed::new("epoch") }\n' + fixture[end:]
        (before / FIXTURE).parent.mkdir(parents=True, exist_ok=True)
        (before / FIXTURE).write_text(fixture)
        if "mod perf_tests;" not in module:
            module_path.write_text(module + "\n#[cfg(test)]\nmod perf_tests;\n")
        shutil.copytree(ROOT / "dist/client", before / "dist/client")
        for label, checkout in [("before", before), ("after", ROOT)]:
            log_path = output / f"{label}.log"
            with log_path.open("w") as log:
                # A shared target reuses dependencies, but never the application's binary or
                # fingerprint from another checkout. No concurrent builds use this runner.
                run(["cargo", "clean", "--release", "-p", "attyd"], checkout, env, log)
                os.utime(checkout / "src/main.rs", None)
                run(["cargo", "test", "--release", "--locked", "feed_single_chunk_in_a_long_session",
                     "--", "--ignored", "--nocapture", "--test-threads=1"], checkout, env, log)
            rows = ROW.findall(log_path.read_text())
            if len(rows) != 6:
                raise RuntimeError(f"{label}: expected six benchmark rows, found {len(rows)}")
            results[label] = rows
    summary = ["## Complete Feed publication benchmark", "", f"Before: `{baseline}`; after: `{current}`.",
               "", "Same runner and release profile; 2,000 entries / 500 turns; 5 warm-ups and 30 measured updates per row.",
               "Each sample measures only production `Feed::commit_ending`, including every publisher it runs and subscriber queueing. The reducer runs before measurement; construction, assertions, queue consumption and network I/O are excluded. The window case follows a 4 KiB output window and consumes each event between commits; unfollowed has no subscribers.",
               "Allocation figures count requested bytes, not RSS or peak memory. This measures publication of a small append, not checkpoint timing, serialization by the HTTP response, or end-to-end Agent latency.",
               "", "| Output bytes / blocks | Mode | Median µs before → after | p95 µs before → after | Allocated bytes before → after | Allocations before → after |",
               "| --- | --- | --- | --- | --- | --- |"]
    for before, after in zip(results["before"], results["after"]):
        if before[:3] != after[:3]:
            raise RuntimeError("the two revisions ran different scenarios")
        size, chunks, mode = before[:3]
        comparisons = [f"{left} → {right}" for left, right in zip(before[3:], after[3:])]
        summary.append(f"| {size} / {chunks} | {mode} | " + " | ".join(comparisons) + " |")
    summary += ["", "No timing threshold is enforced. Full logs are retained as an artifact.", ""]
    report = "\n".join(summary)
    (output / "summary.md").write_text(report)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as target:
            target.write(report)
    print(report)


if __name__ == "__main__":
    main()
