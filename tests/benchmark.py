#!/usr/bin/env python3
"""Reproducible same-second collision benchmark; temporary synthetic data only.

This measures the complete read-only preview. It is not a disk-throughput benchmark.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time


def measure(binary, root, repeats):
    timings = []
    for _ in range(repeats):
        start = time.perf_counter()
        run = subprocess.run(
            [str(binary), "preview", str(root), "--timezone", "utc", "--json", "--duplicates", "keep-all"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=180,
        )
        if run.returncode:
            raise RuntimeError(run.stderr.decode(errors="replace"))
        timings.append(round(time.perf_counter() - start, 6))
    return {"seconds": timings, "median_seconds": statistics.median(timings)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--files", type=int, default=4000)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if args.files < 1 or args.repeats < 1:
        parser.error("--files and --repeats must be positive")
    result = {"scenario": "unique small files, identical mtime, same extension", "files": args.files, "repeats": args.repeats}
    with tempfile.TemporaryDirectory(prefix="hizuke-benchmark-") as directory:
        root = Path(directory)
        for index in range(args.files):
            path = root / f"camera-{index:06}.jpg"
            path.write_bytes(f"synthetic unique photo {index}\n".encode() * 8)
            os.utime(path, ns=(1704164645000000000,) * 2)
        if args.baseline:
            result["baseline"] = measure(args.baseline.resolve(), root, args.repeats)
        result["current"] = measure(args.binary.resolve(), root, args.repeats)
        for state_name in (".hizuke", ".imgrename"):
            assert not (root / state_name).exists(), "preview must not write state"
        if args.baseline:
            result["median_speedup"] = round(result["baseline"]["median_seconds"] / result["current"]["median_seconds"], 2)
    report = json.dumps(result, indent=2) + "\n"
    print(report, end="")
    if args.report:
        args.report.write_text(report)


if __name__ == "__main__":
    main()
