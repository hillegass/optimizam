#!/usr/bin/env python3
"""Solve an optimizam problem file with OR-Tools CP-SAT and write an
optimizam result file: the development reference for the CP engine.

It reuses the problem-format parser and evaluator of tools/highs_reference.py
(an implementation independent of the Rust code), but builds a native CP-SAT
model: AddAllDifferent for all_different, and set-valued domains as CP-SAT
domains. So it shares nothing with the Rust engine's encoding of these
constraints.

Usage (from the repository root):
    .venv/bin/python tools/cpsat_reference.py [--time=SECONDS] PROBLEM RESULT
or tools/cpsat.sh with the same arguments.

Scope: integer and binary variables with finite bounds; expr rows and the
objective with coefficients that scale to integers (by a power of ten up to
1e6); all_different. Other format 1.1 kinds are rejected.
"""

import argparse
import json
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import highs_reference as hr  # noqa: E402
from ortools.sat.python import cp_model  # noqa: E402

ModelError = hr.ModelError
SCALES = [1, 10, 100, 1000, 10**4, 10**5, 10**6]


def integer_scale(values):
    """The smallest SCALES factor making every value integral, or an error."""
    for s in SCALES:
        if all(abs(v * s - round(v * s)) <= 1e-9 * max(1.0, abs(v * s)) for v in values):
            return s
    raise ModelError("CP-SAT reference: coefficients don't scale to integers")


def affine(m, value, what):
    """(coefs, const) of an affine value, checked to be integral."""
    coefs, const = hr.as_lin(value)
    coefs = {j: c for j, c in coefs.items() if c != 0}
    if any(c != round(c) for c in coefs.values()) or const != round(const):
        raise ModelError(f"{what}: items must be integer-valued")
    return {j: int(round(c)) for j, c in coefs.items()}, int(round(const))


def build(problem):
    m = hr.Model(problem)
    model = cp_model.CpModel()
    x = []
    for j, (name, idx, typ) in enumerate(m.cols):
        lo, hi = m.lower[j], m.upper[j]
        if typ == "continuous":
            raise ModelError("CP-SAT reference: continuous variables are not supported")
        if not (math.isfinite(lo) and math.isfinite(hi)):
            raise ModelError("CP-SAT reference: integer variables need finite bounds")
        ranges = m.domain[j] or [[math.ceil(lo), math.floor(hi)]]
        domain = cp_model.Domain.from_intervals([[int(a), int(b)] for a, b in ranges])
        x.append(model.new_int_var_from_domain(domain, f"{name}{list(idx)}"))

    for i, c in enumerate(problem["constraints"]):
        cname = c.get("name", f"#{i}")
        kinds = [k for k in ("expr", "all_different", "element", "table", "min", "max", "abs",
                             "no_overlap", "cumulative") if k in c]
        if kinds not in (["expr"], ["all_different"]) or "enforce_if" in c:
            raise ModelError(f"CP-SAT reference: constraint {cname}: unsupported kind {kinds}")
        if "forall" in c:
            p = hr.Parser(c["forall"])
            bindings, cond = p.bindings()
            p.done()
            envs = m.iterate(bindings, cond, {})
        else:
            bindings, envs = [], [{}]
        parsed = hr.Parser(c["expr"]).relation() if "expr" in c else hr.Parser(c["all_different"]).list()
        for env in envs:
            label = cname + (f"[{','.join(str(env[b[0]]) for b in bindings)}]" if bindings else "")
            if "all_different" in c:
                exprs = []
                for item in m.eval_list(parsed, env):
                    coefs, const = affine(m, item, f"all_different {label}")
                    exprs.append(sum(a * x[j] for j, a in coefs.items()) + const)
                model.add_all_different(exprs)
                continue
            op, lhs, rhs = parsed
            coefs, k = hr.as_lin(m.add(m.ev(lhs, env), m.ev(rhs, env), -1.0))
            coefs = {j: v for j, v in coefs.items() if v != 0}
            if any(m.cols[j][0] in m.symbolic for j in coefs):
                raise ModelError(f"constraint {label}: a symbolic variable can't be used in arithmetic")
            s = integer_scale(list(coefs.values()))
            lhs_expr = sum(int(round(v * s)) * x[j] for j, v in coefs.items())
            bound = -k * s
            if op == "<=":
                model.add(lhs_expr <= math.floor(bound + 1e-9))
            elif op == ">=":
                model.add(lhs_expr >= math.ceil(bound - 1e-9))
            else:
                if abs(bound - round(bound)) > 1e-9:
                    model.add_bool_or([])  # integer left side can't equal a fractional value
                else:
                    model.add(lhs_expr == round(bound))

    solve = problem["solve"]
    method = solve["method"]
    cost, offset = {}, 0.0
    if method != "satisfy":
        cost, offset = hr.as_lin(m.ev(hr.Parser_expr(solve["objective"]), {}))
        cost = {j: v for j, v in cost.items() if v != 0}
    return m, model, x, cost, offset


