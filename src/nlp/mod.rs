//! Nonlinear programming (continuous variables): the expression graph with
//! derivatives (`graph`), a dense symmetric indefinite factorization
//! (`ldl`), and an interior point method after IPOPT (`ipm`).
//!
//! [`solve`] puts a model in the interior point method's standard form:
//! fixed columns become constants; each row `lower <= g(x) <= upper` becomes
//! an equality `g(x) - b = 0`, or `g(x) - s = 0` with a slack `s` bounded by
//! `[lower, upper]`; a maximization is negated. Rows and the objective are
//! scaled at the starting point so that no gradient entry exceeds 100
//! (IPOPT's gradient-based scaling).

pub mod fbbt;
pub mod global;
pub mod graph;
pub mod interval;
pub mod ipm;
pub mod ldl;
pub mod relax;

use crate::model::{LinearModel, Sense};
use graph::{Evaluator, Graph};
use interval::Interval;
use std::time::Instant;

/// A local optimum within this (relative to 1 + |f|) of the relaxation's
/// bound is reported as proven.
const GAP_TOL: f64 = 1e-6;
const INFINITY: f64 = f64::INFINITY;
/// Largest gradient entry after scaling.
const MAX_GRADIENT: f64 = 100.0;
const MIN_SCALE: f64 = 1e-8;
/// Constraint violation (relative to the bound's magnitude) allowed in a
/// reported solution, in the original units.
const VIOLATION_TOL: f64 = 1e-6;

pub enum Outcome {
    /// A local optimum (KKT point); for a problem without nonlinear terms
    /// (convex) or a satisfy problem, `proven` is true.
    Optimal { x: Vec<f64>, proven: bool },
    /// Stopped (time or iteration limit, or numerical trouble) with a
    /// feasible point.
    Feasible(Vec<f64>),
    /// The search converged to a local minimizer of the infeasibility.
    LocallyInfeasible,
    /// A constant row is violated: proven infeasible.
    Infeasible,
    /// Stopped without a feasible point.
    NoSolution,
}

/// A row of the model: `lower <= coefs * x + node(x) <= upper`.
#[derive(Clone)]
pub struct Row {
    pub coefs: Vec<(usize, f64)>,
    pub node: Option<graph::Node>,
    pub lower: f64,
    pub upper: f64,
}

/// One row of the standard form, before scaling: `coefs * x + node(x)`
/// (over the model's columns), with its bounds.
struct RowDef {
    coefs: Vec<(usize, f64)>,
    /// Index of the row's root in the evaluator (after the objective's).
    root: Option<usize>,
    lower: f64,
    upper: f64,
    /// The slack variable's index in z, for an inequality.
    slack: Option<usize>,
}

/// The model in standard form, for the interior point method.
struct Standard<'g> {
    ev: Evaluator<'g>,
    /// Number of model columns, and the z index of each (None if fixed).
    num_cols: usize,
    col_z: Vec<Option<usize>>,
    /// The model column of each of the first z entries.
    z_col: Vec<usize>,
    fixed: Vec<f64>,
    /// The objective: linear part (model columns), constant, root, sign.
    obj: Vec<(usize, f64)>,
    obj_offset: f64,
    obj_root: Option<usize>,
    obj_sign: f64,
    obj_scale: f64,
    rows: Vec<RowDef>,
    row_scale: Vec<f64>,
    n: usize,
    lower: Vec<f64>,
    upper: Vec<f64>,
    roots: usize,
}

impl Standard<'_> {
    /// The model's columns at z.
    fn columns(&self, z: &[f64]) -> Vec<f64> {
        (0..self.num_cols).map(|j| self.col_z[j].map_or(self.fixed[j], |k| z[k])).collect()
    }

    /// g(x) for every row and the objective (unscaled), at the model's
    /// columns x.
    fn values(&mut self, x: &[f64]) -> (f64, Vec<f64>) {
        let roots = if self.roots > 0 { self.ev.eval(x) } else { Vec::new() };
        let lin = |coefs: &[(usize, f64)]| coefs.iter().map(|&(j, a)| a * x[j]).sum::<f64>();
        let f = lin(&self.obj) + self.obj_offset + self.obj_root.map_or(0.0, |k| roots[k]);
        let g = self.rows.iter().map(|r| lin(&r.coefs) + r.root.map_or(0.0, |k| roots[k])).collect();
        (f, g)
    }

    /// Unscaled gradients (dense over the model's columns) of the objective
    /// and of each row, at x (after `values`).
    fn gradients(&mut self, x: &[f64]) -> (Vec<f64>, Vec<Vec<f64>>) {
        if self.roots > 0 {
            self.ev.eval(x);
        }
        let dense = |ev: &mut Evaluator, coefs: &[(usize, f64)], root: Option<usize>, n: usize| {
            let mut g = vec![0.0; n];
            for &(j, a) in coefs {
                g[j] += a;
            }
            if let Some(k) = root {
                for (j, v) in ev.gradient(k) {
                    g[j] += v;
                }
            }
            g
        };
        let n = self.num_cols;
        let obj = dense(&mut self.ev, &self.obj, self.obj_root, n);
        let rows = (0..self.rows.len())
            .map(|r| {
                let (coefs, root) = (self.rows[r].coefs.clone(), self.rows[r].root);
                dense(&mut self.ev, &coefs, root, n)
            })
            .collect();
        (obj, rows)
    }
}

