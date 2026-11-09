//! Outer approximation for convex mixed-integer nonlinear problems (the
//! extended cutting-plane method of Westerlund and Pettersson, with local
//! solves for upper bounds), on top of the MILP engine.
//!
//! When every nonlinear row is convex in the direction it is bounded (a
//! convex function at most something, a concave one at least something) and
//! the objective is convex over the root box, the tangent planes of those
//! functions at any points are valid cuts. So the MILP of the linear rows
//! plus such cuts (with an epigraph variable for a nonlinear objective) is
//! an outer approximation, and its optimum is a lower bound. Each round
//! solves it, adds tangent cuts at its solution for the violated rows (and
//! the objective, if the epigraph variable undercuts it), and runs a local
//! solve with the integers fixed at the solution's values for an incumbent.
//! First, cuts at the continuous relaxation's solutions (LP solves) until
//! its bound stalls, keeping the cuts tight at the end; then MILP rounds.
//! It ends when the bound reaches the incumbent (proven optimal within the
//! gap), when the MILP is infeasible (so is the problem), or when the bound
//! stalls for MAX_STALLED rounds or MAX_ROUNDS pass (the caller then
//! branches as usual).

use super::convexity::{Curvature, curvatures};
use super::interval::Interval;
use super::{Bounding, Local, LocalRun, fbbt};
use crate::lp::{Lp, SparseRow};
use crate::mip::{self, Mip, Settings};
use crate::model::LinearModel;
use std::time::Instant;

const MAX_ROUNDS: usize = 200;
/// MILP rounds without a better bound before giving up.
const MAX_STALLED: usize = 5;
/// Rounds of cuts at the continuous relaxation's solutions, first.
const MAX_LP_ROUNDS: usize = 500;
/// A row counts as satisfied by a MILP solution within this (relative to
/// 1 + |bound|), the feasibility tolerance of the rows.
const ROW_TOL: f64 = 1e-6;
/// Cuts are loosened by this, relative to the size of their terms.
const CUT_TOL: f64 = 1e-9;

pub enum Outcome {
    /// Proven optimal within the gap.
    Optimal(Vec<f64>),
    Infeasible,
    /// The problem isn't convex on the box (no outer approximation).
    NotConvex,
    /// Ran out of rounds or time: the best point found, if any.
    Stopped(Option<(f64, Vec<f64>)>),
}

/// Whether a row with these bounds is convex where the root has this
/// curvature: a convex function at most something, a concave one at least.
fn convex_row(c: Curvature, t: Interval) -> bool {
    match c {
        Curvature::Affine => true,
        Curvature::Convex => t.lo == f64::NEG_INFINITY,
        Curvature::Concave => t.hi == f64::INFINITY,
        Curvature::Unknown => false,
    }
}

