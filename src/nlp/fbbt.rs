//! Bound propagation over the expression graph (feasibility-based bound
//! tightening, FBBT): node ranges from the variables' bounds (forward), and
//! tighter bounds on the children from each constraint's bounds (backward,
//! through the inverse of each op), repeated to a fixed point.
//!
//! The constraints are roots of the graph with intervals they must lie in,
//! such as a row `x^2 + y^2 <= 1` (whose root includes the linear part).
//! Functions are restricted to their domains, so `ln(x)` anywhere also
//! implies `x >= 0`. Every step keeps every point that satisfies the
//! constraints (in exact arithmetic, and the intervals are rounded outward),
//! so an empty range proves that no such point exists in the box.

use super::graph::{Graph, Node, Op};
use super::interval::Interval;
use crate::expr::Func;
use std::f64::consts::FRAC_PI_2;

const INF: f64 = f64::INFINITY;
/// Forward and backward sweeps at most.
const MAX_ROUNDS: usize = 30;
/// A round must shrink some variable's range by this fraction of its
/// width (or make an infinite bound finite) for another round.
const PROGRESS: f64 = 1e-3;

#[allow(clippy::needless_range_loop)] // nodes are indexed by number throughout
/// Ranges of every node, or None if the constraints can't all hold in the
/// box. `bounds` are the variables' bounds; `targets` the constraints.
pub fn propagate(graph: &Graph, bounds: &[Interval], targets: &[(Node, Interval)]) -> Option<Vec<Interval>> {
    let n = graph.len();
    let mut iv = vec![Interval::ENTIRE; n];
    for k in 0..n {
        if let Op::Var(j) = graph.op(k as Node) {
            iv[k] = bounds[*j];
        }
    }
    for round in 0..MAX_ROUNDS {
        let before: Vec<Interval> = iv.clone();
        // Forward: each node's range from its children's.
        for k in 0..n {
            let f = forward(graph.op(k as Node), &iv);
            iv[k] = iv[k].intersect(f);
            if iv[k].is_empty() {
                return None;
            }
        }
        for &(root, t) in targets {
            let r = root as usize;
            iv[r] = iv[r].intersect(t);
            if iv[r].is_empty() {
                return None;
            }
        }
        // Backward: parents before children (reverse topological order).
        for k in (0..n).rev() {
            backward(graph.op(k as Node), iv[k], &mut iv);
        }
        if iv.iter().any(|i| i.is_empty()) {
            return None;
        }
        let progress = (0..n).any(|k| matches!(graph.op(k as Node), Op::Var(_)) && shrunk(before[k], iv[k]));
        if !progress && round > 0 {
            break;
        }
    }
    Some(iv)
}

fn shrunk(old: Interval, new: Interval) -> bool {
    if old.lo.is_infinite() != new.lo.is_infinite() || old.hi.is_infinite() != new.hi.is_infinite() {
        return true;
    }
    let w = old.width();
    w.is_finite() && w > 0.0 && (new.width() < (1.0 - PROGRESS) * w)
}

/// The range of an op from its children's ranges.
pub fn forward(op: &Op, iv: &[Interval]) -> Interval {
    let at = |c: Node| iv[c as usize];
    match op {
        Op::Const(c) => Interval::point(*c),
        Op::Var(_) => Interval::ENTIRE, // the variable's own range is kept
        Op::Sum(c, terms) => terms.iter().fold(Interval::point(*c), |s, &(a, n)| s.add(at(n).scale(a))),
        Op::Mul(a, b) if a == b => at(*a).powi(2),
        Op::Mul(a, b) => at(*a).mul(at(*b)),
        Op::Div(a, b) => at(*a).div(at(*b)),
        Op::PowI(a, n) => at(*a).powi(*n),
        Op::PowC(a, c) => at(*a).powc(*c),
        Op::Pow(a, b) => at(*a).pow(at(*b)),
        Op::Func(f, a) => at(*a).func(*f),
    }
}

fn tighten(iv: &mut [Interval], c: Node, by: Interval) {
    let k = c as usize;
    iv[k] = iv[k].intersect(by);
}

/// The n-th root of a non-negative number, rounded down (`up` false) or up.
fn root(v: f64, n: i32, up: bool) -> f64 {
    if v.is_infinite() {
        return v;
    }
    let r = v.powf(1.0 / n as f64);
    let e = 8.0 * f64::EPSILON * r.abs();
    if up { r + e } else { (r - e).max(0.0) }
}

