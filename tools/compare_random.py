#!/usr/bin/env python3
"""Compare optimizam with the HiGHS reference on random MILPs.

Generates random problems from fixed seeds, solves each with optimizam and
with tools/highs.sh, and checks that they agree: the same status (or both
rejecting the problem), and objectives within 1e-6 relative tolerance.
Problems that disagree are saved so they can be rerun by hand.

Usage (from the repository root, after `cargo build --release`):
    python3 tools/compare_random.py [--kind facility|general|classes|all] [--count N]
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
  classes   Medium-sized instances of classic MILP classes (generalized
            assignment, multidimensional knapsack, lot sizing with big-M
            setups, set covering, time-window scheduling), large enough that
            presolve, cuts, and the heuristics (dives, RENS, RINS) all run.

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


def table(keys, fn):
    return {str(k): fn(k) for k in keys}


def gap_problem(rng):
    agents = [f"a{k}" for k in range(rng.randint(3, 6))]
    jobs = [f"j{k}" for k in range(rng.randint(8, 25))]
    w = {a: table(jobs, lambda j: rng.randint(5, 25)) for a in agents}
    tight = rng.uniform(0.6, 1.0)
    return {
        "version": "1.0", "sets": {"A": agents, "J": jobs},
        "parameters": {
            "w": {"index": ["A", "J"], "values": w},
            "c": {"index": ["A", "J"], "values": {a: table(jobs, lambda j: rng.randint(10, 50)) for a in agents}},
            "cap": {"index": ["A"], "values": {a: int(tight * sum(w[a].values()) / len(agents)) for a in agents}},
        },
        "variables": [{"name": "x", "type": "binary", "index": ["A", "J"]}],
        "constraints": [
            {"name": "assign", "forall": "j in J", "expr": "sum(a in A) x[a,j] == 1"},
            {"name": "capacity", "forall": "a in A", "expr": "sum(j in J) w[a,j] * x[a,j] <= cap[a]"},
        ],
        "solve": {"method": "minimize", "objective": "sum(a in A, j in J) c[a,j] * x[a,j]", "solutions": "one"},
    }


def knapsack_problem(rng):
    items = [f"i{k}" for k in range(rng.randint(10, 40))]
    res = [f"r{k}" for k in range(rng.randint(1, 5))]
    w = {r: table(items, lambda i: rng.randint(5, 60)) for r in res}
    return {
        "version": "1.0", "sets": {"I": items, "R": res},
        "parameters": {
            "w": {"index": ["R", "I"], "values": w},
            "v": {"index": ["I"], "values": table(items, lambda i: rng.randint(5, 80))},
            "cap": {"index": ["R"], "values": {r: int(rng.uniform(0.3, 0.7) * sum(w[r].values())) for r in res}},
        },
        "variables": [{"name": "x", "type": "binary", "index": ["I"]} if rng.random() < 0.5
                      else {"name": "x", "type": "integer", "index": ["I"], "lower": 0, "upper": 3}],
        "constraints": [{"name": "c", "forall": "r in R", "expr": "sum(i in I) w[r,i] * x[i] <= cap[r]"}],
        "solve": {"method": "maximize", "objective": "sum(i in I) v[i] * x[i]", "solutions": "one"},
    }


def lotsize_problem(rng):
    products = [f"p{k}" for k in range(rng.randint(1, 3))]
    T = rng.randint(4, 12)
    d = {p: {str(t): rng.choice([0, rng.randint(5, 40)]) for t in range(1, T + 1)} for p in products}
    total = sum(sum(v.values()) for v in d.values()) or 1
    return {
        "version": "1.0", "sets": {"P": products, "T": {"from": 1, "to": T}},
        "parameters": {
            "d": {"index": ["P", "T"], "values": d},
            "setup": {"index": ["P"], "values": table(products, lambda p: rng.randint(30, 200))},
            "hold": {"index": ["P"], "values": table(products, lambda p: rng.randint(1, 5))},
            "capacity": int(rng.uniform(0.8, 2.0) * total / T) + 1,
            "M": total,
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


def setcover_problem(rng):
    elems = [f"e{k}" for k in range(rng.randint(10, 50))]
    sets = [f"s{k}" for k in range(rng.randint(10, 60))]
    a = {s: table(elems, lambda e: 0) for s in sets}
    for s in sets:
        for e in rng.sample(elems, rng.randint(1, min(6, len(elems)))):
            a[s][e] = 1
    if rng.random() < 0.8:  # usually every element coverable; sometimes maybe not
        for e in elems:
            a[rng.choice(sets)][e] = 1
    return {
        "version": "1.0", "sets": {"E": elems, "S": sets},
        "parameters": {"a": {"index": ["S", "E"], "values": a},
                       "cost": {"index": ["S"], "values": table(sets, lambda s: rng.randint(1, 20))}},
        "variables": [{"name": "x", "type": "binary", "index": ["S"]}],
        "constraints": [{"name": "cov", "forall": "e in E", "expr": "sum(s in S) a[s,e] * x[s] >= 1"}],
        "solve": {"method": "minimize", "objective": "sum(s in S) cost[s] * x[s]", "solutions": "one"},
    }


def windows_problem(rng):
    jobs = [f"j{k}" for k in range(rng.randint(4, 20))]
    T = rng.randint(6, 20)
    allowed = {}
    for j in jobs:
        a = rng.randint(1, T)
        b = min(T, a + rng.randint(0, 6))
        allowed[j] = {str(t): 1 if a <= t <= b else 0 for t in range(1, T + 1)}
    machines = rng.randint(1, 3)
    return {
        "version": "1.0", "sets": {"J": jobs, "T": {"from": 1, "to": T}},
        "parameters": {
            "allowed": {"index": ["J", "T"], "values": allowed},
            "late": {"index": ["J", "T"], "values": {j: {str(t): rng.randint(0, 9) * t for t in range(1, T + 1)} for j in jobs}},
            "machines": machines,
        },
        "variables": [
            {"name": "x", "type": "integer", "index": ["J", "T"], "lower": 0, "upper": "allowed"},
            {"name": "busy", "type": "integer", "index": ["T"], "lower": 0, "upper": machines},
        ],
        "constraints": [
            {"name": "once", "forall": "j in J", "expr": "sum(t in T) x[j,t] == 1"},
            {"name": "slot", "forall": "t in T", "expr": "sum(j in J) x[j,t] <= busy[t]"},
            {"name": "redundant", "forall": "t in T", "expr": "sum(j in J) x[j,t] <= 1000"},
        ],
        "solve": {"method": "minimize", "solutions": "one",
                  "objective": "sum(j in J, t in T) late[j,t] * x[j,t] + sum(t in T) 3 * busy[t]"},
    }


def classes_problem(rng):
    return rng.choice([gap_problem, knapsack_problem, lotsize_problem, setcover_problem, windows_problem])(rng)


GENERATORS = {"facility": facility_problem, "general": general_problem, "classes": classes_problem}


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
    ap.add_argument("--kind", choices=["facility", "general", "classes", "all"], default="all")
    ap.add_argument("--count", type=int, default=200, help="problems per kind (default 200)")
    ap.add_argument("--seed", type=int, default=0, help="first seed (default 0)")
    ap.add_argument("--time", type=float, default=20, help="time limit per solve in seconds (default 20)")
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"), help="optimizam binary")
    ap.add_argument("--save-dir", default="compare_random_failures", help="where disagreeing problems are saved")
    args = ap.parse_args()

    if not os.path.exists(args.bin):
        sys.exit(f"{args.bin} not found; run `cargo build --release` or pass --bin")
    reference = os.path.join(ROOT, "tools", "highs.sh")
    kinds = list(GENERATORS) if args.kind == "all" else [args.kind]

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
