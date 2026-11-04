#!/usr/bin/env python3
"""Solve an optimizam problem file with HiGHS and write an optimizam result file.

This is a development-only reference implementation of docs/problem-format.md
for linear problems. It is used to produce expected results for the
regression tests, and it is independent of the Rust code on purpose.

Usage (from the repository root):
    .venv/bin/python tools/highs_reference.py [--time=SECONDS] PROBLEM RESULT

PROBLEM and RESULT can be "-" for stdin and stdout. HiGHS statistics go to stderr.

Integer variables with a gapped "domain", and every integer variable when
"solutions" is "all", are encoded one-hot: a binary y_v per allowed value v,
with sum(y_v) = 1 and x = sum(v * y_v). "all" then enumerates binary
solutions with no-good cuts.

Limitations, beyond phase-1 scope rules:
  - Those one-hot encoded variables need finite bounds and at most
    MAX_ONE_HOT allowed values.
"""

import argparse
import json
import math
import re
import sys
import time

# highspy is imported only where HiGHS is used, so that other tools (such as
# tools/cpsat_reference.py, whose OR-Tools bundles its own HiGHS) can reuse
# the parser and evaluator without loading it.

INF = float("inf")  # highspy.kHighsInf
KEYWORDS = {"sum", "in", "and", "or", "not"}
MAX_ONE_HOT = 10000


class ModelError(Exception):
    pass


# ---------------------------------------------------------------- tokenizer

TOKEN_RE = re.compile(
    r"\s*(?:(?P<num>\d+\.\d*(?:[eE][+-]?\d+)?|\d*\.\d+(?:[eE][+-]?\d+)?|\d+(?:[eE][+-]?\d+)?)"
    r"|(?P<id>[A-Za-z_][A-Za-z0-9_]*)"
    r"|(?P<op><=|>=|==|!=|[-+*/^()\[\],:<>|]))"
)


def tokenize(text):
    tokens, pos = [], 0
    text = text.rstrip()
    while pos < len(text):
        m = TOKEN_RE.match(text, pos)
        if not m or m.end() == pos:
            raise ModelError(f"cannot tokenize {text[pos:]!r} in {text!r}")
        pos = m.end()
        if m.group("num") is not None:
            s = m.group("num")
            tokens.append(("num", int(s) if re.fullmatch(r"\d+", s) else float(s)))
        elif m.group("id") is not None:
            s = m.group("id")
            tokens.append(("kw" if s in KEYWORDS else "id", s))
        else:
            tokens.append(("op", m.group("op")))
    tokens.append(("end", None))
    return tokens


# ------------------------------------------------------------------- parser
# AST nodes are tuples: ("num", v) ("ref", name, [args] | None) ("neg", e)
# ("bin", op, a, b) for * and /, ("add", [(sign, term), ...]) for + and - (flat,
# so long sums don't recurse per term), ("pow", base, n) ("sum", bindings, cond, body)
# conditions: ("cmp", op, a, b) ("and", a, b) ("or", a, b) ("not", a)