/// Tightens the children of an op whose range is t.
fn backward(op: &Op, t: Interval, iv: &mut [Interval]) {
    if t.is_empty() {
        return;
    }
    match op {
        Op::Const(_) | Op::Var(_) => {}
        Op::Sum(c, terms) => {
            // child_j = (t - c - sum of the others) / a_j, with prefix and
            // suffix sums of the others.
            let k = terms.len();
            let ranges: Vec<Interval> = terms.iter().map(|&(a, n)| iv[n as usize].scale(a)).collect();
            let mut prefix = vec![Interval::point(*c); k + 1];
            for i in 0..k {
                prefix[i + 1] = prefix[i].add(ranges[i]);
            }
            let mut suffix = vec![Interval::point(0.0); k + 1];
            for i in (0..k).rev() {
                suffix[i] = suffix[i + 1].add(ranges[i]);
            }
            for (i, &(a, n)) in terms.iter().enumerate() {
                let others = prefix[i].add(suffix[i + 1]);
                if others.lo.is_infinite() && others.hi.is_infinite() {
                    continue;
                }
                let child = t.add(others.scale(-1.0)).scale(1.0 / a);
                tighten(iv, n, child);
            }
        }
        Op::Mul(a, b) if a == b => square_inverse(*a, t, iv),
        Op::Mul(a, b) => {
            // a = t / b where b excludes 0 (else a is unconstrained).
            let (ia, ib) = (iv[*a as usize], iv[*b as usize]);
            if ib.lo > 0.0 || ib.hi < 0.0 {
                tighten(iv, *a, t.div(ib));
            }
            if ia.lo > 0.0 || ia.hi < 0.0 {
                tighten(iv, *b, t.div(ia));
            }
        }
        Op::Div(a, b) => {
            // a = t * b; b = a / t where t excludes 0.
            let (ia, ib) = (iv[*a as usize], iv[*b as usize]);
            tighten(iv, *a, t.mul(ib));
            if t.lo > 0.0 || t.hi < 0.0 {
                tighten(iv, *b, ia.div(t));
            }
        }
        Op::PowI(a, n) => {
            let n = *n;
            if n == 2 || (n > 0 && n % 2 == 0) {
                even_root_inverse(*a, t, n, iv);
            } else if n > 0 {
                // Odd: monotone.
                let signed = |v: f64, up: bool| {
                    if v >= 0.0 { root(v, n, up) } else { -root(-v, n, !up) }
                };
                tighten(iv, *a, Interval::new(signed(t.lo, false), signed(t.hi, true)));
            }
        }
        Op::PowC(a, c) => {
            // x >= 0, and x = w^(1/c) (monotone).
            tighten(iv, *a, Interval::new(0.0, INF));
            let w = t.intersect(Interval::new(0.0, INF));
            tighten(iv, *a, w.powc(1.0 / *c));
        }
        Op::Pow(a, _) => tighten(iv, *a, Interval::new(0.0, INF)),
        Op::Func(f, a) => {
            let inverse = match f {
                Func::Exp => t.func(Func::Ln),
                Func::Ln => t.func(Func::Exp),
                Func::Sqrt => {
                    let w = t.intersect(Interval::new(0.0, INF));
                    if w.is_empty() { Interval::EMPTY } else { w.powi(2).intersect(Interval::new(0.0, INF)) }
                }
                Func::Atan => {
                    let w = t.intersect(Interval::new(-FRAC_PI_2, FRAC_PI_2));
                    if w.is_empty() {
                        Interval::EMPTY
                    } else if w.lo <= -FRAC_PI_2 + 1e-12 || w.hi >= FRAC_PI_2 - 1e-12 {
                        // Too close to the asymptotes for tan to be safe.
                        Interval::new(
                            if w.lo <= -FRAC_PI_2 + 1e-12 { -INF } else { down(w.lo.tan()) },
                            if w.hi >= FRAC_PI_2 - 1e-12 { INF } else { up(w.hi.tan()) },
                        )
                    } else {
                        Interval::new(down(w.lo.tan()), up(w.hi.tan()))
                    }
                }
                Func::Tanh => {
                    let w = t.intersect(Interval::new(-1.0, 1.0));
                    if w.is_empty() {
                        Interval::EMPTY
                    } else {
                        let lo = if w.lo <= -1.0 + 1e-15 { -INF } else { down(w.lo.atanh()) };
                        let hi = if w.hi >= 1.0 - 1e-15 { INF } else { up(w.hi.atanh()) };
                        Interval::new(lo, hi)
                    }
                }
                Func::Sin | Func::Cos => {
                    if t.lo > 1.0 || t.hi < -1.0 {
                        Interval::EMPTY
                    } else {
                        Interval::ENTIRE
                    }
                }
                Func::Tan => Interval::ENTIRE,
            };
            tighten(iv, *a, inverse);
        }
    }
}

fn down(v: f64) -> f64 {
    v - 8.0 * f64::EPSILON * v.abs()
}

fn up(v: f64) -> f64 {
    v + 8.0 * f64::EPSILON * v.abs()
}

fn square_inverse(a: Node, t: Interval, iv: &mut [Interval]) {
    even_root_inverse(a, t, 2, iv);
}

/// x^n in t for even n: |x| <= t.hi^(1/n), and |x| >= t.lo^(1/n) if that is
/// positive (two pieces, joined unless x's sign is known).
fn even_root_inverse(a: Node, t: Interval, n: i32, iv: &mut [Interval]) {
    if t.hi < 0.0 {
        iv[a as usize] = Interval::EMPTY;
        return;
    }
    let r = root(t.hi, n, true);
    let s = if t.lo > 0.0 { root(t.lo, n, false) } else { 0.0 };
    let x = iv[a as usize];
    let pos = x.intersect(Interval::new(s, r));
    let neg = x.intersect(Interval::new(-r, -s));
    iv[a as usize] = pos.hull(neg);
    if pos.is_empty() && neg.is_empty() {
        iv[a as usize] = Interval::EMPTY;
    }
}

