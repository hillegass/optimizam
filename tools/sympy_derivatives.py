#!/usr/bin/env python3
"""Generate the derivative fixture for optimizam's expression graph.

Builds random expressions in the problem format's grammar (format 1.2: every
function, integer and real powers, products, quotients) over the variables
x1, x2, x3, and uses SymPy to compute each one's value, gradient, and Hessian
at random points. The Rust test `nlp::tests::derivatives_match_sympy` parses
the same strings through the model builder and compares.

Arguments of ln and sqrt, divisors, and power bases are kept positive for
points in [0.5, 1.5]^3, as in the Rust random tests.

Usage (needs the project's .venv with sympy):
    .venv/bin/python tools/sympy_derivatives.py tests/data/sympy_derivatives.json
"""

import json
import random
import sys

import sympy as sp

X = sp.symbols("x1 x2 x3")
NAMES = ["x1", "x2", "x3"]


def num(rng, lo, hi):
    # Short decimals, exact in both the string and SymPy (as rationals).
    v = round(rng.uniform(lo, hi), 2)
    return str(v), sp.Rational(str(v))


def positive(rng, e):
    """Something positive built from e: exp(0.3 * tanh(e)) or 1 + e^2."""
    text, expr = e
    if rng.random() < 0.5:
        return f"exp(0.3 * tanh({text}))", sp.exp(sp.Rational(3, 10) * sp.tanh(expr))
    return f"(1 + ({text})^2)", 1 + expr**2


def random_expr(rng, depth):
    if depth == 0 or rng.random() < 0.25:
        if rng.random() < 0.25:
            return num(rng, 0.5, 2.0)
        k = rng.randrange(3)
        return NAMES[k], X[k]
    a = random_expr(rng, depth - 1)
    choice = rng.randrange(14)
    if choice == 0:
        b = random_expr(rng, depth - 1)
        c1, c2 = num(rng, -2, 2), num(rng, -2, 2)
        return f"({c1[0]} * ({a[0]}) - ({c2[0]}) * ({b[0]}))", c1[1] * a[1] - c2[1] * b[1]
    if choice in (1, 2):
        b = random_expr(rng, depth - 1)
        return f"({a[0]}) * ({b[0]})", a[1] * b[1]
    if choice == 3:
        p = positive(rng, random_expr(rng, depth - 1))
        return f"({a[0]}) / {p[0]}", a[1] / p[1]
    if choice == 4:
        n = rng.choice([2, 3])
        return f"({a[0]})^{n}", a[1] ** n
    if choice == 5:
        p = positive(rng, a)
        n = rng.choice([-1, -2])
        return f"{p[0]}^{n}", p[1] ** n
    if choice == 6:
        p = positive(rng, a)
        c = num(rng, -1.5, 2.5)
        return f"{p[0]}^({c[0]})", p[1] ** c[1]
    if choice == 7:
        p = positive(rng, a)
        b = random_expr(rng, depth - 1)
        return f"{p[0]}^tanh({b[0]})", p[1] ** sp.tanh(b[1])
    if choice == 8:
        p = positive(rng, a)
        f = rng.choice(["ln", "sqrt"])
        return f"{f}({p[0]})", (sp.log if f == "ln" else sp.sqrt)(p[1])
    if choice == 9:
        return f"tan(atan({a[0]}))", sp.tan(sp.atan(a[1]))
    if choice == 10:
        c = num(rng, 0.5, 3.0)
        return f"{c[0]}^({a[0]})", c[1] ** a[1]
    f = rng.choice(["sin", "cos", "atan", "tanh", "exp"])
    inner = (f"tanh({a[0]})", sp.tanh(a[1])) if f == "exp" else a
    return f"{f}({inner[0]})", getattr(sp, f)(inner[1])


def main():
    out_path = sys.argv[1]
    rng = random.Random(20261104)
    cases = []
    while len(cases) < 80:
        text, expr = random_expr(rng, 4)
        if not expr.free_symbols:
            continue
        point = [round(rng.uniform(0.5, 1.5), 3) for _ in X]
        subs = dict(zip(X, [sp.Rational(str(v)) for v in point]))
        value = sp.N(expr.subs(subs), 30)
        grad = [sp.N(sp.diff(expr, v).subs(subs), 30) for v in X]
        hess = [[sp.N(sp.diff(expr, u, v).subs(subs), 30) for v in X] for u in X]
        numbers = [value, *grad, *(h for row in hess for h in row)]
        if not all(n.is_real and n.is_finite for n in numbers):
            continue
        cases.append({
            "expr": text,
            "point": point,
            "value": float(value),
            "gradient": [float(g) for g in grad],
            "hessian": [[float(h) for h in row] for row in hess],
        })
    with open(out_path, "w") as f:
        json.dump({"generator": "tools/sympy_derivatives.py", "cases": cases}, f, indent=1)
        f.write("\n")
    print(f"wrote {len(cases)} cases to {out_path}")


if __name__ == "__main__":
    main()