impl ipm::Problem for Standard<'_> {
    fn n(&self) -> usize {
        self.n
    }
    fn m(&self) -> usize {
        self.rows.len()
    }
    fn lower(&self) -> &[f64] {
        &self.lower
    }
    fn upper(&self) -> &[f64] {
        &self.upper
    }

    fn eval(&mut self, z: &[f64]) -> Option<(f64, Vec<f64>)> {
        let x = self.columns(z);
        let (f, g) = self.values(&x);
        let f = self.obj_scale * self.obj_sign * f;
        let c: Vec<f64> = (0..self.rows.len())
            .map(|r| {
                let row = &self.rows[r];
                let d = self.row_scale[r];
                match row.slack {
                    Some(k) => d * g[r] - z[k],
                    None => d * (g[r] - row.lower),
                }
            })
            .collect();
        (f.is_finite() && c.iter().all(|v| v.is_finite())).then_some((f, c))
    }

    fn violation_ok(&self, z: &[f64], c: &[f64]) -> bool {
        self.rows.iter().zip(c).enumerate().all(|(r, (row, &cr))| {
            let d = self.row_scale[r];
            let tol = |b: f64| VIOLATION_TOL * (1.0 + b.abs());
            match row.slack {
                // c = d g - s: g = (c + s) / d, against the row's bounds.
                Some(k) => {
                    let g = (cr + z[k]) / d;
                    g >= row.lower - tol(row.lower) && g <= row.upper + tol(row.upper)
                }
                None => (cr / d).abs() <= tol(row.lower),
            }
        })
    }

    fn derivatives(&mut self, z: &[f64]) -> (Vec<f64>, Vec<f64>) {
        let x = self.columns(z);
        let (go, gr) = self.gradients(&x);
        let n = self.n;
        let mut grad = vec![0.0; n];
        for (k, &j) in self.z_col.iter().enumerate() {
            grad[k] = self.obj_scale * self.obj_sign * go[j];
        }
        let mut jac = vec![0.0; self.rows.len() * n];
        for (r, row) in self.rows.iter().enumerate() {
            let d = self.row_scale[r];
            for (k, &j) in self.z_col.iter().enumerate() {
                jac[r * n + k] = d * gr[r][j];
            }
            if let Some(s) = row.slack {
                jac[r * n + s] = -1.0;
            }
        }
        (grad, jac)
    }

    fn hessian(&mut self, z: &[f64], sigma: f64, y: &[f64]) -> Vec<f64> {
        let n = self.n;
        let mut h = vec![0.0; n * n];
        if self.roots == 0 {
            return h;
        }
        let x = self.columns(z);
        let mut weights = vec![0.0; self.roots];
        if let Some(k) = self.obj_root {
            weights[k] = sigma * self.obj_scale * self.obj_sign;
        }
        for (r, row) in self.rows.iter().enumerate() {
            if let Some(k) = row.root {
                weights[k] += y[r] * self.row_scale[r];
            }
        }
        self.ev.eval(&x);
        let values = self.ev.hessian(&weights);
        for (&(i, j), v) in self.ev.hessian_pattern().iter().zip(values) {
            if let (Some(a), Some(b)) = (self.col_z[i], self.col_z[j]) {
                let (a, b) = (a.max(b), a.min(b));
                h[a * n + b] += v;
            }
        }
        h
    }
}

/// The pieces of a model that local solves and bounds work on: its rows
/// (affine part plus nonlinear node) and its objective, as a minimization.
pub struct Local<'m> {
    pub model: &'m LinearModel,
    pub graph: &'m Graph,
    pub rows: Vec<Row>,
    pub obj_node: Option<graph::Node>,
    /// -1 for a maximization, else 1.
    pub sign: f64,
    pub satisfy: bool,
    debug: bool,
}

/// The result of a local solve.
pub enum LocalRun {
    /// The functions can't be evaluated at the start.
    BadStart,
    /// A row whose variables are all fixed is violated.
    Infeasible,
    Done {
        status: ipm::Status,
        /// The final iterate, if it satisfies the rows and the bounds.
        x: Option<Vec<f64>>,
        /// The best feasible iterate seen, if any.
        best: Option<Vec<f64>>,
    },
}

