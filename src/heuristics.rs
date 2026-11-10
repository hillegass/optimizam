//! Primal heuristics: find good integer-feasible solutions early, so that
//! branch and bound can prune with them.
//!
//! - Simple rounding (Bixby): round each fractional column in a direction in
//!   which no row restricts it ("lock-free"), which keeps every row satisfied.
//!   Tried on every LP point a dive visits.
//! - Diving: from an LP solution, repeatedly bound one fractional column to
//!   one side and re-solve the LP from the previous basis, with one backtrack
//!   (the other side) on infeasibility. Variants choose the column and side
//!   differently: fractional (closest to an integer, rounded to nearest),
//!   coefficient (fewest locks in the rounding direction), and guided (toward
//!   the incumbent, the closest first).
//! - RENS (Berthold): fix the integer columns that are integral in the root
//!   LP, restrict the others to their floor and ceiling, and solve that as a
//!   sub-MILP with a node limit.
//! - RINS (Danna, Rothberg, Le Pape): fix the integer columns on which the
//!   incumbent and a node's LP solution agree, and solve that as a sub-MILP.
//!
//! Sub-MILPs go through presolve and branch and bound with cuts, heuristics
//! and debug output off, and a cutoff so that only improving solutions count.
//! Every solution found is checked against the original rows before use.

use crate::lp::Lp;
use crate::milp::{self, Candidate, DOWN, Milp, Settings, UP};
use crate::presolve::{self, Presolved};
use crate::simplex::{Basis, LpStatus, Options, Simplex};
use std::time::Instant;

/// Most bound changes in one dive.
const MAX_DIVE_DEPTH: usize = 1000;
/// Node limit of a sub-MILP.
const SUB_MILP_NODES: usize = 500;
/// RENS and RINS need at least this fraction of the integer columns fixed.
const RENS_MIN_FIXED: f64 = 0.5;
const RINS_MIN_FIXED: f64 = 0.3;
/// Tolerance for checking a heuristic solution, relative to max(1, |bound|).
const CHECK_TOLERANCE: f64 = 1e-6;

#[derive(Clone, Copy, Debug)]
pub enum Dive {
    Fractional,
    Coefficient,
    Guided,
}

/// What the heuristics found and how long they took (printed with `OPTIMIZAM_DEBUG`).
#[derive(Debug, Default)]
pub struct Stats {
    pub dives: usize,
    pub dive_successes: usize,
    pub sub_milps: usize,
    pub sub_milp_successes: usize,
    pub seconds: f64,
}

pub struct Heuristics {
    /// Rows that increasing / decreasing a column could violate.
    up_locks: Vec<usize>,
    down_locks: Vec<usize>,
    /// Simplex iterations one dive may use.
    dive_budget: usize,
    pub stats: Stats,
}

/// What a heuristic needs about the problem.
pub struct Context<'a> {
    /// Integrality, and the rows a solution is checked against.
    pub milp: &'a Milp,
    /// The LP with root cuts, for LP solves.
    pub lp: &'a Lp,
    /// Only solutions with a (cut-LP) objective below this are useful.
    pub cutoff: f64,
    pub deadline: Option<Instant>,
}

impl Heuristics {
    pub fn new(lp: &Lp) -> Self {
        let n = lp.num_col;
        let (mut up_locks, mut down_locks) = (vec![0; n], vec![0; n]);
        for i in 0..lp.num_row {
            let (has_lo, has_up) = (lp.row_lower[i].is_finite(), lp.row_upper[i].is_finite());
            for k in lp.ar_start[i]..lp.ar_start[i + 1] {
                let (j, a) = (lp.ar_index[k], lp.ar_value[k]);
                let (up, down) = if a > 0.0 { (has_up, has_lo) } else { (has_lo, has_up) };
                up_locks[j] += up as usize;
                down_locks[j] += down as usize;
            }
        }
        let dive_budget = 1000 + 2 * (lp.num_row + lp.num_col);
        Heuristics { up_locks, down_locks, dive_budget, stats: Stats::default() }
    }

    /// Rounds `x` in lock-free directions, or None if some fractional column
    /// is locked both ways. With no fractional column, this is `x` rounded.
    fn simple_rounding(&self, ctx: &Context, x: &[f64], lower: &[f64], upper: &[f64]) -> Option<Vec<f64>> {
        let mut y = x.to_vec();
        for c in milp::candidates(ctx.milp, x, lower, upper) {
            let j = c.col;
            let down_ok = self.down_locks[j] == 0 && c.down.is_finite();
            let up_ok = self.up_locks[j] == 0 && c.up.is_finite();
            y[j] = match (down_ok, up_ok) {
                (true, true) => {
                    if c.value - c.down <= c.up - c.value {
                        c.down
                    } else {
                        c.up
                    }
                }
                (true, false) => c.down,
                (false, true) => c.up,
                (false, false) => return None,
            };
        }
        for j in 0..y.len() {
            if ctx.milp.integer[j] {
                y[j] = y[j].round();
            }
        }
        Some(y)
    }

