#!/usr/bin/env python3
"""Global reference solver: solve an optimizam problem file with SCIP
(through PySCIPOpt) and write optimizam's result format.

Reads format 1.2 problems with continuous, integer, and binary variables and
`expr` constraints (no enforce_if or other constraint kinds). It reuses
highs_reference.py's parser and its sets, parameters, and variables, and
evaluates expressions into SCIP expressions of its own. SCIP has no tan,
atan, or tanh: tan is sin/cos and tanh is written with exp; atan is an
error. A nonlinear objective becomes an epigraph variable (SCIP's objective
is linear).

Statuses: optimal is complete (SCIP closes the gap to its default of 0);
infeasible; a time limit gives not_proven_optimal (with SCIP's dual bound)
or timeout.

Usage: tools/scip.sh [--time SECONDS] problem.json result.json
(the project's .venv needs pyscipopt).
"""

import argparse
import json
import os
import sys

import pyscipopt as ps

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from highs_reference import INF, Model, ModelError, Parser, Parser_expr, solution_json  # noqa: E402


def tanh(e):
    t = ps.exp(2 * e)
    return (t - 1) / (t + 1)


FUNCTIONS = {"ln": ps.log, "exp": ps.exp, "sqrt": ps.sqrt, "sin": ps.sin, "cos": ps.cos,
             "tan": lambda e: ps.sin(e) / ps.cos(e), "tanh": tanh}


class Evaluator:
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
            base, n = self.ev(node[1], env), node[2]
            if isinstance(base, (int, float)):
                return base ** n
            if n < 0:
                return 1 / base ** (-n)
            return base ** n if n > 0 else 1
        if kind == "powe":
            base, e = self.ev(node[1], env), self.ev(node[2], env)
            if isinstance(e, (int, float)) and not isinstance(base, (int, float)):
                return base ** float(e)
            if isinstance(base, (int, float)) and isinstance(e, (int, float)):
                return base ** e
            return ps.exp(e * ps.log(base))
        if kind == "call":
            name = node[1]
            if name not in FUNCTIONS:
                raise ModelError(f"unsupported: SCIP has no {name}")
            arg = self.ev(node[2], env)
            if isinstance(arg, (int, float)):
                import math
                return {"ln": math.log, "exp": math.exp, "sqrt": math.sqrt, "sin": math.sin, "cos": math.cos,
                        "tan": math.tan, "tanh": math.tanh}[name](arg)
            return FUNCTIONS[name](arg)
        if kind == "sum":
            _, bindings, cond, body = node
            total = 0
            for e in self.m.iterate(bindings, cond, env):
                total = total + self.ev(body, e)
            return total
        raise ModelError(f"bad node {node}")


def build(problem, scip):
    m = Model(problem)
    if any(d is not None for d in m.domain):
        raise ModelError("unsupported: integer domains")
    x = []
    for j, (name, idx, typ) in enumerate(m.cols):
        lo = None if m.lower[j] == -INF else m.lower[j]
        hi = None if m.upper[j] == INF else m.upper[j]
        vtype = {"binary": "B", "integer": "I"}.get(typ, "C")
        x.append(scip.addVar(f"x{j}", vtype=vtype, lb=lo, ub=hi))
    e = Evaluator(m, x)
    for i, c in enumerate(problem["constraints"]):
        if "expr" not in c or "enforce_if" in c:
            raise ModelError("unsupported: only expr constraints without enforce_if")
        op, lhs, rhs = Parser(c["expr"]).relation()
        if "forall" in c:
            p = Parser(c["forall"])
            bindings, cond = p.bindings()
            p.done()
            envs = m.iterate(bindings, cond, {})
        else:
            envs = [{}]
        for env in envs:
            g = e.ev(lhs, env) - e.ev(rhs, env)
            if isinstance(g, (int, float)):
                if (op == "<=" and g > 1e-9) or (op == ">=" and g < -1e-9) or (op == "==" and abs(g) > 1e-9):
                    m.constant_violated = True
                continue
            scip.addCons({"<=": g <= 0, ">=": g >= 0, "==": g == 0}[op])
    solve = problem["solve"]
    if solve["method"] != "satisfy":
        f = e.ev(Parser_expr(solve["objective"]), {})
        sense = "minimize" if solve["method"] == "minimize" else "maximize"
        t = scip.addVar("objective", lb=None, ub=None)
        scip.addCons(t >= f if sense == "minimize" else t <= f)
        scip.setObjective(t, sense)
    return m, x


def run(problem, time_limit):
    scip = ps.Model()
    scip.hideOutput()
    if time_limit is not None:
        scip.setParam("limits/time", float(time_limit))
    m, x = build(problem, scip)
    if getattr(m, "constant_violated", False):
        return {"version": "1.0", "status": "infeasible", "solutions": []}, {"scip_status": "constant row"}
    scip.optimize()
    status = scip.getStatus()
    satisfy = problem["solve"]["method"] == "satisfy"
    sols = []
    if scip.getNSols() > 0:
        best = scip.getBestSol()
        xs = [scip.getSolVal(best, v) for v in x]
        for j, (_, _, typ) in enumerate(m.cols):
            if typ != "continuous":
                xs[j] = round(xs[j])
        sols = [solution_json(m, xs, None if satisfy else scip.getObjVal())]
    result = {"version": "1.0", "solutions": sols}
    if status == "optimal" or (satisfy and sols):
        result["status"] = "complete"
    elif status == "infeasible":
        result["status"] = "infeasible"
    elif status in ("unbounded", "inforunbd"):
        result["status"], result["solutions"] = "unbounded", []
    elif sols:
        result["status"] = "not_proven_optimal"
        bound = scip.getDualbound()
        if abs(bound) < 1e20:
            result["objective_bound"] = bound
    else:
        result["status"] = "timeout"
    return result, {"scip_status": status, "nodes": scip.getNNodes()}


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
    print("scip:", json.dumps(stats), file=sys.stderr)


if __name__ == "__main__":
    main()
