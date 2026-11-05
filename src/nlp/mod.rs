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

/// A model row: coefficients, nonlinear node, lower and upper bounds.
type RowSource = (Vec<(usize, f64)>, Option<graph::Node>, f64, f64);

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

/// What bounding the problem over its box found.
struct Bounds {
    /// Propagation or the relaxation proves that no point satisfies the
    /// rows (within VIOLATION_TOL). The local solve runs anyway: a feasible
    /// point it finds would override this (a numerical failure).
    infeasible: bool,
    /// The columns' bounds implied by the constraints.
    implied: Vec<Interval>,
    /// A lower bound on the (minimization) objective, from the relaxation.
    lower: Option<f64>,
}

/// Propagates bounds and solves a linear relaxation over the model's box.
#[allow(clippy::too_many_arguments)]
fn bound(
    model: &LinearModel,
    graph: &Graph,
    rows: &[RowDef],
    roots: &[graph::Node],
    obj_node: Option<graph::Node>,
    sign: f64,
    satisfy: bool,
    deadline: Option<Instant>,
) -> Bounds {
    let n = model.num_cols();
    let mut g = graph.clone();
    let boxed: Vec<Interval> = (0..n).map(|j| Interval::new(model.col_lower[j], model.col_upper[j])).collect();
    let tol = |b: f64| VIOLATION_TOL * (1.0 + b.abs());
    // Each row's whole left side (affine part plus nonlinear part) as a root.
    let mut targets = Vec::new();
    for row in rows {
        let mut terms: Vec<(f64, graph::Node)> = row.coefs.iter().map(|&(j, a)| (a, g.var(j))).collect();
        if let Some(k) = row.root {
            terms.push((1.0, roots[k]));
        }
        let root = g.sum(0.0, &terms);
        targets.push((root, Interval::new(row.lower - tol(row.lower), row.upper + tol(row.upper))));
    }
    let objective = (!satisfy).then(|| {
        let mut terms: Vec<(f64, graph::Node)> =
            model.obj.iter().enumerate().filter(|c| *c.1 != 0.0).map(|(j, &a)| (sign * a, g.var(j))).collect();
        if let Some(node) = obj_node {
            terms.push((sign, node));
        }
        g.sum(sign * model.obj_offset, &terms)
    });
    let Some(iv) = fbbt::propagate(&g, &boxed, &targets) else {
        return Bounds { infeasible: true, implied: boxed, lower: None };
    };
    let mut implied = boxed.clone();
    for (k, i) in iv.iter().enumerate() {
        if let graph::Op::Var(j) = g.op(k as graph::Node) {
            implied[*j] = *i;
        }
    }
    let mut lower = None;
    let mut relax_roots: Vec<graph::Node> = targets.iter().map(|t| t.0).collect();
    relax_roots.extend(objective);
    if let Some(&first) = relax_roots.first() {
        let mut r = relax::build(&g, &iv, n, &implied, &relax_roots);
        for &(node, t) in &targets {
            r.rows.push((vec![(r.node_col[node as usize].unwrap(), 1.0)], t.lo, t.hi));
        }
        match relax::lower_bound(&r, objective.unwrap_or(first), deadline) {
            relax::Bound::Infeasible => return Bounds { infeasible: true, implied, lower: None },
            relax::Bound::Lower(b) if objective.is_some() => lower = Some(b),
            _ => {}
        }
    }
    Bounds { infeasible: false, implied, lower }
}

