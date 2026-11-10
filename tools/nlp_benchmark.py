#!/usr/bin/env python3
"""Benchmark optimizam's nonlinear engine on the Hock–Schittkowski problems.

The problems are the AMPL versions of the Hock–Schittkowski test set (W.
Hock and K. Schittkowski, "Test Examples for Nonlinear Programming Codes",
1981) from Robert Vanderbei's collection of CUTE models
(vanderbei.princeton.edu/ampl/nlmodels/cute/), downloaded on first use into
target/nlp_benchmark/ and translated into optimizam's problem format (1.2)
by a small interpreter for the AMPL subset they use. Each model's
`display obj - VALUE` line gives the published optimum.

Usage (from the repository root, after `cargo build --release`):
    python3 tools/nlp_benchmark.py [--time SECONDS] [--ipopt] [--bin PATH]
        [--dir DIR] [names...]

Prints, per problem, optimizam's time, status, and objective, the published
optimum, and whether they agree (relative difference 1e-6). Local solvers
may find another local optimum of a nonconvex problem; those are counted
separately from failures. With --ipopt, also runs the IPOPT reference
(tools/ipopt.sh, which needs the project's .venv with casadi) and compares.
Uses only the standard library.
"""

import argparse
import json
import math
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

# One thread unless OPTIMIZAM_THREADS says otherwise (optimizam's own default
# is one per core): timings and search paths then repeat from run to run.
os.environ.setdefault("OPTIMIZAM_THREADS", "1")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASE = "https://vanderbei.princeton.edu/ampl/nlmodels/cute/"


