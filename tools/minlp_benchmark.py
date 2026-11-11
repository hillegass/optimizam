#!/usr/bin/env python3
"""Benchmark optimizam's global search on MINLPLib.

MINLPLib (www.minlplib.org; Bussieck, Drud, Meeraus) collects nonlinear and
mixed-integer nonlinear problems with their best known (and, where proven,
optimal) objective values. This script downloads its instance data and
solution file, and the GAMS scalar-format model of each selected instance,
into target/minlp_benchmark/ on first use, translates each model into
optimizam's problem format (1.2) with a small reader for that format, and
compares optimizam's answers with the proven optima (`=opt=` in
minlplib.solu).

Selected instances: a proven optimum, at most --max-vars variables (default
12), and only operations optimizam has (no abs, min, max, mod, errorf,
gamma, signpower, entropy, semicontinuous variables, or SOS).

Usage (from the repository root, after `cargo build --release`):
    python3 tools/minlp_benchmark.py [--time SECONDS] [--max-vars N] [--scip]
        [--bin PATH] [--dir DIR] [names...]

Per instance it prints optimizam's time, status, and objective, the optimum,
and a verdict:
  proven      complete, at the optimum (within 1e-4 relative, 1e-5 absolute,
              or slightly better, within the feasibility tolerance)
  found       at the optimum, not proven in time
  local       another feasible point (worse than the optimum)
  failed      no feasible point (timeout), or an error
  WRONG       a claim that contradicts the proven optimum: complete at
              another value, infeasible, or a point better than the optimum
              by more than the tolerances
With --scip, SCIP (tools/scip.sh, the project's .venv with pyscipopt) runs
too. Exits with status 1 if any answer is WRONG. Uses only the standard
library.
"""

import argparse
import csv
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
BASE = "https://www.minlplib.org/"
UNSUPPORTED_OPS = ["opabs", "opcentropy", "operrorf", "opgamma", "opmin", "opmod", "opsignpower"]