pub fn solve(
    model: &LinearModel,
    local: &Local,
    bounding: &Bounding,
    root_box: &[Interval],
    incumbent: Option<(f64, Vec<f64>)>,
    gap: &dyn Fn(f64) -> f64,
    deadline: Option<Instant>,
) -> Outcome {
    let debug = crate::config::debug();
    let g = &bounding.graph;
    let n = model.num_cols();
    let Some(iv) = fbbt::propagate(g, root_box, &bounding.targets) else { return Outcome::Infeasible };
    let curv = curvatures(g, &iv);
    // Rows: the linear ones exactly; the nonlinear ones must be convex.
    let mut linear: Vec<SparseRow> = Vec::new();
    let mut nonlinear: Vec<(super::graph::Node, Interval)> = Vec::new();
    // How many rows each column appears in (for relaxing equalities).
    let mut uses = vec![0usize; n];
    for row in &local.rows {
        for &(j, _) in &row.coefs {
            uses[j] += 1;
        }
        if let Some(node) = row.node {
            for j in g.vars_of(node) {
                uses[j] += 1;
            }
        }
    }
    for (row, &(root, t)) in local.rows.iter().zip(&bounding.targets) {
        let c = curv[root as usize];
        if row.node.is_none() {
            linear.push((row.coefs.clone(), row.lower, row.upper));
        } else if convex_row(c, t) {
            nonlinear.push((root, t));
        } else if let Some(relaxed) = relax_equality(model, local, row, c, t, &uses) {
            // An equality defining a variable that only the objective uses
            // (such as `f(x) - objvar = 0` with `min objvar`): the
            // optimization keeps it tight, so its convex side is enough.
            nonlinear.push((root, relaxed));
        } else {
            if debug {
                crate::config::debug_log!("ecp: a row is {:?} with bounds {t:?}: not convex", curv[root as usize]);
                if let super::graph::Op::Sum(_, terms) = g.op(root) {
                    for &(a, c) in terms {
                        crate::config::debug_log!(
                            "ecp:   {a} * {:?} {:?} in {:?}",
                            g.op(c),
                            curv[c as usize],
                            iv[c as usize]
                        );
                    }
                }
            }
            return Outcome::NotConvex;
        }
    }
    // The objective: linear costs, or an epigraph variable t (column n).
    let objective = bounding.objective;
    let epigraph = match objective {
        Some(o) if curv[o as usize] == Curvature::Affine => None,
        Some(o) if curv[o as usize] == Curvature::Convex => Some(o),
        Some(o) => {
            if debug {
                crate::config::debug_log!("ecp: the objective is {:?}: not convex", curv[o as usize]);
            }
            return Outcome::NotConvex;
        }
        None => None,
    };
    let cols = n + epigraph.is_some() as usize;
    let mut lower: Vec<f64> = root_box.iter().map(|b| b.lo).collect();
    let mut upper: Vec<f64> = root_box.iter().map(|b| b.hi).collect();
    let mut cost = vec![0.0; cols];
    if let Some(o) = epigraph {
        lower.push(iv[o as usize].lo);
        upper.push(f64::INFINITY);
        cost[n] = 1.0;
    } else if !local.satisfy {
        for (j, &c) in model.obj.iter().enumerate() {
            cost[j] = local.sign * c;
        }
    }
    let mut integer = model.col_integer.clone();
    let mut domain = model.col_domain.clone();
    if epigraph.is_some() {
        integer.push(false);
        domain.push(None);
    }
    let mut cuts: Vec<SparseRow> = Vec::new();
    // The tangent cut of `root` (with bounds t) at x: none if not violated
    // by more than the tolerance (with `force`, always).
    let cut_at = |root: super::graph::Node, t: Interval, x: &[f64], epi: bool, force: bool| -> Option<SparseRow> {
        let mut vals = Vec::new();
        g.eval(x, &mut vals);
        let v = vals[root as usize];
        if !v.is_finite() {
            return None;
        }
        let grad = g.gradient_of(root, &vals)?;
        let gx: f64 = grad.iter().map(|&(j, a)| a * x[j]).sum();
        let slack = CUT_TOL * (1.0 + v.abs() + grad.iter().map(|&(j, a)| (a * x[j]).abs()).sum::<f64>());
        let mut coefs: Vec<(usize, f64)> = grad.clone();
        if epi {
            // t >= f(x) + grad (y - x): grad y - t <= grad x - f(x).
            if !force && x[n] >= v - ROW_TOL * (1.0 + v.abs()) {
                return None;
            }
            coefs.push((n, -1.0));
            return Some((coefs, f64::NEG_INFINITY, gx - v + slack));
        }
        let tol = |b: f64| ROW_TOL * (1.0 + b.abs());
        if t.hi.is_finite() && (force || v > t.hi + tol(t.hi)) {
            return Some((coefs, f64::NEG_INFINITY, t.hi - v + gx + slack));
        }
        if t.lo.is_finite() && (force || v < t.lo - tol(t.lo)) {
            return Some((coefs, t.lo - v + gx - slack, f64::INFINITY));
        }
        None
    };
    // Initial cuts at the box's center (bounding the epigraph from below).
    let mut center: Vec<f64> = (0..n).map(|j| mid(lower[j], upper[j])).collect();
    center.push(0.0);
    for &(root, t) in &nonlinear {
        cuts.extend(cut_at(root, t, &center, false, true));
    }
    if let Some(o) = epigraph {
        cuts.extend(cut_at(o, Interval::ENTIRE, &center, true, true));
    }
    // And at the starting incumbent (a local solution).
    if let Some((_, y)) = &incumbent {
        let mut y = y.clone();
        y.push(0.0);
        for &(root, t) in &nonlinear {
            cuts.extend(cut_at(root, t, &y, false, true));
        }
        if let Some(o) = epigraph {
            cuts.extend(cut_at(o, Interval::ENTIRE, &y, true, true));
        }
    }
    // First the continuous relaxation: LP solves (cheap), with cuts at their
    // solutions, until the LP's bound stalls.
    let mut lp_bound = f64::NEG_INFINITY;
    let mut stalls = 0;
    let mut last_lp: Option<Vec<f64>> = None;
    for _ in 0..MAX_LP_ROUNDS {
        if crate::config::expired(deadline) {
            break;
        }
        let rows: Vec<SparseRow> = linear.iter().chain(&cuts).cloned().collect();
        let lp = Lp::new(cost.clone(), lower.clone(), upper.clone(), rows);
        let s = super::relax::solve_scaled(&lp, None, &crate::simplex::Options { deadline, ..Default::default() });
        if s.status != crate::simplex::LpStatus::Optimal {
            break;
        }
        let x = s.col_value;
        last_lp = Some(x.clone());
        let before = cuts.len();
        for &(root, t) in &nonlinear {
            cuts.extend(cut_at(root, t, &x, false, false));
        }
        if let Some(o) = epigraph {
            cuts.extend(cut_at(o, Interval::ENTIRE, &x, true, false));
        }
        let improved = s.objective > lp_bound + 1e-6 * (1.0 + lp_bound.abs());
        lp_bound = lp_bound.max(s.objective);
        stalls = if improved { 0 } else { stalls + 1 };
        if cuts.len() == before || stalls >= 3 {
            break;
        }
    }
    // Only the cuts tight at the last LP solution go on to the MILPs (the
    // others don't hold the bound up, and every MILP would carry them).
    let generated = cuts.len();
    if let Some(x) = &last_lp {
        cuts.retain(|(coefs, lo, hi)| {
            let v: f64 = coefs.iter().map(|&(j, a)| a * x[j]).sum();
            let tight = |b: f64| (v - b).abs() <= 1e-6 * (1.0 + b.abs());
            (lo.is_finite() && tight(*lo)) || (hi.is_finite() && tight(*hi))
        });
    }
    if debug {
        crate::config::debug_log!(
            "ecp: continuous relaxation bound {}, {} of {generated} cuts kept",
            local.sign * lp_bound,
            cuts.len()
        );
    }
    let mut best = incumbent;
    let mut bound = f64::NEG_INFINITY;
    let mut stalled = 0;
    for round in 0..MAX_ROUNDS {
        if crate::config::expired(deadline) {
            break;
        }
        let rows: Vec<SparseRow> = linear.iter().chain(&cuts).cloned().collect();
        let mip = Mip {
            lp: Lp::new(cost.clone(), lower.clone(), upper.clone(), rows),
            integer: integer.clone(),
            domain: domain.clone(),
        };
        let settings = Settings { deadline, ..Settings::default() };
        let Ok(r) = mip::branch_and_bound(&mip, &settings) else { break };
        let Some(inc) = r.incumbent else {
            if r.end == mip::End::Complete {
                // The outer approximation is infeasible: so is the problem.
                return if best.is_some() { Outcome::Stopped(best) } else { Outcome::Infeasible };
            }
            break;
        };
        if r.end != mip::End::Complete {
            break;
        }
        // Stop when the bound stalls: branching does better then.
        if r.bound > bound + 1e-6 * (1.0 + bound.abs()) {
            stalled = 0;
        } else {
            stalled += 1;
            if stalled >= MAX_STALLED {
                break;
            }
        }
        bound = bound.max(r.bound);
        let x = inc.x;
        // An incumbent: the MILP's point if feasible, and a local solve with
        // the integers fixed.
        let offer = |y: Vec<f64>, best: &mut Option<(f64, Vec<f64>)>| {
            if local.feasible(&y[..n], &model.col_lower, &model.col_upper) {
                let f = local.objective(&y[..n]);
                if best.as_ref().is_none_or(|b| f < b.0) {
                    *best = Some((f, y[..n].to_vec()));
                }
            }
        };
        offer(x.clone(), &mut best);
        let (mut lo, mut hi) = (model.col_lower.clone(), model.col_upper.clone());
        for j in 0..n {
            if model.col_integer[j] {
                let v = x[j].round();
                (lo[j], hi[j]) = (v, v);
            }
        }
        // The local solution: cuts there too (outer approximation proper,
        // after Duran and Grossmann: tangents at the best point for these
        // integers keep the MILP from returning them with a lower bound).
        let mut oa_point = None;
        if let LocalRun::Done { x: Some(y), .. } = local.run(&lo, &hi, &x[..n], deadline) {
            oa_point = Some(y.clone());
            offer(y, &mut best);
        }
        if debug {
            crate::config::debug_log!(
                "ecp: round {round}: bound {}, incumbent {:?}",
                local.sign * bound,
                best.as_ref().map(|b| local.sign * b.0)
            );
        }
        if let Some((f, y)) = &best
            && bound >= f - gap(*f)
        {
            return Outcome::Optimal(y.clone());
        }
        // Cuts at the MILP's solution (where violated), and at the local
        // solution (all of them).
        let before = cuts.len();
        for &(root, t) in &nonlinear {
            cuts.extend(cut_at(root, t, &x, false, false));
        }
        if let Some(o) = epigraph {
            cuts.extend(cut_at(o, Interval::ENTIRE, &x, true, false));
        }
        if let Some(y) = oa_point {
            let mut y = y;
            y.push(0.0);
            let found = cuts.len();
            for &(root, t) in &nonlinear {
                cuts.extend(cut_at(root, t, &y, false, true));
            }
            if let Some(o) = epigraph {
                cuts.extend(cut_at(o, Interval::ENTIRE, &y, true, true));
            }
            // Keep them only if the MILP's point violates one (else they are
            // repeats of earlier rounds' cuts and add nothing).
            let violated = cuts[found..].iter().any(|(coefs, lo, hi)| {
                let v: f64 = coefs.iter().map(|&(j, a)| a * x[j]).sum();
                v < lo - 1e-9 * (1.0 + lo.abs()) || v > hi + 1e-9 * (1.0 + hi.abs())
            });
            if !violated {
                cuts.truncate(found);
            }
        }
        if cuts.len() == before {
            break; // nothing violated, yet not proven: numerical trouble
        }
    }
    Outcome::Stopped(best)
}