#[cfg(test)]
mod tests {
    use super::super::graph::testing::{Rng, random};
    use super::*;

    fn bounds_of(iv: &[Interval], g: &Graph, num_vars: usize) -> Vec<Interval> {
        let mut out = vec![Interval::ENTIRE; num_vars];
        for (k, i) in iv.iter().enumerate() {
            if let Op::Var(j) = g.op(k as Node) {
                out[*j] = *i;
            }
        }
        out
    }

    /// Random constraints that hold at a reference point: propagation keeps
    /// the reference point and every sampled feasible point, and its node
    /// ranges contain the nodes' values there.
    #[test]
    fn never_cuts_feasible_points() {
        let mut rng = Rng(0xd1b54a32d192ed03);
        let mut tightened = 0;
        for case in 0..300 {
            let mut g = Graph::new();
            let n = 1 + rng.below(3);
            let box_: Vec<Interval> = (0..n)
                .map(|_| {
                    let lo = rng.real(-3.0, 1.0);
                    Interval::new(lo, lo + rng.real(0.5, 4.0))
                })
                .collect();
            let roots: Vec<Node> = (0..1 + rng.below(3)).map(|_| random(&mut g, &mut rng, n, 3)).collect();
            // Targets around the values at a reference point.
            let xr: Vec<f64> = box_.iter().map(|b| rng.real(b.lo, b.hi)).collect();
            let mut vals = Vec::new();
            g.eval(&xr, &mut vals);
            if roots.iter().any(|&r| !vals[r as usize].is_finite()) {
                continue;
            }
            let targets: Vec<(Node, Interval)> = roots
                .iter()
                .map(|&r| {
                    let v = vals[r as usize];
                    let (below, above) = (rng.real(0.0, 0.5), rng.real(0.0, 0.5));
                    let t = match rng.below(3) {
                        0 => Interval::new(-INF, v + above),
                        1 => Interval::new(v - below, INF),
                        _ => Interval::new(v - below, v + above),
                    };
                    (r, t)
                })
                .collect();
            let iv = propagate(&g, &box_, &targets).unwrap_or_else(|| panic!("case {case}: reported infeasible"));
            let b = bounds_of(&iv, &g, n);
            for _ in 0..200 {
                let x: Vec<f64> = box_.iter().map(|b| rng.real(b.lo, b.hi)).collect();
                let mut vals = Vec::new();
                g.eval(&x, &mut vals);
                let feasible = targets.iter().all(|&(r, t)| t.contains(vals[r as usize]));
                if !feasible {
                    continue;
                }
                for j in 0..n {
                    if b[j].width() < box_[j].width() * 0.999 {
                        tightened += 1;
                    }
                    assert!(b[j].contains(x[j]), "case {case}: x{j} = {} cut by {:?}", x[j], b[j]);
                }
                for k in 0..g.len() {
                    if vals[k].is_finite() {
                        assert!(iv[k].contains(vals[k]), "case {case}: node {k} = {} outside {:?}", vals[k], iv[k]);
                    }
                }
            }
        }
        assert!(tightened > 0, "propagation never tightened anything");
    }

    #[test]
    fn tightens_and_proves_infeasibility() {
        let mut g = Graph::new();
        let (x, y) = (g.var(0), g.var(1));
        let (x2, y2) = (g.powi(x, 2), g.powi(y, 2));
        let disk = g.sum(0.0, &[(1.0, x2), (1.0, y2)]);
        let free = vec![Interval::ENTIRE; 2];
        // x^2 + y^2 <= 1: both in [-1, 1].
        let iv = propagate(&g, &free, &[(disk, Interval::new(-INF, 1.0))]).unwrap();
        for v in [x, y] {
            let i = iv[v as usize];
            assert!(i.lo >= -1.0 - 1e-9 && i.hi <= 1.0 + 1e-9, "{i:?}");
        }
        // ... and x + y >= 3: infeasible.
        let line = g.sum(0.0, &[(1.0, x), (1.0, y)]);
        assert!(propagate(&g, &free, &[(disk, Interval::new(-INF, 1.0)), (line, Interval::new(3.0, INF))]).is_none());
        // exp(x) <= 1: x <= 0. ln(y) anywhere: y >= 0.
        let e = g.func(Func::Exp, x);
        let l = g.func(Func::Ln, y);
        let iv = propagate(&g, &free, &[(e, Interval::new(-INF, 1.0)), (l, Interval::ENTIRE)]).unwrap();
        assert!(iv[x as usize].hi <= 1e-9 && iv[y as usize].lo >= 0.0);
        // sqrt(x) >= 3 with x <= 5: infeasible.
        let s = g.func(Func::Sqrt, x);
        let b = vec![Interval::new(-INF, 5.0), Interval::ENTIRE];
        assert!(propagate(&g, &b, &[(s, Interval::new(3.0, INF))]).is_none());
    }
}