class Parser:
    def __init__(self, text):
        self.text = text
        self.toks = tokenize(text)
        self.i = 0

    def peek(self):
        return self.toks[self.i]

    def next(self):
        t = self.toks[self.i]
        self.i += 1
        return t

    def accept(self, kind, value=None):
        t = self.peek()
        if t[0] == kind and (value is None or t[1] == value):
            self.i += 1
            return t
        return None

    def expect(self, kind, value=None):
        t = self.accept(kind, value)
        if not t:
            raise ModelError(f"expected {value or kind} at {self.peek()[1]!r} in {self.text!r}")
        return t

    def done(self):
        if self.peek()[0] != "end":
            raise ModelError(f"unexpected {self.peek()[1]!r} in {self.text!r}")

    def relation(self):
        lhs = self.expr()
        t = self.next()
        if t[0] != "op" or t[1] not in ("<=", ">=", "=="):
            raise ModelError(f"expected <=, >= or == in {self.text!r}")
        rhs = self.expr()
        self.done()
        return t[1], lhs, rhs

    def expr(self):
        terms = [(1.0, self.term())]
        while self.peek() in (("op", "+"), ("op", "-")):
            sign = 1.0 if self.next()[1] == "+" else -1.0
            terms.append((sign, self.term()))
        return terms[0][1] if len(terms) == 1 else ("add", terms)

    def term(self):
        e = self.factor()
        while self.peek() in (("op", "*"), ("op", "/")):
            op = self.next()[1]
            e = ("bin", op, e, self.factor())
        return e

    def factor(self):
        if self.accept("op", "-"):
            return ("neg", self.factor())
        base = self.atom()
        if self.accept("op", "^"):
            t = self.expect("num")
            if not isinstance(t[1], int):
                raise ModelError(f"exponent must be an integer in {self.text!r}")
            return ("pow", base, t[1])
        return base

    def atom(self):
        t = self.next()
        if t[0] == "num":
            return ("num", t[1])
        if t == ("kw", "sum"):
            self.expect("op", "(")
            bindings, cond = self.bindings()
            self.expect("op", ")")
            return ("sum", bindings, cond, self.term())
        if t == ("op", "("):
            e = self.expr()
            self.expect("op", ")")
            return e
        if t[0] == "id":
            args = None
            if self.accept("op", "["):
                args = [self.expr()]
                while self.accept("op", ","):
                    args.append(self.expr())
                self.expect("op", "]")
            return ("ref", t[1], args)
        raise ModelError(f"unexpected {t[1]!r} in {self.text!r}")

    def list(self):
        """A format 1.1 list: ("list", [items]) or ("comp", body, bindings, cond)."""
        self.expect("op", "[")
        first = self.expr()
        if self.accept("op", "|"):
            bindings, cond = self.bindings()
            out = ("comp", first, bindings, cond)
        else:
            items = [first]
            while self.accept("op", ","):
                items.append(self.expr())
            out = ("list", items)
        self.expect("op", "]")
        self.done()
        return out

    def bindings(self):
        bs = []
        while True:
            idx = self.expect("id")[1]
            self.expect("kw", "in")
            bs.append((idx, self.expect("id")[1]))
            if not self.accept("op", ","):
                break
        cond = self.condition() if self.accept("op", ":") else None
        return bs, cond

    def condition(self):
        c = self.conj()
        while self.accept("kw", "or"):
            c = ("or", c, self.conj())
        return c

    def conj(self):
        c = self.neg()
        while self.accept("kw", "and"):
            c = ("and", c, self.neg())
        return c

    def neg(self):
        if self.accept("kw", "not"):
            return ("not", self.neg())
        save = self.i
        if self.accept("op", "("):
            try:
                c = self.condition()
                self.expect("op", ")")
                return c
            except ModelError:
                self.i = save  # it was a parenthesized expression, not a condition
        a = self.expr()
        t = self.next()
        if t[0] != "op" or t[1] not in ("==", "!=", "<", "<=", ">", ">="):
            raise ModelError(f"expected a comparison in {self.text!r}")
        return ("cmp", t[1], a, self.expr())


# ---------------------------------------------------------------- evaluator
# An affine expression is (coefs: dict[col] -> float, const). Constants used as
# set elements (strings, ints) are carried as ("elem", value).