impl<'m> Local<'m> {
    pub fn new(model: &'m LinearModel, graph: &'m Graph) -> Self {
        let mut rows: Vec<Row> = model
            .rows
            .iter()
            .map(|r| Row { coefs: r.coefs.clone(), node: None, lower: r.lower, upper: r.upper })
            .collect();
        let mut obj_node = None;
        if let Some(nl) = &model.nonlinear {
            rows.extend(nl.rows.iter().map(|r| Row {
                coefs: r.coefs.clone(),
                node: Some(r.node),
                lower: r.lower,
                upper: r.upper,
            }));
            obj_node = nl.objective;
        }
        rows.retain(|r| r.lower > f64::NEG_INFINITY || r.upper < f64::INFINITY);
        let sign = if model.sense == Sense::Maximize { -1.0 } else { 1.0 };
        let satisfy = model.sense == Sense::Satisfy;
        Local { model, graph, rows, obj_node, sign, satisfy, debug: std::env::var_os("OPTIMIZAM_DEBUG").is_some() }
    }

    /// The objective (in the minimization sense) at the model's columns x.
    pub fn objective(&self, x: &[f64]) -> f64 {
        if self.satisfy { 0.0 } else { self.sign * self.model.objective_value(x) }
    }

    /// Whether x satisfies every row and the bounds `lower`/`upper` (within
    /// VIOLATION_TOL).
    pub fn feasible(&self, x: &[f64], lower: &[f64], upper: &[f64]) -> bool {
        self.feasible_within(x, lower, upper, VIOLATION_TOL)
    }

    /// Whether x satisfies every row and the bounds within `tol` (relative
    /// to 1 + |bound|).
    pub fn feasible_within(&self, x: &[f64], lower: &[f64], upper: &[f64], tol: f64) -> bool {
        let tol = |b: f64| tol * (1.0 + b.abs());
        let mut vals = Vec::new();
        self.graph.eval(x, &mut vals);
        let bounds_ok = (0..x.len()).all(|j| x[j] >= lower[j] - tol(lower[j]) && x[j] <= upper[j] + tol(upper[j]));
        bounds_ok
            && self.rows.iter().all(|r| {
                let v = r.coefs.iter().map(|&(j, a)| a * x[j]).sum::<f64>() + r.node.map_or(0.0, |n| vals[n as usize]);
                v.is_finite() && v >= r.lower - tol(r.lower) && v <= r.upper + tol(r.upper)
            })
            && (self.satisfy || self.objective(x).is_finite())
    }

    /// A local solve with the columns in [lower, upper] (equal bounds fix a
    /// column), from `start` (moved inside the bounds).
    pub fn run(&self, lower: &[f64], upper: &[f64], start: &[f64], deadline: Option<Instant>) -> LocalRun {
        let model = self.model;
        let graph = self.graph;
        let num_cols = model.num_cols();
        let debug = self.debug;
        let mut col_z = vec![None; num_cols];
        let mut z_col = Vec::new();
        let mut fixed = vec![0.0; num_cols];
        for j in 0..num_cols {
            if lower[j] == upper[j] {
                fixed[j] = lower[j];
            } else {
                col_z[j] = Some(z_col.len());
                z_col.push(j);
            }
        }
        let start: Vec<f64> = (0..num_cols)
            .map(|j| if col_z[j].is_some() { start[j].clamp(lower[j], upper[j]) } else { fixed[j] })
            .collect();
        // Roots: the objective's nonlinear part, then each nonlinear row's.
        let mut roots = Vec::new();
        let obj_root = if self.satisfy {
            None
        } else {
            self.obj_node.map(|node| {
                roots.push(node);
                0
            })
        };
        let mut rows = Vec::new();
        for r in &self.rows {
            let free = r.coefs.iter().any(|&(j, a)| a != 0.0 && col_z[j].is_some())
                || r.node.is_some_and(|n| graph.vars_of(n).iter().any(|&j| col_z[j].is_some()));
            if !free {
                let lin: f64 = r.coefs.iter().map(|&(j, a)| a * fixed[j]).sum();
                let v = lin + r.node.map_or(0.0, |n| graph.value(n, &fixed));
                let tol = |b: f64| VIOLATION_TOL * (1.0 + b.abs());
                if !v.is_finite() || v < r.lower - tol(r.lower) || v > r.upper + tol(r.upper) {
                    return LocalRun::Infeasible;
                }
                continue;
            }
            let root = r.node.map(|n| {
                roots.push(n);
                roots.len() - 1
            });
            rows.push(RowDef { coefs: r.coefs.clone(), root, lower: r.lower, upper: r.upper, slack: None });
        }
        if z_col.is_empty() {
            // Every column fixed: the point is the answer, if it is one.
            let ok = self.feasible(&start, lower, upper);
            return LocalRun::Done {
                status: ipm::Status::Optimal,
                x: ok.then(|| start.clone()),
                best: ok.then(|| start.clone()),
            };
        }
        let ev = Evaluator::new(graph, &roots, num_cols);
        let mut std = Standard {
            ev,
            num_cols,
            col_z,
            z_col,
            fixed,
            obj: if self.satisfy {
                Vec::new()
            } else {
                model.obj.iter().enumerate().filter(|c| *c.1 != 0.0).map(|(j, &a)| (j, a)).collect()
            },
            obj_offset: if self.satisfy { 0.0 } else { model.obj_offset },
            obj_root,
            obj_sign: self.sign,
            obj_scale: 1.0,
            rows,
            row_scale: Vec::new(),
            n: 0,
            lower: Vec::new(),
            upper: Vec::new(),
            roots: roots.len(),
        };
        // Scaling at the starting point.
        let (go, gr) = std.gradients(&start);
        let scale = |g: &[f64]| {
            let big = g.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            if big.is_finite() && big > MAX_GRADIENT { (MAX_GRADIENT / big).max(MIN_SCALE) } else { 1.0 }
        };
        std.obj_scale = scale(&go);
        std.row_scale = gr.iter().map(|g| scale(g)).collect();
        // Variables: the free columns, then a slack per inequality row.
        let (_, g0) = std.values(&start);
        let mut zl: Vec<f64> = std.z_col.iter().map(|&j| lower[j]).collect();
        let mut zu: Vec<f64> = std.z_col.iter().map(|&j| upper[j]).collect();
        let mut z0: Vec<f64> = std.z_col.iter().map(|&j| start[j]).collect();
        for r in 0..std.rows.len() {
            let row = &mut std.rows[r];
            if row.lower != row.upper {
                let d = std.row_scale[r];
                row.slack = Some(zl.len());
                zl.push(d * row.lower);
                zu.push(d * row.upper);
                z0.push(if g0[r].is_finite() { (d * g0[r]).clamp(d * row.lower, d * row.upper) } else { 0.0 });
            }
        }
        std.n = zl.len();
        std.lower = zl;
        std.upper = zu;
        if debug {
            eprintln!(
                "nlp: {} nonlinear functions, Hessian of the Lagrangian with {} entries in {} directions",
                std.roots,
                std.ev.hessian_pattern().len(),
                std.ev.hessian_passes()
            );
        }
        let opts = ipm::Options { deadline, debug, ..ipm::Options::default() };
        let r = ipm::solve(&mut std, &z0, &opts);
        if r.status == ipm::Status::BadStart {
            return LocalRun::BadStart;
        }
        if debug {
            eprintln!("nlp: {:?} after {} iterations", r.status, r.iterations);
        }
        // In the model's columns, checked against the rows and bounds (within
        // VIOLATION_TOL). The interior point method relaxes the bounds very
        // slightly; the point is moved back onto them unless that breaks a
        // row (with bounds in the thousands, the relaxation is not negligible).
        let point = |z: &[f64]| -> Option<Vec<f64>> {
            let x = std.columns(z);
            let clamped: Vec<f64> = x.iter().enumerate().map(|(j, v)| v.clamp(lower[j], upper[j])).collect();
            if self.feasible(&clamped, lower, upper) {
                Some(clamped)
            } else {
                self.feasible(&x, lower, upper).then_some(x)
            }
        };
        let x = matches!(r.status, ipm::Status::Optimal | ipm::Status::Acceptable).then(|| point(&r.z)).flatten();
        let best = r.best_feasible.as_ref().and_then(|(_, z)| point(z));
        LocalRun::Done { status: r.status, x, best }
    }
}