    /// The column to bound next and its preferred side.
    fn select(&self, cands: &[Candidate], kind: Dive, incumbent: Option<&[f64]>) -> Option<(usize, usize)> {
        let mut best: Option<((f64, f64), usize, usize)> = None;
        for (k, c) in cands.iter().enumerate() {
            let (dist_down, dist_up) = (c.value - c.down, c.up - c.value);
            let nearest = if dist_down <= dist_up { DOWN } else { UP };
            let (dir, score) = match kind {
                Dive::Fractional => (nearest, (dist_down.min(dist_up), 0.0)),
                Dive::Coefficient => {
                    let (ld, lu) = (self.down_locks[c.col] as f64, self.up_locks[c.col] as f64);
                    let dir = if ld < lu || (ld == lu && nearest == DOWN) { DOWN } else { UP };
                    let dist = if dir == DOWN { dist_down } else { dist_up };
                    (dir, (ld.min(lu), dist))
                }
                Dive::Guided => {
                    let target = incumbent?[c.col];
                    let dir = if target <= c.value { DOWN } else { UP };
                    (dir, ((target - c.value).abs(), 0.0))
                }
            };
            // A side outside the bounds is not available.
            let dir = match (dir, c.down.is_finite(), c.up.is_finite()) {
                (DOWN, false, _) => UP,
                (UP, _, false) => DOWN,
                _ => dir,
            };
            if best.as_ref().is_none_or(|(s, ..)| score < *s) {
                best = Some((score, k, dir));
            }
        }
        best.map(|(_, k, dir)| (k, dir))
    }

    /// One dive from the LP solution `x` (with `basis`) under the given bounds.
    #[allow(clippy::too_many_arguments)]
    pub fn dive(
        &mut self,
        ctx: &Context,
        lower: &[f64],
        upper: &[f64],
        x: &[f64],
        basis: Option<&Basis>,
        kind: Dive,
        incumbent: Option<&[f64]>,
    ) -> Option<Vec<f64>> {
        let started = Instant::now();
        self.stats.dives += 1;
        let found = self.dive_inner(ctx, lower, upper, x, basis, kind, incumbent).filter(|y| self.accept(ctx, y));
        self.stats.dive_successes += found.is_some() as usize;
        self.stats.seconds += started.elapsed().as_secs_f64();
        found
    }

    #[allow(clippy::too_many_arguments)]
    fn dive_inner(
        &self,
        ctx: &Context,
        lower: &[f64],
        upper: &[f64],
        x: &[f64],
        basis: Option<&Basis>,
        kind: Dive,
        incumbent: Option<&[f64]>,
    ) -> Option<Vec<f64>> {
        let (mut l, mut u) = (lower.to_vec(), upper.to_vec());
        let mut x = x.to_vec();
        let mut basis = basis.cloned();
        let opts = Options { deadline: ctx.deadline, objective_bound: ctx.cutoff, ..Options::default() };
        // Each step's LP starts from the last one's basis: one solver.
        let mut simplex = Simplex::new(ctx.lp);
        let mut iterations = 0;
        for _ in 0..MAX_DIVE_DEPTH {
            if let Some(y) = self.simple_rounding(ctx, &x, &l, &u) {
                return Some(y);
            }
            let cands = milp::candidates(ctx.milp, &x, &l, &u);
            let (k, first) = self.select(&cands, kind, incumbent)?;
            let c = &cands[k];
            let mut moved = false;
            for dir in [first, 1 - first] {
                let bound = if dir == DOWN { c.down } else { c.up };
                if !bound.is_finite() {
                    continue;
                }
                let (mut l2, mut u2) = (l.clone(), u.clone());
                if dir == DOWN {
                    u2[c.col] = bound;
                } else {
                    l2[c.col] = bound;
                }
                let sol = simplex.solve(&l2, &u2, basis.as_ref(), &opts);
                iterations += sol.iterations;
                match sol.status {
                    LpStatus::Optimal => {
                        (l, u, x) = (l2, u2, sol.col_value);
                        basis = sol.basis;
                        moved = true;
                        break;
                    }
                    LpStatus::Infeasible | LpStatus::ObjectiveBound | LpStatus::UnboundedOrInfeasible => {}
                    LpStatus::TimeLimit | LpStatus::Error(_) => return None,
                }
            }
            if !moved || iterations > self.dive_budget {
                return None;
            }
        }
        None
    }

