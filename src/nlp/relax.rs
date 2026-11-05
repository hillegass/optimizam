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

use super::convexity::Curvature::{Concave, Convex};
use super::graph::{Graph, Node, Op};
use super::interval::Interval;
use crate::expr::Func;
use crate::lp::{Lp, SparseRow};
use crate::simplex::{self, Basis};
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
    /// Each node's curvature on the box.
    pub curvature: Vec<super::convexity::Curvature>,
}

pub enum Bound {
    /// No point of the box satisfies the relaxation, so none satisfies the
    /// constraints.
    Infeasible,
    /// The relaxation's minimum of the objective (a lower bound), and the
    /// relaxation's solution (every column).
    Lower(f64, Vec<f64>),
    /// No bound (the relaxation is unbounded, or the LP solve failed).
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Curvature {
    Convex,
    Concave,
    Unknown,
}

/// The relaxation's layout for a graph and its roots, independent of the
/// box: which nodes are reached, each node's column, and the quadratic sums
/// with their convexity. Its rows always have the same shape for a given
/// number of hints: a row that doesn't apply in some box (an infinite bound,
/// unusable coefficients) is a free row there. So a node's LP has its
/// parent's shape and can start from its parent's basis.
pub struct Structure {
    reach: Vec<bool>,
    pub node_col: Vec<Option<usize>>,
    num_cols: usize,
    num_vars: usize,
    var_node: Vec<Option<usize>>,
    /// Convex (true) or concave quadratic sums, by node.
    quadratics: Vec<(Node, Quadratic, bool)>,
    /// Rows `lo <= root <= hi`, at the end of the base block.
    targets: Vec<(Node, Interval)>,
    /// Composite nodes (sums over nonlinear terms, products, quotients,
    /// functions of non-variables), which get outer-approximation cuts
    /// where they are convex or concave on the box.
    composites: Vec<Node>,
}

/// Outer-approximation cuts are loosened by this, relative to the size of
/// their terms (they are computed in floating point).
const OA_TOL: f64 = 1e-9;

/// A point in [lo, hi] (the middle, or near a finite end).
fn mid(lo: f64, hi: f64) -> f64 {
    match (lo.is_finite(), hi.is_finite()) {
        (true, true) => 0.5 * (lo + hi),
        (true, false) => lo + 1.0,
        (false, true) => hi - 1.0,
        (false, false) => 0.0,
    }
}

/// A free row on one column: a placeholder that keeps the shape.
fn free(col: usize) -> SparseRow {
    (vec![(col, 1.0)], -INF, INF)
}

impl Structure {
    pub fn new(graph: &Graph, num_vars: usize, roots: &[Node], targets: &[(Node, Interval)]) -> Self {
        let n = graph.len();
        let mut reach = vec![false; n];
        let mut stack: Vec<Node> = roots.iter().copied().chain(targets.iter().map(|t| t.0)).collect();
        while let Some(k) = stack.pop() {
            if std::mem::replace(&mut reach[k as usize], true) {
                continue;
            }
            children(graph.op(k), |c| stack.push(c));
        }
        let mut node_col = vec![None; n];
        let mut num_cols = num_vars;
        let mut var_node = vec![None; num_vars];
        for k in 0..n {
            if let Op::Var(j) = graph.op(k as Node) {
                var_node[*j] = Some(k);
            }
            if !reach[k] {
                continue;
            }
            node_col[k] = Some(match graph.op(k as Node) {
                Op::Var(j) => *j,
                _ => {
                    num_cols += 1;
                    num_cols - 1
                }
            });
        }
        let mut quadratics = Vec::new();
        for (k, &reached) in reach.iter().enumerate() {
            if reached
                && matches!(graph.op(k as Node), Op::Sum(..))
                && let Some(f) = quadratic(graph, k as Node)
                && let Some(convex) = curvature_of(&f)
            {
                quadratics.push((k as Node, f, convex));
            }
        }
        let nonlinear = |k: usize| !matches!(graph.op(k as Node), Op::Const(_) | Op::Var(_) | Op::Sum(..));
        let mut below = vec![false; n]; // a nonlinear node at or below
        for k in 0..n {
            let mut b = nonlinear(k);
            children(graph.op(k as Node), |c| b |= below[c as usize]);
            below[k] = b;
        }
        let is_var = |c: Node| matches!(graph.op(c), Op::Var(_));
        let affine: Vec<bool> = (0..n).map(|k| !below[k]).collect();
        let composites = (0..n)
            .filter(|&k| reach[k])
            .filter(|&k| match graph.op(k as Node) {
                Op::Sum(..) => below[k],
                // A product of two different affine terms is never convex.
                Op::Mul(a, b) => !(a == b && is_var(*a)) && !(a != b && affine[*a as usize] && affine[*b as usize]),
                Op::Div(..) | Op::Pow(..) => true,
                Op::PowI(a, _) | Op::PowC(a, _) | Op::Func(_, a) => !is_var(*a),
                _ => false,
            })
            .map(|k| k as Node)
            .collect();
        Structure { reach, node_col, num_cols, num_vars, var_node, quadratics, targets: targets.to_vec(), composites }
    }

