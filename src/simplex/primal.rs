//! Primal simplex iterations, for a basis that is primal feasible but not
//! dual feasible: what the dual simplex is left with when it removes its
//! cost perturbation and the true costs leave a few duals with the wrong
//! sign (HiGHS cleans up the same way, with `HEkkPrimal`).
//!
//! The dual simplex would have to give up primal feasibility there (flip
//! bounds, shift costs) and win it back, and after a few rounds of that it
//! settles for a basis that isn't quite optimal. From a nearly optimal basis
//! the primal method takes a few pivots instead. It is not a general solver:
//! there is no phase 1, and when it stalls or meets numerical trouble it
//! stops, leaving a valid basis for the dual simplex to go on from.
//!
//! Tried and dropped: these iterations for bound tightening's LPs, each of
//! which starts at the last one's optimum with a new objective. They took
//! more pivots than the dual simplex, whose ratio test flips many bounds in
//! one step (ex8_4_1: 261,000 iterations against 184,000).

use super::factor::PIVOT_TOLERANCE;
use super::{DUAL_FEASIBILITY_TOLERANCE, NUMERICAL_TROUBLE_TOLERANCE, PRIMAL_FEASIBILITY_TOLERANCE, UPDATE_LIMIT};
use super::{Rebuild, Simplex, is_inf};

/// How the primal iterations ended.
#[derive(Debug, PartialEq)]
pub(super) enum PrimalEnd {
    /// The basis is optimal, with fresh values.
    Optimal,
    /// The objective decreases without bound along the last direction.
    Unbounded,
    /// Stalled, or numerical trouble: the dual simplex should take over.
    GaveUp,
    TimeLimit,
}

/// Columns of the pivotal column this small don't limit the step.
const RATIO_PIVOT: f64 = 1e-9;