def run(problem, time_limit):
    m, model, x, cost, offset = build(problem)
    solve = problem["solve"]
    method, want_all = solve["method"], solve.get("solutions", "one") == "all"
    optimizing = method != "satisfy" and bool(cost)
    obj_scale = integer_scale(list(cost.values())) if cost else 1
    obj_expr = sum(int(round(v * obj_scale)) * x[j] for j, v in cost.items())

    def objective_of(values):
        if method == "satisfy":
            return None
        return offset + sum(c * values[j] for j, c in cost.items())

    def solution(values):
        sol = hr.solution_json(m, values, objective_of(values))
        for name, elems in m.symbolic.items():  # positions back to set elements
            table = sol["variables"][name]
            if not isinstance(table, dict):
                sol["variables"][name] = elems[table - 1]
                continue
            stack = [table]
            while stack:
                node = stack.pop()
                for k, v in node.items():
                    if isinstance(v, dict):
                        stack.append(v)
                    else:
                        node[k] = elems[v - 1]
        return sol

    solver = cp_model.CpSolver()
    if time_limit is not None:
        solver.parameters.max_time_in_seconds = time_limit

    class Collect(cp_model.CpSolverSolutionCallback):
        def __init__(self):
            super().__init__()
            self.found = []

        def on_solution_callback(self):
            self.found.append([self.value(v) for v in x])

    best_status = None
    if optimizing:
        if method == "maximize":
            model.maximize(obj_expr)
        else:
            model.minimize(obj_expr)
        status = solver.solve(model)
        if status == cp_model.INFEASIBLE:
            return {"version": "1.0", "status": "infeasible", "solutions": []}
        if status not in (cp_model.OPTIMAL, cp_model.FEASIBLE):
            return {"version": "1.0", "status": "timeout", "solutions": []}
        values = [solver.value(v) for v in x]
        if status == cp_model.FEASIBLE:
            bound = offset + solver.best_objective_bound / obj_scale
            return {"version": "1.0", "status": "not_proven_optimal", "solutions": [solution(values)],
                    "objective_bound": bound}
        if not want_all:
            return {"version": "1.0", "status": "complete", "solutions": [solution(values)]}
        best_status = solver.objective_value
        model.clear_objective()
        model.add(obj_expr == round(best_status))

    if not want_all:
        status = solver.solve(model)
        if status in (cp_model.OPTIMAL, cp_model.FEASIBLE):
            return {"version": "1.0", "status": "complete", "solutions": [solution([solver.value(v) for v in x])]}
        st = "infeasible" if status == cp_model.INFEASIBLE else "timeout"
        return {"version": "1.0", "status": st, "solutions": []}

    solver.parameters.enumerate_all_solutions = True
    cb = Collect()
    status = solver.solve(model, cb)
    sols = [solution(v) for v in cb.found]
    if status == cp_model.OPTIMAL:
        return {"version": "1.0", "status": "complete" if sols else "infeasible", "solutions": sols}
    if status == cp_model.INFEASIBLE:
        return {"version": "1.0", "status": "infeasible", "solutions": []}
    return {"version": "1.0", "status": "incomplete" if sols else "timeout", "solutions": sols}


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--time", type=float, default=None)
    ap.add_argument("problem")
    ap.add_argument("result")
    a = ap.parse_args()
    try:
        problem = json.load(sys.stdin if a.problem == "-" else open(a.problem))
        result = run(problem, a.time)
    except ModelError as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
    text = json.dumps(result, indent=2) + "\n"
    if a.result == "-":
        sys.stdout.write(text)
    else:
        with open(a.result, "w") as f:
            f.write(text)


if __name__ == "__main__":
    main()