    /// RENS from the root LP solution `x`.
    pub fn rens(&mut self, ctx: &Context, x: &[f64], lower: &[f64], upper: &[f64]) -> Option<Vec<f64>> {
        let (mut l, mut u) = (lower.to_vec(), upper.to_vec());
        let mut fractional = vec![false; x.len()];
        for c in milp::candidates(ctx.milp, x, lower, upper) {
            // Fractional: keep only the two neighboring values.
            fractional[c.col] = true;
            if c.down.is_finite() {
                l[c.col] = l[c.col].max(c.down);
            }
            if c.up.is_finite() {
                u[c.col] = u[c.col].min(c.up);
            }
        }
        let (mut ints, mut fixed) = (0, 0);
        for j in 0..x.len() {
            if ctx.milp.integer[j] {
                ints += 1;
                if !fractional[j] {
                    let v = x[j].round();
                    (l[j], u[j]) = (v, v);
                    fixed += 1;
                }
            }
        }
        if (fixed as f64) < RENS_MIN_FIXED * ints as f64 {
            return None;
        }
        self.sub_milp(ctx, l, u)
    }

    /// RINS from a node's LP solution `x` and the incumbent.
    pub fn rins(
        &mut self,
        ctx: &Context,
        x: &[f64],
        incumbent: &[f64],
        lower: &[f64],
        upper: &[f64],
    ) -> Option<Vec<f64>> {
        let (mut l, mut u) = (lower.to_vec(), upper.to_vec());
        let (mut ints, mut fixed) = (0, 0);
        for j in 0..x.len() {
            if ctx.milp.integer[j] {
                ints += 1;
                if (x[j] - incumbent[j]).abs() <= milp::INTEGRALITY_TOLERANCE {
                    (l[j], u[j]) = (incumbent[j], incumbent[j]);
                    fixed += 1;
                }
            }
        }
        if (fixed as f64) < RINS_MIN_FIXED * ints as f64 || fixed == ints {
            return None;
        }
        self.sub_milp(ctx, l, u)
    }

    /// Solves the problem restricted to `[lower, upper]` with a node limit.
    fn sub_milp(&mut self, ctx: &Context, lower: Vec<f64>, upper: Vec<f64>) -> Option<Vec<f64>> {
        let started = Instant::now();
        self.stats.sub_milps += 1;
        let found = self.sub_milp_inner(ctx, lower, upper).filter(|y| self.accept(ctx, y));
        self.stats.sub_milp_successes += found.is_some() as usize;
        self.stats.seconds += started.elapsed().as_secs_f64();
        found
    }

    fn sub_milp_inner(&self, ctx: &Context, lower: Vec<f64>, upper: Vec<f64>) -> Option<Vec<f64>> {
        let mut sub = Milp { lp: ctx.lp.clone(), integer: ctx.milp.integer.clone(), domain: ctx.milp.domain.clone() };
        (sub.lp.col_lower, sub.lp.col_upper) = (lower, upper);
        let mut stats = presolve::Stats::default();
        let (reduced, post) =
            match presolve::presolve(&sub, &presolve::Options { dual: true, frozen: vec![] }, &mut stats) {
                Presolved::Infeasible => return None,
                Presolved::Reduced(reduced, post) => (reduced, post),
            };
        if reduced.lp.num_col == 0 {
            return Some(post.restore(&[]));
        }
        let settings = Settings {
            deadline: ctx.deadline,
            cutoff: ctx.cutoff - post.offset,
            node_limit: Some(SUB_MILP_NODES),
            sub_milp: true,
            ..Settings::default()
        };
        let result = milp::branch_and_bound(&reduced, &settings).ok()?;
        result.incumbent.map(|inc| post.restore(&inc.x))
    }

    /// Whether `y` is integer-feasible for the original rows and improves on the cutoff.
    fn accept(&self, ctx: &Context, y: &[f64]) -> bool {
        let lp = &ctx.milp.lp;
        let near = |v: f64, b: f64| (v - b).abs() <= CHECK_TOLERANCE * b.abs().max(1.0);
        let cols_ok = (0..lp.num_col).all(|j| {
            let v = y[j];
            (v >= lp.col_lower[j] || near(v, lp.col_lower[j]))
                && (v <= lp.col_upper[j] || near(v, lp.col_upper[j]))
                && (!ctx.milp.integer[j] || (v.fract() == 0.0 && milp::in_domain(&ctx.milp.domain[j], v)))
        });
        let rows_ok = (0..lp.num_row).all(|i| {
            let ax: f64 = (lp.ar_start[i]..lp.ar_start[i + 1]).map(|k| lp.ar_value[k] * y[lp.ar_index[k]]).sum();
            (ax >= lp.row_lower[i] || near(ax, lp.row_lower[i])) && (ax <= lp.row_upper[i] || near(ax, lp.row_upper[i]))
        });
        cols_ok && rows_ok && ctx.lp.objective(y) < ctx.cutoff
    }
}
