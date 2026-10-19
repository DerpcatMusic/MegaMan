#!/usr/bin/env python3
"""Run the owned directory-model fixture; retain failures and raw samples."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess
import time


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def percentile(values, fraction):
    # Explicit nearest-rank definition; retain all raw observations as well.
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def summarize(values):
    return {
        "count": len(values),
        "minimum_ns": min(values),
        "median_ns": statistics.median(values),
        "p95_nearest_rank_ns": percentile(values, 0.95),
        "maximum_ns": max(values),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--entries", nargs="+", type=int, default=[1000, 10000, 100000])
    parser.add_argument("--samples", type=int, default=15)
    args = parser.parse_args()
    if (
        not args.binary.is_absolute()
        or not args.binary.is_file()
        or not os.access(args.binary, os.X_OK)
        or not args.output.is_absolute()
        or not 3 <= args.samples <= 100
        or len(set(args.entries)) != len(args.entries)
        or not args.entries
        or any(not 1 <= count <= 1000000 for count in args.entries)
    ):
        parser.error("use an absolute executable, new absolute output, unique bounded counts and 3..100 samples")
    repo = Path(__file__).resolve().parent.parent
    source = git(repo, "rev-parse", "HEAD")
    if not re.fullmatch(r"[0-9a-f]{40}", source):
        raise RuntimeError("expected full source commit")
    if git(repo, "status", "--porcelain", "--untracked-files=all"):
        raise RuntimeError("commit the fixture source before measuring")
    args.output.mkdir()  # Never reuse, clear or adopt an existing output tree.
    manifest = {
        "schema_version": 1,
        "kind": "directory_model_measurement",
        "status": "running",
        "source_commit": source,
        "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
        "binary_bytes": args.binary.stat().st_size,
        "utc_started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "logical_cpus": os.cpu_count(),
        "cpu_model": next(
            (line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")),
            "unknown",
        ) if Path("/proc/cpuinfo").exists() else "unknown",
        "fixture_filesystem": subprocess.check_output(
            ["findmnt", "--target", str(args.output), "--noheadings", "--output", "FSTYPE"], text=True,
        ).strip() if shutil.which("findmnt") else "unknown",
        "samples_per_count": args.samples,
        "cache_state": "freshly created empty files; no OS cache flush; repeated warm rescans",
        "polling": "100-microsecond sleep between nonblocking model polls; scheduling/observation overhead included",
        "measurement_boundaries": {
            "initial": "Browser construction, worker spawn, enumeration, sort, catalog publication and observed poll acceptance",
            "rescan": "same-path navigate through fresh enumeration and observed poll acceptance; fixture creation and full result oracle excluded",
            "selection": "100000 Clear+SelectAll action pairs including black_box/count and loop overhead; full one-range oracle excluded",
        },
        "limitations": [
            "not native UI startup, rendering or input-to-present latency",
            "not worker-only CPU time or an isolated action cost",
            "not cold-cache or network-filesystem qualification",
            "not a comparative performance claim",
            "whole run shares host load with other processes",
        ],
        "results": [],
    }
    result_path = args.output / "result.json"

    def save():
        result_path.write_text(json.dumps(manifest, indent=2) + "\n")

    save()
    try:
        for count in args.entries:
            command = [str(args.binary), str(args.output / f"fixture-{count}"), str(count), str(args.samples)]
            completed = subprocess.run(command, capture_output=True, text=True, timeout=300, check=False)
            (args.output / f"{count}-stdout.json").write_text(completed.stdout)
            (args.output / f"{count}-stderr.txt").write_text(completed.stderr)
            if completed.returncode:
                raise RuntimeError(f"fixture {count} failed with exit {completed.returncode}; inspect retained logs")
            result = json.loads(completed.stdout)
            if (
                result.get("source_commit") != source
                or result.get("entries") != count
                or result.get("samples") != args.samples
                or result.get("correctness") != "passed"
                or len(result.get("same_path_rescan_to_polled_delivery_ns", [])) != args.samples
                or len(result.get("selection_pairs_total_ns", [])) != args.samples
            ):
                raise RuntimeError(f"fixture {count} provenance or correctness mismatch")
            result["rescan_summary"] = summarize(result["same_path_rescan_to_polled_delivery_ns"])
            result["selection_pairs_summary"] = summarize(result["selection_pairs_total_ns"])
            manifest["results"].append(result)
            save()
        if hashlib.sha256(args.binary.read_bytes()).hexdigest() != manifest["binary_sha256"]:
            raise RuntimeError("measurement executable changed during the run")
        manifest["status"] = "passed"
        save()
    except BaseException as error:
        manifest["status"] = "failed"
        manifest["failure"] = str(error)
        save()
        raise
    print(result_path)


if __name__ == "__main__":
    main()