/// An equality row `g(x) + a v = c`, convex (concave) in x, where v appears
/// in no other row and its cost d has the opposite (same) sign as a, relaxes
/// to `g + a v <= c` (`>= c`): minimizing d v pushes v until the row is
/// tight. The relaxed bounds, or None.
fn relax_equality(
    model: &LinearModel,
    local: &Local,
    row: &super::Row,
    c: Curvature,
    t: Interval,
    uses: &[usize],
) -> Option<Interval> {
    if local.satisfy || !(t.lo.is_finite() && t.hi.is_finite()) || model.nonlinear.is_none() {
        return None;
    }
    let node_vars = row.node.map(|n| local.graph.vars_of(n)).unwrap_or_default();
    let defined = row.coefs.iter().find(|&&(j, a)| {
        let d = local.sign * model.obj[j];
        a != 0.0
            && d != 0.0
            && uses[j] == 1
            && !node_vars.contains(&j)
            && !model.col_integer[j]
            && match c {
                Curvature::Convex => a * d < 0.0,
                Curvature::Concave => a * d > 0.0,
                _ => false,
            }
    })?;
    let _ = defined;
    Some(match c {
        Curvature::Convex => Interval::new(f64::NEG_INFINITY, t.hi),
        _ => Interval::new(t.lo, f64::INFINITY),
    })
}

fn mid(lo: f64, hi: f64) -> f64 {
    match (lo.is_finite(), hi.is_finite()) {
        (true, true) => 0.5 * (lo + hi),
        (true, false) => lo + 1.0,
        (false, true) => hi - 1.0,
        (false, false) => 0.0,
    }
}
