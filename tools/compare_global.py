#!/usr/bin/env python3
"""Differential test of optimizam's global search against SCIP.

Generates random small nonlinear problems from fixed seeds: 2 to 4 bounded
variables (one of them integer in a third of the problems), a random
nonconvex objective, and 1 to 3 random constraints, made to hold at a
random reference point (most problems are feasible; a negative slack can make one infeasible), all built from
+ - * /, powers, ln, exp, sqrt, sin, cos, and tanh. Solves each with the
release binary and with SCIP (tools/scip.sh, the project's .venv with
pyscipopt) and reports disagreements:

- one proves the problem infeasible and the other finds a feasible point;
- both prove an optimum, and they differ by more than the optimality gap
  (1e-4 relative, 1e-6 absolute, plus a little for feasibility tolerances);
- one proves an optimum and the other finds a better feasible point.

Disagreeing problems are saved to compare_global_failures/ (gitignored),
and the script exits with status 1.

Usage (from the repository root, after `cargo build --release`):
    python3 tools/compare_global.py [--count N] [--seed S] [--time SECONDS]
"""

import argparse
import json
import os
import random
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def number(rng, lo, hi):
    return round(rng.uniform(lo, hi), 2)


def expression(rng, names, depth):
    """A random expression string; arguments of ln, sqrt, and divisors are
    kept positive (1 + square, or exp)."""
    if depth == 0 or rng.random() < 0.3:
        if rng.random() < 0.3:
            return str(number(rng, -3, 3))
        return rng.choice(names)
    a = expression(rng, names, depth - 1)
    b = expression(rng, names, depth - 1)
    positive = lambda e: f"(1 + ({e})^2)" if rng.random() < 0.6 else f"exp(0.3 * ({e}))"
    choice = rng.randrange(10)
    if choice <= 2:
        return f"({a}) {rng.choice(['+', '-'])} {number(rng, 0.5, 3)} * ({b})"
    if choice <= 4:
        return f"({a}) * ({b})"
    if choice == 5:
        return f"({a}) / {positive(b)}"
    if choice == 6:
        return f"({a})^{rng.choice([2, 3])}"
    if choice == 7:
        return f"{rng.choice(['ln', 'sqrt'])}({positive(a)})"
    return f"{rng.choice(['sin', 'cos', 'tanh', 'exp'])}(0.5 * ({a}))"


def generate(rng):
    n = rng.randint(2, 4)
    names = [f"x{i}" for i in range(1, n + 1)]
    integer = rng.random() < 1 / 3
    variables = []
    for k, name in enumerate(names):
        lo = number(rng, -3, 1)
        hi = round(lo + rng.uniform(1, 4), 2)
        if integer and k == 0:
            variables.append({"name": name, "type": "integer", "lower": int(lo) - 1, "upper": int(hi) + 1})
        else:
            variables.append({"name": name, "type": "continuous", "lower": lo, "upper": hi})
    objective = expression(rng, names, rng.choice([3, 4]))
    constraints = []
    ref = {}
    for v in variables:
        if v["type"] == "integer":
            ref[v["name"]] = rng.randint(v["lower"], v["upper"])
        else:
            ref[v["name"]] = rng.uniform(v["lower"], v["upper"])
    for c in range(rng.randint(1, 3)):
        e = expression(rng, names, 2)
        value = evaluate(e, ref)
        if value is None:
            continue
        # Mostly feasible at the reference point; a negative slack can make
        # the problem infeasible.
        slack = round(rng.uniform(-0.6, 1.0), 3)
        if rng.random() < 0.5:
            constraints.append({"name": f"c{c}", "expr": f"{e} <= {round(value + slack, 4)}"})
        else:
            constraints.append({"name": f"c{c}", "expr": f"{e} >= {round(value - slack, 4)}"})
    sense = rng.choice(["minimize", "maximize"])
    return {"version": "1.2", "variables": variables, "constraints": constraints,
            "solve": {"method": sense, "objective": objective}}


def evaluate(expr, env):
    import math
    text = expr.replace("^", "**").replace("ln", "math.log").replace("sqrt", "math.sqrt").replace(
        "exp", "math.exp").replace("sin", "math.sin").replace("cos", "math.cos").replace("tanh", "math.tanh")
    try:
        v = eval(text, {"math": math}, env)
        return v if isinstance(v, float) and math.isfinite(v) or isinstance(v, int) else None
    except (ValueError, OverflowError, ZeroDivisionError, TypeError):
        return None


def run(command, path, time_limit):
    r = subprocess.run([command, f"--time={time_limit}", path, "-"], capture_output=True, text=True)
    if r.returncode != 0:
        return {"status": "error", "message": (r.stderr.strip().splitlines() or ["?"])[-1]}
    out = json.loads(r.stdout)
    sols = out["solutions"]
    return {"status": out["status"], "objective": sols[0].get("objective") if sols else None}


def disagreement(ours, scip, sense):
    if "error" in (ours["status"], scip["status"]):
        return f"error: {ours.get('message') or scip.get('message')}"
    feasible = lambda r: r["objective"] is not None
    if ours["status"] == "infeasible" and feasible(scip):
        return "optimizam proves infeasible; SCIP finds a point"
    if scip["status"] == "infeasible" and feasible(ours):
        return "SCIP proves infeasible; optimizam finds a point"
    if not (feasible(ours) and feasible(scip)):
        return None
    a, b = ours["objective"], scip["objective"]
    tol = max(1e-4 * max(abs(a), abs(b)), 1e-6) + 1e-5 * (1 + max(abs(a), abs(b)))
    better = lambda x, y: x < y - tol if sense == "minimize" else x > y + tol
    if ours["status"] == "complete" and better(b, a):
        return f"optimizam proves {a}; SCIP finds {b}"
    if scip["status"] == "complete" and better(a, b):
        return f"SCIP proves {b}; optimizam finds {a}"
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--count", type=int, default=100)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--time", type=float, default=10.0)
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"))
    a = ap.parse_args()
    rng = random.Random(a.seed)
    out_dir = os.path.join(ROOT, "compare_global_failures")
    work = os.path.join(ROOT, "target", "compare_global")
    os.makedirs(work, exist_ok=True)
    counts = {}
    failures = 0
    for k in range(a.count):
        problem = generate(rng)
        path = os.path.join(work, f"p{k}.json")
        with open(path, "w") as f:
            json.dump(problem, f, indent=1)
        ours = run(a.bin, path, a.time)
        scip = run(os.path.join(ROOT, "tools", "scip.sh"), path, a.time)
        key = f"{ours['status']} / {scip['status']}"
        counts[key] = counts.get(key, 0) + 1
        why = disagreement(ours, scip, problem["solve"]["method"])
        if why:
            failures += 1
            os.makedirs(out_dir, exist_ok=True)
            with open(os.path.join(out_dir, f"seed{a.seed}_p{k}.json"), "w") as f:
                json.dump(problem, f, indent=1)
            print(f"problem {k}: {why}")
    print("statuses (optimizam / scip):")
    for key, c in sorted(counts.items(), key=lambda kv: -kv[1]):
        print(f"  {c:4} {key}")
    print(f"{failures} disagreements in {a.count} problems")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
