#!/usr/bin/env python3
"""Rewrite the examples' expected results as the release binary writes them.

For each `examples/<name>.json`, the binary's result (one thread, so that a
rerun gives the same solutions) replaces `examples/<name>.result.json`, but
only if it agrees with the file that is there: the same status, the same
objective (1e-6, relative), and with "solutions": "all" (or a number that
found them all) the same set of solutions. The expected results came from
reference solvers and brute force (CLAUDE.md), so a disagreement is a finding,
not something to overwrite: the file is left as it is, and the exit status is
1. A file that would change in nothing but its solve time is left alone too.

    cargo build --release && python3 tools/regenerate_results.py [NAMES...]

Use it after the result format changes. For a new example, make its first
result with a reference solver (CLAUDE.md), then run this.
"""
import glob
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("OPTIMIZAM_BIN", os.path.join(ROOT, "target", "release", "optimizam"))
TIME = 600  # tests/regression.rs's limit


def close(a, b):
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return abs(a - b) <= 1e-6 * max(1.0, abs(a), abs(b))
    return a == b


def rounded(value):
    """A value with its numbers rounded, as text: solutions compare as sets."""
    if isinstance(value, dict):
        return "{" + ",".join(f"{k}:{rounded(v)}" for k, v in sorted(value.items())) + "}"
    if isinstance(value, float) or isinstance(value, int):
        return f"{round(float(value), 6) + 0.0:g}"
    return json.dumps(value)


def layout(value):
    """The keys of a solution's variables, without their values."""
    return {k: layout(v) for k, v in value.items()} if isinstance(value, dict) else None


def differs(problem, old, new):
    """Why the new result disagrees with the old one, or None."""
    if old["status"] != new["status"]:
        return f"status {new['status']}, was {old['status']}"
    if len(old["solutions"]) != len(new["solutions"]):
        return f"{len(new['solutions'])} solutions, were {len(old['solutions'])}"
    wanted = problem["solve"].get("solutions", "one")
    every = wanted == "all" or (isinstance(wanted, int) and old["status"] == "complete")
    if every:
        if sorted(rounded(s) for s in old["solutions"]) != sorted(rounded(s) for s in new["solutions"]):
            return "another set of solutions"
        return None
    for a, b in zip(old["solutions"], new["solutions"]):
        if not close(a.get("objective"), b.get("objective")):
            return f"objective {b.get('objective')}, was {a.get('objective')}"
        if layout(a["variables"]) != layout(b["variables"]):
            return "other variables"
    return None


def without_time(result):
    result = json.loads(json.dumps(result))
    result.get("statistics", {}).pop("time", None)
    return result


def main():
    names = sys.argv[1:] or sorted(
        os.path.basename(f)[: -len(".json")]
        for f in glob.glob(os.path.join(ROOT, "examples", "*.json"))
        if not f.endswith(".result.json")
    )
    bad = 0
    for name in names:
        problem_path = os.path.join(ROOT, "examples", name + ".json")
        result_path = os.path.join(ROOT, "examples", name + ".result.json")
        run = subprocess.run(
            [BIN, "--threads=1", f"--time={TIME}", "--quiet", problem_path, "-"], capture_output=True, text=True
        )
        if run.returncode != 0:
            print(f"{name:28s} FAILED     {run.stderr.strip()}")
            bad += 1
            continue
        new = json.loads(run.stdout)
        if not os.path.exists(result_path):
            print(f"{name:28s} NO RESULT  make the first one with a reference solver")
            bad += 1
            continue
        with open(result_path) as f:
            old = json.load(f)
        with open(problem_path) as f:
            why = differs(json.load(f), old, new)
        if why:
            print(f"{name:28s} DIFFERENT  {why}")
            bad += 1
        elif without_time(old) == without_time(new):
            print(f"{name:28s} unchanged")
        else:
            with open(result_path, "w") as f:
                f.write(run.stdout)
            print(f"{name:28s} rewritten  {new['status']}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