/// What bounding the problem over a box found.
pub enum BoxBound {
    /// Propagation or the relaxation proves that no point of the box
    /// satisfies the rows (within VIOLATION_TOL).
    Infeasible,
    Bounded {
        /// The columns' bounds implied by the constraints.
        implied: Vec<Interval>,
        /// A lower bound on the (minimization) objective, from the
        /// relaxation, with the relaxation's solution in the model's
        /// columns and its value for every node of `Bounding::graph`.
        lower: Option<(f64, Vec<f64>, Vec<Option<f64>>)>,
    },
}

/// The problem for bounding: each row's whole left side, and the
/// objective (minimization), as roots of a copy of the graph.
pub struct Bounding {
    pub graph: Graph,
    pub targets: Vec<(graph::Node, Interval)>,
    pub objective: Option<graph::Node>,
    pub num_cols: usize,
    /// Integer columns, whose implied bounds are rounded inward.
    pub integer: Vec<bool>,
}

impl Bounding {
    pub fn new(local: &Local) -> Self {
        let model = local.model;
        let mut g = local.graph.clone();
        let tol = |b: f64| VIOLATION_TOL * (1.0 + b.abs());
        let mut targets = Vec::new();
        for row in &local.rows {
            let mut terms: Vec<(f64, graph::Node)> = row.coefs.iter().map(|&(j, a)| (a, g.var(j))).collect();
            if let Some(n) = row.node {
                terms.push((1.0, n));
            }
            let root = g.sum(0.0, &terms);
            targets.push((root, Interval::new(row.lower - tol(row.lower), row.upper + tol(row.upper))));
        }
        let sign = local.sign;
        let objective = (!local.satisfy).then(|| {
            let mut terms: Vec<(f64, graph::Node)> =
                model.obj.iter().enumerate().filter(|c| *c.1 != 0.0).map(|(j, &a)| (sign * a, g.var(j))).collect();
            if let Some(node) = local.obj_node {
                terms.push((sign, node));
            }
            g.sum(sign * model.obj_offset, &terms)
        });
        Bounding { graph: g, targets, objective, num_cols: model.num_cols(), integer: model.col_integer.clone() }
    }

