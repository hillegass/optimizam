//! Convexity of the graph's nodes over a box, by the composition rules of
//! convex analysis, with the signs of each univariate op's first and second
//! derivatives taken from interval arithmetic over its argument's range:
//!
//! - variables and constants are affine; a sum `c + sum(a_i g_i)` is convex
//!   if every `a_i g_i` is convex or affine (concave likewise);
//! - `f(g)` is convex if f'' >= 0 and either g is affine, or g is convex and
//!   f' >= 0, or g is concave and f' <= 0 (concave with the signs reversed);
//!   this covers powers, `exp`, `ln`, `sqrt`, the trigonometric functions,
//!   squares `g * g`, reciprocals `c / g`, and exponentials `c^g` (c > 0);
//! - entropy terms `u * (c + ln u)` and `u * (c + ln(u / v))` with u, v
//!   affine, u >= 0 and v > 0 are convex (the second is the relative
//!   entropy, jointly convex: the perspective of -ln).
//!
//! Everything else is of unknown curvature. A convex node's tangent plane
//! at any point of the box (in the variables) underestimates it on the
//! whole box, which `relax` uses for outer-approximation cuts.

use super::graph::{Graph, Node, Op};
use super::interval::Interval;
use crate::expr::Func;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Curvature {
    Affine,
    Convex,
    Concave,
    Unknown,
}

use Curvature::*;

fn negate(c: Curvature) -> Curvature {
    match c {
        Convex => Concave,
        Concave => Convex,
        other => other,
    }
}

/// The curvature of `a * g` for a constant a.
fn scaled(a: f64, c: Curvature) -> Curvature {
    if a >= 0.0 { c } else { negate(c) }
}

/// The curvature of a sum of terms of these curvatures.
fn sum(terms: impl Iterator<Item = Curvature>) -> Curvature {
    let mut out = Affine;
    for t in terms {
        out = match (out, t) {
            (Unknown, _) | (_, Unknown) => return Unknown,
            (Affine, t) => t,
            (o, Affine) => o,
            (o, t) if o == t => o,
            _ => return Unknown,
        };
    }
    out
}

/// Whether an interval is (rounded) nonnegative, nonpositive.
fn nonneg(i: Interval) -> bool {
    !i.is_empty() && i.lo >= 0.0
}

fn nonpos(i: Interval) -> bool {
    !i.is_empty() && i.hi <= 0.0
}

/// f(g) for f with derivative ranges (d1, d2) over g's range.
fn compose(d1: Interval, d2: Interval, g: Curvature) -> Curvature {
    let convex = nonneg(d2) && (g == Affine || (g == Convex && nonneg(d1)) || (g == Concave && nonpos(d1)));
    let concave = nonpos(d2) && (g == Affine || (g == Concave && nonneg(d1)) || (g == Convex && nonpos(d1)));
    match (convex, concave) {
        (true, true) => Affine, // f is linear on the range (f'' = 0)
        (true, false) => Convex,
        (false, true) => Concave,
        _ => Unknown,
    }
}

/// The ranges of f' and f'' of a univariate op over x's range.
fn derivatives(op: &Op, x: Interval) -> Option<(Interval, Interval)> {
    let one = Interval::point(1.0);
    Some(match op {
        Op::Mul(..) => (x.scale(2.0), Interval::point(2.0)), // x * x
        Op::PowI(_, n) => {
            let n = *n;
            (
                x.powi(n - 1).scale(n as f64),
                if n == 1 { Interval::point(0.0) } else { x.powi(n - 2).scale((n * (n - 1)) as f64) },
            )
        }
        Op::PowC(_, c) => {
            let x = x.intersect(Interval::new(0.0, f64::INFINITY));
            (x.powc(c - 1.0).scale(*c), x.powc(c - 2.0).scale(c * (c - 1.0)))
        }
        Op::Func(f, _) => match f {
            Func::Exp => (x.func(Func::Exp), x.func(Func::Exp)),
            Func::Ln => (x.recip(), x.powi(2).recip().scale(-1.0)),
            Func::Sqrt => {
                let x = x.intersect(Interval::new(0.0, f64::INFINITY));
                (x.powc(-0.5).scale(0.5), x.powc(-1.5).scale(-0.25))
            }
            Func::Sin => (x.func(Func::Cos), x.func(Func::Sin).scale(-1.0)),
            Func::Cos => (x.func(Func::Sin).scale(-1.0), x.func(Func::Cos).scale(-1.0)),
            Func::Tan => {
                let t = x.func(Func::Tan);
                let s = one.add(t.powi(2));
                (s, t.mul(s).scale(2.0))
            }
            Func::Atan => {
                let s = one.add(x.powi(2)).recip();
                (s, x.mul(s.powi(2)).scale(-2.0))
            }
            Func::Tanh => {
                let t = x.func(Func::Tanh);
                let s = one.add(t.powi(2).scale(-1.0));
                (s, t.mul(s).scale(-2.0))
            }
        },
        _ => return None,
    })
}

