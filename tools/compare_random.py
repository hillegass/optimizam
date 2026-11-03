#!/usr/bin/env python3
"""Compare optimizam with the HiGHS reference on random MILPs.

Generates random problems from fixed seeds, solves each with optimizam and
with tools/highs.sh, and checks that they agree: the same status (or both
rejecting the problem), and objectives within 1e-6 relative tolerance.
Problems that disagree are saved so they can be rerun by hand.

Usage (from the repository root, after `cargo build --release`):
    python3 tools/compare_random.py [--kind facility|general|both] [--count N]
        [--seed S] [--time SECONDS] [--bin PATH] [--save-dir DIR]

Kinds:
  facility  Facility location with binary open, integer modules with
            `modules <= k * open`, continuous serve with
            `serve <= demand * open`, and sometimes a side row with
            fractional data. Exercises variable-bound substitution and
            projection in the c-MIR cuts.
  general   Unstructured problems: random rows (<=, >=, ==) over a mix of
            binary, integer and continuous columns, negative and missing
            bounds, fractional data, minimize or maximize.

Exits with status 1 if any problem disagrees. Uses only the standard
library; the reference needs the project's .venv (see CLAUDE.md).
"""

import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL_TOL = 1e-6


def facility_problem(rng):
    F, C = rng.randint(2, 5), rng.randint(2, 7)
    kmax, size = rng.randint(1, 4), rng.randint(5, 40)
    fs = [f"F{i}" for i in range(F)]
    cs = [f"C{i}" for i in range(C)]
    p = {
        "version": "1.0",
        "sets": {"F": fs, "C": cs},
        "parameters": {
            "oc": {"index": ["F"], "values": {f: rng.randint(0, 900) for f in fs}},
            "mc": {"index": ["F"], "values": {f: rng.randint(0, 300) for f in fs}},
            "uc": {"index": ["F", "C"], "values": {f: {c: rng.randint(0, 20) for c in cs} for f in fs}},
            "d": {"index": ["C"], "values": {c: rng.randint(1, 30) for c in cs}},
            "size": size,
        },
        "variables": [
            {"name": "open", "type": "binary", "index": ["F"]},
            {"name": "mods", "type": "integer", "index": ["F"], "lower": 0, "upper": kmax},
            {"name": "serve", "type": "continuous", "index": ["F", "C"], "lower": 0},
        ],
        "constraints": [
            {"name": "dem", "forall": "c in C", "expr": f"sum(f in F) serve[f,c] {rng.choice(['==', '>='])} d[c]"},
            {"name": "cap", "forall": "f in F", "expr": "sum(c in C) serve[f,c] <= size * mods[f]"},
            {"name": "link", "forall": "f in F", "expr": f"mods[f] <= {kmax} * open[f]"},
            {"name": "vub", "forall": "f in F, c in C", "expr": "serve[f,c] <= d[c] * open[f]"},
        ],
        "solve": {
            "method": "minimize",
            "objective": "sum(f in F) (oc[f]*open[f] + mc[f]*mods[f]) + sum(f in F, c in C) uc[f,c]*serve[f,c]",
            "solutions": "one",
        },
    }
    if rng.random() < 0.3:
        p["constraints"].append({
            "name": "side",
            "expr": f"sum(f in F) {rng.randint(1, 9)}.5 * mods[f] - sum(f in F) open[f] >= {rng.randint(1, 20)}.25",
        })
    return p


def general_problem(rng):
    n, m = rng.randint(2, 8), rng.randint(1, 6)
    variables = []
    for j in range(n):
        t = rng.choice(["integer", "integer", "binary", "continuous"])
        v = {"name": f"x{j}", "type": t}
        if t != "binary":
            lo = rng.choice([0, 0, -3])
            v["lower"] = lo
            if rng.random() < 0.8:
                v["upper"] = lo + rng.randint(1, 9)
        variables.append(v)
    constraints = []
    for _ in range(m):
        terms = [f"{rng.randint(-9, 9) + rng.choice([0, 0, 0.5, 0.25])} * x{j}" for j in range(n) if rng.random() < 0.7]
        lhs = " + ".join(terms) or "0 * x0"
        rhs = rng.randint(-10, 30) + rng.choice([0, 0.5])
        constraints.append({"expr": f"{lhs} {rng.choice(['<=', '<=', '>=', '=='])} {rhs}"})
    return {
        "version": "1.0",
        "variables": variables,
        "constraints": constraints,
        "solve": {
            "method": rng.choice(["minimize", "maximize"]),
            "objective": " + ".join(f"{rng.randint(-9, 9)} * x{j}" for j in range(n)),
            "solutions": "one",
        },
    }


GENERATORS = {"facility": facility_problem, "general": general_problem}


def solve(command, path, time_limit):
    """Returns ("error", stderr) if the solver rejected the problem, else ("ok", result)."""
    r = subprocess.run([command, f"--time={time_limit}", path, "-"], capture_output=True, text=True)
    if r.returncode != 0:
        return "error", r.stderr.strip()
    return "ok", json.loads(r.stdout)


def disagreement(a, b):
    """None if the two outcomes agree, else a description."""
    if a[0] != b[0]:
        return f"optimizam {a[0]}: {a[1] if a[0] == 'error' else a[1]['status']}; " \
               f"highs {b[0]}: {b[1] if b[0] == 'error' else b[1]['status']}"
    if a[0] == "error":
        return None
    ra, rb = a[1], b[1]
    if ra["status"] != rb["status"]:
        return f"status: optimizam {ra['status']}, highs {rb['status']}"
    if ra["solutions"] and rb["solutions"]:
        oa, ob = ra["solutions"][0].get("objective"), rb["solutions"][0].get("objective")
        if oa is not None and ob is not None and abs(oa - ob) > REL_TOL * max(1.0, abs(ob)):
            return f"objective: optimizam {oa}, highs {ob}"
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--kind", choices=["facility", "general", "both"], default="both")
    ap.add_argument("--count", type=int, default=200, help="problems per kind (default 200)")
    ap.add_argument("--seed", type=int, default=0, help="first seed (default 0)")
    ap.add_argument("--time", type=float, default=20, help="time limit per solve in seconds (default 20)")
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"), help="optimizam binary")
    ap.add_argument("--save-dir", default="compare_random_failures", help="where disagreeing problems are saved")
    args = ap.parse_args()

    if not os.path.exists(args.bin):
        sys.exit(f"{args.bin} not found; run `cargo build --release` or pass --bin")
    reference = os.path.join(ROOT, "tools", "highs.sh")
    kinds = ["facility", "general"] if args.kind == "both" else [args.kind]

    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        for kind in kinds:
            statuses = {}
            for seed in range(args.seed, args.seed + args.count):
                problem = GENERATORS[kind](random.Random(f"{kind}-{seed}"))
                path = os.path.join(tmp, f"{kind}_{seed}.json")
                with open(path, "w") as f:
                    json.dump(problem, f, indent=2)
                a = solve(args.bin, path, args.time)
                b = solve(reference, path, args.time)
                status = a[1]["status"] if a[0] == "ok" else "error"
                statuses[status] = statuses.get(status, 0) + 1
                why = disagreement(a, b)
                if why:
                    failures += 1
                    os.makedirs(args.save_dir, exist_ok=True)
                    saved = shutil.copy(path, args.save_dir)
                    print(f"MISMATCH {kind} seed {seed} ({saved}): {why}")
            summary = ", ".join(f"{k} {v}" for k, v in sorted(statuses.items()))
            print(f"{kind}: {args.count} problems ({summary})")
    print(f"{failures} mismatches")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