class Model:
    def __init__(self, problem):
        self.p = problem
        self.sets = {k: self._set(v) for k, v in problem.get("sets", {}).items()}
        self.params = problem.get("parameters", {})
        self.vars = {}  # name -> dict(index tuple -> col)
        self.var_index = {}
        self.cols = []  # (name, index tuple, type)
        self.lower, self.upper, self.integer = [], [], []
        self.domain = []  # list of [lo, hi] ranges for integer columns with a domain, else None
        self.symbolic = {}  # variable name -> its set's elements, for symbolic set-valued variables
        for v in problem["variables"]:
            self._add_var(v)

    @staticmethod
    def _set(s):
        if isinstance(s, dict):
            return list(range(s["from"], s["to"] + 1))
        return list(s)

    def _product(self, set_names):
        tuples = [()]
        for name in set_names:
            if name not in self.sets:
                raise ModelError(f"unknown set {name}")
            tuples = [t + (e,) for t in tuples for e in self.sets[name]]
        return tuples

    def _bound(self, b, idx, default):
        if b is None:
            return default
        if isinstance(b, (int, float)):
            return float(b)
        return float(self.param(b, list(idx)))

    def _add_var(self, v):
        name, typ, index = v["name"], v["type"], v.get("index", [])
        self.var_index[name] = index
        self.vars[name] = {}
        for idx in self._product(index):
            if typ == "binary":
                lo, hi = 0.0, 1.0
            elif "domain" in v:
                ranges = self.domain_ranges(v)
                lo, hi = float(ranges[0][0]), float(ranges[-1][1])
            else:
                lo = self._bound(v.get("lower"), idx, -INF)
                hi = self._bound(v.get("upper"), idx, INF)
            self.vars[name][idx] = len(self.cols)
            self.cols.append((name, idx, typ))
            self.lower.append(lo)
            self.upper.append(hi)
            self.integer.append(typ in ("integer", "binary"))
            self.domain.append(self.domain_ranges(v) if typ == "integer" and "domain" in v else None)

    def domain_ranges(self, v):
        """A variable's domain as sorted [lo, hi] ranges. A domain naming a set
        (format 1.1) is the set's integers, or positions 1..k for a set with
        strings (a symbolic variable, recorded in self.symbolic)."""
        d = v["domain"]
        if not isinstance(d, str):
            return d
        elems = self.sets[d]
        if all(isinstance(e, int) for e in elems):
            ranges = []
            for e in sorted(elems):
                if ranges and e == ranges[-1][1] + 1:
                    ranges[-1][1] = e
                else:
                    ranges.append([e, e])
            return ranges
        self.symbolic[v["name"]] = elems
        return [[1, len(elems)]]

    def eval_list(self, lst, env):
        """A list's items as values (constants or ("lin", coefs, const))."""
        if lst[0] == "list":
            return [self.ev(e, env) for e in lst[1]]
        _, body, bindings, cond = lst
        return [self.ev(body, e) for e in self.iterate(bindings, cond, env)]

    def param(self, name, args):
        p = self.params[name]
        if isinstance(p, (int, float)):
            if args:
                raise ModelError(f"scalar parameter {name} used with an index")
            return p
        if len(args) != len(p["index"]):
            raise ModelError(f"parameter {name} needs {len(p['index'])} indices")
        node = p["values"]
        for a in args:
            if not isinstance(node, dict) or str(a) not in node:
                if "default" in p:
                    return p["default"]
                raise ModelError(f"parameter {name}{list(args)} has no value")
            node = node[str(a)]
        return node

    # values: float/int/str constants, or ("lin", coefs, const)
    def ev(self, node, env):
        kind = node[0]
        if kind == "num":
            return node[1]
        if kind == "ref":
            _, name, args = node
            if args is None and name in env:
                return env[name]
            argv = None if args is None else [self.const(self.ev(a, env)) for a in args]
            if name in self.params:
                return self.param(name, argv or [])
            if name in self.vars:
                idx = tuple(argv or [])
                if idx not in self.vars[name]:
                    raise ModelError(f"{name}{list(idx)} is outside its index sets")
                return ("lin", {self.vars[name][idx]: 1.0}, 0.0)
            raise ModelError(f"unknown name {name}")
        if kind == "neg":
            return self.scale(self.ev(node[1], env), -1.0)
        if kind == "add":
            # Accumulate into one coefficient map: linear in the number of terms.
            coefs, constant = {}, 0.0
            for sign, t in node[1]:
                c, k = as_lin(self.ev(t, env))
                for j, v in c.items():
                    coefs[j] = coefs.get(j, 0.0) + sign * v
                constant += sign * k
            return ("lin", coefs, constant) if coefs else constant
        if kind == "bin":
            _, op, a, b = node
            x, y = self.ev(a, env), self.ev(b, env)
            if op == "*":
                if is_lin(x) and is_lin(y):
                    raise NonLinear()
                return self.scale(y, x) if not is_lin(x) else self.scale(x, y)
            if is_lin(y):
                raise NonLinear()
            return self.scale(x, 1.0 / y)
        if kind == "pow":
            _, b, n = node
            x = self.ev(b, env)
            if not is_lin(x):
                return x ** n
            if n == 1:
                return x
            if n == 0:
                return 1
            raise NonLinear()
        if kind == "sum":
            _, bindings, cond, body = node
            total = 0
            for e in self.iterate(bindings, cond, env):
                total = self.add(total, self.ev(body, e), 1.0)
            return total
        raise ModelError(f"bad node {node}")

    def const(self, x):
        if is_lin(x):
            raise ModelError("an index or condition depends on a variable")
        return x

    def add(self, x, y, sign):
        if not is_lin(x) and not is_lin(y):
            return x + sign * y
        cx, kx = as_lin(x)
        cy, ky = as_lin(y)
        c = dict(cx)
        for k, v in cy.items():
            c[k] = c.get(k, 0.0) + sign * v
        return ("lin", c, kx + sign * ky)

    @staticmethod
    def scale(x, s):
        if not is_lin(x):
            return x * s
        return ("lin", {k: v * s for k, v in x[1].items()}, x[2] * s)

    def test(self, c, env):
        kind = c[0]
        if kind == "and":
            return self.test(c[1], env) and self.test(c[2], env)
        if kind == "or":
            return self.test(c[1], env) or self.test(c[2], env)
        if kind == "not":
            return not self.test(c[1], env)
        _, op, a, b = c
        x, y = self.const(self.ev(a, env)), self.const(self.ev(b, env))
        return {"==": x == y, "!=": x != y, "<": x < y, "<=": x <= y, ">": x > y, ">=": x >= y}[op]

    def iterate(self, bindings, cond, env):
        envs = [dict(env)]
        for idx, set_name in bindings:
            if set_name not in self.sets:
                raise ModelError(f"unknown set {set_name}")
            envs = [{**e, idx: el} for e in envs for el in self.sets[set_name]]
        return [e for e in envs if cond is None or self.test(cond, e)]