/// Every node's curvature over the box with node ranges `iv`.
pub fn curvatures(graph: &Graph, iv: &[Interval]) -> Vec<Curvature> {
    let n = graph.len();
    let mut c = vec![Unknown; n];
    for k in 0..n {
        let op = graph.op(k as Node);
        c[k] = match op {
            Op::Const(_) | Op::Var(_) => Affine,
            Op::Sum(_, terms) => sum(terms.iter().map(|&(a, t)| scaled(a, c[t as usize]))),
            Op::Mul(a, b) if a == b => match derivatives(op, iv[*a as usize]) {
                Some((d1, d2)) => compose(d1, d2, c[*a as usize]),
                None => Unknown,
            },
            Op::Mul(a, b) => match entropy(graph, *a, *b, &c, iv) {
                Unknown => monomial(graph, k as Node, &c, iv),
                known => known,
            },
            Op::Div(a, b) => match graph.op(*a) {
                // c / g: f(g) = c / g.
                Op::Const(v) => {
                    let g = iv[*b as usize];
                    if g.lo > 0.0 || g.hi < 0.0 {
                        let d1 = g.powi(2).recip().scale(-v);
                        let d2 = g.powi(3).recip().scale(2.0 * v);
                        compose(d1, d2, c[*b as usize])
                    } else {
                        Unknown
                    }
                }
                _ => Unknown,
            },
            Op::PowI(a, _) | Op::PowC(a, _) | Op::Func(_, a) => match derivatives(op, iv[*a as usize]) {
                Some((d1, d2)) => match compose(d1, d2, c[*a as usize]) {
                    Unknown if !matches!(op, Op::Func(..)) => monomial(graph, k as Node, &c, iv),
                    known => known,
                },
                None => Unknown,
            },
            // c^g = exp(g ln c) for a constant c > 0.
            Op::Pow(a, b) => match graph.op(*a) {
                Op::Const(v) if *v > 0.0 => {
                    let l = v.ln();
                    let e = iv[k];
                    compose(e.scale(l), e.scale(l * l), c[*b as usize])
                }
                _ => Unknown,
            },
        };
    }
    c
}

/// A product of powers of affine bases, `prod b_i^a_i` (through nested
/// products and powers): concave if every b_i >= 0, a_i >= 0, and
/// sum a_i <= 1 (a weighted geometric mean, such as Cobb-Douglas); convex if
/// every b_i > 0 and a_i <= 0.
fn monomial(graph: &Graph, root: Node, c: &[Curvature], iv: &[Interval]) -> Curvature {
    let mut factors: Vec<(Node, f64)> = Vec::new();
    // The product is coefficient * prod(b_i^a_i).
    let mut coefficient = 1.0f64;
    let mut stack = vec![(root, 1.0)];
    while let Some((k, e)) = stack.pop() {
        match graph.op(k) {
            Op::Mul(a, b) => {
                stack.push((*a, e));
                stack.push((*b, e));
            }
            Op::PowC(a, p) => stack.push((*a, e * p)),
            Op::PowI(a, n) => stack.push((*a, e * *n as f64)),
            // A scaled factor a * g (to the power e).
            Op::Sum(k0, terms) if *k0 == 0.0 && terms.len() == 1 && c[k as usize] != Affine => {
                let (a, child) = terms[0];
                if a > 0.0 {
                    coefficient *= a.powf(e);
                } else if e == 1.0 {
                    coefficient *= a;
                } else {
                    return Unknown;
                }
                stack.push((child, e));
            }
            _ if c[k as usize] == Affine => factors.push((k, e)),
            _ => return Unknown,
        }
        if factors.len() + stack.len() > 1000 {
            return Unknown;
        }
    }
    // Merge repeated bases.
    factors.sort_by_key(|f| f.0);
    let mut merged: Vec<(Node, f64)> = Vec::new();
    for (k, e) in factors {
        match merged.last_mut() {
            Some(last) if last.0 == k => last.1 += e,
            _ => merged.push((k, e)),
        }
    }
    merged.retain(|f| f.1 != 0.0);
    let total: f64 = merged.iter().map(|f| f.1).sum();
    let base = if merged.iter().all(|&(k, e)| e >= 0.0 && nonneg(iv[k as usize])) && total <= 1.0 + 1e-12 {
        Concave
    } else if merged.iter().all(|&(k, e)| e <= 0.0 && iv[k as usize].lo > 0.0) {
        Convex
    } else {
        return Unknown;
    };
    scaled(coefficient, base)
}