    /// Bounds over the box, with the objective at most `cutoff` if given
    /// (and the relaxation tightened at the point `hint`, in the model's
    /// columns).
    pub fn bound(
        &self,
        boxed: &[Interval],
        cutoff: Option<f64>,
        hint: Option<&[f64]>,
        deadline: Option<Instant>,
    ) -> BoxBound {
        let g = &self.graph;
        let n = self.num_cols;
        let mut targets = self.targets.clone();
        if let (Some(c), Some(o)) = (cutoff, self.objective) {
            targets.push((o, Interval::new(f64::NEG_INFINITY, c)));
        }
        let Some(iv) = fbbt::propagate(g, boxed, &targets) else { return BoxBound::Infeasible };
        let mut implied = boxed.to_vec();
        for (k, i) in iv.iter().enumerate() {
            if let graph::Op::Var(j) = g.op(k as graph::Node) {
                implied[*j] = *i;
            }
        }
        for j in 0..n {
            if self.integer[j] {
                let i = implied[j];
                implied[j] = Interval::new((i.lo - 1e-9).ceil(), (i.hi + 1e-9).floor());
                if implied[j].is_empty() {
                    return BoxBound::Infeasible;
                }
            }
        }
        // The relaxation has the rows but not the cutoff: its bound is
        // compared with the incumbent instead (with the cutoff as a row, an
        // LP at the optimum is infeasible by a margin too thin to confirm).
        let targets = &self.targets;
        let mut roots: Vec<graph::Node> = targets.iter().map(|t| t.0).collect();
        roots.extend(self.objective);
        let Some(&first) = roots.first() else { return BoxBound::Bounded { implied, lower: None } };
        let mut hints: Vec<Vec<f64>> = Vec::new();
        if let Some(x) = hint {
            let mut vals = Vec::new();
            g.eval(x, &mut vals);
            hints.push(vals);
        }
        // Cutting-plane rounds: tangents at the last relaxation's solution
        // tighten convex terms, until the bound stops improving.
        let mut lower: Option<(f64, Vec<f64>, Vec<Option<f64>>)> = None;
        for round in 0..=CUT_ROUNDS {
            let mut r = relax::build(g, &iv, n, &implied, &roots, &hints);
            for &(node, t) in targets {
                r.rows.push((vec![(r.node_col[node as usize].unwrap(), 1.0)], t.lo, t.hi));
            }
            let (b, x) = match relax::lower_bound(&r, self.objective.unwrap_or(first), deadline) {
                relax::Bound::Infeasible => return BoxBound::Infeasible,
                relax::Bound::Lower(b, x) => (if self.objective.is_some() { b } else { 0.0 }, x),
                relax::Bound::Unknown => break,
            };
            let nodes: Vec<Option<f64>> = r.node_col.iter().map(|c| c.map(|c| x[c])).collect();
            let improved = lower.as_ref().is_none_or(|l| b > l.0 + 1e-9 * (1.0 + l.0.abs()));
            if !improved {
                break;
            }
            hints.push(nodes.iter().map(|v| v.unwrap_or(f64::NAN)).collect());
            lower = Some((b, x[..n].to_vec(), nodes));
            if self.objective.is_none() || round == CUT_ROUNDS {
                break;
            }
        }
        BoxBound::Bounded { implied, lower }
    }
}

/// Cutting-plane rounds per box, after the first relaxation.
const CUT_ROUNDS: usize = 3;

/// The model's bounds as intervals.
pub fn model_box(model: &LinearModel) -> Vec<Interval> {
    (0..model.num_cols()).map(|j| Interval::new(model.col_lower[j], model.col_upper[j])).collect()
}

/// The starting point: `start` values (0 by default) in the model's
/// bounds, moved inside the bounds that the constraints imply (such as
/// x > 0 under ln(x)).
pub fn starting_point(model: &LinearModel, implied: &[Interval]) -> Vec<f64> {
    (0..model.num_cols())
        .map(|j| {
            let (l, u) = (model.col_lower[j], model.col_upper[j]);
            let mut s = model.col_start.get(j).copied().flatten().unwrap_or(0.0).clamp(l, u);
            let i = implied[j];
            let push = |b: f64| 1e-2 * b.abs().max(1.0);
            let width = if i.width().is_finite() { 1e-2 * i.width() } else { INFINITY };
            if i.lo > l && s < i.lo + push(i.lo).min(width) {
                s = i.lo + push(i.lo).min(width);
            }
            if i.hi < u && s > i.hi - push(i.hi).min(width) {
                s = i.hi - push(i.hi).min(width);
            }
            s.clamp(l, u)
        })
        .collect()
}

pub const BAD_START: &str = "the nonlinear functions (or their derivatives) can't be evaluated at the starting \
                             point, for example ln or sqrt of a number <= 0, or a division by 0: give the \
                             variables `start` values where the functions are defined, or bounds that keep them \
                             there";

