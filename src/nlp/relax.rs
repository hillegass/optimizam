//! Linear relaxations of nonlinear problems over a box: a lower bound on
//! the global optimum, or a proof that no feasible point exists.
//!
//! Every node of the graph that the roots reach gets a column (a variable's
//! node is that variable's column), bounded by its range from bound
//! propagation, with linear rows that every point of the box satisfies with
//! the nodes' true values:
//!
//! - a sum: `w = c + sum(a_i * child_i)`;
//! - a product `w = x * y`: McCormick's envelope (the four inequalities
//!   from `(x - xl)(y - yl) >= 0` and so on, for the finite bounds); a
//!   quotient `w = x / y` as the product `x = w * y`;
//! - a function of one child (powers, `exp`, `ln`, `sqrt`, and the
//!   trigonometric functions on intervals where their curvature has one
//!   sign): tangents on the convex side and the secant on the concave side;
//! - anything else (a general power `a^b`, mixed curvature): only the
//!   column's bounds.
//!
//! Each row is loosened by a tiny relative tolerance, so that rounding in
//! its coefficients can't make it cut off a true point.

use super::graph::{Graph, Node, Op};
use super::interval::Interval;
use crate::expr::Func;
use crate::lp::{Lp, SparseRow};
use crate::simplex;
use std::f64::consts::{FRAC_PI_2, PI};
use std::time::Instant;

const INF: f64 = f64::INFINITY;
/// Rows are loosened by this, relative to `1 + |rhs|`.
const ROW_TOL: f64 = 1e-9;
/// Tangent points per convex or concave function, at most.
const TANGENTS: usize = 5;

/// The relaxation: columns (the variables first), rows, and each node's
/// column.
pub struct Relaxation {
    pub col_lower: Vec<f64>,
    pub col_upper: Vec<f64>,
    pub rows: Vec<SparseRow>,
    pub node_col: Vec<Option<usize>>,
}