    /// The base block over node ranges `iv` (from `fbbt::propagate`) and the
    /// variables' bounds.
    pub fn base(&self, graph: &Graph, iv: &[Interval], var_bounds: &[Interval]) -> Relaxation {
        let n = graph.len();
        let mut col_lower = vec![-INF; self.num_cols];
        let mut col_upper = vec![INF; self.num_cols];
        for j in 0..self.num_vars {
            (col_lower[j], col_upper[j]) = (var_bounds[j].lo, var_bounds[j].hi);
        }
        for k in 0..n {
            if let Some(c) = self.node_col[k] {
                col_lower[c] = col_lower[c].max(iv[k].lo);
                col_upper[c] = col_upper[c].min(iv[k].hi);
            }
        }
        let mut rows: Vec<SparseRow> = Vec::new();
        let col = |c: Node| self.node_col[c as usize].unwrap();
        for k in 0..n {
            if !self.reach[k] {
                continue;
            }
            let w = self.node_col[k].unwrap();
            match graph.op(k as Node) {
                Op::Var(_) | Op::Const(_) | Op::Pow(..) => {}
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
            }
        }
        // Tangent planes of the convex (or concave) quadratic sums, at the
        // box's center.
        for (k, f, convex) in &self.quadratics {
            let center: Vec<Option<f64>> =
                f.vars.iter().map(|&j| Some(0.5 * (col_lower[j] + col_upper[j])).filter(|v| v.is_finite())).collect();
            rows.push(quadratic_plane(
                f,
                *convex,
                self.node_col[*k as usize].unwrap(),
                &center,
                &col_lower,
                &col_upper,
            ));
        }
        for &(node, t) in &self.targets {
            rows.push((vec![(col(node), 1.0)], t.lo, t.hi));
        }
        let curvature = super::convexity::curvatures(graph, iv);
        let mut r = Relaxation { col_lower, col_upper, rows, node_col: self.node_col.clone(), curvature };
        // Outer approximation at the box's center.
        let center: Vec<f64> = (0..self.num_vars).map(|j| mid(r.col_lower[j], r.col_upper[j])).collect();
        let rows = self.oa_rows(graph, &r, &center);
        r.rows.extend(rows);
        r
    }

    /// Outer-approximation cuts at the point x (variables' values, clamped to
    /// the box): for each composite node, its tangent plane in the variables
    /// where it is convex (an underestimator) or concave (overestimator) on
    /// the box, else a free row.
    fn oa_rows(&self, graph: &Graph, r: &Relaxation, x: &[f64]) -> Vec<SparseRow> {
        let p: Vec<f64> = (0..self.num_vars).map(|j| x[j].clamp(r.col_lower[j], r.col_upper[j])).collect();
        let mut vals = Vec::new();
        if !self.composites.is_empty() {
            graph.eval(&p, &mut vals);
        }
        self.composites
            .iter()
            .map(|&k| {
                let w = self.node_col[k as usize].unwrap();
                let curv = r.curvature[k as usize];
                let row = (matches!(curv, Convex | Concave) && vals[k as usize].is_finite())
                    .then(|| graph.gradient_of(k, &vals))
                    .flatten()
                    .and_then(|grad| {
                        let f = vals[k as usize];
                        let gp: f64 = grad.iter().map(|&(j, g)| g * p[j]).sum();
                        let slack = OA_TOL * (1.0 + f.abs() + grad.iter().map(|&(j, g)| (g * p[j]).abs()).sum::<f64>());
                        let mut coefs = vec![(w, 1.0)];
                        coefs.extend(grad.iter().map(|&(j, g)| (j, -g)));
                        // w >= f(p) + g (x - p) where convex; <= where concave.
                        if curv == Convex { at_least(coefs, f - gp - slack) } else { at_most(coefs, f - gp + slack) }
                    });
                row.unwrap_or_else(|| free(w))
            })
            .collect()
    }