impl Simplex<'_> {
    /// Iterates from the present basis, which must be primal feasible for
    /// the bounds in place, until no nonbasic variable's dual is infeasible.
    pub(super) fn primal(&mut self) -> PrimalEnd {
        let tot = self.n + self.m;
        // Devex reference weights for pricing, reset at each start.
        let mut weight = vec![1.0f64; tot];
        let limit = self.iteration_count + 200 + 2 * tot;
        let mut fresh = false;
        loop {
            if self.iteration_count.is_multiple_of(super::CLOCK_EVERY) && self.timed_out() {
                return PrimalEnd::TimeLimit;
            }
            if self.iteration_count >= limit {
                return PrimalEnd::GaveUp;
            }
            let updates = self.factor.as_ref().map_or(0, |f| f.num_updates());
            if updates >= UPDATE_LIMIT || self.rebuild_reason == Rebuild::PossiblySingular {
                if !self.primal_values(true) {
                    return PrimalEnd::GaveUp;
                }
                fresh = true;
            }
            let Some(q) = self.primal_choose_column(&weight) else {
                // Optimal by the updated duals: confirm with fresh ones.
                if fresh {
                    return PrimalEnd::Optimal;
                }
                if !self.primal_values(false) {
                    return PrimalEnd::GaveUp;
                }
                fresh = true;
                continue;
            };
            fresh = false;
            match self.primal_iterate(q, &mut weight) {
                Step::Done => {}
                Step::Unbounded => return PrimalEnd::Unbounded,
                Step::Trouble => {
                    // After a fresh factorization there is nothing left to try.
                    if updates == 0 {
                        return PrimalEnd::GaveUp;
                    }
                    self.rebuild_reason = Rebuild::PossiblySingular;
                }
            }
        }
    }

    /// Fresh duals and basic values (from a new factorization if `refactor`
    /// or if the updated one fails its check). False if that fails or the
    /// basic values turn out infeasible, which only rounding can cause.
    fn primal_values(&mut self, refactor: bool) -> bool {
        self.rebuild_reason = Rebuild::No;
        if refactor && !self.factorize() {
            return false;
        }
        self.compute_dual();
        self.compute_primal();
        if !refactor && self.factor.as_ref().is_some_and(|f| f.num_updates() > 0) && !self.accurate() {
            if !self.factorize() {
                return false;
            }
            self.compute_dual();
            self.compute_primal();
        }
        self.infeasible.is_empty()
    }

    /// CHUZC: the nonbasic variable with the largest dual infeasibility
    /// squared over its reference weight.
    fn primal_choose_column(&self, weight: &[f64]) -> Option<usize> {
        let mut best = None;
        let mut best_merit = 0.0;
        for j in 0..self.n + self.m {
            if !self.nonbasic_flag[j] {
                continue;
            }
            let (lo, hi, d) = (self.work_lower[j], self.work_upper[j], self.work_dual[j]);
            let infeasibility = if lo == hi {
                0.0
            } else if is_inf(lo) && is_inf(hi) {
                d.abs()
            } else {
                -(self.nonbasic_move[j] as f64) * d
            };
            if infeasibility > DUAL_FEASIBILITY_TOLERANCE && infeasibility * infeasibility > best_merit * weight[j] {
                best_merit = infeasibility * infeasibility / weight[j];
                best = Some(j);
            }
        }
        best
    }

    /// One iteration with entering variable `q`: a bound flip, or a pivot
    /// with the updates of the values, the duals, both methods' weights,
    /// the basis, and the factorization.
    fn primal_iterate(&mut self, q: usize, weight: &mut [f64]) -> Step {
        // The entering variable moves up if its dual is negative.
        let direction = if self.work_dual[q] < 0.0 { 1.0 } else { -1.0 };
        let mut col = std::mem::take(&mut self.col_aq);
        col.clear();
        self.collect_column(q, 1.0, &mut col);
        self.factor.as_mut().unwrap().ftran_entering(&mut col, &mut self.spike);
        self.col_aq = col;

        // CHUZR, in Harris's two passes: the longest step that keeps every
        // basic variable within its bounds loosened by the tolerance, then
        // among the rows that block a step no longer than that, the largest
        // pivot.
        let tolerance = PRIMAL_FEASIBILITY_TOLERANCE;
        let range = self.work_range[q];
        let mut longest = range;
        self.col_aq.for_each(|p, a| {
            let alpha = direction * a;
            let room = if alpha > RATIO_PIVOT {
                self.base_value[p] - self.base_lower[p]
            } else if alpha < -RATIO_PIVOT {
                self.base_upper[p] - self.base_value[p]
            } else {
                f64::INFINITY
            };
            if room < f64::INFINITY {
                longest = longest.min((room + tolerance) / alpha.abs());
            }
        });
        if longest == f64::INFINITY {
            return Step::Unbounded;
        }
        let mut row: Option<(usize, f64)> = None; // position, step
        let mut largest = 0.0;
        self.col_aq.for_each(|p, a| {
            let alpha = direction * a;
            let room = if alpha > RATIO_PIVOT {
                self.base_value[p] - self.base_lower[p]
            } else if alpha < -RATIO_PIVOT {
                self.base_upper[p] - self.base_value[p]
            } else {
                f64::INFINITY
            };
            if room < f64::INFINITY && room / alpha.abs() <= longest && alpha.abs() > largest {
                largest = alpha.abs();
                row = Some((p, (room / alpha.abs()).max(0.0)));
            }
        });
        self.fresh_duals = false;
        self.fresh_primals = false;
        let Some((p, step)) = row.filter(|&(_, step)| step < range) else {
            // The entering variable reaches its other bound first.
            let col = std::mem::take(&mut self.col_aq);
            col.for_each(|i, a| self.base_value[i] -= range * direction * a);
            self.col_aq = col;
            self.flip_bound(q);
            self.iteration_count += 1;
            return Step::Done;
        };

        // The pivotal row, for the duals and the weights.
        let alpha_col = self.col_aq.array[p];
        if alpha_col.abs() < PIVOT_TOLERANCE {
            return Step::Trouble;
        }
        self.factor.as_mut().unwrap().btran_leaving(p, &mut self.row_ep, &mut self.row_ep_partial);
        self.computed_edge_weight = self.row_ep.norm2();
        self.price();
        let alpha_row = if q < self.n { self.row_ap.array[q] } else { self.row_ep.array[q - self.n] };
        let agreement = (alpha_col.abs() - alpha_row.abs()).abs() / alpha_col.abs().min(alpha_row.abs());
        if agreement.is_nan() || agreement > NUMERICAL_TROUBLE_TOLERANCE {
            return Step::Trouble;
        }

        // Duals, and the devex weights of the nonbasic variables.
        let out = self.basic_index[p];
        let theta_dual = self.work_dual[q] / alpha_col;
        let reference = weight[q];
        for &(j, alpha) in &self.pack {
            self.work_dual[j] -= theta_dual * alpha;
            let ratio = alpha / alpha_col;
            weight[j] = weight[j].max(ratio * ratio * reference);
        }
        self.work_dual[q] = 0.0;
        self.work_dual[out] = -theta_dual;
        weight[out] = (reference / (alpha_col * alpha_col)).max(1.0);

        // Basic values, and the dual method's steepest edge weights, which
        // stay exact through any pivot.
        self.update_ftran_dse();
        self.edge_weight[p] = self.computed_edge_weight;
        let pivotal_weight = self.edge_weight[p] / (alpha_col * alpha_col);
        let kai = -2.0 / alpha_col;
        let col = std::mem::take(&mut self.col_aq);
        col.for_each(|i, a| {
            self.base_value[i] -= step * direction * a;
            let w = self.edge_weight[i] + a * (pivotal_weight * a + kai * self.col_dse.array[i]);
            self.edge_weight[i] = w.max(super::MIN_DUAL_STEEPEST_EDGE_WEIGHT);
        });
        self.col_aq = col;
        self.edge_weight[p] = pivotal_weight;

        // The basis: `out` leaves at the bound it reached.
        let at_lower = direction * alpha_col > 0.0;
        self.basic_index[p] = q;
        self.nonbasic_flag[q] = false;
        self.nonbasic_move[q] = 0;
        self.base_value[p] = self.work_value[q] + step * direction;
        self.base_lower[p] = self.work_lower[q];
        self.base_upper[p] = self.work_upper[q];
        self.nonbasic_flag[out] = true;
        let (lo, hi) = (self.work_lower[out], self.work_upper[out]);
        (self.work_value[out], self.nonbasic_move[out]) = if lo == hi {
            (lo, 0)
        } else if at_lower {
            (lo, 1)
        } else {
            (hi, -1)
        };
        self.factor.as_mut().unwrap().update(&self.spike, &self.row_ep_partial, p, alpha_col);
        self.iteration_count += 1;
        Step::Done
    }
}

enum Step {
    Done,
    Unbounded,
    /// The pivot is too small, or the pivotal row and column disagree on it.
    Trouble,
}
