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

Scope: integer and binary variables with finite bounds; expr rows (with
enforce_if) and the objective with coefficients that scale to integers (by a
power of ten up to 1e6); all_different, table, element, min, max, and abs.
Intervals, no_overlap, and cumulative too (native interval variables,
AddNoOverlap, AddCumulative). Native CP-SAT constraints:
AddMinEquality / AddMaxEquality / AddAbsEquality, OnlyEnforceIf for enforce_if,
AddAllowedAssignments / AddForbiddenAssignments for table, and AddElement for
element (several or filtered indexes go through a combination number k, with
the allowed (k, indexes) assignments as a table).
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


def parse_literals(text):
    """enforce_if: [(negated, ref, compare)] with compare None or (equal, expr)."""
    p = hr.Parser(text)
    lits = []
    while True:
        negated = bool(p.accept("kw", "not"))
        ref = p.atom()
        if ref[0] != "ref":
            raise ModelError(f"enforce_if literal must be a variable in {text!r}")
        compare = None
        if not negated:
            if p.accept("op", "=="):
                compare = (True, p.expr())
            elif p.accept("op", "!="):
                compare = (False, p.expr())
        lits.append((negated, ref, compare))
        if not p.accept("kw", "and"):
            break
    p.done()
    return lits


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

    # Intervals: native (optional) interval variables, by name and index tuple.
    intervals = {}
    for iv in problem.get("intervals", []):
        intervals[iv["name"]] = {}
        for idx in m._product(iv.get("index", [])):
            def field(b):
                if isinstance(b, (int, float)):
                    return int(b)
                if b in m.vars:
                    return x[m.vars[b][idx]]
                return int(m.param(b, list(idx)))
            start, size = field(iv["start"]), field(iv["duration"])
            if "presence" in iv:
                present = model.new_bool_var("")
                model.add(x[m.vars[iv["presence"]][idx]] == present)
                var = model.new_optional_interval_var(start, size, start + size, present, "")
            else:
                var = model.new_interval_var(start, size, start + size, "")
            intervals[iv["name"]][idx] = var

    def interval_list(text, env):
        lst = hr.Parser(text).list()
        if lst[0] == "list":
            items = [(item, env) for item in lst[1]]
        else:
            _, body, bindings, cond = lst
            items = [(body, e) for e in m.iterate(bindings, cond, env)]
        out = []
        for item, e in items:
            if item[0] != "ref" or item[1] not in intervals:
                raise ModelError("a list of intervals must name intervals")
            key = tuple(m.const(m.ev(a, e)) for a in (item[2] or []))
            out.append(intervals[item[1]][key])
        return out

    symbolic_set = {}  # column -> its set's elements, for symbolic columns
    for j, (name, _, _) in enumerate(m.cols):
        if name in m.symbolic:
            symbolic_set[j] = m.symbolic[name]

    def single(value):
        coefs, const = hr.as_lin(value)
        coefs = {j: c for j, c in coefs.items() if c != 0}
        if len(coefs) == 1 and list(coefs.values())[0] == 1 and const == 0:
            return list(coefs)[0]
        return None

    def column_value(col, elem):
        """The integer a column takes for an element (a position for symbolic columns), or None."""
        if col in symbolic_set:
            elems = symbolic_set[col]
            return elems.index(elem) + 1 if elem in elems else None
        if isinstance(elem, str):
            raise ModelError(f"{elem!r} compared with a numeric variable")
        return int(elem)

    def literal(lit, env):
        negated, ref, compare = lit
        col = single(m.ev(ref, env))
        if col is None:
            raise ModelError("an enforce_if literal must name one variable")
        b = model.new_bool_var("")
        if compare is None:
            model.add(x[col] == 1).only_enforce_if(b)
            model.add(x[col] == 0).only_enforce_if(~b)
            return ~b if negated else b
        equal, e = compare
        v = column_value(col, m.const(m.ev(e, env)))
        if v is None:  # not in the set: the comparison is constant
            model.add(b == (0 if equal else 1))
            return b
        model.add(x[col] == v).only_enforce_if(b)
        model.add(x[col] != v).only_enforce_if(~b)
        return b if equal else ~b

    for i, c in enumerate(problem["constraints"]):
        cname = c.get("name", f"#{i}")
        kinds = [k for k in ("expr", "all_different", "element", "table", "min", "max", "abs",
                             "no_overlap", "cumulative") if k in c]
        supported = (["expr"], ["all_different"], ["table"], ["element"], ["min"], ["max"], ["abs"],
                     ["no_overlap"], ["cumulative"])
        if kinds not in supported or ("enforce_if" in c and "expr" not in c):
            raise ModelError(f"CP-SAT reference: constraint {cname}: unsupported kind {kinds}")
        if "forall" in c:
            p = hr.Parser(c["forall"])
            bindings, cond = p.bindings()
            p.done()
            envs = m.iterate(bindings, cond, {})
        else:
            bindings, envs = [], [{}]
        if "expr" in c:
            parsed = hr.Parser(c["expr"]).relation()
        elif "all_different" in c:
            parsed = hr.Parser(c["all_different"]).list()
        elif "table" in c:
            parsed = hr.Parser(c["table"]["vars"]).list()
        elif "min" in c or "max" in c:
            parsed = hr.Parser(c.get("min", c.get("max"))["of"]).list()
        elif "abs" in c:
            parsed = hr.Parser_expr(c["abs"]["of"])
        elif "no_overlap" in c or "cumulative" in c:
            parsed = None
        else:
            parsed = hr.Parser(c["element"]["array"]).list()
            if parsed[0] != "comp":
                raise ModelError(f"constraint {cname}: element's array must be a comprehension")
        lits = parse_literals(c["enforce_if"]) if "enforce_if" in c else []
        for env in envs:
            label = cname + (f"[{','.join(str(env[b[0]]) for b in bindings)}]" if bindings else "")
            if "no_overlap" in c:
                model.add_no_overlap(interval_list(c["no_overlap"], env))
                continue
            if "cumulative" in c:
                cu = c["cumulative"]
                ivs = interval_list(cu["intervals"], env)
                demands = []
                for v in m.eval_list(hr.Parser(cu["demands"]).list(), env):
                    coefs, const = affine(m, v, f"cumulative {label} demand")
                    demands.append(sum(a * x[j] for j, a in coefs.items()) + const)
                coefs, const = affine(m, m.ev(hr.Parser_expr(cu["capacity"]), env), f"cumulative {label} capacity")
                capacity = sum(a * x[j] for j, a in coefs.items()) + const
                if len(demands) != len(ivs):
                    raise ModelError(f"constraint {label}: {len(ivs)} intervals but {len(demands)} demands")
                model.add_cumulative(ivs, demands, capacity)
                continue
            if "min" in c or "max" in c or "abs" in c:
                spec = c.get("min") or c.get("max") or c.get("abs")

                def int_expr(value, what):
                    coefs, const = affine(m, value, f"{what} {label}")
                    if any(j in symbolic_set for j in coefs):
                        raise ModelError(f"constraint {label}: a symbolic variable can't be used in arithmetic")
                    return sum(a * x[j] for j, a in coefs.items()) + const

                target = model.new_int_var(-10**9, 10**9, "")
                model.add(target == int_expr(m.ev(hr.Parser_expr(spec["value"]), env), "value"))
                if "abs" in c:
                    model.add_abs_equality(target, int_expr(m.ev(parsed, env), "abs"))
                else:
                    items = [int_expr(v, "item") for v in m.eval_list(parsed, env)]
                    if "max" in c:
                        model.add_max_equality(target, items)
                    else:
                        model.add_min_equality(target, items)
                continue
            if "table" in c:
                cols = [single(v) for v in m.eval_list(parsed, env)]
                if None in cols:
                    raise ModelError(f"constraint {label}: table vars must be variables")
                tuples = []
                for t in c["table"]["tuples"]:
                    vals = [column_value(col, e) for col, e in zip(cols, t)]
                    if None not in vals:
                        tuples.append(vals)
                vars_ = [x[col] for col in cols]
                if c["table"].get("forbidden", False):
                    model.add_forbidden_assignments(vars_, tuples)
                else:
                    model.add_allowed_assignments(vars_, tuples)
                continue
            if "element" in c:
                e = c["element"]
                index_texts = e["index"] if isinstance(e["index"], list) else [e["index"]]
                _, body, comp_bindings, comp_cond = parsed
                combos, items = [], []
                for env2 in m.iterate(comp_bindings, comp_cond, env):
                    key = []
                    for idx_name, set_name in comp_bindings:
                        el = env2[idx_name]
                        elems = m.sets[set_name]
                        key.append(elems.index(el) + 1 if any(isinstance(z, str) for z in elems) else el)
                    combos.append(key)
                    coefs, const = hr.as_lin(m.ev(body, env2))
                    if any(v != round(v) for v in coefs.values()) or const != round(const):
                        raise ModelError(f"constraint {label}: element items must be integer-valued")
                    items.append(sum(int(round(v)) * x[j] for j, v in coefs.items() if v) + int(round(const)))
                if not combos:
                    model.add_bool_or([])
                    continue
                k = model.new_int_var(0, len(combos) - 1, "")
                index_vars = []
                for text in index_texts:
                    coefs, const = affine(m, m.ev(hr.Parser_expr(text), env), f"element {label} index")
                    expr_ = sum(a * x[j] for j, a in coefs.items()) + const
                    lo = min(z[len(index_vars)] for z in combos)
                    hi = max(z[len(index_vars)] for z in combos)
                    v = model.new_int_var(lo, hi, "")
                    model.add(v == expr_)
                    index_vars.append(v)
                model.add_allowed_assignments([k] + index_vars, [[n] + key for n, key in enumerate(combos)])
                vcoefs, vconst = affine(m, m.ev(hr.Parser_expr(e["value"]), env), f"element {label} value")
                target = model.new_int_var(-10**9, 10**9, "")
                model.add(target == sum(a * x[j] for j, a in vcoefs.items()) + vconst)
                model.add_element(k, items, target)
                continue
            if "all_different" in c:
                exprs = []
                for item in m.eval_list(parsed, env):
                    coefs, const = affine(m, item, f"all_different {label}")
                    exprs.append(sum(a * x[j] for j, a in coefs.items()) + const)
                model.add_all_different(exprs)
                continue
            op, lhs, rhs = parsed
            enforce = [literal(lit, env) for lit in lits]
            lv, rv = m.ev(lhs, env), m.ev(rhs, env)
            # A symbolic variable compared with an element.
            sym = None
            for a, b in ((lv, rv), (rv, lv)):
                col = single(a) if hr.is_lin(a) else None
                if op == "==" and col in symbolic_set and not hr.is_lin(b):
                    sym = (col, column_value(col, b))
            if sym is not None:
                col, pos = sym
                ct = model.add(x[col] == pos) if pos is not None else model.add_bool_or([])
                if enforce:
                    ct.only_enforce_if(enforce)
                continue
            coefs, k = hr.as_lin(m.add(lv, rv, -1.0))
            coefs = {j: v for j, v in coefs.items() if v != 0}
            if any(j in symbolic_set for j in coefs):
                raise ModelError(f"constraint {label}: a symbolic variable can't be used in arithmetic")
            s = integer_scale(list(coefs.values()))
            lhs_expr = sum(int(round(v * s)) * x[j] for j, v in coefs.items())
            bound = -k * s
            if op == "<=":
                ct = model.add(lhs_expr <= math.floor(bound + 1e-9))
            elif op == ">=":
                ct = model.add(lhs_expr >= math.ceil(bound - 1e-9))
            elif abs(bound - round(bound)) > 1e-9:
                ct = model.add_bool_or([])  # integer left side can't equal a fractional value
            else:
                ct = model.add(lhs_expr == round(bound))
            if enforce:
                ct.only_enforce_if(enforce)

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
