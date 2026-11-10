#!/usr/bin/env python3
"""Scaling benchmark of optimizam's local nonlinear solve (--engine=nlp).

Generates continuous nonlinear problems of growing size from fixed seeds
and times the release binary on each, which shows how the KKT
factorization scales (sparse LDL^T for large sparse systems, dense
Bunch-Kaufman otherwise):

  chain-N   N variables in [-10, 10], objective sum (x_i - a_i)^2 +
            0.1 x_i x_{i+1}, and N rows x_i^2 + x_{i+1}^2 + sin(x_{i+7}) <= b
            (each row touches 3 variables: a very sparse KKT system);
  wide-N    the same objective, N rows sum of 20 random c_k x_k^2 <= b
            (20 variables per row: much more fill).

With --ipopt, IPOPT (tools/ipopt.sh, the project's .venv with casadi) solves
each problem too, and objectives that differ by more than 1e-6 relative are
flagged (both are local solvers; on these problems they should agree).
The times include optimizam's root relaxation (used to report proven
optima), which for the largest problems costs more than the local solve.

Usage (from the repository root, after `cargo build --release`):
    python3 tools/nlp_scaling.py [--ipopt] [--sizes 50,100,...] [names...]
"""

import argparse
import json
import os
import random
import subprocess
import sys
import time

# One thread unless OPTIMIZAM_THREADS says otherwise (optimizam's own default
# is one per core): timings and search paths then repeat from run to run.
os.environ.setdefault("OPTIMIZAM_THREADS", "1")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def problem(kind, n):
    rng = random.Random(f"{kind}-{n}")
    variables = [{"name": f"x{i}", "type": "continuous", "lower": -10, "upper": 10, "start": 0.5} for i in range(n)]
    objective = " + ".join(f"(x{i} - {rng.uniform(-2, 2):.2f})^2 + 0.1 * x{i} * x{(i + 1) % n}" for i in range(n))
    constraints = []
    for k in range(n):
        if kind == "chain":
            i, j, l = k % n, (k + 1) % n, (k + 7) % n
            expr = f"x{i}^2 + x{j}^2 + sin(x{l}) <= {rng.uniform(2, 5):.2f}"
        else:
            terms = " + ".join(f"{rng.uniform(0.5, 1.5):.2f} * x{i}^2" for i in rng.sample(range(n), min(20, n)))
            expr = f"{terms} <= {rng.uniform(50, 80):.1f}"
        constraints.append({"name": f"c{k}", "expr": expr})
    return {"version": "1.2", "variables": variables, "constraints": constraints,
            "solve": {"method": "minimize", "objective": objective}}


def run(command, path, time_limit):
    start = time.time()
    r = subprocess.run(command + [f"--time={time_limit}", path, "-"], capture_output=True, text=True)
    elapsed = time.time() - start
    if r.returncode != 0:
        return elapsed, "error", None
    out = json.loads(r.stdout)
    sols = out["solutions"]
    return elapsed, out["status"], sols[0].get("objective") if sols else None


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--ipopt", action="store_true")
    ap.add_argument("--sizes", default="50,100,200,400,1000")
    ap.add_argument("--time", type=float, default=120.0)
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"))
    ap.add_argument("names", nargs="*")
    a = ap.parse_args()
    sizes = [int(s) for s in a.sizes.split(",")]
    cases = [f"chain-{n}" for n in sizes] + [f"wide-{n}" for n in sizes if 20 <= n <= 400]
    if a.names:
        cases = [c for c in cases if any(c == x or c.startswith(x) for x in a.names)]
    work = os.path.join(ROOT, "target", "nlp_scaling")
    os.makedirs(work, exist_ok=True)
    header = f"{'problem':12} {'seconds':>8} {'status':18} {'objective':>16}"
    if a.ipopt:
        header += f" {'ipopt s':>8} {'ipopt objective':>16}"
    print(header)
    flagged = 0
    for case in cases:
        kind, n = case.split("-")
        path = os.path.join(work, f"{case}.json")
        with open(path, "w") as f:
            json.dump(problem(kind, int(n)), f)
        t, status, obj = run([a.bin, "--engine=nlp"], path, a.time)
        line = f"{case:12} {t:8.2f} {status:18} {obj if obj is not None else '':>16}"
        if a.ipopt:
            ti, _, obj_i = run([os.path.join(ROOT, "tools", "ipopt.sh")], path, a.time)
            line += f" {ti:8.2f} {obj_i if obj_i is not None else '':>16}"
            if obj is not None and obj_i is not None and abs(obj - obj_i) > 1e-6 * (1 + abs(obj_i)):
                line += "  DIFFERENT"
                flagged += 1
        print(line, flush=True)
    if a.ipopt:
        print(f"{flagged} objectives differ from IPOPT's")
    sys.exit(0)


if __name__ == "__main__":
    main()