class NonLinear(Exception):
    pass


def is_lin(x):
    return isinstance(x, tuple) and x[0] == "lin"


def as_lin(x):
    return (x[1], x[2]) if is_lin(x) else ({}, float(x))


def build(problem):
    m = Model(problem)
    rows = []  # (name, coefs, lo, hi)
    for i, c in enumerate(problem["constraints"]):
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
            try:
                coefs, k = as_lin(m.add(m.ev(lhs, env), m.ev(rhs, env), -1.0))
            except NonLinear:
                raise ModelError(f"unsupported: nonlinear term in constraint {label}")
            coefs = {j: v for j, v in coefs.items() if v != 0.0}
            lo, hi = {"<=": (-INF, -k), ">=": (-k, INF), "==": (-k, -k)}[op]
            rows.append((label, coefs, lo, hi))
    solve = problem["solve"]
    cost, offset = {}, 0.0
    if solve["method"] != "satisfy":
        try:
            cost, offset = as_lin(m.ev(Parser_expr(solve["objective"]), {}))
        except NonLinear:
            raise ModelError("unsupported: nonlinear term in the objective")
    return m, rows, cost, offset


def Parser_expr(text):
    p = Parser(text)
    e = p.expr()
    p.done()
    return e


# ------------------------------------------------------------------- solving


def one_hot(m, rows, want_all):
    """Encodes integer columns one-hot where needed (see the module doc).
    Appends the binary columns to m, which keeps them out of m.vars and so out
    of the result, and returns the extra rows."""
    extra = []
    for j in range(len(m.cols)):
        name, idx, typ = m.cols[j]
        if typ != "integer" or not (want_all or (m.domain[j] and len(m.domain[j]) > 1)):
            continue
        lo, hi = m.lower[j], m.upper[j]
        if not (math.isfinite(lo) and math.isfinite(hi)):
            raise ModelError(f"reference tool: integer {name} needs finite bounds here")
        ranges = m.domain[j] or [[math.ceil(lo), math.floor(hi)]]
        allowed = [v for a, b in ranges for v in range(a, b + 1)]
        if len(allowed) > MAX_ONE_HOT:
            raise ModelError(f"reference tool: integer {name} has too many values to encode")
        link, pick = {j: 1.0}, {}
        for v in allowed:
            y = len(m.cols)
            m.cols.append(("__one_hot", (j, v), "binary"))
            m.lower.append(0.0)
            m.upper.append(1.0)
            m.integer.append(True)
            m.domain.append(None)
            link[y] = -float(v)
            pick[y] = 1.0
        extra.append((f"__one_hot_link[{j}]", link, 0.0, 0.0))
        extra.append((f"__one_hot_pick[{j}]", pick, 1.0, 1.0))
    return extra


def make_highs(m, rows, cost, offset, sense, time_limit):
    import highspy

    lp = highspy.HighsLp()
    n = len(m.cols)
    lp.num_col_ = n
    lp.num_row_ = len(rows)
    lp.col_cost_ = [cost.get(j, 0.0) for j in range(n)]
    lp.col_lower_ = m.lower
    lp.col_upper_ = m.upper
    lp.row_lower_ = [r[2] for r in rows]
    lp.row_upper_ = [r[3] for r in rows]
    lp.offset_ = offset
    lp.sense_ = highspy.ObjSense.kMaximize if sense == "maximize" else highspy.ObjSense.kMinimize
    lp.a_matrix_.format_ = highspy.MatrixFormat.kRowwise
    start, index, value = [0], [], []
    for _, coefs, _, _ in rows:
        for j in sorted(coefs):
            index.append(j)
            value.append(coefs[j])
        start.append(len(index))
    lp.a_matrix_.start_, lp.a_matrix_.index_, lp.a_matrix_.value_ = start, index, value
    if any(m.integer):
        lp.integrality_ = [highspy.HighsVarType.kInteger if b else highspy.HighsVarType.kContinuous for b in m.integer]
    h = highspy.Highs()
    h.silent()
    if time_limit is not None:
        h.setOptionValue("time_limit", float(time_limit))
    h.passModel(lp)
    return h


def values(m, h):
    x = list(h.getSolution().col_value)
    return [round(v) if m.integer[j] else v for j, v in enumerate(x)]


