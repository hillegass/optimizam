//! Integer local search: a heuristic for problems with integer variables,
//! which the global search runs from rounded relaxation solutions and from
//! every new incumbent.
//!
//! A point is compared by its violation of the rows and bounds first and by
//! its objective second, so the same search first repairs an infeasible
//! rounding and then improves a feasible point. The moves change the integer
//! variables: one by ±1 (repeating with doubled steps while that keeps
//! improving), then pairs by ±1 each. The search takes the first improving
//! move and stops when none improves, or at its budget of evaluations.
//!
//! After each move the continuous variables are recomputed: a continuous
//! variable that an equality row determines (it appears in the row linearly,
//! and the row's other variables are integers or already determined, such as
//! the `objvar = f(x)` of an objective moved into the rows) is solved for
//! directly. If some aren't determined that way, a local solve over them
//! (with the integers fixed) finds them, which costs more, so the budget is
//! smaller.

use super::{Local, LocalRun, VIOLATION_TOL};
use std::cell::Cell;
use std::time::Instant;

/// Evaluations per search when they are cheap (no local solves); with local
/// solves, the budget is their iterations (in total, per search).
const BUDGET_DIRECT: usize = 4000;
pub const BUDGET_ITERATIONS: usize = 1500;
/// Pair moves are tried only with at most this many integer variables in
/// the box (there are 2n(n - 1) of them).
const MAX_PAIR_VARS: usize = 40;
/// Iterations per local solve: a fixing of the integers that leaves the
/// rows infeasible can keep the restoration phase going for thousands.
const MAX_ITER: usize = 60;

pub struct IntSearch<'a, 'm> {
    local: &'a Local<'m>,
    /// The integer columns.
    integers: Vec<usize>,
    /// Determined continuous columns, in order: (column, row, coefficient).
    defined: Vec<(usize, usize, f64)>,
    /// Whether some continuous column isn't determined, so evaluating a
    /// point needs a local solve.
    pub solves: bool,
}

/// A point's violation (summed, relative to 1 + |bound|, beyond the
/// feasibility tolerance) and objective (minimization).
#[derive(Clone, Copy)]
struct Score {
    violation: f64,
    f: f64,
}

impl Score {
    fn better(self, other: Score) -> bool {
        if self.violation > 0.0 || other.violation > 0.0 {
            self.violation < other.violation * (1.0 - 1e-9) - 1e-12
        } else {
            self.f < other.f - 1e-9 * (1.0 + other.f.abs())
        }
    }
}

impl<'a, 'm> IntSearch<'a, 'm> {
    /// None for a problem without integer variables.
    pub fn new(local: &'a Local<'m>) -> Option<Self> {
        let model = local.model;
        let n = model.num_cols();
        let integers: Vec<usize> = (0..n).filter(|&j| model.col_integer[j]).collect();
        if integers.is_empty() {
            return None;
        }
        // Each row's variables (linear and nonlinear), and which appear in
        // its nonlinear part.
        let vars: Vec<(Vec<usize>, Vec<usize>)> = local
            .rows
            .iter()
            .map(|r| {
                let nl = r.node.map_or(Vec::new(), |n| local.graph.vars_of(n));
                let mut all: Vec<usize> = r.coefs.iter().filter(|c| c.1 != 0.0).map(|c| c.0).collect();
                all.extend(&nl);
                all.sort_unstable();
                all.dedup();
                (all, nl)
            })
            .collect();
        let mut known: Vec<bool> = model.col_integer.clone();
        for j in 0..n {
            known[j] |= model.col_lower[j] == model.col_upper[j];
        }
        let mut defined = Vec::new();
        let mut used = vec![false; local.rows.len()];
        loop {
            let mut progress = false;
            for (r, row) in local.rows.iter().enumerate() {
                if used[r] || row.lower != row.upper {
                    continue;
                }
                let unknown: Vec<usize> = vars[r].0.iter().copied().filter(|&j| !known[j]).collect();
                let [c] = unknown[..] else { continue };
                if vars[r].1.contains(&c) {
                    continue;
                }
                let a: f64 = row.coefs.iter().filter(|e| e.0 == c).map(|e| e.1).sum();
                let big = row.coefs.iter().fold(0.0f64, |m, e| m.max(e.1.abs()));
                if a.abs() <= 1e-9 * big {
                    continue;
                }
                defined.push((c, r, a));
                known[c] = true;
                used[r] = true;
                progress = true;
            }
            if !progress {
                break;
            }
        }
        let solves = !known.iter().all(|&k| k);
        Some(IntSearch { local, integers, defined, solves })
    }

