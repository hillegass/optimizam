#!/usr/bin/env python3
"""Reference solver for nonlinear problems: solve an optimizam problem file
with IPOPT (through CasADi) and write optimizam's result format.

Reads format 1.2 problems with continuous variables and `expr` constraints
(no integer variables, enforce_if, or other constraint kinds). It reuses
highs_reference.py's parser and its sets, parameters, and variables, but
evaluates expressions into CasADi expressions of its own, so it shares no
code with optimizam's model builder or derivatives.

Statuses follow optimizam's nonlinear engine: a local optimum is
locally_optimal, or complete when the problem has no nonlinear terms or is
a satisfy problem; IPOPT's infeasibility detection is locally_infeasible;
limits give not_proven_optimal (with a feasible point) or timeout.

Usage: tools/ipopt.sh [--time SECONDS] problem.json result.json
(the project's .venv needs casadi).
"""

import argparse
import json
import os
import sys

import casadi as ca

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from highs_reference import INF, Model, ModelError, Parser, Parser_expr, solution_json  # noqa: E402

FUNCTIONS = {"ln": ca.log, "exp": ca.exp, "sqrt": ca.sqrt, "sin": ca.sin, "cos": ca.cos, "tan": ca.tan,
             "atan": ca.atan, "tanh": ca.tanh}


class Evaluator:
    """Evaluates parsed expressions into CasADi SX (or Python numbers)."""

    def __init__(self, m, x):
        self.m, self.x = m, x

    def ev(self, node, env):
        kind = node[0]
        if kind == "num":
            return node[1]
        if kind == "ref":
            _, name, args = node
            if args is None and name in env:
                return env[name]
            argv = [] if args is None else [self.m.const(self.m.ev(a, env)) for a in args]
            if name in self.m.params:
                return self.m.param(name, argv)
            if name in self.m.vars:
                idx = tuple(argv)
                if idx not in self.m.vars[name]:
                    raise ModelError(f"{name}{list(idx)} is outside its index sets")
                return self.x[self.m.vars[name][idx]]
            raise ModelError(f"unknown name {name}")
        if kind == "neg":
            return -self.ev(node[1], env)
        if kind == "add":
            total = 0
            for sign, t in node[1]:
                total = total + sign * self.ev(t, env)
            return total
        if kind == "bin":
            _, op, a, b = node
            x, y = self.ev(a, env), self.ev(b, env)
            return x * y if op == "*" else x / y
        if kind == "pow":
            return self.ev(node[1], env) ** node[2]
        if kind == "powe":
            return self.ev(node[1], env) ** self.ev(node[2], env)
        if kind == "call":
            return FUNCTIONS[node[1]](self.ev(node[2], env))
        if kind == "sum":
            _, bindings, cond, body = node
            total = 0
            for e in self.m.iterate(bindings, cond, env):
                total = total + self.ev(body, e)
            return total
        raise ModelError(f"bad node {node}")


def build(problem):
    m = Model(problem)
    if any(m.integer):
        raise ModelError("unsupported: integer or binary variables (IPOPT solves continuous problems)")
    n = len(m.cols)
    x = ca.SX.sym("x", n)
    e = Evaluator(m, x)
    rows = []  # (label, expression, lo, hi)
    for i, c in enumerate(problem["constraints"]):
        if "expr" not in c or "enforce_if" in c:
            raise ModelError("unsupported: only expr constraints without enforce_if")
        cname = c.get("name", f"#{i}")
        op, lhs, rhs = Parser(c["expr"]).relation()
        if "forall" in c:
            p = Parser(c["forall"])
            bindings, cond = p.bindings()
            p.done()
            envs = m.iterate(bindings, cond, {})
        else:
            bindings, envs = [], [{}]
        for env in envs:
            label = cname + (f"[{','.join(str(env[b[0]]) for b in bindings)}]" if bindings else "")
            g = e.ev(lhs, env) - e.ev(rhs, env)
            lo, hi = {"<=": (-INF, 0.0), ">=": (0.0, INF), "==": (0.0, 0.0)}[op]
            rows.append((label, g, lo, hi))
    solve = problem["solve"]
    f = 0
    if solve["method"] != "satisfy":
        f = e.ev(Parser_expr(solve["objective"]), {})
        if solve["method"] == "maximize":
            f = -f
    # Starting values (clipped to the bounds), default 0.
    x0 = []
    for v in problem["variables"]:
        for idx in m._product(v.get("index", [])):
            j = m.vars[v["name"]][idx]
            s = m._bound(v.get("start"), idx, 0.0)
            x0.append(min(max(s, m.lower[j]), m.upper[j]))
    return m, x, f, rows, x0


def run(problem, time_limit):
    m, x, f, rows, x0 = build(problem)
    satisfy = problem["solve"]["method"] == "satisfy"
    g = ca.vertcat(*[ca.SX(r[1]) for r in rows]) if rows else ca.SX(0, 1)
    f = ca.SX(f)
    linear = ca.is_linear(f, x) and all(ca.is_linear(ca.SX(r[1]), x) for r in rows)
    opts = {"print_time": 0, "ipopt.print_level": 0, "ipopt.sb": "yes"}
    if time_limit is not None:
        opts["ipopt.max_wall_time"] = float(time_limit)
    solver = ca.nlpsol("s", "ipopt", {"x": x, "f": f, "g": g}, opts)
    lbg = [r[2] for r in rows]
    ubg = [r[3] for r in rows]
    sol = solver(x0=x0, lbx=m.lower, ubx=m.upper, lbg=lbg, ubg=ubg)
    status = solver.stats()["return_status"]
    xs = [float(v) for v in sol["x"].full().flatten()]
    gs = [float(v) for v in sol["g"].full().flatten()] if rows else []
    feasible = all(lo - 1e-6 * (1 + abs(lo)) <= v <= hi + 1e-6 * (1 + abs(hi))
                   for v, lo, hi in zip(gs, lbg, ubg) if v == v)
    sign = -1.0 if problem["solve"]["method"] == "maximize" else 1.0
    objective = None if satisfy else sign * float(sol["f"])
    if status in ("Solve_Succeeded", "Solved_To_Acceptable_Level", "Feasible_Point_Found") and feasible:
        name = "complete" if satisfy or linear else "locally_optimal"
        solutions = [solution_json(m, xs, objective)]
    elif status == "Infeasible_Problem_Detected":
        name, solutions = "locally_infeasible", []
    elif feasible and status != "Diverging_Iterates":
        name, solutions = ("complete" if satisfy else "not_proven_optimal"), [solution_json(m, xs, objective)]
    else:
        name, solutions = "timeout", []
    version = "1.1" if name.startswith("locally") else "1.0"
    return {"version": version, "status": name, "solutions": solutions}, {"ipopt_status": status,
                                                                            "iterations": solver.stats()["iter_count"]}


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--time", type=float, default=None)
    ap.add_argument("problem")
    ap.add_argument("result")
    a = ap.parse_args()
    try:
        problem = json.load(sys.stdin if a.problem == "-" else open(a.problem))
        result, stats = run(problem, a.time)
    except ModelError as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
    text = json.dumps(result, indent=2) + "\n"
    if a.result == "-":
        sys.stdout.write(text)
    else:
        with open(a.result, "w") as f:
            f.write(text)
    print("ipopt:", json.dumps(stats), file=sys.stderr)


if __name__ == "__main__":
    main()