    /// A hint block: tangents at the point whose node values are `hint`
    /// (one row per univariate node and per quadratic sum, free where they
    /// don't apply), over the relaxation's column bounds.
    pub fn hint_rows(&self, graph: &Graph, iv: &[Interval], r: &Relaxation, hint: &[f64]) -> Vec<SparseRow> {
        let mut rows = Vec::new();
        let col = |c: Node| self.node_col[c as usize].unwrap();
        for k in 0..graph.len() {
            if !self.reach[k] {
                continue;
            }
            let w = self.node_col[k].unwrap();
            let (child, f) = match graph.op(k as Node) {
                Op::Mul(a, b) if a == b => (*a, Uni::PowI(2)),
                Op::PowI(a, p) => (*a, Uni::PowI(*p)),
                Op::PowC(a, c) => (*a, Uni::PowC(*c)),
                Op::Func(f, a) => (*a, Uni::Func(*f)),
                _ => continue,
            };
            let ix = iv[child as usize];
            let h = hint[child as usize];
            let row = (h.is_finite() && ix.lo <= h && h <= ix.hi).then(|| tangent(w, col(child), ix, f, h)).flatten();
            rows.push(row.unwrap_or_else(|| free(w)));
        }
        for (k, f, convex) in &self.quadratics {
            let point: Vec<Option<f64>> = f
                .vars
                .iter()
                .map(|&j| {
                    self.var_node[j]
                        .map(|n| hint[n])
                        .filter(|v| v.is_finite())
                        .map(|v| v.clamp(r.col_lower[j], r.col_upper[j]))
                })
                .collect();
            rows.push(quadratic_plane(
                f,
                *convex,
                self.node_col[*k as usize].unwrap(),
                &point,
                &r.col_lower,
                &r.col_upper,
            ));
        }
        // Outer approximation at the hint's variables.
        let x: Vec<f64> = (0..self.num_vars).map(|j| self.var_node[j].map_or(f64::NAN, |n| hint[n])).collect();
        if x.iter().all(|v| v.is_finite()) {
            rows.extend(self.oa_rows(graph, r, &x));
        } else {
            rows.extend(self.composites.iter().map(|&k| free(self.node_col[k as usize].unwrap())));
        }
        rows
    }
}

/// Builds the relaxation of the roots over node ranges `iv` (from
/// `fbbt::propagate`), with a hint block for each of `hints` (every node's
/// value at some point, such as the incumbent): tangents there, where a
/// tight relaxation matters most.
#[cfg(test)]
pub fn build(
    graph: &Graph,
    iv: &[Interval],
    num_vars: usize,
    var_bounds: &[Interval],
    roots: &[Node],
    hints: &[Vec<f64>],
) -> Relaxation {
    let s = Structure::new(graph, num_vars, roots, &[]);
    let mut r = s.base(graph, iv, var_bounds);
    for h in hints {
        let rows = s.hint_rows(graph, iv, &r, h);
        r.rows.extend(rows);
    }
    r
}

/// A quadratic function `c + b x + x^T Q x` of variables (Q symmetric, over
/// the variables listed), or None if the node isn't one.
struct Quadratic {
    vars: Vec<usize>,
    c: f64,
    b: Vec<f64>,
    q: Vec<f64>,
}

/// An affine expression `c + sum(a_j x_j)` of nested sums of variables.
fn affine(graph: &Graph, root: Node) -> Option<(f64, Vec<(usize, f64)>)> {
    let (mut c, mut lin) = (0.0, Vec::new());
    let mut stack = vec![(root, 1.0)];
    while let Some((k, a)) = stack.pop() {
        match graph.op(k) {
            Op::Const(v) => c += a * v,
            Op::Var(j) => lin.push((*j, a)),
            Op::Sum(k0, terms) => {
                c += a * k0;
                stack.extend(terms.iter().map(|&(b, t)| (t, a * b)));
            }
            _ => return None,
        }
        if stack.len() > 10_000 {
            return None;
        }
    }
    Some((c, lin))
}

/// Expands a sum into a quadratic function, through nested sums and
/// products and squares of affine expressions; at most MAX_QUADRATIC_VARS
/// variables.
fn quadratic(graph: &Graph, root: Node) -> Option<Quadratic> {
    let mut c = 0.0;
    let mut lin: Vec<(usize, f64)> = Vec::new();
    let mut quad: Vec<(usize, usize, f64)> = Vec::new();
    let mut stack = vec![(root, 1.0)];
    let mut nonlinear = false;
    // a * (c1 + l1 x)(c2 + l2 x), expanded.
    let mut product =
        |a: f64, p: (f64, Vec<(usize, f64)>), q: (f64, Vec<(usize, f64)>), c: &mut f64, lin: &mut Vec<(usize, f64)>| {
            *c += a * p.0 * q.0;
            lin.extend(p.1.iter().map(|&(j, v)| (j, a * v * q.0)));
            lin.extend(q.1.iter().map(|&(j, v)| (j, a * v * p.0)));
            for &(i, u) in &p.1 {
                for &(j, v) in &q.1 {
                    quad.push((i, j, a * u * v));
                }
            }
        };
    while let Some((k, a)) = stack.pop() {
        match graph.op(k) {
            Op::Const(v) => c += a * v,
            Op::Var(j) => lin.push((*j, a)),
            Op::Sum(k0, terms) => {
                c += a * k0;
                stack.extend(terms.iter().map(|&(b, t)| (t, a * b)));
            }
            Op::Mul(x, y) => {
                product(a, affine(graph, *x)?, affine(graph, *y)?, &mut c, &mut lin);
                nonlinear = true;
            }
            Op::PowI(x, 2) => {
                let e = affine(graph, *x)?;
                product(a, e.clone(), e, &mut c, &mut lin);
                nonlinear = true;
            }
            _ => return None,
        }
        if stack.len() > 10_000 {
            return None;
        }
    }
    if !nonlinear {
        return None;
    }
    let mut vars: Vec<usize> = quad.iter().flat_map(|&(i, j, _)| [i, j]).chain(lin.iter().map(|l| l.0)).collect();
    vars.sort_unstable();
    vars.dedup();
    if vars.len() > MAX_QUADRATIC_VARS {
        return None;
    }
    let at = |j: usize| vars.binary_search(&j).unwrap();
    let m = vars.len();
    let mut b = vec![0.0; m];
    for (j, a) in lin {
        b[at(j)] += a;
    }
    let mut q = vec![0.0; m * m];
    for (i, j, a) in quad {
        let (p, r) = (at(i), at(j));
        if p == r {
            q[p * m + p] += a;
        } else {
            q[p * m + r] += 0.5 * a;
            q[r * m + p] += 0.5 * a;
        }
    }
    Some(Quadratic { vars, c, b, q })
}

/// Quadratic forms with more variables than this aren't checked.
const MAX_QUADRATIC_VARS: usize = 60;

/// Whether a quadratic function is convex (Some(true)), concave
/// (Some(false)), or neither: the inertia of Q + eps I, eps relative to Q's
/// size. (Each plane is then loosened by eps times the box's squared
/// diameter, which keeps it valid even if Q has eigenvalues as small as
/// -eps.)
fn curvature_of(f: &Quadratic) -> Option<bool> {
    let m = f.vars.len();
    let scale = f.q.iter().fold(0.0f64, |a, v| a.max(v.abs()));
    if scale == 0.0 {
        return None;
    }
    let eps = QUADRATIC_EPS * scale;
    let shifted = |sign: f64| -> super::ldl::Inertia {
        let mut a = vec![0.0; m * m];
        for i in 0..m {
            for j in 0..=i {
                a[i * m + j] = sign * f.q[i * m + j];
            }
            a[i * m + i] += eps;
        }
        super::ldl::Ldl::factor(m, &a).inertia
    };
    if shifted(1.0).negative == 0 {
        Some(true)
    } else if shifted(-1.0).negative == 0 {
        Some(false)
    } else {
        None
    }
}

const QUADRATIC_EPS: f64 = 1e-10;

/// The tangent plane of a convex (or concave) quadratic sum at the point
/// p (free if p or the box's bounds aren't all finite).
fn quadratic_plane(
    f: &Quadratic,
    convex: bool,
    w: usize,
    p: &[Option<f64>],
    lower: &[f64],
    upper: &[f64],
) -> SparseRow {
    let m = f.vars.len();
    if p.iter().any(|v| v.is_none()) || f.vars.iter().any(|&j| !(lower[j].is_finite() && upper[j].is_finite())) {
        return free(w);
    }
    let p: Vec<f64> = p.iter().map(|v| v.unwrap()).collect();
    let scale = f.q.iter().fold(0.0f64, |a, v| a.max(v.abs()));
    let diameter2: f64 = f.vars.iter().map(|&j| (upper[j] - lower[j]).powi(2)).sum();
    let slack = QUADRATIC_EPS * scale * diameter2;
    // q(p) and its gradient b + 2 Q p.
    let qp: Vec<f64> = (0..m).map(|i| (0..m).map(|j| f.q[i * m + j] * p[j]).sum()).collect();
    let value = f.c + (0..m).map(|i| f.b[i] * p[i] + p[i] * qp[i]).sum::<f64>();
    let grad: Vec<f64> = (0..m).map(|i| f.b[i] + 2.0 * qp[i]).collect();
    // w >= q(p) + grad (x - p) for convex; <= for concave.
    let rhs = value - (0..m).map(|i| grad[i] * p[i]).sum::<f64>();
    let mut coefs = vec![(w, 1.0)];
    coefs.extend(f.vars.iter().zip(&grad).map(|(&j, &g)| (j, -g)));
    let row = if convex { at_least(coefs, rhs - slack) } else { at_most(coefs, rhs + slack) };
    row.unwrap_or_else(|| free(w))
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
    // Always four rows (free where a bound is infinite), for a fixed shape.
    let finite = |a: f64, b: f64| a.is_finite() && b.is_finite();
    let rows4 = [
        finite(xl, yl).then(|| at_least(vec![(p, 1.0), (y, -xl), (x, -yl)], -xl * yl)).flatten(),
        finite(xu, yu).then(|| at_least(vec![(p, 1.0), (y, -xu), (x, -yu)], -xu * yu)).flatten(),
        finite(xu, yl).then(|| at_most(vec![(p, 1.0), (y, -xu), (x, -yl)], -xu * yl)).flatten(),
        finite(xl, yu).then(|| at_most(vec![(p, 1.0), (y, -xl), (x, -yu)], -xl * yu)).flatten(),
    ];
    rows.extend(rows4.into_iter().map(|r| r.unwrap_or_else(|| free(p))));
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

/// The tangent of `w = f(x)` at p (on the convex side), if f's curvature
/// on ix has one sign and the tangent is usable.
fn tangent(w: usize, x: usize, ix: Interval, f: Uni, p: f64) -> Option<SparseRow> {
    let curvature = f.curvature(ix.lo, ix.hi);
    if curvature == Curvature::Unknown {
        return None;
    }
    let (fp, dp) = f.eval(p);
    if !(fp.is_finite() && dp.is_finite()) {
        return None;
    }
    // w >= f(p) + f'(p)(x - p) for convex; <= for concave.
    let rhs = fp - dp * p;
    let coefs = vec![(w, 1.0), (x, -dp)];
    match curvature {
        Curvature::Convex => at_least(coefs, rhs),
        _ => at_most(coefs, rhs),
    }
}

/// Rows for `w = f(x)` with x in ix: TANGENTS tangents and the secant, each
/// free where it doesn't apply.
fn univariate(rows: &mut Vec<SparseRow>, w: usize, x: usize, ix: Interval, f: Uni) {
    let (l, u) = (ix.lo, ix.hi);
    // Tangent points: spread over the interval (or near its finite end).
    let points: Vec<f64> = match (l.is_finite(), u.is_finite()) {
        (true, true) => (0..TANGENTS).map(|i| l + (u - l) * i as f64 / (TANGENTS - 1) as f64).collect(),
        (true, false) => (0..TANGENTS).map(|i| l + (2f64.powi(i as i32) - 1.0)).collect(),
        (false, true) => (0..TANGENTS).map(|i| u - (2f64.powi(i as i32) - 1.0)).collect(),
        (false, false) => (0..TANGENTS).map(|i| i as f64 - (TANGENTS / 2) as f64).collect(),
    };
    for p in points {
        rows.push(tangent(w, x, ix, f, p).unwrap_or_else(|| free(w)));
    }
    // The secant on the other side.
    let curvature = f.curvature(l, u);
    let mut secant = None;
    if curvature != Curvature::Unknown && l.is_finite() && u.is_finite() && u > l {
        let ((fl, _), (fu, _)) = (f.eval(l), f.eval(u));
        if fl.is_finite() && fu.is_finite() {
            let s = (fu - fl) / (u - l);
            let coefs = vec![(w, 1.0), (x, -s)];
            let rhs = fl - s * l;
            // Rounding in s matters over a wide interval: loosen by s's error too.
            let slack = 4.0 * f64::EPSILON * (fl.abs() + fu.abs()) * (1.0 + (u - l).abs()).max(1.0);
            secant = match curvature {
                Curvature::Convex => at_most(coefs, rhs + slack),
                _ => at_least(coefs, rhs - slack),
            };
        }
    }
    rows.push(secant.unwrap_or_else(|| free(w)));
}

/// Minimizes the objective root's column over the relaxation.
#[cfg(test)]
pub fn lower_bound(r: &Relaxation, objective: Node, deadline: Option<Instant>) -> Bound {
    lower_bound_from(r, objective, None, deadline).0
}

/// `lower_bound`, starting from a basis (of this relaxation, or of one with
/// fewer rows), and returning the final basis.
pub fn lower_bound_from(
    r: &Relaxation,
    objective: Node,
    start: Option<&Basis>,
    deadline: Option<Instant>,
) -> (Bound, Option<Basis>) {
    let cols = r.col_lower.len();
    if (0..cols).any(|j| r.col_lower[j] > r.col_upper[j]) {
        return (Bound::Infeasible, None);
    }
    let mut cost = vec![0.0; cols];
    cost[r.node_col[objective as usize].expect("the objective is a root")] = 1.0;
    let lp = Lp::new(cost, r.col_lower.clone(), r.col_upper.clone(), r.rows.clone());
    let opts = simplex::Options { deadline, ..simplex::Options::default() };
    let s = solve_scaled(&lp, start, &opts);
    match s.status {
        simplex::LpStatus::Optimal => (Bound::Lower(s.objective, s.col_value), s.basis),
        // The simplex method's infeasibility verdicts (even the elastic
        // check's) aren't reliable on LPs whose coefficients spread over many
        // orders of magnitude: there, the box is only branched on.
        simplex::LpStatus::Infeasible if well_scaled(r) && confirm_infeasible(r, deadline) => (Bound::Infeasible, None),
        _ => (Bound::Unknown, None),
    }
}

/// The relaxation's minimum (or maximum, with `maximize`) of one column, if
/// the LP is well scaled and solves to optimality.
pub fn extreme(r: &Relaxation, col: usize, maximize: bool, deadline: Option<Instant>) -> Option<f64> {
    if !well_scaled(r) {
        return None;
    }
    let mut cost = vec![0.0; r.col_lower.len()];
    cost[col] = if maximize { -1.0 } else { 1.0 };
    let lp = Lp::new(cost, r.col_lower.clone(), r.col_upper.clone(), r.rows.clone());
    let opts = simplex::Options { deadline, ..simplex::Options::default() };
    let s = solve_scaled(&lp, None, &opts);
    (s.status == simplex::LpStatus::Optimal).then(|| s.col_value[col])
}

/// Solves an LP after scaling its rows and columns by powers of 2
/// (geometric-mean equilibration, a few passes), and maps the solution back.
/// Relaxations mix coefficients and bounds over many orders of magnitude
/// (a product of ten variables up to 9 reaches 3.5e9), and the simplex
/// method has no scaling of its own: unscaled, it has returned wrong optima.
pub fn solve_scaled(lp: &Lp, start: Option<&Basis>, opts: &simplex::Options) -> simplex::LpSolution {
    let (n, m) = (lp.num_col, lp.num_row);
    let mut col = vec![1.0f64; n];
    let mut row = vec![1.0f64; m];
    let pow2 = |v: f64| if v.is_finite() && v > 0.0 { 2f64.powi(v.log2().round() as i32) } else { 1.0 };
    for _ in 0..SCALE_PASSES {
        // Rows: 1 / sqrt(min * max) of the scaled entries.
        for i in 0..m {
            let (mut lo, mut hi) = (INF, 0.0f64);
            for k in lp.ar_start[i]..lp.ar_start[i + 1] {
                let a = (lp.ar_value[k] * col[lp.ar_index[k]]).abs();
                if a > 0.0 {
                    lo = lo.min(a);
                    hi = hi.max(a);
                }
            }
            row[i] = if hi > 0.0 { pow2(1.0 / (lo * hi).sqrt()) } else { 1.0 };
        }
        for j in 0..n {
            let (mut lo, mut hi) = (INF, 0.0f64);
            for k in lp.a_start[j]..lp.a_start[j + 1] {
                let a = (lp.a_value[k] * row[lp.a_index[k]]).abs();
                if a > 0.0 {
                    lo = lo.min(a);
                    hi = hi.max(a);
                }
            }
            col[j] = if hi > 0.0 { pow2(1.0 / (lo * hi).sqrt()) } else { 1.0 };
        }
    }
    // x = col * x', and row i is multiplied by row[i].
    let rows: Vec<SparseRow> = (0..m)
        .map(|i| {
            let coefs = (lp.ar_start[i]..lp.ar_start[i + 1])
                .map(|k| (lp.ar_index[k], lp.ar_value[k] * row[i] * col[lp.ar_index[k]]))
                .collect();
            (coefs, lp.row_lower[i] * row[i], lp.row_upper[i] * row[i])
        })
        .collect();
    let cost: Vec<f64> = (0..n).map(|j| lp.col_cost[j] * col[j]).collect();
    let lower: Vec<f64> = (0..n).map(|j| lp.col_lower[j] / col[j]).collect();
    let upper: Vec<f64> = (0..n).map(|j| lp.col_upper[j] / col[j]).collect();
    let scaled = Lp::new(cost, lower, upper, rows);
    let fitted = start.and_then(|b| fit_basis(b, lp));
    let mut s = simplex::solve(&scaled, &scaled.col_lower, &scaled.col_upper, fitted.as_ref(), opts);
    if fitted.is_some() && !matches!(s.status, simplex::LpStatus::Optimal | simplex::LpStatus::Infeasible) {
        // A warm start that didn't work out: from scratch.
        s = simplex::solve(&scaled, &scaled.col_lower, &scaled.col_upper, None, opts);
    }
    for (v, c) in s.col_value.iter_mut().zip(&col) {
        *v *= c;
    }
    s
}

const SCALE_PASSES: usize = 4;

/// A basis from an LP with the same columns and the first rows of this one:
/// the new rows' slacks join it as basic, and nonbasic variables move to a
/// finite bound of this LP (a placeholder row's slack is free). None if the
/// basis doesn't fit.
pub fn fit_basis(b: &Basis, lp: &Lp) -> Option<Basis> {
    let (n, m) = (lp.num_col, lp.num_row);
    let old_m = b.basic_index.len();
    if old_m > m || b.nonbasic_move.len() != n + old_m {
        return None;
    }
    let mut basic_index = b.basic_index.clone();
    basic_index.extend(n + old_m..n + m);
    let mut nonbasic_move = b.nonbasic_move.clone();
    nonbasic_move.extend(std::iter::repeat_n(0, m - old_m));
    let mut dual_edge_weight = b.dual_edge_weight.clone();
    dual_edge_weight.extend(std::iter::repeat_n(1.0, m - old_m));
    let mut basic = vec![false; n + m];
    for &v in &basic_index {
        basic[v] = true;
    }
    for j in 0..n + m {
        if basic[j] {
            continue;
        }
        // Slack i has bounds [-row_upper, -row_lower].
        let (lo, hi) =
            if j < n { (lp.col_lower[j], lp.col_upper[j]) } else { (-lp.row_upper[j - n], -lp.row_lower[j - n]) };
        nonbasic_move[j] = match (lo.is_finite(), hi.is_finite()) {
            _ if lo == hi => 0,
            (true, true) => {
                if nonbasic_move[j] == 0 {
                    1
                } else {
                    nonbasic_move[j]
                }
            }
            (true, false) => 1,
            (false, true) => -1,
            (false, false) => 0,
        };
    }
    Some(Basis { basic_index, nonbasic_move, dual_edge_weight })
}

/// Whether the relaxation's row coefficients span at most MAX_SPREAD.
fn well_scaled(r: &Relaxation) -> bool {
    let mags = r.rows.iter().flat_map(|row| row.0.iter().map(|c| c.1.abs())).filter(|&a| a > 0.0);
    let (lo, hi) = mags.fold((INF, 0.0f64), |(l, h), a| (l.min(a), h.max(a)));
    hi <= MAX_SPREAD * lo || hi == 0.0
}

/// The coefficient spread beyond which an LP's infeasibility isn't trusted.
const MAX_SPREAD: f64 = 1e10;

/// Confirms an infeasible relaxation with its elastic form: each row gets
/// slacks s+, s- >= 0, and the LP minimizes their sum. That LP is always
/// feasible and bounded, so its optimum (the least total violation) is a
/// more reliable answer than the simplex method's infeasibility verdict on
/// a badly scaled LP.
fn confirm_infeasible(r: &Relaxation, deadline: Option<Instant>) -> bool {
    let cols = r.col_lower.len();
    let m = r.rows.len();
    let mut lower = r.col_lower.clone();
    let mut upper = r.col_upper.clone();
    lower.extend(std::iter::repeat_n(0.0, 2 * m));
    upper.extend(std::iter::repeat_n(INF, 2 * m));
    let mut cost = vec![0.0; cols];
    cost.extend(std::iter::repeat_n(1.0, 2 * m));
    let rows: Vec<SparseRow> = r
        .rows
        .iter()
        .enumerate()
        .map(|(i, (coefs, lo, hi))| {
            let mut c = coefs.clone();
            c.push((cols + 2 * i, 1.0));
            c.push((cols + 2 * i + 1, -1.0));
            (c, *lo, *hi)
        })
        .collect();
    let lp = Lp::new(cost, lower, upper, rows);
    let opts = simplex::Options { deadline, ..simplex::Options::default() };
    let s = solve_scaled(&lp, None, &opts);
    s.status == simplex::LpStatus::Optimal && s.objective > ELASTIC_TOL
}

/// A relaxation counts as infeasible when its least total violation is
/// above this.
const ELASTIC_TOL: f64 = 1e-6;

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
            let r = build(&g, &iv, n, &box_, &roots, &[]);
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
            let mut r = build(&g, &iv, n, &box_, &[obj, con], &[]);
            let c = r.node_col[con as usize].unwrap();
            r.rows.push((vec![(c, 1.0)], -INF, target.hi));
            let bound = match lower_bound(&r, obj, None) {
                Bound::Lower(b, _) => b,
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
        let r = build(&g, &iv, 1, &b, &[f], &[]);
        match lower_bound(&r, f, None) {
            Bound::Lower(v, _) => assert!(v <= 2.0 - 2.0 * 2f64.ln() + 1e-9 && v > 0.4, "{v}"),
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
        let mut r = build(&g, &iv, 2, &b, &[p, s], &[]);
        for &(node, t) in &targets {
            r.rows.push((vec![(r.node_col[node as usize].unwrap(), 1.0)], t.lo, t.hi));
        }
        assert!(matches!(lower_bound(&r, s, None), Bound::Lower(..)));
        let b = vec![Interval::new(0.0, 2.0); 2];
        let iv = fbbt::propagate(&g, &b, &[]).unwrap();
        let mut r = build(&g, &iv, 2, &b, &[p], &[]);
        r.rows.push((vec![(r.node_col[p as usize].unwrap(), 1.0)], 5.0, INF));
        assert!(matches!(lower_bound(&r, p, None), Bound::Infeasible));
    }
}