def download(name, directory):
    path = os.path.join(directory, "ampl", name + ".mod")
    if not os.path.exists(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        req = urllib.request.Request(BASE + name + ".mod", headers={"User-Agent": "optimizam-benchmark"})
        with urllib.request.urlopen(req, timeout=60) as r, open(path + ".part", "wb") as f:
            f.write(r.read())
        os.replace(path + ".part", path)
    with open(path) as f:
        return f.read()


def problem_names(directory):
    """hs001 ... from the collection's index (cached)."""
    path = os.path.join(directory, "index.html")
    if not os.path.exists(path):
        os.makedirs(directory, exist_ok=True)
        req = urllib.request.Request(BASE, headers={"User-Agent": "optimizam-benchmark"})
        with urllib.request.urlopen(req, timeout=60) as r, open(path, "wb") as f:
            f.write(r.read())
    with open(path) as f:
        listed = sorted(set(re.findall(r'(?i)href="(hs[0-9a-z]+)\.mod"', f.read())))
    # The index links hs1.mod; the files are hs001.mod.
    names = []
    for n in listed:
        m = re.fullmatch(r"hs(\d+)(.*)", n)
        names.append(f"hs{int(m.group(1)):03d}{m.group(2)}")
    return sorted(set(names))


# ------------------------------------------------------- AMPL (subset) reader


class Unsupported(Exception):
    pass


TOKEN = re.compile(r"\s*(?:(?P<num>(?:\d+(?:\.(?!\.)\d*)?|\.\d+)(?:[eE][-+]?\d+)?)|(?P<id>[A-Za-z_][A-Za-z0-9_.]*)"
                   r"|(?P<op>\.\.|<=|>=|==|!=|<>|:=|\*\*|[-+*/^()\[\]{},:;<>=]))")
FUNCS = {"log": "ln", "exp": "exp", "sqrt": "sqrt", "sin": "sin", "cos": "cos", "tan": "tan", "atan": "atan",
         "tanh": "tanh"}
PYFUNCS = {"log": math.log, "exp": math.exp, "sqrt": math.sqrt, "sin": math.sin, "cos": math.cos, "tan": math.tan,
           "atan": math.atan, "tanh": math.tanh, "abs": abs}


def tokens(text):
    out, pos = [], 0
    while pos < len(text):
        if text[pos:].strip() == "":
            break
        m = TOKEN.match(text, pos)
        if not m or m.end() == pos:
            raise Unsupported(f"cannot read {text[pos:pos + 20]!r}")
        pos = m.end()
        for kind in ("num", "id", "op"):
            if m.group(kind) is not None:
                v = m.group(kind)
                out.append((kind, float(v) if kind == "num" else v))
    out.append(("end", None))
    return out


class P:
    """Recursive-descent parser for AMPL expressions into tuples."""

    def __init__(self, toks):
        self.t, self.i = toks, 0

    def peek(self, k=0):
        return self.t[self.i + k]

    def next(self):
        tok = self.t[self.i]
        self.i += 1
        return tok

    def accept(self, v):
        if self.peek()[1] == v and self.peek()[0] in ("op", "id"):
            self.i += 1
            return True
        return False

    def expect(self, v):
        if not self.accept(v):
            raise Unsupported(f"expected {v} at {self.peek()[1]!r}")

    def indexing(self, sets=None):
        """{i in 1..n, j in 1..m: cond} -> ([(name or None, lo, hi)], cond)."""
        sets = sets if sets is not None else getattr(self, "sets", {})
        self.expect("{")
        items = []
        while True:
            name = None
            if self.peek()[0] == "id" and self.peek(1)[1] == "in":
                name = self.next()[1]
                self.next()
            if self.peek()[0] == "id" and self.peek()[1] in sets and self.peek(1)[1] != "..":
                lo, hi = sets[self.next()[1]]
            else:
                lo = self.sum_expr()
                self.expect("..")
                hi = self.sum_expr()
            items.append((name, lo, hi))
            if not self.accept(","):
                break
        cond = self.condition() if self.accept(":") else None
        self.expect("}")
        return items, cond

    def condition(self):
        a = self.sum_expr()
        op = self.next()[1]
        if op not in ("<", "<=", ">", ">=", "=", "==", "!=", "<>"):
            raise Unsupported(f"condition operator {op}")
        return ("cmp", op, a, self.sum_expr())

    def sum_expr(self):
        terms = [(1, self.iterated())]
        while self.peek()[1] in ("+", "-") and self.peek()[0] == "op":
            sign = 1 if self.next()[1] == "+" else -1
            terms.append((sign, self.iterated()))
        return terms[0][1] if len(terms) == 1 else ("add", terms)

    def iterated(self):
        if self.peek() in (("id", "sum"), ("id", "prod")):
            kind = self.next()[1]
            idx = self.indexing()
            return (kind, idx, self.iterated())
        return self.product()

    def product(self):
        e = self.unary()
        while self.peek()[1] in ("*", "/") and self.peek()[0] == "op":
            op = self.next()[1]
            e = ("bin", op, e, self.unary())
        return e

    def unary(self):
        if self.accept("-"):
            return ("neg", self.unary())
        if self.accept("+"):
            return self.unary()
        return self.power()

    def power(self):
        base = self.atom()
        if self.peek()[1] in ("^", "**") and self.peek()[0] == "op":
            self.next()
            return ("pow", base, self.unary())  # right-associative, allows x^-2
        return base

    def atom(self):
        kind, v = self.next()
        if kind == "num":
            return ("num", v)
        if (kind, v) == ("op", "("):
            e = self.sum_expr()
            self.expect(")")
            return e
        if kind == "id":
            if v in ("sum", "prod"):
                self.i -= 1
                return self.iterated()
            if self.peek()[1] == "(":
                if v not in PYFUNCS:
                    raise Unsupported(f"function {v}")
                self.next()
                arg = self.sum_expr()
                self.expect(")")
                return ("call", v, arg)
            args = None
            if self.accept("["):
                args = [self.sum_expr()]
                while self.accept(","):
                    args.append(self.sum_expr())
                self.expect("]")
            return ("ref", v, args)
        raise Unsupported(f"unexpected {v!r}")


class Ampl:
    """The declarations and data of one model, then its translation."""

    def __init__(self, text):
        text = re.sub(r"#[^\n]*", "", text)
        self.params = {}  # name -> {"index": indexing or None, "expr": ast or None, "default": ast, "values": {}}
        self.vars = {}  # name -> {"index": indexing, "lower", "upper", "start": ast or None, "values": {}}
        self.objective = None  # (sense, ast)
        self.constraints = []  # (name, indexing or None, [ast, op, ast, (op, ast)])
        self.optimum = None
        self.fixed = {}  # (name, idx) -> value
        self.defined = {}  # name -> (indexing or None, expression)
        self.sets = {}  # name -> (lo, hi) for `set I := a .. b`
        for stmt in text.split(";"):
            stmt = stmt.strip()
            if stmt:
                self.statement(stmt)

    def statement(self, stmt):
        word = stmt.split()[0]
        if word in ("option", "printf", "print"):
            return
        toks = tokens(stmt)
        word = toks[0][1]
        if word in ("data", "model", "solve", "option", "printf", "expand", "end", "reset", "objective", "write"):
            return
        if word == "display":
            m = re.fullmatch(r"display\s+obj\s*([-+])\s*([-+]?[\d.]+(?:[eE][-+]?\d+)?)", stmt)
            if m:
                v = float(m.group(2))
                self.optimum = v if m.group(1) == "-" else -v
            return
        p = P(toks)
        p.sets = self.sets
        p.next()
        if word == "set":
            name = p.next()[1]
            p.expect(":=") if p.peek()[1] == ":=" else p.expect("=")
            lo = p.sum_expr()
            p.expect("..")
            self.sets[name] = (lo, p.sum_expr())
            return
        if word == "param" and p.peek()[1] == ":":
            self.columns(p)
            return
        if word == "param":
            name = p.next()[1]
            if p.peek()[1] == "{":
                index = p.indexing()
            else:
                index = None
            if name in self.params and index is None and p.accept(":="):
                self.data(name, p)
                return
            if name in self.params and index is None and p.peek()[1] == ":":
                self.table(name, p)
                return
            entry = self.params.setdefault(name, {"index": index, "expr": None, "default": None, "values": {}})
            while p.peek()[0] != "end":
                if p.accept(":=") or p.accept("="):
                    if index is None or p.peek()[0] == "end":
                        entry["expr"] = p.sum_expr()
                    else:
                        entry["expr"] = p.sum_expr()
                elif p.accept("default"):
                    entry["default"] = p.sum_expr()
                elif p.accept(">=") or p.accept("<=") or p.accept(">") or p.accept("<"):
                    p.sum_expr()
                elif p.accept(",") or p.accept("integer"):
                    pass
                else:
                    raise Unsupported(f"param syntax at {p.peek()[1]!r}")
            return
        if word == "var":
            name = p.next()[1]
            index = p.indexing() if p.peek()[1] == "{" else None
            v = {"index": index, "lower": None, "upper": None, "start": None, "values": {}}
            while p.peek()[0] != "end":
                if p.accept(">="):
                    v["lower"] = p.sum_expr()
                elif p.accept("<="):
                    v["upper"] = p.sum_expr()
                elif p.accept(":=") or p.accept("default"):
                    v["start"] = p.sum_expr()
                elif p.accept("="):
                    # A defined variable: substituted where it is used.
                    self.defined[name] = (index, p.sum_expr())
                    if p.peek()[0] != "end":
                        raise Unsupported("defined variable syntax")
                    return
                elif p.accept(","):
                    pass
                elif p.peek()[1] in ("integer", "binary"):
                    raise Unsupported("integer variables")
                else:
                    raise Unsupported(f"var syntax at {p.peek()[1]!r}")
            self.vars[name] = v
            return
        if word in ("minimize", "maximize"):
            p.next()  # objective name
            p.expect(":")
            self.objective = (word, p.sum_expr())
            if p.peek()[0] != "end":
                raise Unsupported("objective syntax")
            return
        if word in ("subject", "s.t."):
            if word == "subject":
                p.expect("to")
            name = p.next()[1]
            index = p.indexing() if p.peek()[1] == "{" else None
            p.expect(":")
            parts = [p.sum_expr()]
            while p.peek()[0] != "end":
                op = p.next()[1]
                if op not in ("<=", ">=", "=", "=="):
                    raise Unsupported(f"relation {op}")
                parts += [op, p.sum_expr()]
            if len(parts) not in (3, 5):
                raise Unsupported("constraint form")
            self.constraints.append((name, index, parts))
            return
        if word in ("let", "fix"):
            index = p.indexing() if p.peek()[1] == "{" else None
            target = p.atom()
            if target[0] != "ref":
                raise Unsupported("let target")
            value = p.sum_expr() if (p.accept(":=") or p.accept("=")) else None
            self.lets = getattr(self, "lets", [])
            self.lets.append((word, index, target, value))
            return
        raise Unsupported(f"statement {word}")

    def numbers(self, p):
        values = []
        while p.peek()[0] != "end" and p.peek()[1] != ":=":
            kind, v = p.next()
            if kind == "op" and v == "-":
                kind, v = p.next()
                v = -v
            elif kind == "op" and v in (",", ":"):
                continue
            if kind != "num":
                raise Unsupported("param data")
            values.append(v)
        return values

    def table(self, name, p):
        """param name : c1 c2 ... := r v v ... (a two-index table)."""
        p.expect(":")
        cols = [int(v) for v in self.numbers(p)]
        p.expect(":=")
        values = self.numbers(p)
        width = len(cols) + 1
        if len(values) % width:
            raise Unsupported("param table shape")
        for k in range(0, len(values), width):
            row = int(values[k])
            for c, v in zip(cols, values[k + 1:k + width]):
                self.params[name]["values"][(row, c)] = v

    def columns(self, p):
        """param : a b := i a_i b_i ... (several one-index params)."""
        p.expect(":")
        names = []
        while p.peek()[1] != ":=":
            names.append(p.next()[1])
        p.expect(":=")
        values = self.numbers(p)
        width = len(names) + 1
        if len(values) % width or not all(n in self.params for n in names):
            raise Unsupported("param columns")
        for k in range(0, len(values), width):
            for name, v in zip(names, values[k + 1:k + width]):
                self.params[name]["values"][(int(values[k]),)] = v

    def data(self, name, p):
        """param name := k v k v ...  (one index) or := v (scalar)."""
        values = []
        while p.peek()[0] != "end":
            kind, v = p.next()
            if kind == "op" and v == "-":
                kind, v = p.next()
                v = -v
            elif kind == "op" and v in (",",):
                continue
            if kind != "num":
                raise Unsupported("param data")
            values.append(v)
        entry = self.params[name]
        if entry["index"] is None:
            entry["expr"] = ("num", values[0])
            return
        dims = len(entry["index"][0])
        if len(values) % (dims + 1):
            raise Unsupported("param data shape")
        for k in range(0, len(values), dims + 1):
            idx = tuple(int(x) for x in values[k:k + dims])
            entry["values"][idx] = values[k + dims]

    # ------------------------------------------------------- evaluation

    def bindings(self, indexing, env):
        items, cond = indexing
        envs = [dict(env)]
        for name, lo, hi in items:
            nxt = []
            for e in envs:
                a, b = self.num(lo, e), self.num(hi, e)
                for v in range(int(round(a)), int(round(b)) + 1):
                    d = dict(e)
                    d[name or f"_{id(lo)}"] = v
                    d.setdefault("_tuple", ())
                    d["_tuple"] = d["_tuple"] + (v,)
                    nxt.append(d)
            envs = nxt
        if cond is not None:
            envs = [e for e in envs if self.test(cond, e)]
        return envs

    def test(self, c, env):
        _, op, a, b = c
        x, y = self.num(a, env), self.num(b, env)
        return {"<": x < y, "<=": x <= y, ">": x > y, ">=": x >= y, "=": x == y, "==": x == y,
                "!=": x != y, "<>": x != y}[op]

    def param(self, name, idx):
        entry = self.params[name]
        if idx in entry["values"]:
            return entry["values"][idx]
        if entry["expr"] is not None:
            env = {}
            if entry["index"] is not None:
                names = [n for n, _, _ in entry["index"][0]]
                env = {n: v for n, v in zip(names, idx) if n}
            return self.num(entry["expr"], env)
        if entry["default"] is not None:
            return self.num(entry["default"], {})
        raise Unsupported(f"param {name}{list(idx)} has no value")

    def num(self, e, env):
        v = self.emit(e, env)
        if isinstance(v, str):
            raise Unsupported("a variable where a number is needed")
        return v

    def emit(self, e, env):
        """A number, or a string in optimizam's grammar."""
        kind = e[0]
        if kind == "num":
            return e[1]
        if kind == "ref":
            _, name, args = e
            if args is None and name in env:
                return env[name]
            idx = tuple(int(round(self.num(a, env))) for a in (args or []))
            if name == "Infinity":
                return math.inf
            if name in self.params:
                return self.param(name, idx)
            if name in self.defined:
                index, expr = self.defined[name]
                inner = dict(env)
                if index is not None:
                    for (n, _, _), v in zip(index[0], idx):
                        if n:
                            inner[n] = v
                return self.emit(expr, inner)
            if name in self.vars:
                if (name, idx) in self.fixed:
                    return self.fixed[(name, idx)]
                return f"{name}[{','.join(map(str, idx))}]" if idx else name
            raise Unsupported(f"unknown name {name}")
        if kind == "neg":
            v = self.emit(e[1], env)
            return -v if not isinstance(v, str) else f"(-{v})"
        if kind == "add":
            nums, parts = 0.0, []
            for sign, t in e[1]:
                v = self.emit(t, env)
                if isinstance(v, str):
                    parts.append(("+ " if sign > 0 else "- ") + v)
                else:
                    nums += sign * v
            if not parts:
                return nums
            text = " ".join(parts)
            return f"({fmt(nums)} {text})" if nums else f"(0 {text})"
        if kind == "bin":
            _, op, a, b = e
            x, y = self.emit(a, env), self.emit(b, env)
            if not isinstance(x, str) and not isinstance(y, str):
                return x * y if op == "*" else x / y
            return f"({wrap(x)} {op} {wrap(y)})"
        if kind == "pow":
            x, y = self.emit(e[1], env), self.emit(e[2], env)
            if not isinstance(x, str) and not isinstance(y, str):
                return x ** y
            return f"({wrap(x)}^{wrap(y)})"
        if kind == "call":
            v = self.emit(e[2], env)
            if not isinstance(v, str):
                return PYFUNCS[e[1]](v)
            if e[1] not in FUNCS:
                raise Unsupported(f"function {e[1]} of a variable")
            return f"{FUNCS[e[1]]}({v})"
        if kind in ("sum", "prod"):
            _, indexing, body = e
            parts = [self.emit(body, b) for b in self.bindings(indexing, env)]
            if all(not isinstance(v, str) for v in parts):
                return sum(parts) if kind == "sum" else math.prod(parts)
            if not parts:
                return 0.0 if kind == "sum" else 1.0
            joiner = " + " if kind == "sum" else " * "
            return "(" + joiner.join(wrap(v) for v in parts) + ")"
        raise Unsupported(f"expression {kind}")

    def translate(self):
        if self.objective is None:
            raise Unsupported("no objective")
        # Lets: parameter values first, then variable starts and fixings.
        starts = {}
        for word, index, target, value in getattr(self, "lets", []):
            _, name, args = target
            for env in (self.bindings(index, {}) if index else [{}]):
                idx = tuple(int(round(self.num(a, env))) for a in (args or []))
                if name in self.params:
                    self.params[name]["values"][idx] = self.num(value, env)
                elif name in self.vars:
                    v = self.num(value, env) if value is not None else None
                    if word == "fix":
                        self.fixed[(name, idx)] = v if v is not None else starts.get((name, idx), 0.0)
                    else:
                        starts[(name, idx)] = v
                else:
                    raise Unsupported(f"let of {name}")
        sets, variables, params = {}, [], {}
        for name, v in self.vars.items():
            envs = self.bindings(v["index"], {}) if v["index"] else [{}]
            idxs = [e.get("_tuple", ()) for e in envs]
            dims = len(idxs[0])
            set_names = []
            for d in range(dims):
                sname = f"{name.upper()}_{d + 1}"
                sets[sname] = sorted({i[d] for i in idxs})
                set_names.append(sname)
            if dims and len(idxs) != math.prod(len(sets[s]) for s in set_names):
                raise Unsupported("a non-rectangular variable index")
            decl = {"name": name, "type": "continuous"}
            if dims:
                decl["index"] = set_names
            for field in ("lower", "upper", "start"):
                values = {}
                for env, idx in zip(envs, idxs):
                    if field == "start" and (name, idx) in starts:
                        values[idx] = starts[(name, idx)]
                    elif v[field] is not None:
                        b = self.num(v[field], env)
                        if math.isfinite(b):
                            values[idx] = b
                if not values:
                    continue
                if dims == 0:
                    decl[field] = values[()]
                    continue
                if len(set(values.values())) == 1 and len(values) == len(idxs):
                    decl[field] = next(iter(values.values()))
                    continue
                if len(values) != len(idxs):
                    if field == "start":
                        values = {i: values.get(i, 0.0) for i in idxs}
                    else:
                        raise Unsupported(f"{field} given for only some elements of {name}")
                table = {}
                for idx, val in values.items():
                    node = table
                    for e in idx[:-1]:
                        node = node.setdefault(str(e), {})
                    node[str(idx[-1])] = val
                pname = f"{name}_{field}"
                params[pname] = {"index": set_names, "values": table}
                decl[field] = pname
            variables.append(decl)
        # Fixed variables: equal bounds.
        for (name, idx), val in self.fixed.items():
            raise Unsupported("fixed variables")
        constraints = []
        for name, index, parts in self.constraints:
            for env in (self.bindings(index, {}) if index else [{}]):
                label = name + (f"_{'_'.join(map(str, env.get('_tuple', ())))}" if index else "")
                sides = [self.emit(x, env) for x in parts[0::2]]
                ops = [{"=": "=="}.get(o, o) for o in parts[1::2]]
                if len(sides) == 2:
                    exprs = [f"{wrap(sides[0])} {ops[0]} {wrap(sides[1])}"]
                else:
                    exprs = [f"{wrap(sides[0])} {ops[0]} {wrap(sides[1])}", f"{wrap(sides[1])} {ops[1]} {wrap(sides[2])}"]
                for k, text in enumerate(exprs):
                    if not any(isinstance(s, str) for s in sides):
                        continue  # constant
                    constraints.append({"name": re.sub(r"\W", "_", label) + (f"_{k + 1}" if len(exprs) > 1 else ""),
                                        "expr": text})
        sense, obj = self.objective
        objective = self.emit(obj, {})
        problem = {"version": "1.2", "sets": sets, "parameters": params, "variables": variables,
                   "constraints": constraints,
                   "solve": {"method": sense, "objective": wrap(objective)}}
        if not sets:
            del problem["sets"]
        if not params:
            del problem["parameters"]
        return problem


def fmt(v):
    r = repr(float(v))
    return r if not r.startswith("-") else f"({r})"


def wrap(v):
    return v if isinstance(v, str) else fmt(v)


# ------------------------------------------------------------------- running


def run(command, path, time_limit):
    start = time.monotonic()
    r = subprocess.run([command, f"--time={time_limit}", path, "-"], capture_output=True, text=True)
    seconds = time.monotonic() - start
    if r.returncode != 0:
        return {"seconds": seconds, "status": "error", "message": (r.stderr.strip().splitlines() or ["?"])[-1]}
    result = json.loads(r.stdout)
    sols = result["solutions"]
    return {"seconds": seconds, "status": result["status"], "objective": sols[0].get("objective") if sols else None}


def agrees(a, b):
    return a is not None and b is not None and abs(a - b) <= 1e-6 * max(1.0, abs(a), abs(b))


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--time", type=float, default=10.0)
    ap.add_argument("--ipopt", action="store_true")
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"))
    ap.add_argument("--dir", default=os.path.join(ROOT, "target", "nlp_benchmark"))
    ap.add_argument("names", nargs="*")
    a = ap.parse_args()
    names = problem_names(a.dir)
    if a.names:
        names = [n for n in names if any(n.startswith(p) for p in a.names)]
    os.makedirs(os.path.join(a.dir, "instances"), exist_ok=True)
    counts = {"optimum": 0, "other": 0, "unknown": 0, "failed": 0, "skipped": 0}
    ipopt_counts = {"optimum": 0, "other": 0, "unknown": 0, "failed": 0}

    def verdict_of(res, known):
        if res["status"] not in ("locally_optimal", "complete"):
            return "failed"
        if known is None:
            return "unknown"
        return "optimum" if agrees(res["objective"], known) else "other"
    total = 0.0
    proven = 0
    header = f"{'problem':12} {'seconds':>8} {'status':20} {'objective':>16} {'published':>16}"
    print(header + ("  ipopt" if a.ipopt else ""))
    for name in names:
        try:
            ampl = Ampl(download(name, a.dir))
            problem = ampl.translate()
        except (Unsupported, ValueError, ZeroDivisionError, OverflowError, KeyError, IndexError,
                urllib.error.URLError) as e:
            counts["skipped"] += 1
            print(f"{name:12} skipped: {e}")
            continue
        path = os.path.join(a.dir, "instances", name + ".json")
        with open(path, "w") as f:
            json.dump(problem, f, indent=1)
        r = run(a.bin, path, a.time)
        total += r["seconds"]
        known = ampl.optimum
        verdict = verdict_of(r, known)
        counts[verdict] += 1
        proven += r["status"] in ("complete", "infeasible")
        line = (f"{name:12} {r['seconds']:8.2f} {r['status']:20} {fmt_num(r.get('objective')):>16} "
                f"{fmt_num(known):>16}")
        if r["status"] == "error":
            line += f"  {r['message']}"
        if verdict == "other":
            line += "  (another local optimum)"
        if a.ipopt:
            q = run(os.path.join(ROOT, "tools", "ipopt.sh"), path, a.time)
            ipopt_counts[verdict_of(q, known)] += 1
            solved = ("locally_optimal", "complete")
            same_status = q["status"] == r["status"] or (q["status"] in solved and r["status"] in solved) or \
                (q["status"], r["status"]) == ("locally_infeasible", "infeasible")
            same = same_status and (agrees(q.get("objective"), r.get("objective")) or r["status"] == "infeasible")
            line += f"  ipopt {q['status']} {fmt_num(q.get('objective'))}{'' if same else '  DIFFERS'}"
        print(line)
    summary = "{optimum} at the published optimum, {other} at another local optimum, {unknown} solved " \
              "(no published optimum), {failed} failed"
    print(f"optimizam: {summary.format(**counts)}, {counts['skipped']} skipped; {total:.1f}s; "
          f"{proven} proven (globally optimal or infeasible)")
    if a.ipopt:
        print(f"ipopt:     {summary.format(**ipopt_counts)}")


def fmt_num(v):
    return "" if v is None else f"{v:.10g}"


if __name__ == "__main__":
    main()