/// `u * L` with L = c + ln u, or c + ln(u / v): convex if u and v are
/// affine, u >= 0, and v > 0.
fn entropy(graph: &Graph, a: Node, b: Node, c: &[Curvature], iv: &[Interval]) -> Curvature {
    for (u, l) in [(a, b), (b, a)] {
        if c[u as usize] != Affine || !nonneg(iv[u as usize]) {
            continue;
        }
        // L = Ln(arg), or Sum(k, [(1, Ln(arg))]).
        let ln_arg = match graph.op(l) {
            Op::Func(Func::Ln, arg) => Some(*arg),
            Op::Sum(_, terms) if terms.len() == 1 && terms[0].0 == 1.0 => match graph.op(terms[0].1) {
                Op::Func(Func::Ln, arg) => Some(*arg),
                _ => None,
            },
            _ => None,
        };
        let Some(arg) = ln_arg else { continue };
        if arg == u {
            return Convex;
        }
        if let Op::Div(num, den) = graph.op(arg)
            && *num == u
            && c[*den as usize] == Affine
            && iv[*den as usize].lo > 0.0
        {
            return Convex;
        }
    }
    Unknown
}

#[cfg(test)]
mod tests {
    use super::super::fbbt;
    use super::super::graph::testing::{Rng, random};
    use super::*;

    /// Wherever a node is reported convex (concave) on a box, it lies above
    /// (below) its chords between sampled points of the box.
    #[test]
    #[allow(clippy::needless_range_loop)] // nodes by number read more clearly
    fn reported_curvature_holds_on_chords() {
        let mut rng = Rng(0x7a5c3e1f9b2d4068);
        let mut convex = 0;
        for case in 0..400 {
            let mut g = Graph::new();
            let n = 1 + rng.below(3);
            let box_: Vec<Interval> = (0..n)
                .map(|_| {
                    let lo = rng.real(0.2, 1.2);
                    Interval::new(lo, lo + rng.real(0.1, 2.0))
                })
                .collect();
            let root = random(&mut g, &mut rng, n, 4);
            // Sometimes a product of powers (concave when the exponents add
            // up to at most 1, convex when they are negative).
            if rng.below(3) == 0 {
                let mut terms = Vec::new();
                for j in 0..n {
                    let v = g.var(j);
                    let e = g.constant(if rng.below(2) == 0 { rng.real(0.05, 0.4) } else { -rng.real(0.1, 1.0) });
                    terms.push(g.pow(v, e));
                }
                // A scaled first factor, as the model builds `-0.2 * x^a * y^b`.
                let mut p = g.scale(rng.real(-2.0, 2.0), terms[0]);
                for &t in &terms[1..] {
                    p = g.mul(p, t);
                }
            }
            // Sometimes an entropy term over the first variable.
            if rng.below(3) == 0 {
                let (x, y) = (g.var(0), g.var(n - 1));
                let s = g.sum(0.5, &[(1.0, x), (1.0, y)]);
                let d = g.div(x, s);
                let l = g.func(Func::Ln, d);
                let lc = g.sum(rng.real(-2.0, 2.0), &[(1.0, l)]);
                g.mul(x, lc);
            }
            let Some(iv) = fbbt::propagate(&g, &box_, &[]) else { continue };
            let curv = curvatures(&g, &iv);
            let _ = root;
            for k in 0..g.len() {
                if !matches!(curv[k], Convex | Concave) {
                    continue;
                }
                convex += 1;
                for _ in 0..30 {
                    let p: Vec<f64> = box_.iter().map(|b| rng.real(b.lo, b.hi)).collect();
                    let q: Vec<f64> = box_.iter().map(|b| rng.real(b.lo, b.hi)).collect();
                    let t = rng.real(0.0, 1.0);
                    let m: Vec<f64> = p.iter().zip(&q).map(|(a, b)| a + t * (b - a)).collect();
                    let (fp, fq, fm) = (g.value(k as Node, &p), g.value(k as Node, &q), g.value(k as Node, &m));
                    if !(fp.is_finite() && fq.is_finite() && fm.is_finite()) {
                        continue;
                    }
                    let chord = fp + t * (fq - fp);
                    let tol = 1e-9 * (1.0 + fp.abs() + fq.abs());
                    match curv[k] {
                        Convex => assert!(fm <= chord + tol, "case {case}: node {k} {:?} not convex", g.op(k as Node)),
                        _ => assert!(fm >= chord - tol, "case {case}: node {k} {:?} not concave", g.op(k as Node)),
                    }
                }
            }
        }
        assert!(convex > 500, "only {convex} convex or concave nodes");
    }
}