pub enum Bound {
    /// No point of the box satisfies the relaxation, so none satisfies the
    /// constraints.
    Infeasible,
    /// The relaxation's minimum of the objective: a lower bound.
    Lower(f64),
    /// No bound (the relaxation is unbounded, or the LP solve failed).
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Curvature {
    Convex,
    Concave,
    Unknown,
}

/// Builds the relaxation of the targets (roots with intervals) and the
/// objective root, over node ranges `iv` (from `fbbt::propagate`).
pub fn build(graph: &Graph, iv: &[Interval], num_vars: usize, var_bounds: &[Interval], roots: &[Node]) -> Relaxation {
    let n = graph.len();
    let mut reach = vec![false; n];
    let mut stack: Vec<Node> = roots.to_vec();
    while let Some(k) = stack.pop() {
        if std::mem::replace(&mut reach[k as usize], true) {
            continue;
        }
        children(graph.op(k), |c| stack.push(c));
    }
    let mut col_lower: Vec<f64> = var_bounds.iter().map(|b| b.lo).collect();
    let mut col_upper: Vec<f64> = var_bounds.iter().map(|b| b.hi).collect();
    let mut node_col = vec![None; n];
    for k in 0..n {
        if !reach[k] {
            continue;
        }
        match graph.op(k as Node) {
            Op::Var(j) => {
                node_col[k] = Some(*j);
                col_lower[*j] = col_lower[*j].max(iv[k].lo);
                col_upper[*j] = col_upper[*j].min(iv[k].hi);
            }
            _ => {
                node_col[k] = Some(col_lower.len());
                col_lower.push(iv[k].lo);
                col_upper.push(iv[k].hi);
            }
        }
    }
    debug_assert!(col_lower.len() >= num_vars);
    let mut rows: Vec<SparseRow> = Vec::new();
    let col = |c: Node| node_col[c as usize].unwrap();
    for k in 0..n {
        if !reach[k] {
            continue;
        }
        let w = node_col[k].unwrap();
        let op = graph.op(k as Node);
        match op {
            Op::Var(_) => {}
            Op::Const(_) => {} // its column is fixed by its range
            Op::Sum(c, terms) => {
                let mut coefs = vec![(w, 1.0)];
                coefs.extend(terms.iter().map(|&(a, ch)| (col(ch), -a)));
                rows.push(merge(coefs, *c, *c));
            }
            Op::Mul(a, b) if a == b => univariate(&mut rows, w, col(*a), iv[*a as usize], Uni::PowI(2)),
            Op::Mul(a, b) => mccormick(&mut rows, w, col(*a), iv[*a as usize], col(*b), iv[*b as usize]),
            // x / y = w  <=>  x = w * y (y != 0).
            Op::Div(a, b) => mccormick(&mut rows, col(*a), w, iv[k], col(*b), iv[*b as usize]),
            Op::PowI(a, p) => univariate(&mut rows, w, col(*a), iv[*a as usize], Uni::PowI(*p)),
            Op::PowC(a, c) => univariate(&mut rows, w, col(*a), iv[*a as usize], Uni::PowC(*c)),
            Op::Func(f, a) => univariate(&mut rows, w, col(*a), iv[*a as usize], Uni::Func(*f)),
            Op::Pow(..) => {}
        }
    }
    Relaxation { col_lower, col_upper, rows, node_col }
}

fn children(op: &Op, mut f: impl FnMut(Node)) {
    match op {
        Op::Const(_) | Op::Var(_) => {}
        Op::Sum(_, terms) => terms.iter().for_each(|&(_, c)| f(c)),
        Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => {
            f(*a);
            f(*b);
        }
        Op::PowI(a, _) | Op::PowC(a, _) | Op::Func(_, a) => f(*a),
    }
}

/// A row `lo <= sum(coefs) <= hi` with repeated columns merged.
fn merge(coefs: Vec<(usize, f64)>, lo: f64, hi: f64) -> SparseRow {
    let mut out: Vec<(usize, f64)> = Vec::new();
    for (j, a) in coefs {
        match out.iter_mut().find(|c| c.0 == j) {
            Some(c) => c.1 += a,
            None => out.push((j, a)),
        }
    }
    out.retain(|c| c.1 != 0.0);
    (out, lo, hi)
}

/// Whether an estimator's coefficients are usable: none huge, and not
/// spread over more than 1e9 (from bounds like [1e-5, 1e10], they would make
/// the LP numerically meaningless; dropping a row only weakens it).
fn usable(coefs: &[(usize, f64)], rhs: f64) -> bool {
    let mags: Vec<f64> = coefs.iter().map(|c| c.1.abs()).filter(|&a| a > 0.0).collect();
    let (lo, hi) = (mags.iter().copied().fold(INF, f64::min), mags.iter().copied().fold(0.0, f64::max));
    rhs.is_finite() && rhs.abs() <= MAX_COEF && hi <= MAX_COEF && hi <= 1e9 * lo
}

/// Estimators with coefficients or right-hand sides above this are dropped.
const MAX_COEF: f64 = 1e8;

/// `sum(coefs) >= rhs`, loosened (None if unusable).
fn at_least(coefs: Vec<(usize, f64)>, rhs: f64) -> Option<SparseRow> {
    usable(&coefs, rhs).then(|| merge(coefs, rhs - ROW_TOL * (1.0 + rhs.abs()), INF))
}

fn at_most(coefs: Vec<(usize, f64)>, rhs: f64) -> Option<SparseRow> {
    usable(&coefs, rhs).then(|| merge(coefs, -INF, rhs + ROW_TOL * (1.0 + rhs.abs())))
}

/// McCormick's envelope of `p = x * y` (columns p, x, y; ranges of x, y).
fn mccormick(rows: &mut Vec<SparseRow>, p: usize, x: usize, ix: Interval, y: usize, iy: Interval) {
    let (xl, xu, yl, yu) = (ix.lo, ix.hi, iy.lo, iy.hi);
    // p >= xl y + yl x - xl yl, from (x - xl)(y - yl) >= 0; and so on.
    if xl.is_finite() && yl.is_finite() {
        rows.extend(at_least(vec![(p, 1.0), (y, -xl), (x, -yl)], -xl * yl));
    }
    if xu.is_finite() && yu.is_finite() {
        rows.extend(at_least(vec![(p, 1.0), (y, -xu), (x, -yu)], -xu * yu));
    }
    if xu.is_finite() && yl.is_finite() {
        rows.extend(at_most(vec![(p, 1.0), (y, -xu), (x, -yl)], -xu * yl));
    }
    if xl.is_finite() && yu.is_finite() {
        rows.extend(at_most(vec![(p, 1.0), (y, -xl), (x, -yu)], -xl * yu));
    }
}

#[derive(Clone, Copy)]
enum Uni {
    PowI(i32),
    PowC(f64),
    Func(Func),
}

impl Uni {
    /// f(x) and f'(x).
    fn eval(self, x: f64) -> (f64, f64) {
        match self {
            Uni::PowI(n) => (x.powi(n), n as f64 * x.powi(n - 1)),
            Uni::PowC(c) => (x.powf(c), c * x.powf(c - 1.0)),
            Uni::Func(f) => {
                let v = f.apply(x);
                let d = match f {
                    Func::Exp => v,
                    Func::Ln => 1.0 / x,
                    Func::Sqrt => 0.5 / v,
                    Func::Sin => x.cos(),
                    Func::Cos => -x.sin(),
                    Func::Tan => 1.0 + v * v,
                    Func::Atan => 1.0 / (1.0 + x * x),
                    Func::Tanh => 1.0 - v * v,
                };
                (v, d)
            }
        }
    }