/// Solves a model with continuous variables (and nonlinear terms, or not).
/// It is an error if the functions can't be evaluated at the start.
pub fn solve(model: &LinearModel, deadline: Option<Instant>) -> Result<Outcome, String> {
    let debug = std::env::var_os("OPTIMIZAM_DEBUG").is_some();
    let num_cols = model.num_cols();
    if (0..num_cols).any(|j| model.col_lower[j] > model.col_upper[j]) {
        return Ok(Outcome::Infeasible);
    }
    let empty = Graph::new();
    let (graph, nl_rows, obj_node) = match &model.nonlinear {
        Some(nl) => (&nl.graph, nl.rows.as_slice(), nl.objective),
        None => (&empty, &[][..], None),
    };
    // Roots: the objective's nonlinear part, then each nonlinear row's.
    let mut roots = Vec::new();
    let obj_root = obj_node.map(|node| {
        roots.push(node);
        0
    });
    // Fixed columns are constants.
    let mut col_z = vec![None; num_cols];
    let mut z_col = Vec::new();
    let mut fixed = vec![0.0; num_cols];
    for j in 0..num_cols {
        let (l, u) = (model.col_lower[j], model.col_upper[j]);
        if l == u {
            fixed[j] = l;
        } else {
            col_z[j] = Some(z_col.len());
            z_col.push(j);
        }
    }
    let start: Vec<f64> = (0..num_cols)
        .map(|j| {
            let s = model.col_start.get(j).copied().flatten().unwrap_or(0.0);
            s.clamp(model.col_lower[j], model.col_upper[j])
        })
        .collect();
    let mut start = (0..num_cols).map(|j| if col_z[j].is_some() { start[j] } else { fixed[j] }).collect::<Vec<_>>();
    // Rows, with the constant ones checked now.
    // (coefs, nonlinear node, lower, upper) of every row.
    let mut defs: Vec<RowSource> = Vec::new();
    for r in &model.rows {
        defs.push((r.coefs.clone(), None, r.lower, r.upper));
    }
    for r in nl_rows {
        defs.push((r.coefs.clone(), Some(r.node), r.lower, r.upper));
    }
    let mut rows = Vec::new();
    for (coefs, node, lower, upper) in defs {
        if lower == f64::NEG_INFINITY && upper == f64::INFINITY {
            continue;
        }
        let free = coefs.iter().any(|&(j, a)| a != 0.0 && col_z[j].is_some())
            || node.is_some_and(|n| graph.vars_of(n).iter().any(|&j| col_z[j].is_some()));
        if !free {
            let lin: f64 = coefs.iter().map(|&(j, a)| a * fixed[j]).sum();
            let v = lin + node.map_or(0.0, |n| graph.value(n, &fixed));
            let tol = VIOLATION_TOL * (1.0 + v.abs());
            if !v.is_finite() || v < lower - tol || v > upper + tol {
                return Ok(Outcome::Infeasible);
            }
            continue;
        }
        let root = node.map(|n| {
            roots.push(n);
            roots.len() - 1
        });
        rows.push(RowDef { coefs, root, lower, upper, slack: None });
    }
    let obj_sign = match model.sense {
        Sense::Maximize => -1.0,
        _ => 1.0,
    };
    let satisfy = model.sense == Sense::Satisfy;
    // Bounds over the box: propagation and a linear relaxation can prove
    // that no point is feasible, imply bounds for the start, and bound the
    // optimum from below.
    let bounds = bound(model, graph, &rows, &roots, obj_node, obj_sign, satisfy, deadline);
    if debug {
        if bounds.infeasible {
            eprintln!("nlp: bound propagation or the relaxation proves the problem infeasible");
        }
        eprintln!("nlp: relaxation bound {:?}", bounds.lower.map(|b| obj_sign * b));
    }
    // A start outside the implied bounds (such as x = 0 under ln(x)) moves
    // inside them.
    for j in 0..num_cols {
        if col_z[j].is_none() {
            continue;
        }
        let i = bounds.implied[j];
        let push = |b: f64| 1e-2 * b.abs().max(1.0);
        let width = if i.width().is_finite() { 1e-2 * i.width() } else { INFINITY };
        if i.lo > model.col_lower[j] && start[j] < i.lo + push(i.lo).min(width) {
            start[j] = i.lo + push(i.lo).min(width);
        }
        if i.hi < model.col_upper[j] && start[j] > i.hi - push(i.hi).min(width) {
            start[j] = i.hi - push(i.hi).min(width);
        }
    }
    let ev = Evaluator::new(graph, &roots, num_cols);
    let mut std = Standard {
        ev,
        num_cols,
        col_z,
        z_col,
        fixed,
        obj: if satisfy {
            Vec::new()
        } else {
            model.obj.iter().enumerate().filter(|c| *c.1 != 0.0).map(|(j, &a)| (j, a)).collect()
        },
        obj_offset: if satisfy { 0.0 } else { model.obj_offset },
        obj_root: if satisfy { None } else { obj_root },
        obj_sign,
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
    let mut lower: Vec<f64> = std.z_col.iter().map(|&j| model.col_lower[j]).collect();
    let mut upper: Vec<f64> = std.z_col.iter().map(|&j| model.col_upper[j]).collect();
    let mut z0: Vec<f64> = std.z_col.iter().map(|&j| start[j]).collect();
    for r in 0..std.rows.len() {
        let row = &mut std.rows[r];
        if row.lower != row.upper {
            let d = std.row_scale[r];
            row.slack = Some(lower.len());
            lower.push(d * row.lower);
            upper.push(d * row.upper);
            z0.push(if g0[r].is_finite() { (d * g0[r]).clamp(d * row.lower, d * row.upper) } else { 0.0 });
        }
    }
    std.n = lower.len();
    std.lower = lower;
    std.upper = upper;
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
    if r.status == ipm::Status::BadStart && bounds.infeasible {
        return Ok(Outcome::Infeasible);
    }
    if r.status == ipm::Status::BadStart {
        return Err("the nonlinear functions (or their derivatives) can't be evaluated at the starting point, \
                    for example ln or sqrt of a number <= 0, or a division by 0: give the variables `start` \
                    values where the functions are defined, or bounds that keep them there"
            .into());
    }
    if debug {
        eprintln!("nlp: {:?} after {} iterations", r.status, r.iterations);
    }
    // In the model's columns, checked against the original rows and bounds
    // (within VIOLATION_TOL). The interior point method relaxes the bounds
    // very slightly; the point is moved back onto them unless that breaks a
    // row (with bounds in the thousands, the relaxation is not negligible).
    let tol = |b: f64| VIOLATION_TOL * (1.0 + b.abs());
    let rows_ok = |std: &mut Standard, x: &[f64]| -> bool {
        let (_, g) = std.values(x);
        std.rows.iter().zip(&g).enumerate().all(|(r, (row, &v))| {
            let ok = v.is_finite() && v >= row.lower - tol(row.lower) && v <= row.upper + tol(row.upper);
            if !ok && debug {
                eprintln!("nlp: row {r} is {v:e}, outside [{:e}, {:e}]", row.lower, row.upper);
            }
            ok
        })
    };
    let feasible = |std: &mut Standard, z: &[f64]| -> Option<Vec<f64>> {
        let x = std.columns(z);
        let clamped: Vec<f64> =
            x.iter().enumerate().map(|(j, v)| v.clamp(model.col_lower[j], model.col_upper[j])).collect();
        if rows_ok(std, &clamped) {
            return Some(clamped);
        }
        let in_bounds = (0..x.len()).all(|j| {
            x[j] >= model.col_lower[j] - tol(model.col_lower[j]) && x[j] <= model.col_upper[j] + tol(model.col_upper[j])
        });
        (in_bounds && rows_ok(std, &x)).then_some(x)
    };
    let proven = satisfy || model.nonlinear.is_none();
    let fallback = |std: &mut Standard| match r.best_feasible.as_ref().and_then(|(_, z)| feasible(std, z)) {
        Some(x) if satisfy => Outcome::Optimal { x, proven: true },
        Some(x) => Outcome::Feasible(x),
        None => Outcome::NoSolution,
    };
    let outcome = match r.status {
        ipm::Status::Optimal | ipm::Status::Acceptable => match feasible(&mut std, &r.z) {
            Some(x) => {
                // The relaxation's bound can prove a local optimum global.
                let f = obj_sign * model.objective_value(&x);
                let tol = GAP_TOL * (1.0 + f.abs());
                // A bound above a feasible point's objective (or a proof
                // of infeasibility) is a numerical failure: ignore it.
                let consistent = !bounds.infeasible && bounds.lower.is_none_or(|b| b <= f + tol);
                if debug && !consistent {
                    eprintln!("nlp: warning: the relaxation contradicts a feasible point (ignored)");
                }
                let closed = consistent && bounds.lower.is_some_and(|b| b >= f - tol);
                if debug && closed {
                    eprintln!("nlp: the relaxation bound proves the local optimum global");
                }
                Outcome::Optimal { x, proven: proven || closed }
            }
            None => fallback(&mut std),
        },
        ipm::Status::LocallyInfeasible => match fallback(&mut std) {
            Outcome::NoSolution => Outcome::LocallyInfeasible,
            other => other,
        },
        _ => fallback(&mut std),
    };
    // Without a feasible point, a proof of infeasibility settles it.
    Ok(match outcome {
        Outcome::NoSolution | Outcome::LocallyInfeasible if bounds.infeasible => Outcome::Infeasible,
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

    fn run(json: &str) -> Result<(Status, Vec<f64>, f64), String> {
        let m = model::build(&Problem::from_json(json)?)?;
        let out = solve(&m, None, Engine::Auto)?;
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
        let (status, _, obj) = run(hs071).unwrap();
        assert_eq!(status, Status::LocallyOptimal);
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
        assert_eq!(status, Status::LocallyOptimal);
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
        // ln(x * y) at (0, 0): nothing implies a better start.
        let e = run(&problem(
            r#"{"name": "x", "type": "continuous"}, {"name": "y", "type": "continuous"}"#,
            "",
            r#"{"method": "minimize", "objective": "x^2 + y^2 - ln(x * y)"}"#,
        ))
        .unwrap_err();
        assert!(e.contains("starting point"), "{e}");
        // Integer variables with nonlinear terms: not supported (yet).
        let e = run(&problem(
            r#"{"name": "x", "type": "integer", "lower": 0, "upper": 3}"#,
            "",
            r#"{"method": "minimize", "objective": "(x - 1.5)^2"}"#,
        ))
        .unwrap_err();
        assert!(e.contains("mixed-integer nonlinear"), "{e}");
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