def solution_json(m, x, objective):
    vars_out = {}
    for name, cols in m.vars.items():
        if not m.var_index[name]:
            vars_out[name] = x[cols[()]]
            continue
        table = {}
        for idx, j in cols.items():
            node = table
            for e in idx[:-1]:
                node = node.setdefault(str(e), {})
            node[str(idx[-1])] = x[j]
        vars_out[name] = table
    sol = {}
    if objective is not None:
        sol["objective"] = objective
    sol["variables"] = vars_out
    return sol


def run(problem, time_limit):
    start = time.monotonic()
    m, rows, cost, offset = build(problem)
    solve = problem["solve"]
    method, want_all = solve["method"], solve.get("solutions", "one") == "all"
    if want_all and any(t == "continuous" for _, _, t in m.cols):
        raise ModelError("unsupported: \"solutions\": \"all\" requires every variable to be integer or binary")
    rows = rows + one_hot(m, rows, want_all)
    optimizing = method != "satisfy"
    stats = {"cols": len(m.cols), "rows": len(rows), "integer_cols": sum(m.integer)}

    def remaining():
        return None if time_limit is None else max(0.0, time_limit - (time.monotonic() - start))

    def objective_of(x):
        return offset + sum(c * x[j] for j, c in cost.items()) if optimizing else None

    def finish(status, sols, bound=None):
        res = {"version": "1.0", "status": status, "solutions": sols}
        if bound is not None:
            res["objective_bound"] = bound
        return res, stats

    h = make_highs(m, rows, cost, offset, method, remaining())
    h.run()
    st, info = h.getModelStatus(), h.getInfo()
    stats.update(seconds=round(time.monotonic() - start, 3), status=h.modelStatusToString(st),
                 simplex_iterations=info.simplex_iteration_count, mip_nodes=info.mip_node_count)
    import highspy

    S = highspy.HighsModelStatus

    if st in (S.kUnboundedOrInfeasible, S.kUnbounded):
        # Decide precisely: look for any feasible point with a zero objective.
        h0 = make_highs(m, rows, {}, 0.0, "minimize", remaining())
        h0.run()
        st0 = h0.getModelStatus()
        if st0 == S.kOptimal:
            return finish("unbounded" if optimizing else "complete",
                          [] if optimizing else [solution_json(m, values(m, h0), None)])
        if st0 == S.kInfeasible:
            return finish("infeasible", [])
        return finish("timeout", [])
    if st == S.kInfeasible:
        return finish("infeasible", [])
    if st == S.kTimeLimit:
        if any(m.integer) and info.primal_solution_status == 2:  # feasible incumbent
            x = values(m, h)
            return finish("not_proven_optimal", [solution_json(m, x, objective_of(x))], info.mip_dual_bound)
        return finish("timeout", [])
    if st != S.kOptimal:
        raise ModelError(f"HiGHS ended with {h.modelStatusToString(st)}")

    x = values(m, h)
    if not want_all:
        return finish("complete", [solution_json(m, x, objective_of(x))])

    # Enumerate all optimal (or all feasible) binary solutions with no-good cuts.
    best = objective_of(x)
    extra = []
    if optimizing:
        tol = 1e-6 * max(1.0, abs(best))
        coefs = {j: c for j, c in cost.items() if c != 0.0}
        lo, hi = (best - offset - tol, INF) if method == "maximize" else (-INF, best - offset + tol)
        extra.append(("optimal", coefs, lo, hi))
    sols = []
    while True:
        sols.append(solution_json(m, x, objective_of(x)))
        # Only binary columns: integers are fixed by their one-hot encoding.
        binary = [j for j, (_, _, t) in enumerate(m.cols) if t == "binary"]
        ones = [j for j in binary if x[j] == 1]
        zeros = [j for j in binary if x[j] == 0]
        # sum_{ones}(1 - x) + sum_{zeros} x >= 1
        extra.append(("nogood", {**{j: -1.0 for j in ones}, **{j: 1.0 for j in zeros}}, 1.0 - len(ones), INF))
        h = make_highs(m, rows + extra, {}, 0.0, "minimize", remaining())
        h.run()
        st = h.getModelStatus()
        if st == S.kInfeasible:
            return finish("complete", sols)
        if st == S.kTimeLimit:
            return finish("incomplete", sols)
        if st != S.kOptimal:
            raise ModelError(f"HiGHS ended with {h.modelStatusToString(st)} while enumerating")
        x = values(m, h)


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
    print("highs:", json.dumps(stats), file=sys.stderr)


if __name__ == "__main__":
    main()