    /// The curvature on [l, u] (Unknown if it changes sign or is unclear).
    fn curvature(self, l: f64, u: f64) -> Curvature {
        use Curvature::*;
        match self {
            Uni::PowI(n) if n > 0 && n % 2 == 0 => Convex,
            Uni::PowI(1) => Unknown, // linear: never built
            Uni::PowI(n) if n > 0 => {
                if l >= 0.0 {
                    Convex
                } else if u <= 0.0 {
                    Concave
                } else {
                    Unknown
                }
            }
            Uni::PowI(n) => {
                // x^n for n < 0: convex for x > 0; for x < 0, convex if n is even.
                if l > 0.0 || (u < 0.0 && n % 2 == 0) {
                    Convex
                } else if u < 0.0 {
                    Concave
                } else {
                    Unknown
                }
            }
            Uni::PowC(c) if l >= 0.0 => {
                if c > 1.0 || c < 0.0 {
                    Convex
                } else {
                    Concave
                }
            }
            Uni::PowC(_) => Unknown,
            Uni::Func(f) => match f {
                Func::Exp => Convex,
                Func::Ln | Func::Sqrt => Concave,
                Func::Atan | Func::Tanh => {
                    if l >= 0.0 {
                        Concave
                    } else if u <= 0.0 {
                        Convex
                    } else {
                        Unknown
                    }
                }
                Func::Tan => {
                    if l >= 0.0 && u < FRAC_PI_2 {
                        Convex
                    } else if u <= 0.0 && l > -FRAC_PI_2 {
                        Concave
                    } else {
                        Unknown
                    }
                }
                // sin'' = -sin: concave where sin >= 0, i.e. on [2k pi, (2k+1) pi].
                Func::Sin => sine_curvature(l, u),
                Func::Cos => sine_curvature(l + FRAC_PI_2, u + FRAC_PI_2),
            },
        }
    }
}

fn sine_curvature(l: f64, u: f64) -> Curvature {
    if !(l.is_finite() && u.is_finite()) || u - l > PI {
        return Curvature::Unknown;
    }
    let k = (l / PI).floor();
    if u > (k + 1.0) * PI {
        return Curvature::Unknown;
    }
    if (k as i64).rem_euclid(2) == 0 { Curvature::Concave } else { Curvature::Convex }
}

/// Rows for `w = f(x)` with x in ix.
fn univariate(rows: &mut Vec<SparseRow>, w: usize, x: usize, ix: Interval, f: Uni) {
    let (l, u) = (ix.lo, ix.hi);
    let curvature = f.curvature(l, u);
    if curvature == Curvature::Unknown {
        return;
    }
    // Tangent points: spread over the interval (or near its finite end).
    let points: Vec<f64> = match (l.is_finite(), u.is_finite()) {
        (true, true) => (0..TANGENTS).map(|i| l + (u - l) * i as f64 / (TANGENTS - 1) as f64).collect(),
        (true, false) => (0..TANGENTS).map(|i| l + (2f64.powi(i as i32) - 1.0)).collect(),
        (false, true) => (0..TANGENTS).map(|i| u - (2f64.powi(i as i32) - 1.0)).collect(),
        (false, false) => vec![-1.0, 0.0, 1.0],
    };
    for p in points {
        let (fp, dp) = f.eval(p);
        if !(fp.is_finite() && dp.is_finite()) {
            continue;
        }
        // w >= f(p) + f'(p)(x - p) for convex; <= for concave.
        let rhs = fp - dp * p;
        let coefs = vec![(w, 1.0), (x, -dp)];
        rows.extend(match curvature {
            Curvature::Convex => at_least(coefs, rhs),
            _ => at_most(coefs, rhs),
        });
    }
    // The secant on the other side.
    if l.is_finite() && u.is_finite() && u > l {
        let ((fl, _), (fu, _)) = (f.eval(l), f.eval(u));
        if fl.is_finite() && fu.is_finite() {
            let s = (fu - fl) / (u - l);
            let coefs = vec![(w, 1.0), (x, -s)];
            let rhs = fl - s * l;
            // Rounding in s matters over a wide interval: loosen by s's error too.
            let slack = 4.0 * f64::EPSILON * (fl.abs() + fu.abs()) * (1.0 + (u - l).abs()).max(1.0);
            rows.extend(match curvature {
                Curvature::Convex => at_most(coefs, rhs + slack),
                _ => at_least(coefs, rhs - slack),
            });
        }
    }
}

/// Minimizes the objective root's column over the relaxation.
pub fn lower_bound(r: &Relaxation, objective: Node, deadline: Option<Instant>) -> Bound {
    let cols = r.col_lower.len();
    if (0..cols).any(|j| r.col_lower[j] > r.col_upper[j]) {
        return Bound::Infeasible;
    }
    let mut cost = vec![0.0; cols];
    cost[r.node_col[objective as usize].expect("the objective is a root")] = 1.0;
    let lp = Lp::new(cost, r.col_lower.clone(), r.col_upper.clone(), r.rows.clone());
    let opts = simplex::Options { deadline, ..simplex::Options::default() };
    let s = simplex::solve(&lp, &lp.col_lower, &lp.col_upper, None, &opts);
    match s.status {
        simplex::LpStatus::Optimal => Bound::Lower(s.objective),
        simplex::LpStatus::Infeasible => Bound::Infeasible,
        _ => Bound::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::super::fbbt;
    use super::super::graph::testing::{Rng, random};
    use super::*;

    /// At sampled points of the box, the variables and the nodes' true values
    /// satisfy every row of the relaxation.
    #[test]
    fn rows_hold_at_true_values() {
        let mut rng = Rng(0x94d049bb133111eb);
        let mut checked = 0;
        for case in 0..400 {
            let mut g = Graph::new();
            let n = 1 + rng.below(3);
            let box_: Vec<Interval> = (0..n)
                .map(|_| {
                    let lo = rng.real(-2.0, 1.5);
                    Interval::new(lo, lo + rng.real(0.1, 3.0))
                })
                .collect();
            let roots: Vec<Node> = (0..1 + rng.below(2)).map(|_| random(&mut g, &mut rng, n, 3)).collect();
            let Some(iv) = fbbt::propagate(&g, &box_, &[]) else { continue };
            let r = build(&g, &iv, n, &box_, &roots);
            for _ in 0..100 {
                let x: Vec<f64> = box_.iter().map(|b| rng.real(b.lo, b.hi)).collect();
                let mut vals = Vec::new();
                g.eval(&x, &mut vals);
                let mut point = vec![0.0; r.col_lower.len()];
                let mut defined = true;
                for (k, c) in r.node_col.iter().enumerate() {
                    if let Some(c) = c {
                        defined &= vals[k].is_finite();
                        point[*c] = vals[k];
                    }
                }
                if !defined {
                    continue;
                }
                for (i, (coefs, lo, hi)) in r.rows.iter().enumerate() {
                    let v: f64 = coefs.iter().map(|&(j, a)| a * point[j]).sum();
                    let scale = 1.0 + coefs.iter().map(|&(j, a)| (a * point[j]).abs()).sum::<f64>();
                    let tol = 1e-9 * scale;
                    assert!(v >= lo - tol && v <= hi + tol, "case {case}: row {i} {coefs:?} = {v} not in [{lo}, {hi}]");
                    checked += 1;
                }
            }
        }
        assert!(checked > 10_000);
    }

    /// The relaxation's bound is at most the objective at every sampled
    /// feasible point.
    #[test]
    fn bounds_are_valid() {
        let mut rng = Rng(0xbf58476d1ce4e5b9);
        let mut bounded = 0;
        for case in 0..200 {
            let mut g = Graph::new();
            let n = 1 + rng.below(3);
            let box_: Vec<Interval> = (0..n).map(|_| Interval::new(rng.real(0.5, 1.0), rng.real(1.2, 2.0))).collect();
            let obj = random(&mut g, &mut rng, n, 3);
            let con = random(&mut g, &mut rng, n, 2);
            let mut vals = Vec::new();
            let xr: Vec<f64> = box_.iter().map(|b| rng.real(b.lo, b.hi)).collect();
            g.eval(&xr, &mut vals);
            let target = Interval::new(-INF, vals[con as usize] + 0.1);
            let Some(iv) = fbbt::propagate(&g, &box_, &[(con, target)]) else {
                panic!("case {case}: propagation cut the reference point")
            };
            let mut r = build(&g, &iv, n, &box_, &[obj, con]);
            let c = r.node_col[con as usize].unwrap();
            r.rows.push((vec![(c, 1.0)], -INF, target.hi));
            let bound = match lower_bound(&r, obj, None) {
                Bound::Lower(b) => b,
                Bound::Infeasible => panic!("case {case}: relaxation infeasible, but the reference point is feasible"),
                Bound::Unknown => continue,
            };
            bounded += 1;
            for _ in 0..300 {
                let x: Vec<f64> = box_.iter().map(|b| rng.real(b.lo, b.hi)).collect();
                g.eval(&x, &mut vals);
                if target.contains(vals[con as usize]) && vals[obj as usize].is_finite() {
                    let f = vals[obj as usize];
                    assert!(bound <= f + 1e-7 * (1.0 + f.abs()), "case {case}: bound {bound} above f = {f}");
                }
            }
        }
        assert!(bounded > 100);
    }

    #[test]
    fn tight_bounds_and_infeasibility() {
        // A convex function's tangents give a tight bound: min exp(x) - 2x
        // on [0, 2] is 2 - 2 ln 2 = 0.6137.
        let mut g = Graph::new();
        let x = g.var(0);
        let e = g.func(Func::Exp, x);
        let f = g.sum(0.0, &[(1.0, e), (-2.0, x)]);
        let b = vec![Interval::new(0.0, 2.0)];
        let iv = fbbt::propagate(&g, &b, &[]).unwrap();
        let r = build(&g, &iv, 1, &b, &[f]);
        match lower_bound(&r, f, None) {
            Bound::Lower(v) => assert!(v <= 2.0 - 2.0 * 2f64.ln() + 1e-9 && v > 0.4, "{v}"),
            _ => panic!("no bound"),
        }
        // A feasible problem (x * y >= 3, x + y <= 3.6 on [0, 3]^2) gets a
        // bound, and x * y >= 5 on [0, 2]^2 is infeasible (the envelope's
        // maximum is 4).
        let mut g = Graph::new();
        let (x, y) = (g.var(0), g.var(1));
        let p = g.mul(x, y);
        let s = g.sum(0.0, &[(1.0, x), (1.0, y)]);
        let b = vec![Interval::new(0.0, 3.0); 2];
        let targets = [(p, Interval::new(3.0, INF)), (s, Interval::new(-INF, 3.6))];
        let iv = fbbt::propagate(&g, &b, &targets).unwrap();
        let mut r = build(&g, &iv, 2, &b, &[p, s]);
        for &(node, t) in &targets {
            r.rows.push((vec![(r.node_col[node as usize].unwrap(), 1.0)], t.lo, t.hi));
        }
        assert!(matches!(lower_bound(&r, s, None), Bound::Lower(_)));
        let b = vec![Interval::new(0.0, 2.0); 2];
        let iv = fbbt::propagate(&g, &b, &[]).unwrap();
        let mut r = build(&g, &iv, 2, &b, &[p]);
        r.rows.push((vec![(r.node_col[p as usize].unwrap(), 1.0)], 5.0, INF));
        assert!(matches!(lower_bound(&r, p, None), Bound::Infeasible));
    }
}