    /// The search with the integers within [lo, hi] from `start` (its
    /// integers rounded): the best feasible point found, if any.
    pub fn run(&self, lo: &[f64], hi: &[f64], start: &[f64], deadline: Option<Instant>) -> Option<Vec<f64>> {
        self.run_with(lo, hi, start, deadline, BUDGET_ITERATIONS)
    }

    /// `run` with a budget of `budget` iterations of local solves.
    pub fn run_with(
        &self,
        lo_box: &[f64],
        hi_box: &[f64],
        start: &[f64],
        deadline: Option<Instant>,
        budget: usize,
    ) -> Option<Vec<f64>> {
        // The integers' bounds rounded inward (within rounding errors); the
        // continuous variables' are the model's: a box narrowed by the
        // incumbent's cutoff would leave only points that beat it within the
        // feasibility tolerance (where an objective measures a row's slack).
        let model = self.local.model;
        let (mut lo, mut hi) = (model.col_lower.clone(), model.col_upper.clone());
        for &j in &self.integers {
            (lo[j], hi[j]) = (lo_box[j], hi_box[j]);
        }
        for &j in &self.integers {
            (lo[j], hi[j]) = ((lo[j] - 1e-9).ceil(), (hi[j] + 1e-9).floor());
            if lo[j] > hi[j] {
                return None;
            }
        }
        let (lo, hi) = (&lo[..], &hi[..]);
        let mut x: Vec<f64> = start.iter().enumerate().map(|(j, v)| v.max(lo[j]).min(hi[j])).collect();
        for &j in &self.integers {
            x[j] = x[j].round();
        }
        let began = Instant::now();
        let iterations = Cell::new(0);
        let mut evals = 0;
        let mut vals = Vec::new();
        let mut eval = |x: &mut Vec<f64>, evals: &mut usize| -> Score {
            *evals += 1;
            self.complete(x, lo, hi, deadline, &iterations, budget);
            self.score(x, lo, hi, &mut vals)
        };
        let mut score = eval(&mut x, &mut evals);
        let free: Vec<usize> = self.integers.iter().copied().filter(|&j| lo[j] < hi[j]).collect();
        let out_of_time = |evals: usize| {
            let spent = if self.solves { iterations.get() >= budget } else { evals >= BUDGET_DIRECT };
            spent || crate::config::expired(deadline)
        };
        'search: loop {
            if self.local.satisfy && score.violation == 0.0 {
                break;
            }
            // Single moves, first improvement, with growing steps.
            let mut improved = false;
            for &j in &free {
                for dir in [1.0, -1.0] {
                    let mut step = 1.0;
                    loop {
                        if out_of_time(evals) {
                            break 'search;
                        }
                        let v = x[j] + dir * step;
                        if v < lo[j] || v > hi[j] {
                            break;
                        }
                        let mut y = x.clone();
                        y[j] = v;
                        let s = eval(&mut y, &mut evals);
                        if !s.better(score) {
                            break;
                        }
                        (x, score, improved) = (y, s, true);
                        step *= 2.0;
                    }
                }
            }
            if improved {
                continue;
            }
            // Pair moves.
            if free.len() > MAX_PAIR_VARS {
                break;
            }
            for (a, &j) in free.iter().enumerate() {
                for &k in &free[a + 1..] {
                    for (dj, dk) in [(1.0, -1.0), (-1.0, 1.0), (1.0, 1.0), (-1.0, -1.0)] {
                        if out_of_time(evals) {
                            break 'search;
                        }
                        let (vj, vk) = (x[j] + dj, x[k] + dk);
                        if vj < lo[j] || vj > hi[j] || vk < lo[k] || vk > hi[k] {
                            continue;
                        }
                        let mut y = x.clone();
                        (y[j], y[k]) = (vj, vk);
                        let s = eval(&mut y, &mut evals);
                        if s.better(score) {
                            (x, score) = (y, s);
                            continue 'search;
                        }
                    }
                }
            }
            break;
        }
        if crate::config::debug() {
            let what =
                if score.violation == 0.0 { format!("{}", self.local.sign * score.f) } else { "no point".into() };
            crate::config::debug_log!(
                "intsearch: {evals} evaluations in {:.3}s: {what}",
                began.elapsed().as_secs_f64()
            );
        }
        (score.violation == 0.0).then_some(x)
    }

    /// Sets the continuous columns of x for its integers: the determined
    /// ones from their rows, the others by a local solve.
    fn complete(
        &self,
        x: &mut [f64],
        lo: &[f64],
        hi: &[f64],
        deadline: Option<Instant>,
        iterations: &Cell<usize>,
        budget: usize,
    ) {
        let local = self.local;
        let mut vals = Vec::new();
        for &(c, r, a) in &self.defined {
            let row = &local.rows[r];
            x[c] = 0.0;
            let mut rest: f64 = row.coefs.iter().map(|&(j, b)| b * x[j]).sum();
            if let Some(n) = row.node {
                local.graph.eval(x, &mut vals);
                rest += vals[n as usize];
            }
            x[c] = (row.lower - rest) / a;
        }
        if self.solves {
            let (mut l, mut u) = (lo.to_vec(), hi.to_vec());
            for &j in &self.integers {
                (l[j], u[j]) = (x[j], x[j]);
            }
            let max_iter = MAX_ITER.min(budget.saturating_sub(iterations.get())).max(1);
            if let LocalRun::Done { x: y, best, iterations: k, .. } = local.run_limited(&l, &u, x, deadline, max_iter) {
                iterations.set(iterations.get() + k);
                if let Some(y) = y.or(best) {
                    x.copy_from_slice(&y);
                }
            }
        }
    }

    fn score(&self, x: &[f64], lo: &[f64], hi: &[f64], vals: &mut Vec<f64>) -> Score {
        let local = self.local;
        local.graph.eval(x, vals);
        let excess = |v: f64, l: f64, u: f64| {
            let over = |d: f64, b: f64| (d / (1.0 + b.abs()) - VIOLATION_TOL).max(0.0);
            if v.is_nan() { f64::INFINITY } else { over(l - v, l) + over(v - u, u) }
        };
        let mut violation: f64 = (0..x.len()).map(|j| excess(x[j], lo[j], hi[j])).sum();
        for r in &local.rows {
            let v = r.coefs.iter().map(|&(j, a)| a * x[j]).sum::<f64>() + r.node.map_or(0.0, |n| vals[n as usize]);
            violation += excess(v, r.lower, r.upper);
        }
        let f = if local.satisfy {
            0.0
        } else {
            let model = local.model;
            let nl = local.obj_node.map_or(0.0, |n| vals[n as usize]);
            local.sign * (model.obj_offset + model.obj.iter().zip(x).map(|(c, v)| c * v).sum::<f64>() + nl)
        };
        if !f.is_finite() {
            violation = f64::INFINITY;
        }
        Score { violation, f }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::Problem;

    fn model(json: &str) -> crate::model::LinearModel {
        crate::model::build(&Problem::from_json(json).unwrap()).unwrap()
    }

    /// A concave objective over integers, written with an objective
    /// variable defined by an equality row (so no local solves): single
    /// moves stop at (4, 3); pair moves walk on to the optimum (7, 0).
    #[test]
    fn pair_moves_reach_the_optimum() {
        let m = model(
            r#"{"version": "1.2", "variables": [
                {"name": "x", "type": "integer", "lower": 0, "upper": 10},
                {"name": "y", "type": "integer", "lower": 0, "upper": 10},
                {"name": "t", "type": "continuous"}],
              "constraints": [{"name": "c", "expr": "x + y <= 7.5"},
                {"name": "d", "expr": "t == -(x^2 + y^2)"}],
              "solve": {"method": "minimize", "objective": "t"}}"#,
        );
        let graph = &m.nonlinear.as_ref().unwrap().graph;
        let local = Local::new(&m, graph);
        let s = IntSearch::new(&local).unwrap();
        assert_eq!(s.defined.len(), 1);
        assert!(!s.solves);
        let x = s.run(&m.col_lower, &m.col_upper, &[3.0, 3.0, 0.0], None).unwrap();
        assert_eq!(x, vec![7.0, 0.0, -49.0]);
    }

    /// An infeasible rounding is repaired first; a continuous variable no
    /// row determines is found by local solves.
    #[test]
    fn repairs_and_solves_continuous_variables() {
        let m = model(
            r#"{"version": "1.2", "variables": [
                {"name": "n", "type": "integer", "lower": 0, "upper": 20},
                {"name": "z", "type": "continuous", "lower": 0, "upper": 10}],
              "constraints": [{"name": "c", "expr": "n * z >= 12"}, {"name": "d", "expr": "z <= 2"}],
              "solve": {"method": "minimize", "objective": "n^2 + z"}}"#,
        );
        let graph = &m.nonlinear.as_ref().unwrap().graph;
        let local = Local::new(&m, graph);
        let s = IntSearch::new(&local).unwrap();
        assert!(s.solves);
        let x = s.run(&m.col_lower, &m.col_upper, &[1.0, 1.0], None).unwrap();
        // n = 6, z = 2 (37) beats n = 7, z = 12/7.
        assert_eq!(x[0], 6.0);
        assert!((x[1] - 2.0).abs() < 1e-6);
    }
}