def fetch(url, path):
    if not os.path.exists(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        req = urllib.request.Request(url, headers={"User-Agent": "optimizam-benchmark"})
        with urllib.request.urlopen(req, timeout=120) as r, open(path + ".part", "wb") as f:
            f.write(r.read())
        os.replace(path + ".part", path)
    with open(path) as f:
        return f.read()


def instances(directory, max_vars):
    """(name, number of variables, problem type) of the selected instances,
    and the proven optima."""
    data = fetch(BASE + "instancedata.csv", os.path.join(directory, "instancedata.csv"))
    solu = fetch(BASE + "minlplib.solu", os.path.join(directory, "minlplib.solu"))
    optima = {}
    for line in solu.splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[0] == "=opt=":
            optima[parts[1]] = float(parts[2])
    selected = []
    for r in csv.DictReader(data.splitlines(), delimiter=";"):
        if r["name"] not in optima or int(r["nvars"]) > max_vars:
            continue
        if any(r.get(op) == "True" for op in UNSUPPORTED_OPS):
            continue
        if any(int(r.get(k) or 0) for k in ("nsemi", "nsos1", "nsos2")):
            continue
        selected.append((r["name"], int(r["nvars"]), r["probtype"]))
    return selected, optima


# ------------------------------------------------- GAMS scalar format reader


class Unsupported(Exception):
    pass


TOKEN = re.compile(r"\s*(?:(?P<num>(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?)|(?P<id>[A-Za-z_][A-Za-z0-9_]*)"
                   r"|(?P<op>\*\*|=[EeLlGgNn]=|[-+*/(),]))")
# GAMS function -> (optimizam function or None for a power, arity).
FUNCTIONS = {"exp": "exp", "log": "ln", "sqrt": "sqrt", "sin": "sin", "cos": "cos", "tan": "tan",
             "tanh": "tanh", "arctan": "atan"}


def tokens(text):
    out, pos = [], 0
    text = text.strip()
    while pos < len(text):
        m = TOKEN.match(text, pos)
        if not m or m.end() == pos:
            raise Unsupported(f"cannot read {text[pos:pos + 20]!r}")
        pos = m.end()
        for kind in ("num", "id", "op"):
            if m.group(kind) is not None:
                out.append((kind, m.group(kind)))
                break
    out.append(("end", None))
    return out


class Expr:
    """GAMS expression -> an optimizam expression string (fully
    parenthesized)."""

    def __init__(self, toks, variables):
        self.t, self.i, self.vars = toks, 0, variables

    def peek(self):
        return self.t[self.i]

    def next(self):
        tok = self.t[self.i]
        self.i += 1
        return tok

    def expect(self, v):
        if self.next()[1] != v:
            raise Unsupported(f"expected {v}")

    def sum(self):
        out = self.term()
        while self.peek()[1] in ("+", "-") and self.peek()[0] == "op":
            op = self.next()[1]
            out = f"{out} {op} {self.term()}"
        return out

    def term(self):
        out = self.unary()
        while self.peek()[1] in ("*", "/") and self.peek()[0] == "op":
            op = self.next()[1]
            out = f"{out} {op} {self.unary()}"
        return out

    def unary(self):
        if self.peek() == ("op", "-"):
            self.next()
            return f"(-{self.unary()})"
        if self.peek() == ("op", "+"):
            self.next()
            return self.unary()
        return self.power()

    def power(self):
        base = self.atom()
        if self.peek() == ("op", "**"):
            self.next()
            return f"({base}^{self.unary()})"
        return base

    def atom(self):
        kind, v = self.next()
        if kind == "num":
            return f"({float(v)!r})" if float(v) < 0 else repr(float(v))
        if (kind, v) == ("op", "("):
            e = self.sum()
            self.expect(")")
            return f"({e})"
        if kind == "id":
            name = v.lower()
            if self.peek() == ("op", "("):
                self.next()
                args = [self.sum()]
                while self.peek() == ("op", ","):
                    self.next()
                    args.append(self.sum())
                self.expect(")")
                if name in FUNCTIONS and len(args) == 1:
                    return f"{FUNCTIONS[name]}({args[0]})"
                if name == "sqr" and len(args) == 1:
                    return f"(({args[0]})^2)"
                if name == "log10" and len(args) == 1:
                    return f"(ln({args[0]}) / {math.log(10)!r})"
                if name in ("power", "rpower", "vcpower", "cvpower") and len(args) == 2:
                    return f"(({args[0]})^({args[1]}))"
                raise Unsupported(f"function {name}")
            if name not in self.vars:
                raise Unsupported(f"unknown name {name}")
            return name
        raise Unsupported(f"unexpected {v!r}")


def translate(text):
    """A MINLPLib GAMS model -> (optimizam problem, objective sense)."""
    # Comments start with * in the first column (an indented * continues a
    # product from the line before).
    lines = [l for l in text.splitlines() if l.strip() and not l.startswith(("*", "$"))]
    statements = [s.strip() for s in " ".join(lines).split(";") if s.strip()]
    kinds = {}  # name -> continuous/positive/negative/binary/integer
    order = []
    equations = []
    bounds = {}  # name -> {"lo", "up", "l"}
    objective = None
    for s in statements:
        low = s.lower()
        m = re.match(r"(positive|negative|binary|integer|free)?\s*variables?\s+(.*)$", low, re.S)
        if m:
            kind = m.group(1) or "free"
            for name in re.split(r"[\s,]+", m.group(2).strip()):
                if not name:
                    continue
                if name not in kinds:
                    order.append(name)
                    kinds[name] = "free"
                if kind != "free":
                    kinds[name] = kind
            continue
        if re.match(r"equations?\s", low):
            continue
        m = re.match(r"(\w+)\s*\.\.\s*(.*)$", s, re.S)
        if m:
            equations.append((m.group(1).lower(), m.group(2)))
            continue
        m = re.match(r"(\w+)\.(lo|up|fx|l)\s*=\s*(.*)$", low, re.S)
        if m:
            name, attr, value = m.group(1), m.group(2), m.group(3).strip()
            if name in kinds:
                try:
                    v = float(value)
                except ValueError:
                    raise Unsupported(f"bound {s!r}")
                b = bounds.setdefault(name, {})
                if attr == "fx":
                    b["lo"] = b["up"] = b["l"] = v
                else:
                    b[attr] = v
            continue
        m = re.match(r"solve\s+\w+\s+using\s+\S+\s+(minimizing|maximizing)\s+(\w+)", low)
        if m:
            objective = ("minimize" if m.group(1) == "minimizing" else "maximize", m.group(2))
            continue
        if re.match(r"(model|option|\w+\.\w+\s*=)", low):
            continue
        raise Unsupported(f"statement {s[:40]!r}")
    if objective is None:
        raise Unsupported("no solve statement")
    variables = []
    for name in order:
        kind = kinds[name]
        b = bounds.get(name, {})
        decl = {"name": name, "type": {"binary": "binary", "integer": "integer"}.get(kind, "continuous")}
        if kind == "binary":
            if b.get("lo", 0) == b.get("up", 1) and "lo" in b:
                decl = {"name": name, "type": "integer", "lower": b["lo"], "upper": b["lo"]}
        else:
            lo = b.get("lo", 0.0 if kind in ("positive", "integer") else -math.inf)
            up = b.get("up", 0.0 if kind == "negative" else (100.0 if kind == "integer" else math.inf))
            if math.isfinite(lo) and abs(lo) < 1e19:
                decl["lower"] = lo
            if math.isfinite(up) and abs(up) < 1e19:
                decl["upper"] = up
        if "l" in b and kind != "binary":
            decl["start"] = b["l"]
        variables.append(decl)
    names = set(order)
    constraints = []
    for name, body in equations:
        m = re.search(r"=([EeLlGgNn])=", body)
        if not m:
            raise Unsupported(f"equation {name}")
        rel = {"e": "==", "l": "<=", "g": ">=", "n": None}[m.group(1).lower()]
        if rel is None:
            continue
        lhs, rhs = body[: m.start()], body[m.end():]
        lt, rt = Expr(tokens(lhs), names), Expr(tokens(rhs), names)
        left, right = lt.sum(), rt.sum()
        if lt.peek()[0] != "end" or rt.peek()[0] != "end":
            raise Unsupported(f"equation {name}")
        constraints.append({"name": name, "expr": f"{left} {rel} {right}"})
    problem = {"version": "1.3", "variables": variables, "constraints": constraints,
               "solve": {"method": objective[0], "objective": objective[1]}}
    return problem


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


def verdict(res, optimum, sense):
    tol = max(1e-4 * abs(optimum), 1e-5)
    f = res.get("objective")
    if res["status"] == "infeasible":
        return "WRONG"
    if f is None:
        return "failed"
    better = (f < optimum - tol) if sense == "minimize" else (f > optimum + tol)
    if better and abs(f - optimum) > 1e-3 * max(1.0, abs(optimum)):
        return "WRONG"  # better than the proven optimum: infeasible beyond tolerances
    # Slightly better than the optimum: rows met within the feasibility
    # tolerance (1e-6 relative) can allow that.
    at = abs(f - optimum) <= tol or better
    if res["status"] == "complete":
        return "proven" if at else "WRONG"
    return "found" if at else "local"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--time", type=float, default=10.0)
    ap.add_argument("--max-vars", type=int, default=12)
    ap.add_argument("--scip", action="store_true")
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"))
    ap.add_argument("--dir", default=os.path.join(ROOT, "target", "minlp_benchmark"))
    ap.add_argument("names", nargs="*")
    a = ap.parse_args()
    selected, optima = instances(a.dir, max(a.max_vars, 1000 if a.names else a.max_vars))
    if a.names:
        selected = [s for s in selected if any(s[0].startswith(p) for p in a.names)]
    os.makedirs(os.path.join(a.dir, "instances"), exist_ok=True)
    counts = {k: 0 for k in ("proven", "found", "local", "failed", "WRONG", "skipped")}
    scip_counts = {k: 0 for k in ("proven", "found", "local", "failed", "WRONG")}
    total = scip_total = 0.0
    print(f"{'instance':24} {'vars':>4} {'type':7} {'seconds':>8} {'status':19} {'objective':>15} {'optimum':>15}  verdict"
          + ("  | scip" if a.scip else ""))
    for name, nvars, ptype in selected:
        try:
            text = fetch(BASE + "gms/" + name + ".gms", os.path.join(a.dir, "gms", name + ".gms"))
            problem = translate(text)
        except (Unsupported, ValueError, urllib.error.URLError) as e:
            counts["skipped"] += 1
            print(f"{name:24} {nvars:4} {ptype:7} skipped: {e}")
            continue
        path = os.path.join(a.dir, "instances", name + ".json")
        with open(path, "w") as f:
            json.dump(problem, f, indent=1)
        sense = problem["solve"]["method"]
        r = run(a.bin, path, a.time)
        total += r["seconds"]
        v = verdict(r, optima[name], sense)
        counts[v] += 1
        line = (f"{name:24} {nvars:4} {ptype:7} {r['seconds']:8.2f} {r['status']:19} {fmt(r.get('objective')):>15} "
                f"{fmt(optima[name]):>15}  {v}")
        if r["status"] == "error":
            line += f" ({r['message'][:60]})"
        if a.scip:
            q = run(os.path.join(ROOT, "tools", "scip.sh"), path, a.time)
            scip_total += q["seconds"]
            w = verdict(q, optima[name], sense)
            scip_counts[w] += 1
            line += f"  | {q['status']} {fmt(q.get('objective'))} {w} {q['seconds']:.1f}s"
        print(line, flush=True)
    summary = "{proven} proven, {found} found (not proven), {local} local, {failed} failed, {WRONG} WRONG"
    print(f"optimizam: {summary.format(**counts)}, {counts['skipped']} skipped; {total:.1f}s")
    if a.scip:
        print(f"scip:      {summary.format(**scip_counts)}; {scip_total:.1f}s")
    sys.exit(1 if counts["WRONG"] else 0)


def fmt(v):
    return "" if v is None else f"{v:.8g}"


if __name__ == "__main__":
    main()
