#!/usr/bin/env python3
"""Benchmark optimizam's MILP solver on generated instances of classic classes.

The instances are generated from fixed seeds, so every run solves the same
problems. They are sized to take real effort (seconds to a minute), and each
class stresses a different part of the solver:

  cfl_large    capacitated facility location with modules: cuts (c-MIR
               with variable bounds and projections)
  gap          generalized assignment: finding feasible solutions (RENS)
  lotsize      lot sizing with loose big-M setups: presolve's coefficient
  lotsize_big  tightening, then RINS and the root bound
  mknap        multidimensional knapsack: weak bounds, a large tree
  setcover     set covering: heuristics and branching
  windows      time-window scheduling with many fixed columns: presolve

Usage (from the repository root, after `cargo build --release`):
    python3 tools/milp_benchmark.py [--time SECONDS] [--highs] [--bin PATH]
        [--dir DIR] [names...]

Prints, per instance, the wall time, status, objective and (when not proven
optimal) the bound. With --highs, also runs the HiGHS reference
(tools/highs.sh, which needs the project's .venv) and flags objectives that
disagree. Instances are written to target/milp_benchmark/ by default.
Uses only the standard library.
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
REL_TOL = 1e-6


def tab(keys, fn):
    return {str(k): fn(k) for k in keys}


def cfl(rng, F, C, K, size):
    fs = [f"F{i:02d}" for i in range(F)]
    cs = [f"C{i:03d}" for i in range(C)]
    return {
        "version": "1.3", "sets": {"F": fs, "C": cs},
        "parameters": {
            "oc": {"index": ["F"], "values": tab(fs, lambda f: rng.randint(300, 900))},
            "mc": {"index": ["F"], "values": tab(fs, lambda f: rng.randint(150, 300))},
            "uc": {"index": ["F", "C"], "values": tab(fs, lambda f: tab(cs, lambda c: rng.randint(1, 12)))},
            "d": {"index": ["C"], "values": tab(cs, lambda c: rng.randint(5, 35))},
            "size": size, "K": K,
        },
        "variables": [
            {"name": "open", "type": "binary", "index": ["F"]},
            {"name": "mods", "type": "integer", "index": ["F"], "lower": 0, "upper": K},
            {"name": "serve", "type": "continuous", "index": ["F", "C"], "lower": 0},
        ],
        "constraints": [
            {"name": "dem", "forall": "c in C", "expr": "sum(f in F) serve[f,c] == d[c]"},
            {"name": "cap", "forall": "f in F", "expr": "sum(c in C) serve[f,c] <= size * mods[f]"},
            {"name": "link", "forall": "f in F", "expr": "mods[f] <= K * open[f]"},
            {"name": "vub", "forall": "f in F, c in C", "expr": "serve[f,c] <= d[c] * open[f]"},
        ],
        "solve": {"method": "minimize", "solutions": "one",
                  "objective": "sum(f in F) (oc[f]*open[f] + mc[f]*mods[f]) + sum(f in F, c in C) uc[f,c]*serve[f,c]"},
    }


def mknap(rng, n, m, tight):
    items = [f"i{j:02d}" for j in range(n)]
    res = [f"r{k}" for k in range(m)]
    w = {r: {i: rng.randint(5, 60) for i in items} for r in res}
    return {
        "version": "1.3", "sets": {"I": items, "R": res},
        "parameters": {
            "w": {"index": ["R", "I"], "values": w},
            "v": {"index": ["I"], "values": {i: sum(w[r][i] for r in res) // m + rng.randint(0, 30) for i in items}},
            "cap": {"index": ["R"], "values": {r: int(tight * sum(w[r].values())) for r in res}},
        },
        "variables": [{"name": "x", "type": "binary", "index": ["I"]}],
        "constraints": [{"name": "c", "forall": "r in R", "expr": "sum(i in I) w[r,i] * x[i] <= cap[r]"}],
        "solve": {"method": "maximize", "objective": "sum(i in I) v[i] * x[i]", "solutions": "one"},
    }


def gap(rng, A, J, tight):
    agents = [f"a{k}" for k in range(A)]
    jobs = [f"j{k:02d}" for k in range(J)]
    w = {a: {j: rng.randint(5, 25) for j in jobs} for a in agents}
    return {
        "version": "1.3", "sets": {"A": agents, "J": jobs},
        "parameters": {
            "w": {"index": ["A", "J"], "values": w},
            "c": {"index": ["A", "J"], "values": {a: {j: rng.randint(10, 50) for j in jobs} for a in agents}},
            "cap": {"index": ["A"], "values": {a: int(tight * sum(w[a].values()) / A) for a in agents}},
        },
        "variables": [{"name": "x", "type": "binary", "index": ["A", "J"]}],
        "constraints": [
            {"name": "assign", "forall": "j in J", "expr": "sum(a in A) x[a,j] == 1"},
            {"name": "capacity", "forall": "a in A", "expr": "sum(j in J) w[a,j] * x[a,j] <= cap[a]"},
        ],
        "solve": {"method": "minimize", "objective": "sum(a in A, j in J) c[a,j] * x[a,j]", "solutions": "one"},
    }


def lotsize(rng, P, T):
    products = [f"p{k}" for k in range(P)]
    d = {p: {str(t): rng.choice([0, rng.randint(10, 60)]) for t in range(1, T + 1)} for p in products}
    big_m = sum(sum(v.values()) for v in d.values())  # deliberately loose
    return {
        "version": "1.3", "sets": {"P": products, "T": {"from": 1, "to": T}},
        "parameters": {
            "d": {"index": ["P", "T"], "values": d},
            "setup": {"index": ["P"], "values": {p: rng.randint(80, 200) for p in products}},
            "hold": {"index": ["P"], "values": {p: rng.randint(1, 4) for p in products}},
            "capacity": int(1.3 * big_m / T), "M": big_m,
        },
        "variables": [
            {"name": "make", "type": "continuous", "index": ["P", "T"], "lower": 0},
            {"name": "stock", "type": "continuous", "index": ["P", "T"], "lower": 0},
            {"name": "on", "type": "binary", "index": ["P", "T"]},
        ],
        "constraints": [
            {"name": "first", "forall": "p in P, t in T: t == 1", "expr": "make[p,t] - stock[p,t] == d[p,t]"},
            {"name": "flow", "forall": "p in P, t in T: t > 1", "expr": "stock[p,t-1] + make[p,t] - stock[p,t] == d[p,t]"},
            {"name": "setup_link", "forall": "p in P, t in T", "expr": "make[p,t] <= M * on[p,t]"},
            {"name": "cap", "forall": "t in T", "expr": "sum(p in P) make[p,t] <= capacity"},
        ],
        "solve": {"method": "minimize", "solutions": "one",
                  "objective": "sum(p in P, t in T) (setup[p] * on[p,t] + hold[p] * stock[p,t])"},
    }


def setcover(rng, E, S, k):
    elems = [f"e{i:03d}" for i in range(E)]
    sets = [f"s{i:03d}" for i in range(S)]
    cover = {s: {e: 0 for e in elems} for s in sets}
    for s in sets:
        for e in rng.sample(elems, rng.randint(2, k)):
            cover[s][e] = 1
    for e in elems:  # every element coverable
        cover[rng.choice(sets)][e] = 1
    return {
        "version": "1.3", "sets": {"E": elems, "S": sets},
        "parameters": {"a": {"index": ["S", "E"], "values": cover},
                       "cost": {"index": ["S"], "values": {s: rng.randint(1, 20) for s in sets}}},
        "variables": [{"name": "x", "type": "binary", "index": ["S"]}],
        "constraints": [{"name": "cov", "forall": "e in E", "expr": "sum(s in S) a[s,e] * x[s] >= 1"}],
        "solve": {"method": "minimize", "objective": "sum(s in S) cost[s] * x[s]", "solutions": "one"},
    }


def windows(rng, J, T, M):
    jobs = [f"j{k:02d}" for k in range(J)]
    win = {}
    for j in jobs:
        a = rng.randint(1, T - 6)
        b = min(T, a + rng.randint(3, 10))
        win[j] = {str(t): 1 if a <= t <= b else 0 for t in range(1, T + 1)}
    return {
        "version": "1.3", "sets": {"J": jobs, "T": {"from": 1, "to": T}},
        "parameters": {
            "allowed": {"index": ["J", "T"], "values": win},
            "late": {"index": ["J", "T"], "values": {j: {str(t): rng.randint(0, 9) * t for t in range(1, T + 1)} for j in jobs}},
            "machines": M,
        },
        "variables": [
            {"name": "x", "type": "integer", "index": ["J", "T"], "lower": 0, "upper": "allowed"},
            {"name": "busy", "type": "integer", "index": ["T"], "lower": 0, "upper": M},
        ],
        "constraints": [
            {"name": "once", "forall": "j in J", "expr": "sum(t in T) x[j,t] == 1"},
            {"name": "slot", "forall": "t in T", "expr": "sum(j in J) x[j,t] <= busy[t]"},
            {"name": "slot2", "forall": "t in T", "expr": "sum(j in J) 2 * x[j,t] <= 2 * machines"},
            {"name": "redundant", "forall": "t in T", "expr": "sum(j in J) x[j,t] <= 1000"},
        ],
        "solve": {"method": "minimize", "solutions": "one",
                  "objective": "sum(j in J, t in T) late[j,t] * x[j,t] + sum(t in T) 3 * busy[t]"},
    }


INSTANCES = {
    "cfl_large": lambda: cfl(random.Random(1), 30, 120, 5, 70),
    "gap": lambda: gap(random.Random(3), 10, 80, 0.75),
    "lotsize": lambda: lotsize(random.Random(4), 4, 30),
    "lotsize_big": lambda: lotsize(random.Random(7), 6, 40),
    "mknap": lambda: mknap(random.Random(2), 120, 10, 0.5),
    "setcover": lambda: setcover(random.Random(5), 500, 1200, 10),
    "windows": lambda: windows(random.Random(6), 120, 80, 2),
}


def run(command, path, time_limit):
    """(seconds, status, objective, bound); status is "error" if the solver failed."""
    start = time.monotonic()
    r = subprocess.run([command, f"--time={time_limit}", path, "-"], capture_output=True, text=True)
    seconds = time.monotonic() - start
    if r.returncode != 0:
        return seconds, "error", None, None
    result = json.loads(r.stdout)
    objective = result["solutions"][0].get("objective") if result["solutions"] else None
    return seconds, result["status"], objective, result.get("objective_bound")


def fmt(value):
    return "" if value is None else f"{value:.6g}"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("names", nargs="*", help=f"instances to run (default: all of {', '.join(INSTANCES)})")
    ap.add_argument("--time", type=float, default=60, help="time limit per solve in seconds (default 60)")
    ap.add_argument("--highs", action="store_true", help="also run the HiGHS reference and compare")
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"), help="optimizam binary")
    ap.add_argument("--dir", default=os.path.join(ROOT, "target", "milp_benchmark"), help="where instances are written")
    args = ap.parse_args()

    unknown = [n for n in args.names if n not in INSTANCES]
    if unknown:
        sys.exit(f"unknown instances: {', '.join(unknown)} (known: {', '.join(INSTANCES)})")
    if not os.path.exists(args.bin):
        sys.exit(f"{args.bin} not found; run `cargo build --release` or pass --bin")
    os.makedirs(args.dir, exist_ok=True)

    header = f"{'instance':12} {'seconds':>8} {'status':19} {'objective':>12} {'bound':>12}"
    if args.highs:
        header += f" | {'HiGHS s':>8} {'HiGHS status':19} {'HiGHS objective':>15}"
    print(header)
    disagreements, total = 0, 0.0
    for name in args.names or INSTANCES:
        path = os.path.join(args.dir, f"{name}.json")
        with open(path, "w") as f:
            json.dump(INSTANCES[name](), f)
        seconds, status, objective, bound = run(args.bin, path, args.time)
        total += seconds
        line = f"{name:12} {seconds:8.2f} {status:19} {fmt(objective):>12} {fmt(bound):>12}"
        if args.highs:
            hs, hstatus, hobjective, _ = run(os.path.join(ROOT, "tools", "highs.sh"), path, args.time)
            line += f" | {hs:8.2f} {hstatus:19} {fmt(hobjective):>15}"
            both_proven = status == hstatus == "complete"
            if both_proven and abs(objective - hobjective) > REL_TOL * max(1.0, abs(hobjective)):
                line += "  OBJECTIVES DISAGREE"
                disagreements += 1
        print(line, flush=True)
    print(f"total {total:.2f}s")
    if disagreements:
        print(f"{disagreements} instances where proven optima disagree")
        sys.exit(1)


if __name__ == "__main__":
    main()
