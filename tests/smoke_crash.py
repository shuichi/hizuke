#!/usr/bin/env python3
"""Observe actual filesystem phases; verify SIGKILL recovery and SIGINT rollback."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

BINARY: Path
COUNT = 48
RESULTS = []


def fixture(root: Path):
    manifest = {}
    for index in range(COUNT):
        name = f"camera-{index:03}.jpg"
        contents = (f"synthetic image {index:03}; ".encode() * 16000)[:262144]
        path = root / name
        path.write_bytes(contents)
        os.utime(path, ns=((1700000000 + index) * 1000000000,) * 2)
        with path.open("rb") as stream:
            os.fsync(stream.fileno())
        manifest[name] = (hashlib.sha256(contents).hexdigest(), path.stat().st_mtime_ns)
    return manifest


def manifest(root: Path):
    return {
        path.name: (hashlib.sha256(path.read_bytes()).hexdigest(), path.stat().st_mtime_ns)
        for path in root.glob("*.jpg")
    }


def snapshot(root: Path):
    paths = list(root.glob("*.jpg"))
    staging = list(root.glob(".hizuke/transactions/*/staging/*"))
    return {
        "original": sum(path.name.startswith("camera-") for path in paths),
        "final": sum(not path.name.startswith("camera-") for path in paths),
        "staged": len(staging),
    }


def run_cli(root: Path, command: str):
    commandline = [str(BINARY), command, str(root), "--yes"]
    if command == "apply":
        commandline += ["--timezone", "utc", "--duplicates", "keep-all"]
    result = subprocess.run(commandline, capture_output=True, text=True, timeout=60)
    if result.returncode:
        raise AssertionError(f"{command} failed: {result.stdout}\n{result.stderr}")
    return result


def kill_on_phase(root: Path, logdir: Path, command: str, phase: str, requested_signal=signal.SIGKILL):
    commandline = [str(BINARY), command, str(root), "--yes"]
    if command == "apply":
        commandline += ["--timezone", "utc", "--duplicates", "keep-all"]
    with (logdir / "stdout.log").open("w") as out, (logdir / "stderr.log").open("w") as err:
        process = subprocess.Popen(commandline, stdout=out, stderr=err)
        deadline = time.monotonic() + 60
        while True:
            state = snapshot(root)
            if phase == "apply_staging":
                reached = state["staged"] >= 3 and state["original"] > 10 and state["final"] == 0
            elif phase == "apply_finalizing":
                reached = state["final"] >= 3 and state["staged"] > 10 and state["original"] == 0
            elif phase == "undo_restoring":
                reached = state["original"] >= 3 and state["staged"] > 10 and state["final"] == 0
            else:
                raise AssertionError(phase)
            if reached:
                os.kill(process.pid, requested_signal)
                exit_code = process.wait(timeout=60)
                expected_code = 130 if requested_signal == signal.SIGINT else -requested_signal
                assert exit_code == expected_code, (exit_code, (logdir / "stderr.log").read_text())
                return state, snapshot(root)
            if process.poll() is not None:
                raise AssertionError(f"process exited before observed phase {phase}: {(logdir / 'stderr.log').read_text()}")
            if time.monotonic() >= deadline:
                process.kill()
                process.wait(timeout=10)
                raise AssertionError(f"timed out observing {phase}: {state}")
            # Polling is gated on observed entries; elapsed time never selects a phase.
            time.sleep(0.001)


def scenario(phase: str):
    with tempfile.TemporaryDirectory(prefix=f"hizuke-crash-{phase}-") as temporary:
        logdir = Path(temporary)
        root = logdir / "photos"
        root.mkdir()
        expected = fixture(root)
        if phase == "undo_restoring":
            run_cli(root, "apply")
            assert snapshot(root) == {"original": 0, "final": COUNT, "staged": 0}
        observed, after_kill = kill_on_phase(root, logdir, "undo" if phase == "undo_restoring" else "apply", phase)
        run_cli(root, "recover")
        assert manifest(root) == expected, f"restoration mismatch in {phase}"
        assert snapshot(root) == {"original": COUNT, "final": 0, "staged": 0}
        records = subprocess.run([str(BINARY), "history", str(root), "--json"], capture_output=True, text=True, check=True)
        history = json.loads(records.stdout)
        assert history[-1]["status"] == ("undone" if phase == "undo_restoring" else "rolled_back")
        result = {"phase": phase, "observed_before_sigkill": observed, "after_sigkill": after_kill,
                  "restored_files": COUNT, "bytes_and_mtimes_restored": True,
                  "status": history[-1]["status"]}
        RESULTS.append(result)
        print(json.dumps(result), flush=True)


def graceful_interrupt_scenario():
    with tempfile.TemporaryDirectory(prefix="hizuke-sigint-staging-") as temporary:
        logdir = Path(temporary)
        root = logdir / "photos"
        root.mkdir()
        expected = fixture(root)
        observed, after_interrupt = kill_on_phase(
            root, logdir, "apply", "apply_staging", requested_signal=signal.SIGINT,
        )
        # The first SIGINT performs rollback itself; no recover command is needed.
        assert manifest(root) == expected, "SIGINT failed to restore bytes and mtimes"
        assert snapshot(root) == {"original": COUNT, "final": 0, "staged": 0}
        records = subprocess.run(
            [str(BINARY), "history", str(root), "--json"],
            capture_output=True, text=True, check=True,
        )
        history = json.loads(records.stdout)
        assert history[-1]["status"] == "rolled_back", history
        result = {
            "phase": "sigint_apply_staging", "observed_before_sigint": observed,
            "after_sigint": after_interrupt, "exit_code": 130,
            "restored_files": COUNT, "bytes_and_mtimes_restored": True,
            "status": "rolled_back",
        }
        RESULTS.append(result)
        print(json.dumps(result), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/hizuke"), help="Path to the built hizuke executable")
    parser.add_argument("--report", type=Path, help="Optional JSON result file")
    options = parser.parse_args()
    if os.name != "posix":
        parser.error("this SIGKILL smoke test requires macOS or Linux")
    BINARY = options.binary.resolve(strict=True)
    for phase in ["apply_staging", "apply_finalizing", "undo_restoring"]:
        scenario(phase)
    graceful_interrupt_scenario()
    if options.report:
        options.report.write_text(json.dumps(RESULTS, indent=2) + "\n")