/// Solves a model with continuous variables (and nonlinear terms, or not)
/// locally. It is an error if the functions can't be evaluated at the start.
pub fn solve(model: &LinearModel, deadline: Option<Instant>) -> Result<Outcome, String> {
    let debug = std::env::var_os("OPTIMIZAM_DEBUG").is_some();
    let num_cols = model.num_cols();
    if (0..num_cols).any(|j| model.col_lower[j] > model.col_upper[j]) {
        return Ok(Outcome::Infeasible);
    }
    let empty = Graph::new();
    let graph = model.nonlinear.as_ref().map_or(&empty, |nl| &nl.graph);
    let local = Local::new(model, graph);
    // Bounds over the box: propagation and a linear relaxation can prove
    // that no point is feasible, imply bounds for the start, and bound the
    // optimum from below.
    let boxed = model_box(model);
    let bounding = Bounding::new(&local);
    let (infeasible, implied, lower) = match bounding.bound(&boxed, None, None, deadline) {
        BoxBound::Infeasible => (true, boxed.clone(), None),
        BoxBound::Bounded { implied, lower } => (false, implied, lower.map(|l| l.0)),
    };
    if debug {
        if infeasible {
            eprintln!("nlp: bound propagation or the relaxation proves the problem infeasible");
        }
        eprintln!("nlp: relaxation bound {:?}", lower.map(|b| local.sign * b));
    }
    let start = starting_point(model, &implied);
    let (status, x, best) = match local.run(&model.col_lower, &model.col_upper, &start, deadline) {
        LocalRun::BadStart if infeasible => return Ok(Outcome::Infeasible),
        LocalRun::BadStart => return Err(BAD_START.into()),
        LocalRun::Infeasible => return Ok(Outcome::Infeasible),
        LocalRun::Done { status, x, best } => (status, x, best),
    };
    let satisfy = local.satisfy;
    let proven = satisfy || model.nonlinear.is_none();
    let fallback = |best: Option<Vec<f64>>| match best {
        Some(x) if satisfy => Outcome::Optimal { x, proven: true },
        Some(x) => Outcome::Feasible(x),
        None => Outcome::NoSolution,
    };
    let outcome = match (status, x) {
        (ipm::Status::Optimal | ipm::Status::Acceptable, Some(x)) => {
            // The relaxation's bound can prove a local optimum global.
            let f = local.objective(&x);
            let tol = GAP_TOL * (1.0 + f.abs());
            // A bound above a feasible point's objective (or a proof of
            // infeasibility) is a numerical failure: ignore it.
            let consistent = !infeasible && lower.is_none_or(|b| b <= f + tol);
            if debug && !consistent {
                eprintln!("nlp: warning: the relaxation contradicts a feasible point (ignored)");
            }
            let closed = consistent && lower.is_some_and(|b| b >= f - tol);
            if debug && closed {
                eprintln!("nlp: the relaxation bound proves the local optimum global");
            }
            Outcome::Optimal { x, proven: proven || closed }
        }
        (ipm::Status::LocallyInfeasible, _) => match fallback(best) {
            Outcome::NoSolution => Outcome::LocallyInfeasible,
            other => other,
        },
        _ => fallback(best),
    };
    // Without a feasible point, a proof of infeasibility settles it.
    Ok(match outcome {
        Outcome::NoSolution | Outcome::LocallyInfeasible if infeasible => Outcome::Infeasible,
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::graph::Evaluator;
    use crate::model;
    use crate::problem::Problem;
    use serde_json::Value;

    use crate::result::Status;
    use crate::solve::{Engine, solve};

    /// Solves with auto (global for nonlinear problems).
    fn run(json: &str) -> Result<(Status, Vec<f64>, f64), String> {
        run_with(json, Engine::Auto)
    }

    fn run_with(json: &str, engine: Engine) -> Result<(Status, Vec<f64>, f64), String> {
        let m = model::build(&Problem::from_json(json)?)?;
        let out = solve(&m, None, engine)?;
        let x = out.solutions.first().cloned().unwrap_or_default();
        let obj = if x.is_empty() { f64::NAN } else { m.objective_value(&x) };
        Ok((out.status, x, obj))
    }

    fn problem(vars: &str, constraints: &str, solve: &str) -> String {
        format!(r#"{{"version": "1.2", "variables": [{vars}], "constraints": [{constraints}], "solve": {solve}}}"#)
    }

    #[test]
    fn solves_through_the_model() {
        // HS071, with the inequality as written (a slack) and start values.
        let hs071 = r#"{"version": "1.2", "sets": {"I": {"from": 1, "to": 4}},
            "parameters": {"x0": {"index": ["I"], "values": {"1": 1, "2": 5, "3": 5, "4": 1}}},
            "variables": [{"name": "x", "type": "continuous", "index": ["I"], "lower": 1, "upper": 5, "start": "x0"}],
            "constraints": [{"expr": "x[1] * x[2] * x[3] * x[4] >= 25"}, {"expr": "sum(i in I) x[i]^2 == 40"}],
            "solve": {"method": "minimize", "objective": "x[1] * x[4] * (x[1] + x[2] + x[3]) + x[3]"}}"#;
        // Locally (the interior point method alone), then proven global.
        let (status, _, obj) = run_with(hs071, Engine::Nlp).unwrap();
        assert_eq!(status, Status::LocallyOptimal);
        assert!((obj - 17.0140173).abs() < 1e-6, "{obj}");
        let (status, _, obj) = run(hs071).unwrap();
        assert_eq!(status, Status::Complete);
        assert!((obj - 17.0140173).abs() < 1e-6, "{obj}");
        // Maximize, with a fixed variable and a nonlinear objective only.
        let (status, x, obj) = run(&problem(
            r#"{"name": "x", "type": "continuous", "lower": 0, "upper": 3},
               {"name": "k", "type": "continuous", "lower": 2, "upper": 2}"#,
            "",
            r#"{"method": "maximize", "objective": "k * ln(1 + x) - x^2 / 4"}"#,
        ))
        .unwrap();
        // d/dx: 2 / (1 + x) = x / 2, so x^2 + x - 4 = 0.
        let want = (-1.0 + 17f64.sqrt()) / 2.0;
        assert_eq!(status, Status::Complete);
        assert!((x[0] - want).abs() < 1e-6 && x[1] == 2.0, "{x:?}");
        assert!((obj - (2.0 * (1.0 + want).ln() - want * want / 4.0)).abs() < 1e-8);
        // Satisfy: a feasible point is proven by itself.
        let (status, x, _) = run(&problem(
            r#"{"name": "x", "type": "continuous", "start": 1}, {"name": "y", "type": "continuous"}"#,
            r#"{"expr": "x^2 + y^2 == 4"}, {"expr": "y == x^2"}"#,
            r#"{"method": "satisfy"}"#,
        ))
        .unwrap();
        assert_eq!(status, Status::Complete);
        assert!((x[0] * x[0] + x[1] * x[1] - 4.0).abs() < 1e-6 && (x[1] - x[0] * x[0]).abs() < 1e-6, "{x:?}");
    }

    #[test]
    fn infeasibility_and_errors() {
        // A disk and a half-plane that miss each other: proven by bound
        // propagation (x, y in [-1, 1]).
        let (status, _, _) = run(&problem(
            r#"{"name": "x", "type": "continuous"}, {"name": "y", "type": "continuous"}"#,
            r#"{"expr": "x^2 + y^2 <= 1"}, {"expr": "x + y >= 3"}"#,
            r#"{"method": "minimize", "objective": "x * y"}"#,
        ))
        .unwrap();
        assert_eq!(status, Status::Infeasible);
        // x * y >= 5 on [0, 2]^2: propagation narrows it, the relaxation
        // proves it.
        let (status, _, _) = run(&problem(
            r#"{"name": "x", "type": "continuous", "lower": 0, "upper": 2},
               {"name": "y", "type": "continuous", "lower": 0, "upper": 2}"#,
            r#"{"expr": "x * y >= 5"}"#,
            r#"{"method": "minimize", "objective": "x + y"}"#,
        ))
        .unwrap();
        assert_eq!(status, Status::Infeasible);
        // Nonconvex, really infeasible, and not provably so from the box:
        // a circle x^2 + y^2 == 4 and a hyperbola x * y == 3 (x y <= 2 on
        // the circle) only show local infeasibility.
        let (status, _, _) = run(&problem(
            r#"{"name": "x", "type": "continuous", "start": 1}, {"name": "y", "type": "continuous", "start": 2}"#,
            r#"{"expr": "x^2 + y^2 == 4"}, {"expr": "x * y == 3"}"#,
            r#"{"method": "satisfy"}"#,
        ))
        .unwrap();
        assert!(matches!(status, Status::LocallyInfeasible | Status::Infeasible), "{status:?}");
        // Constant rows are checked exactly: proven infeasible.
        let (status, _, _) = run(&problem(
            r#"{"name": "x", "type": "continuous", "lower": 1, "upper": 1}, {"name": "y", "type": "continuous"}"#,
            r#"{"expr": "exp(x) <= 2"}, {"expr": "y^2 <= 1"}"#,
            r#"{"method": "minimize", "objective": "y"}"#,
        ))
        .unwrap();
        assert_eq!(status, Status::Infeasible);
        // ln(x) at the default start x = 0: the implied bound x >= 0 moves
        // the start inside. The relaxation's bound (tangents of the convex
        // -ln) proves x = 1 optimal.
        let (status, x, _) = run(&problem(
            r#"{"name": "x", "type": "continuous", "upper": 10}"#,
            "",
            r#"{"method": "minimize", "objective": "x - ln(x)"}"#,
        ))
        .unwrap();
        assert!(matches!(status, Status::LocallyOptimal | Status::Complete), "{status:?}");
        assert!((x[0] - 1.0).abs() < 1e-6, "{x:?}");
        // ln(x * y) at (0, 0): nothing implies a better start, so the local
        // engine fails; the global search starts elsewhere too, and finds
        // the optimum 1 - ln(1/2) at x = y = +-1/sqrt(2) (unbounded
        // variables: it can't prove it within the node limit).
        let p = problem(
            r#"{"name": "x", "type": "continuous"}, {"name": "y", "type": "continuous"}"#,
            "",
            r#"{"method": "minimize", "objective": "x^2 + y^2 - ln(x * y)"}"#,
        );
        let e = run_with(&p, Engine::Nlp).unwrap_err();
        assert!(e.contains("starting point"), "{e}");
        let (status, _, obj) = run(&p).unwrap();
        assert!(matches!(status, Status::Complete | Status::LocallyOptimal), "{status:?}");
        assert!((obj - (1.0 - 0.5f64.ln())).abs() < 1e-6, "{obj}");
        // Integer variables with nonlinear terms: only the global search.
        let p = problem(
            r#"{"name": "x", "type": "integer", "lower": 0, "upper": 3}"#,
            "",
            r#"{"method": "minimize", "objective": "(x - 1.5)^2 + 0.1 * x"}"#,
        );
        let e = run_with(&p, Engine::Nlp).unwrap_err();
        assert!(e.contains("mixed-integer nonlinear"), "{e}");
        let (status, x, _) = run(&p).unwrap();
        assert_eq!((status, x), (Status::Complete, vec![1.0]));
        // The other engines refuse nonlinear problems.
        let m = model::build(
            &Problem::from_json(&problem(
                r#"{"name": "x", "type": "continuous"}"#,
                "",
                r#"{"method": "minimize", "objective": "x^2"}"#,
            ))
            .unwrap(),
        )
        .unwrap();
        let e = solve(&m, None, Engine::Mip).err().unwrap();
        assert!(e.contains("can't solve nonlinear problems"), "{e}");
    }

    /// The interior point method on a linear program agrees with the
    /// simplex method, and reports it proven (convex).
    #[test]
    fn linear_programs_with_the_nlp_engine() {
        let json = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/production.json")).unwrap();
        let m = model::build(&Problem::from_json(&json).unwrap()).unwrap();
        let simplex = solve(&m, None, Engine::Mip).unwrap();
        let ipm = solve(&m, None, Engine::Nlp).unwrap();
        assert_eq!(ipm.status, Status::Complete);
        let (a, b) = (m.objective_value(&simplex.solutions[0]), m.objective_value(&ipm.solutions[0]));
        assert!((a - b).abs() <= 1e-6 * a.abs().max(1.0), "{a} vs {b}");
    }

    /// Expressions in the problem format's grammar, built through the model
    /// (parser, affine/nonlinear split, graph), have the value, gradient, and
    /// Hessian that SymPy computed (tests/data/sympy_derivatives.json, from
    /// tools/sympy_derivatives.py).
    #[test]
    fn derivatives_match_sympy() {
        let data: Value = serde_json::from_str(include_str!("../../tests/data/sympy_derivatives.json")).unwrap();
        let cases = data["cases"].as_array().unwrap();
        assert!(cases.len() >= 50);
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * (1.0 + a.abs().max(b.abs()));
        let numbers = |v: &Value| -> Vec<f64> { v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect() };
        for (k, case) in cases.iter().enumerate() {
            let text = case["expr"].as_str().unwrap();
            let json = format!(
                r#"{{"version": "1.2", "variables": [{{"name": "x1", "type": "continuous"}},
                    {{"name": "x2", "type": "continuous"}}, {{"name": "x3", "type": "continuous"}}],
                    "constraints": [], "solve": {{"method": "minimize", "objective": {}}}}}"#,
                serde_json::to_string(text).unwrap()
            );
            let m = model::build(&Problem::from_json(&json).unwrap()).unwrap_or_else(|e| panic!("case {k}: {e}"));
            let x = numbers(&case["point"]);
            let what = format!("case {k}: {text}");
            assert!(close(m.objective_value(&x), case["value"].as_f64().unwrap()), "{what}: value");
            let mut grad = m.obj.clone();
            let mut hess = vec![vec![0.0; 3]; 3];
            if let Some(nl) = &m.nonlinear {
                let mut ev = Evaluator::new(&nl.graph, &[nl.objective.unwrap()], 3);
                ev.eval(&x);
                for (j, v) in ev.gradient(0) {
                    grad[j] += v;
                }
                let values = ev.hessian(&[1.0]);
                for (&(i, j), v) in ev.hessian_pattern().iter().zip(values) {
                    hess[i][j] = v;
                    hess[j][i] = v;
                }
            }
            for (j, want) in numbers(&case["gradient"]).into_iter().enumerate() {
                assert!(close(grad[j], want), "{what}: d/dx{} = {} vs {want}", j + 1, grad[j]);
            }
            for (i, row) in case["hessian"].as_array().unwrap().iter().enumerate() {
                for (j, want) in numbers(row).into_iter().enumerate() {
                    assert!(close(hess[i][j], want), "{what}: H[{i}][{j}] = {} vs {want}", hess[i][j]);
                }
            }
        }
    }
}
