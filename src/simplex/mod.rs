//! Dual revised simplex, after HiGHS's `HEkk` / `HEkkDual`
//! (highs/simplex/HEkk.cpp, HEkkDual.cpp, HEkkDualRow.cpp, HEkkDualRHS.cpp).
//!
//! Conventions follow HEkk: there are `n + m` variables. Variable `n + i` is
//! the logical (slack) for row i, with column e_i and bounds
//! [-row_upper, -row_lower], so that [A I] x = 0. Nonbasic variables sit at
//! a bound (`nonbasic_move` +1 = at lower, may move up; -1 = at upper; 0 =
//! fixed or free). Basic variables are indexed by basis position, which (as in
//! HFactor) is also the row of their pivot.
//!
//! Structure of an iteration (HEkkDual::iterate): CHUZR with dual steepest
//! edge weights; BTRAN and PRICE for the pivotal row; CHUZC with the
//! bound-flipping ratio test; FTRANs for the BFRT flips, the pivotal column,
//! and the DSE update; then dual, primal, weight and basis updates.
//! Phase 1 uses HiGHS's artificial box bounds; costs are perturbed unless
//! the start is near-optimal, and perturbations and shifts are removed
//! ("cleanup") before optimality is declared.

pub mod factor;

use crate::lp::Lp;
use factor::{Factor, PIVOT_THRESHOLD};
use std::time::Instant;

pub const PRIMAL_FEASIBILITY_TOLERANCE: f64 = 1e-7;
pub const DUAL_FEASIBILITY_TOLERANCE: f64 = 1e-7;
const UPDATE_LIMIT: usize = 100;
const MIN_DUAL_STEEPEST_EDGE_WEIGHT: f64 = 1e-4;
const ACCEPT_DSE_WEIGHT_THRESHOLD: f64 = 0.25;
const NUMERICAL_TROUBLE_TOLERANCE: f64 = 1e-7;
const MAX_PIVOT_THRESHOLD: f64 = 0.5;
const INITIAL_TOTAL_CHANGE: f64 = 1e-12;
const INITIAL_REMAIN_THETA: f64 = 1e100;
const MAX_SELECT_THETA: f64 = 1e18;
const PIVOT_GROWTH_TOLERANCE: f64 = 1e-9;
const COST_PERTURBATION_MULTIPLIER: f64 = 1.0;
const MAX_CLEANUP_LEVEL: usize = 4;
const EXCESSIVE_PRIMAL_VALUE: f64 = 1e25;

#[derive(Clone, Debug, PartialEq)]
pub enum LpStatus {
    Optimal,
    Infeasible,
    /// The dual is infeasible: the LP is unbounded or infeasible.
    UnboundedOrInfeasible,
    TimeLimit,
    /// The dual objective exceeded `Options::objective_bound` (a MIP cutoff).
    ObjectiveBound,
    Error(String),
}

/// A simplex basis that can warm-start a later solve (HiGHS `SimplexBasis`
/// plus the dual edge weights, which the HiGHS MIP solver also carries).
#[derive(Clone, Debug)]
pub struct Basis {
    pub basic_index: Vec<usize>,
    pub nonbasic_move: Vec<i8>,
    pub dual_edge_weight: Vec<f64>,
}

pub struct LpSolution {
    pub status: LpStatus,
    pub col_value: Vec<f64>,
    /// Objective of `col_value` in the LP's (minimization) costs.
    pub objective: f64,
    pub basis: Option<Basis>,
    /// Simplex iterations used.
    pub iterations: usize,
    /// The columns' reduced costs `c - A^T y` at an optimum (empty
    /// otherwise), from the true costs.
    pub col_dual: Vec<f64>,
}

#[derive(Clone)]
pub struct Options {
    pub deadline: Option<Instant>,
    /// Stop with `ObjectiveBound` once the (valid) dual objective exceeds this.
    pub objective_bound: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options { deadline: None, objective_bound: f64::INFINITY }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rebuild {
    No,
    UpdateLimit,
    PossiblyOptimal,
    PossiblyDualUnbounded,
    PossiblySingular,
    ChooseColumnFail,
    ExcessivePrimal,
}

fn is_inf(x: f64) -> bool {
    x.is_infinite()
}

/// Solves `lp` with column bounds overridden by `col_lower`/`col_upper`.
pub fn solve(lp: &Lp, col_lower: &[f64], col_upper: &[f64], start: Option<&Basis>, opts: &Options) -> LpSolution {
    // Free rows never bind, and their logicals stay basic in the dual
    // simplex, so they are left out of the solve (relaxations keep their
    // shape with many free placeholder rows).
    let is_free = |i: usize| lp.row_lower[i] == f64::NEG_INFINITY && lp.row_upper[i] == f64::INFINITY;
    if (0..lp.num_row).any(is_free) {
        let n = lp.num_col;
        let keep: Vec<bool> = (0..lp.num_row).map(|i| !is_free(i)).collect();
        let kept: Vec<usize> = (0..lp.num_row).filter(|&i| keep[i]).collect();
        let reduced = lp.with_rows(&keep);
        let mut new_index = vec![usize::MAX; lp.num_row];
        for (r, &i) in kept.iter().enumerate() {
            new_index[i] = n + r;
        }
        let start = start.map(|b| Basis {
            basic_index: b
                .basic_index
                .iter()
                .filter_map(|&v| if v < n { Some(v) } else { Some(new_index[v - n]).filter(|&r| r != usize::MAX) })
                .collect(),
            nonbasic_move: (0..n)
                .map(|j| b.nonbasic_move[j])
                .chain(kept.iter().map(|&i| b.nonbasic_move[n + i]))
                .collect(),
            dual_edge_weight: b
                .basic_index
                .iter()
                .zip(&b.dual_edge_weight)
                .filter(|&(&v, _)| v < n || new_index[v - n] != usize::MAX)
                .map(|(_, &w)| w)
                .collect(),
        });
        let mut sol = solve_rows(&reduced, col_lower, col_upper, start.as_ref(), opts);
        if let Some(b) = sol.basis.as_mut() {
            for v in b.basic_index.iter_mut() {
                if *v >= n {
                    *v = n + kept[*v - n];
                }
            }
            let mut nonbasic_move = vec![0i8; n + lp.num_row];
            nonbasic_move[..n].copy_from_slice(&b.nonbasic_move[..n]);
            for (r, &i) in kept.iter().enumerate() {
                nonbasic_move[n + i] = b.nonbasic_move[n + r];
            }
            b.nonbasic_move = nonbasic_move;
            for i in (0..lp.num_row).filter(|&i| !keep[i]) {
                b.basic_index.push(n + i);
                b.dual_edge_weight.push(1.0);
            }
        }
        return sol;
    }
    solve_rows(lp, col_lower, col_upper, start, opts)
}

fn solve_rows(lp: &Lp, col_lower: &[f64], col_upper: &[f64], start: Option<&Basis>, opts: &Options) -> LpSolution {
    let mut ekk = Ekk::new(lp, col_lower, col_upper, opts.clone());
    if let Some(msg) = ekk.inconsistent_bounds() {
        let _ = msg;
        return ekk.finish(LpStatus::Infeasible);
    }
    ekk.set_basis(start);
    let status = ekk.run();
    ekk.finish(status)
}

struct Ekk<'a> {
    lp: &'a Lp,
    n: usize,
    m: usize,
    opts: Options,
    lp_lower: Vec<f64>,
    lp_upper: Vec<f64>,
    work_cost: Vec<f64>,
    work_shift: Vec<f64>,
    work_lower: Vec<f64>,
    work_upper: Vec<f64>,
    work_range: Vec<f64>,
    work_value: Vec<f64>,
    work_dual: Vec<f64>,
    random: Vec<f64>,
    basic_index: Vec<usize>,
    nonbasic_flag: Vec<bool>,
    nonbasic_move: Vec<i8>,
    base_value: Vec<f64>,
    base_lower: Vec<f64>,
    base_upper: Vec<f64>,
    work_infeasibility: Vec<f64>,
    edge_weight: Vec<f64>,
    factor: Option<Factor>,
    pivot_threshold: f64,
    costs_perturbed: bool,
    num_shifts: usize,
    allow_perturbation: bool,
    phase: u8,
    iteration_count: usize,
    iteration_limit: usize,
    rebuild_reason: Rebuild,
    fresh_rebuild: bool,
    dual_infeas_count: usize,
    // Iteration data
    row_out: Option<usize>,
    variable_in: Option<usize>,
    variable_out: usize,
    delta_primal: f64,
    move_out: i8,
    theta_dual: f64,
    theta_primal: f64,
    alpha_col: f64,
    alpha_row: f64,
    computed_edge_weight: f64,
    row_ep: Vec<f64>,
    row_ap: Vec<f64>,
    col_aq: Vec<f64>,
    col_bfrt: Vec<f64>,
    col_dse: Vec<f64>,
    pack: Vec<(usize, f64)>,
    work_data: Vec<(usize, f64)>,
    work_group: Vec<usize>,
    work_theta: f64,
    flips: Vec<(usize, f64)>,
    free_list: Vec<usize>,
}

impl<'a> Ekk<'a> {
    fn new(lp: &'a Lp, col_lower: &[f64], col_upper: &[f64], opts: Options) -> Self {
        let (n, m) = (lp.num_col, lp.num_row);
        let tot = n + m;
        let mut lp_lower = col_lower.to_vec();
        let mut lp_upper = col_upper.to_vec();
        for i in 0..m {
            lp_lower.push(-lp.row_upper[i]);
            lp_upper.push(-lp.row_lower[i]);
        }
        // Deterministic random values in [0, 1) per variable (HEkk numTotRandomValue_).
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let random = (0..tot)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 11) as f64 / (1u64 << 53) as f64
            })
            .collect();
        Ekk {
            lp,
            n,
            m,
            opts,
            lp_lower,
            lp_upper,
            work_cost: vec![0.0; tot],
            work_shift: vec![0.0; tot],
            work_lower: vec![0.0; tot],
            work_upper: vec![0.0; tot],
            work_range: vec![0.0; tot],
            work_value: vec![0.0; tot],
            work_dual: vec![0.0; tot],
            random,
            basic_index: vec![0; m],
            nonbasic_flag: vec![true; tot],
            nonbasic_move: vec![0; tot],
            base_value: vec![0.0; m],
            base_lower: vec![0.0; m],
            base_upper: vec![0.0; m],
            work_infeasibility: vec![0.0; m],
            edge_weight: vec![1.0; m],
            factor: None,
            pivot_threshold: PIVOT_THRESHOLD,
            costs_perturbed: false,
            num_shifts: 0,
            allow_perturbation: true,
            phase: 2,
            iteration_count: 0,
            iteration_limit: 100_000 + 100 * (n + m),
            rebuild_reason: Rebuild::No,
            fresh_rebuild: false,
            dual_infeas_count: 0,
            row_out: None,
            variable_in: None,
            variable_out: 0,
            delta_primal: 0.0,
            move_out: 0,
            theta_dual: 0.0,
            theta_primal: 0.0,
            alpha_col: 0.0,
            alpha_row: 0.0,
            computed_edge_weight: 1.0,
            row_ep: vec![0.0; m],
            row_ap: vec![0.0; n],
            col_aq: vec![0.0; m],
            col_bfrt: vec![0.0; m],
            col_dse: vec![0.0; m],
            pack: Vec::new(),
            work_data: Vec::new(),
            work_group: Vec::new(),
            work_theta: 0.0,
            flips: Vec::new(),
            free_list: Vec::new(),
        }
    }

    fn inconsistent_bounds(&self) -> Option<usize> {
        (0..self.n + self.m).find(|&j| self.lp_lower[j] > self.lp_upper[j] + PRIMAL_FEASIBILITY_TOLERANCE)
    }

    fn set_basis(&mut self, start: Option<&Basis>) {
        let tot = self.n + self.m;
        let valid = start.is_some_and(|b| {
            b.basic_index.len() == self.m && b.nonbasic_move.len() == tot && b.dual_edge_weight.len() == self.m
        });
        self.nonbasic_flag = vec![true; tot];
        if let (true, Some(b)) = (valid, start) {
            self.basic_index = b.basic_index.clone();
            self.nonbasic_move = b.nonbasic_move.clone();
            self.edge_weight = b.dual_edge_weight.clone();
        } else {
            self.basic_index = (self.n..tot).collect();
            self.nonbasic_move = vec![0; tot];
            self.edge_weight = vec![1.0; self.m];
        }
        for &v in &self.basic_index {
            self.nonbasic_flag[v] = false;
            self.nonbasic_move[v] = 0;
        }
    }

    // ---------------------------------------------------------------- setup

    /// HEkk::initialiseBound
    fn initialise_bound(&mut self, phase: u8) {
        let tot = self.n + self.m;
        self.work_lower.copy_from_slice(&self.lp_lower);
        self.work_upper.copy_from_slice(&self.lp_upper);
        if phase == 1 {
            for j in 0..tot {
                let (lo, hi) = (self.work_lower[j], self.work_upper[j]);
                let (l, u) = match (is_inf(lo), is_inf(hi)) {
                    (true, true) => (-1000.0, 1000.0), // free
                    (true, false) => (-1.0, 0.0),      // upper
                    (false, true) => (0.0, 1.0),       // lower
                    (false, false) => (0.0, 0.0),      // boxed or fixed
                };
                self.work_lower[j] = l;
                self.work_upper[j] = u;
            }
        }
        for j in 0..tot {
            self.work_range[j] = self.work_upper[j] - self.work_lower[j];
        }
        for p in 0..self.m {
            let v = self.basic_index[p];
            self.base_lower[p] = self.work_lower[v];
            self.base_upper[p] = self.work_upper[v];
        }
    }

    /// HEkk::initialiseCost, including dual simplex cost perturbation.
    fn initialise_cost(&mut self, perturb: bool) {
        let (n, tot) = (self.n, self.n + self.m);
        self.work_cost[..n].copy_from_slice(&self.lp.col_cost);
        self.work_cost[n..tot].fill(0.0);
        self.work_shift.iter_mut().for_each(|s| *s = 0.0);
        self.num_shifts = 0;
        self.costs_perturbed = false;
        if !perturb {
            return;
        }
        let mut max_abs_cost = self.work_cost[..n].iter().fold(0.0f64, |a, c| a.max(c.abs()));
        if max_abs_cost == 0.0 {
            max_abs_cost = 1.0;
        }
        if max_abs_cost > 100.0 {
            max_abs_cost = max_abs_cost.sqrt().sqrt();
        }
        let boxed_rate = (0..tot).filter(|&j| self.work_range[j] < 1e30).count() as f64 / tot.max(1) as f64;
        if boxed_rate < 0.01 {
            max_abs_cost = max_abs_cost.min(1.0);
        }
        let base = COST_PERTURBATION_MULTIPLIER * 5e-7 * max_abs_cost;
        for j in 0..n {
            let (lo, hi) = (self.lp_lower[j], self.lp_upper[j]);
            let xpert = (1.0 + self.random[j]) * (self.work_cost[j].abs() + 1.0) * base;
            if is_inf(lo) && is_inf(hi) {
                // free: no perturbation
            } else if is_inf(hi) {
                self.work_cost[j] += xpert;
            } else if is_inf(lo) {
                self.work_cost[j] -= xpert;
            } else if lo != hi {
                self.work_cost[j] += if self.work_cost[j] >= 0.0 { xpert } else { -xpert };
            }
        }
        let row_base = COST_PERTURBATION_MULTIPLIER * 1e-12;
        for j in n..tot {
            self.work_cost[j] += (0.5 - self.random[j]) * row_base;
        }
        self.costs_perturbed = true;
    }

    /// HEkk::initialiseNonbasicValueAndMove
    fn initialise_nonbasic_value_and_move(&mut self) {
        for j in 0..self.n + self.m {
            if !self.nonbasic_flag[j] {
                self.nonbasic_move[j] = 0;
                continue;
            }
            let (lo, hi) = (self.work_lower[j], self.work_upper[j]);
            let original = self.nonbasic_move[j];
            let (value, mv) = if lo == hi {
                (lo, 0)
            } else if !is_inf(lo) {
                if !is_inf(hi) { if original == -1 { (hi, -1) } else { (lo, 1) } } else { (lo, 1) }
            } else if !is_inf(hi) {
                (hi, -1)
            } else {
                (0.0, 0)
            };
            self.work_value[j] = value;
            self.nonbasic_move[j] = mv;
        }
    }

    fn flip_bound(&mut self, j: usize) {
        let mv = -self.nonbasic_move[j];
        self.nonbasic_move[j] = mv;
        self.work_value[j] = if mv == 1 { self.work_lower[j] } else { self.work_upper[j] };
    }

    // ------------------------------------------------------- linear algebra

    fn collect_column(&self, var: usize, mult: f64, out: &mut [f64]) {
        if var < self.n {
            for k in self.lp.a_start[var]..self.lp.a_start[var + 1] {
                out[self.lp.a_index[k]] += mult * self.lp.a_value[k];
            }
        } else {
            out[var - self.n] += mult;
        }
    }

    /// Factorizes the basis, replacing dependent columns by logicals
    /// (HFactor::buildHandleRankDeficiency) and permuting basic_index so that
    /// each basic variable's position is its pivot row.
    fn factorize(&mut self) -> bool {
        for _attempt in 0..10 {
            let (start, index, value) = basis_columns(self.lp, &self.basic_index);
            let built = Factor::build(self.m, &start, &index, &value, self.pivot_threshold);
            if built.singular_cols.is_empty() {
                let mut basic = vec![0; self.m];
                let mut weight = vec![1.0; self.m];
                for (j, &p) in built.position.iter().enumerate() {
                    basic[p] = self.basic_index[j];
                    weight[p] = self.edge_weight[j];
                }
                self.basic_index = basic;
                self.edge_weight = weight;
                for p in 0..self.m {
                    let v = self.basic_index[p];
                    self.base_lower[p] = self.work_lower[v];
                    self.base_upper[p] = self.work_upper[v];
                }
                self.factor = Some(built.factor);
                return true;
            }
            for (&j, &r) in built.singular_cols.iter().zip(&built.unpivoted_rows) {
                let out = self.basic_index[j];
                let logical = self.n + r;
                self.nonbasic_flag[out] = true;
                self.nonbasic_move[out] = 0;
                self.nonbasic_flag[logical] = false;
                self.nonbasic_move[logical] = 0;
                self.basic_index[j] = logical;
                self.edge_weight[j] = 1.0;
            }
            self.initialise_nonbasic_value_and_move();
        }
        false
    }

    fn ftran(&self, v: &mut [f64]) {
        self.factor.as_ref().unwrap().ftran(v);
    }

    fn btran(&self, v: &mut [f64]) {
        self.factor.as_ref().unwrap().btran(v);
    }

    /// HEkk::computePrimal: B x_B = -N x_N
    fn compute_primal(&mut self) {
        let mut rhs = vec![0.0; self.m];
        for j in 0..self.n + self.m {
            if self.nonbasic_flag[j] && self.work_value[j] != 0.0 {
                self.collect_column(j, -self.work_value[j], &mut rhs);
            }
        }
        self.ftran(&mut rhs);
        self.base_value = rhs;
        self.compute_infeasibilities();
    }

    fn compute_infeasibilities(&mut self) {
        for p in 0..self.m {
            self.work_infeasibility[p] = self.primal_infeasibility(p).powi(2);
        }
    }

    fn primal_infeasibility(&self, p: usize) -> f64 {
        let (v, lo, hi) = (self.base_value[p], self.base_lower[p], self.base_upper[p]);
        if v < lo - PRIMAL_FEASIBILITY_TOLERANCE {
            lo - v
        } else if v > hi + PRIMAL_FEASIBILITY_TOLERANCE {
            v - hi
        } else {
            0.0
        }
    }

    /// HEkk::computeDual: y = B^{-T} c_B; d_j = c_j - a_j^T y
    fn compute_dual(&mut self) {
        let mut y: Vec<f64> = self.basic_index.iter().map(|&v| self.work_cost[v] + self.work_shift[v]).collect();
        self.btran(&mut y);
        for j in 0..self.n + self.m {
            if !self.nonbasic_flag[j] {
                self.work_dual[j] = 0.0;
                continue;
            }
            let c = self.work_cost[j] + self.work_shift[j];
            self.work_dual[j] = if j < self.n {
                let mut d = c;
                for k in self.lp.a_start[j]..self.lp.a_start[j + 1] {
                    d -= self.lp.a_value[k] * y[self.lp.a_index[k]];
                }
                d
            } else {
                c - y[j - self.n]
            };
        }
    }

    fn dual_objective(&self) -> f64 {
        (0..self.n + self.m).filter(|&j| self.nonbasic_flag[j]).map(|j| self.work_value[j] * self.work_dual[j]).sum()
    }

    /// Dual infeasibilities that a bound flip cannot remove (free or one-sided
    /// variables), which decide between phase 1 and phase 2.
    fn count_unflippable_dual_infeasibilities(&self) -> usize {
        (0..self.n + self.m)
            .filter(|&j| self.nonbasic_flag[j])
            .filter(|&j| {
                let (lo, hi, d) = (self.work_lower[j], self.work_upper[j], self.work_dual[j]);
                if is_inf(lo) && is_inf(hi) {
                    d.abs() >= DUAL_FEASIBILITY_TOLERANCE
                } else if is_inf(lo) || is_inf(hi) {
                    let mv = if is_inf(hi) { 1.0 } else { -1.0 };
                    -mv * d >= DUAL_FEASIBILITY_TOLERANCE
                } else {
                    false
                }
            })
            .count()
    }

    fn count_dual_infeasibilities(&self) -> usize {
        (0..self.n + self.m)
            .filter(|&j| self.nonbasic_flag[j])
            .filter(|&j| {
                let (lo, hi, d) = (self.work_lower[j], self.work_upper[j], self.work_dual[j]);
                if lo == hi {
                    false
                } else if is_inf(lo) && is_inf(hi) {
                    d.abs() >= DUAL_FEASIBILITY_TOLERANCE
                } else {
                    -(self.nonbasic_move[j] as f64) * d >= DUAL_FEASIBILITY_TOLERANCE
                }
            })
            .count()
    }

    /// HEkkDual::correctDualInfeasibilities: flip boxed variables, shift the
    /// costs of one-sided ones; count free variables that are dual infeasible.
    fn correct_dual_infeasibilities(&mut self) {
        let mut free_infeasible = 0;
        for j in 0..self.n + self.m {
            if !self.nonbasic_flag[j] {
                continue;
            }
            let (lo, hi, d) = (self.work_lower[j], self.work_upper[j], self.work_dual[j]);
            let mv = self.nonbasic_move[j] as f64;
            if is_inf(lo) && is_inf(hi) {
                if d.abs() >= DUAL_FEASIBILITY_TOLERANCE {
                    free_infeasible += 1;
                }
                continue;
            }
            if -mv * d < DUAL_FEASIBILITY_TOLERANCE {
                continue;
            }
            if !is_inf(lo) && !is_inf(hi) {
                if lo != hi {
                    self.flip_bound(j);
                }
                continue;
            }
            // Shift the cost so the dual is just feasible.
            let new_dual = mv * (1.0 + self.random[j]) * DUAL_FEASIBILITY_TOLERANCE;
            let shift = new_dual - d;
            self.work_dual[j] = new_dual;
            if self.work_shift[j] == 0.0 {
                self.num_shifts += 1;
            }
            self.work_shift[j] += shift;
        }
        self.dual_infeas_count = free_infeasible;
    }

    fn compute_dse_weights(&mut self) {
        let mut e = vec![0.0; self.m];
        for p in 0..self.m {
            e.iter_mut().for_each(|x| *x = 0.0);
            e[p] = 1.0;
            self.btran(&mut e);
            self.edge_weight[p] = e.iter().map(|x| x * x).sum::<f64>().max(MIN_DUAL_STEEPEST_EDGE_WEIGHT);
        }
    }

    // ------------------------------------------------------------- driver

    fn timed_out(&self) -> bool {
        self.opts.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// HEkkDual::solve
    fn run(&mut self) -> LpStatus {
        self.initialise_bound(2);
        self.initialise_nonbasic_value_and_move();
        self.initialise_cost(false);
        if !self.factorize() {
            return LpStatus::Error("could not factorize the basis".into());
        }
        let logical_basis = self.basic_index.iter().all(|&v| v >= self.n);
        let weights_given = self.edge_weight.iter().any(|&w| w != 1.0);
        if !logical_basis && !weights_given {
            self.compute_dse_weights();
        }
        // Near-optimal starts (typical in branch and bound) are not perturbed.
        self.compute_dual();
        let unflippable = self.count_unflippable_dual_infeasibilities();
        self.compute_primal();
        let (mut num_pinf, mut max_pinf) = (0usize, 0.0f64);
        for p in 0..self.m {
            let inf = self.primal_infeasibility(p);
            if inf > 0.0 {
                num_pinf += 1;
                max_pinf = max_pinf.max(inf);
            }
        }
        let near_optimal = unflippable == 0 && num_pinf < 1000 && max_pinf < 1e-3;
        self.initialise_cost(!near_optimal && self.allow_perturbation);
        self.compute_dual();
        self.phase = if self.count_unflippable_dual_infeasibilities() > 0 { 1 } else { 2 };

        let mut cleanups = 0;
        loop {
            if self.timed_out() {
                return LpStatus::TimeLimit;
            }
            if self.phase == 1 {
                match self.solve_phase1() {
                    Phase1::Phase2 => self.phase = 2,
                    Phase1::DualInfeasible => return LpStatus::UnboundedOrInfeasible,
                    Phase1::Stop(s) => return s,
                }
                continue;
            }
            match self.solve_phase2() {
                Phase2::Optimal => {
                    if self.costs_perturbed || self.num_shifts > 0 {
                        // HEkkDual::cleanup: remove perturbations and shifts.
                        cleanups += 1;
                        self.initialise_cost(false);
                        self.allow_perturbation = false;
                        self.compute_dual();
                        if self.count_dual_infeasibilities() == 0 || cleanups > MAX_CLEANUP_LEVEL {
                            return LpStatus::Optimal;
                        }
                        self.phase = if self.count_unflippable_dual_infeasibilities() > 0 { 1 } else { 2 };
                        continue;
                    }
                    return LpStatus::Optimal;
                }
                Phase2::BackToPhase1 => self.phase = 1,
                Phase2::Stop(s) => return s,
            }
        }
    }

    fn solve_phase1(&mut self) -> Phase1 {
        self.initialise_bound(1);
        self.initialise_nonbasic_value_and_move();
        loop {
            self.rebuild();
            if self.rebuild_reason == Rebuild::ChooseColumnFail {
                return Phase1::Stop(LpStatus::Error("factorization failed".into()));
            }
            if let Some(s) = self.inner_loop() {
                return Phase1::Stop(s);
            }
            let reason = self.rebuild_reason;
            let fresh = self.fresh_rebuild;
            match reason {
                Rebuild::PossiblyOptimal if fresh => break,
                Rebuild::PossiblyDualUnbounded if fresh => {
                    // Cannot happen with phase-1 boxes unless costs are perturbed.
                    if self.costs_perturbed {
                        self.initialise_cost(false);
                        self.allow_perturbation = false;
                        continue;
                    }
                    return Phase1::Stop(LpStatus::Error("dual phase 1 unbounded".into()));
                }
                Rebuild::ChooseColumnFail => return Phase1::Stop(LpStatus::Error("ratio test failed".into())),
                Rebuild::ExcessivePrimal => {
                    return Phase1::Stop(LpStatus::Error("excessive primal values".into()));
                }
                _ => {}
            }
        }
        // Phase 1 optimal: return to the true bounds and see whether the
        // basis is dual feasible (HEkkDual::assessPhase1Optimality).
        self.initialise_bound(2);
        self.initialise_nonbasic_value_and_move();
        self.compute_dual();
        if self.count_unflippable_dual_infeasibilities() == 0 {
            return Phase1::Phase2;
        }
        if self.costs_perturbed {
            self.initialise_cost(false);
            self.allow_perturbation = false;
            self.compute_dual();
            if self.count_unflippable_dual_infeasibilities() == 0 {
                return Phase1::Phase2;
            }
        }
        Phase1::DualInfeasible
    }

    fn solve_phase2(&mut self) -> Phase2 {
        self.initialise_bound(2);
        self.initialise_nonbasic_value_and_move();
        self.free_list = (0..self.n + self.m)
            .filter(|&j| self.nonbasic_flag[j] && is_inf(self.work_lower[j]) && is_inf(self.work_upper[j]))
            .collect();
        loop {
            self.rebuild();
            if self.rebuild_reason == Rebuild::ChooseColumnFail {
                return Phase2::Stop(LpStatus::Error("factorization failed".into()));
            }
            if self.dual_infeas_count > 0 {
                return Phase2::BackToPhase1;
            }
            if let Some(s) = self.check_objective_bound() {
                return Phase2::Stop(s);
            }
            if let Some(s) = self.inner_loop() {
                return Phase2::Stop(s);
            }
            let fresh = self.fresh_rebuild;
            match self.rebuild_reason {
                Rebuild::PossiblyOptimal if fresh => return Phase2::Optimal,
                // CHUZC found no candidate right after a rebuild: the dual
                // is unbounded, so the primal is infeasible
                // (HEkkDual::assessPossiblyDualUnbounded).
                Rebuild::PossiblyDualUnbounded if fresh => return Phase2::Stop(LpStatus::Infeasible),
                Rebuild::ChooseColumnFail => return Phase2::Stop(LpStatus::Error("ratio test failed".into())),
                Rebuild::ExcessivePrimal => return Phase2::Stop(LpStatus::Error("excessive primal values".into())),
                _ => {}
            }
        }
    }

    fn check_objective_bound(&self) -> Option<LpStatus> {
        if self.phase == 2
            && self.opts.objective_bound < f64::INFINITY
            && !self.costs_perturbed
            && self.num_shifts == 0
            && self.dual_objective() > self.opts.objective_bound
        {
            Some(LpStatus::ObjectiveBound)
        } else {
            None
        }
    }

    /// Iterates until a rebuild is needed. Returns a final status on time
    /// limit, iteration limit or objective bound.
    fn inner_loop(&mut self) -> Option<LpStatus> {
        loop {
            if self.timed_out() {
                return Some(LpStatus::TimeLimit);
            }
            if self.iteration_count >= self.iteration_limit {
                return Some(LpStatus::Error("iteration limit reached".into()));
            }
            self.iterate();
            if self.rebuild_reason != Rebuild::No {
                return None;
            }
            self.fresh_rebuild = false;
            if self.iteration_count % 10 == 0
                && let Some(s) = self.check_objective_bound()
            {
                return Some(s);
            }
        }
    }

    /// HEkkDual::rebuild
    fn rebuild(&mut self) {
        let need_refactor = self.factor.as_ref().is_none_or(|f| f.num_updates() > 0)
            || self.rebuild_reason == Rebuild::PossiblySingular;
        self.rebuild_reason = Rebuild::No;
        if need_refactor && !self.factorize() {
            self.rebuild_reason = Rebuild::ChooseColumnFail;
            return;
        }
        self.compute_dual();
        self.correct_dual_infeasibilities();
        self.compute_primal();
        self.fresh_rebuild = true;
    }

    // ---------------------------------------------------------- iteration

    /// HEkkDual::iterate
    fn iterate(&mut self) {
        self.choose_row();
        if self.rebuild_reason != Rebuild::No {
            return;
        }
        self.choose_column();
        if self.rebuild_reason != Rebuild::No {
            return;
        }
        self.update_ftran_bfrt();
        self.update_ftran();
        self.update_ftran_dse();
        if self.update_verify() {
            return;
        }
        self.update_dual();
        if !self.update_primal() {
            return;
        }
        self.update_pivots();
        self.iteration_count += 1;
    }

    /// HEkkDual::chooseRow with HEkkDualRHS::chooseNormal
    fn choose_row(&mut self) {
        loop {
            let mut best_merit = 0.0;
            let mut best = None;
            for p in 0..self.m {
                let infeas = self.work_infeasibility[p];
                if infeas > 0.0 && best_merit * self.edge_weight[p] < infeas {
                    best_merit = infeas / self.edge_weight[p];
                    best = Some(p);
                }
            }
            let Some(p) = best else {
                self.row_out = None;
                self.rebuild_reason = Rebuild::PossiblyOptimal;
                return;
            };
            self.row_ep.iter_mut().for_each(|x| *x = 0.0);
            self.row_ep[p] = 1.0;
            let mut ep = std::mem::take(&mut self.row_ep);
            self.btran(&mut ep);
            self.row_ep = ep;
            let updated = self.edge_weight[p];
            self.computed_edge_weight = self.row_ep.iter().map(|x| x * x).sum();
            self.edge_weight[p] = self.computed_edge_weight;
            if updated >= ACCEPT_DSE_WEIGHT_THRESHOLD * self.computed_edge_weight {
                self.row_out = Some(p);
                break;
            }
        }
        let p = self.row_out.unwrap();
        self.variable_out = self.basic_index[p];
        self.delta_primal = if self.base_value[p] < self.base_lower[p] {
            self.base_value[p] - self.base_lower[p]
        } else {
            self.base_value[p] - self.base_upper[p]
        };
        self.move_out = if self.delta_primal < 0.0 { -1 } else { 1 };
    }

    fn ta(&self) -> f64 {
        let updates = self.factor.as_ref().map_or(0, |f| f.num_updates());
        if updates < 10 {
            1e-9
        } else if updates < 20 {
            3e-8
        } else {
            1e-6
        }
    }

    /// HEkkDual::chooseColumn: PRICE, then the BFRT ratio test (HEkkDualRow).
    fn choose_column(&mut self) {
        // PRICE: row_ap = row_ep^T A, row-wise
        self.row_ap.iter_mut().for_each(|x| *x = 0.0);
        for i in 0..self.m {
            let y = self.row_ep[i];
            if y != 0.0 {
                for k in self.lp.ar_start[i]..self.lp.ar_start[i + 1] {
                    self.row_ap[self.lp.ar_index[k]] += y * self.lp.ar_value[k];
                }
            }
        }
        self.pack.clear();
        for j in 0..self.n {
            if self.nonbasic_flag[j] && self.row_ap[j] != 0.0 {
                self.pack.push((j, self.row_ap[j]));
            }
        }
        for i in 0..self.m {
            if self.nonbasic_flag[self.n + i] && self.row_ep[i] != 0.0 {
                self.pack.push((self.n + i, self.row_ep[i]));
            }
        }
        // createFreemove
        let ta = self.ta();
        let move_out = self.move_out as f64;
        for idx in 0..self.free_list.len() {
            let j = self.free_list[idx];
            if !self.nonbasic_flag[j] {
                continue;
            }
            let alpha = if j < self.n { self.row_ap[j] } else { self.row_ep[j - self.n] };
            if alpha.abs() > ta {
                self.nonbasic_move[j] = if alpha * move_out > 0.0 { 1 } else { -1 };
            }
        }
        self.variable_in = None;
        let row_scale = self.pack.iter().fold(0.0f64, |a, &(_, v)| a.max(v.abs()));
        loop {
            self.choose_possible();
            if self.work_theta <= 0.0 || self.work_data.is_empty() {
                self.rebuild_reason = Rebuild::PossiblyDualUnbounded;
                break;
            }
            match self.choose_final() {
                None => {
                    self.rebuild_reason = Rebuild::ChooseColumnFail;
                    break;
                }
                Some((pivot, alpha)) => {
                    if alpha.abs() <= PIVOT_GROWTH_TOLERANCE * row_scale.max(1.0) {
                        // Remove the small pivot and repeat CHUZC.
                        self.pack.retain(|&(j, _)| j != pivot);
                        if self.pack.is_empty() {
                            self.rebuild_reason = Rebuild::PossiblyDualUnbounded;
                            break;
                        }
                        continue;
                    }
                    self.variable_in = Some(pivot);
                    self.alpha_row = alpha;
                    self.theta_dual = self.work_theta;
                    break;
                }
            }
        }
        // deleteFreemove
        for idx in 0..self.free_list.len() {
            let j = self.free_list[idx];
            if self.nonbasic_flag[j] {
                self.nonbasic_move[j] = 0;
            }
        }
    }

    /// HEkkDualRow::choosePossible
    fn choose_possible(&mut self) {
        let ta = self.ta();
        let td = DUAL_FEASIBILITY_TOLERANCE;
        let move_out = self.move_out as f64;
        self.work_theta = f64::INFINITY;
        self.work_data.clear();
        for &(j, v) in &self.pack {
            let mv = self.nonbasic_move[j] as f64;
            let alpha = v * move_out * mv;
            if alpha > ta {
                self.work_data.push((j, alpha));
                let relax = self.work_dual[j] * mv + td;
                if self.work_theta * alpha > relax {
                    self.work_theta = relax / alpha;
                }
            }
        }
    }

    /// HEkkDualRow::chooseFinal (quadratic group sort). Returns the entering
    /// variable and its signed pivot value from the row, and sets the flips.
    // The loops below partition `work_data` in place: `work_count` grows while
    // the range `work_count..full_count` (evaluated once) is scanned.
    #[allow(clippy::mut_range_bound)]
    fn choose_final(&mut self) -> Option<(usize, f64)> {
        let td = DUAL_FEASIBILITY_TOLERANCE;
        let total_delta = self.delta_primal.abs();
        // 1. Reduce by large-step BFRT
        let full_count = self.work_data.len();
        let mut work_count = 0;
        let mut total_change = 0.0;
        let mut select_theta = 10.0 * self.work_theta + 1e-7;
        loop {
            for i in work_count..full_count {
                let (j, alpha) = self.work_data[i];
                let tight = self.nonbasic_move[j] as f64 * self.work_dual[j];
                if alpha * select_theta >= tight {
                    self.work_data.swap(work_count, i);
                    work_count += 1;
                    total_change += self.work_range[j] * alpha;
                }
            }
            select_theta *= 10.0;
            if total_change >= total_delta || work_count == full_count {
                break;
            }
        }
        // 2. chooseFinalWorkGroupQuad
        let full_count = work_count;
        let mut work_count = 0;
        let mut total_change = INITIAL_TOTAL_CHANGE;
        let mut select_theta = self.work_theta;
        self.work_group.clear();
        self.work_group.push(0);
        let (mut prev_count, mut prev_remain, mut prev_select) = (work_count, INITIAL_REMAIN_THETA, select_theta);
        while select_theta < MAX_SELECT_THETA {
            let mut remain_theta = INITIAL_REMAIN_THETA;
            for i in work_count..full_count {
                let (j, value) = self.work_data[i];
                let dual = self.nonbasic_move[j] as f64 * self.work_dual[j];
                if dual <= select_theta * value {
                    self.work_data.swap(work_count, i);
                    work_count += 1;
                    total_change += value * self.work_range[j];
                } else if dual + td < remain_theta * value {
                    remain_theta = (dual + td) / value;
                }
            }
            self.work_group.push(work_count);
            select_theta = remain_theta;
            if work_count == prev_count && prev_select == select_theta && prev_remain == remain_theta {
                return None;
            }
            prev_count = work_count;
            prev_remain = remain_theta;
            prev_select = select_theta;
            if total_change >= total_delta || work_count == full_count {
                break;
            }
        }
        if self.work_group.len() <= 1 {
            return None;
        }
        // 3. chooseFinalLargeAlpha
        let max_alpha = self.work_data[..work_count].iter().fold(0.0f64, |a, &(_, v)| a.max(v));
        let final_compare = (0.1 * max_alpha).min(1.0);
        let count_group = self.work_group.len() - 1;
        let mut break_index = None;
        let mut break_group = 0;
        for g in (0..count_group).rev() {
            let mut d_max = 0.0;
            let mut i_max = None;
            for i in self.work_group[g]..self.work_group[g + 1] {
                let (j, v) = self.work_data[i];
                if d_max < v || (d_max == v && i_max.is_some_and(|k: usize| j < self.work_data[k].0)) {
                    d_max = v;
                    i_max = Some(i);
                }
            }
            if let Some(k) = i_max
                && self.work_data[k].1 > final_compare
            {
                break_index = Some(k);
                break_group = g;
                break;
            }
        }
        let break_index = break_index?;
        let pivot = self.work_data[break_index].0;
        let mv = self.nonbasic_move[pivot] as f64;
        let alpha = self.work_data[break_index].1 * self.move_out as f64 * mv;
        self.work_theta = if self.work_dual[pivot] * mv > 0.0 { self.work_dual[pivot] / alpha } else { 0.0 };
        // 4. Flips: everything in the groups before the break group
        self.flips.clear();
        if self.work_theta != 0.0 {
            for i in 0..self.work_group[break_group] {
                let j = self.work_data[i].0;
                self.flips.push((j, self.nonbasic_move[j] as f64 * self.work_range[j]));
            }
        }
        self.flips.sort_unstable_by_key(|&(j, _)| j);
        Some((pivot, alpha))
    }

    /// HEkkDual::updateFtranBFRT with HEkkDualRow::updateFlip
    fn update_ftran_bfrt(&mut self) {
        self.col_bfrt.iter_mut().for_each(|x| *x = 0.0);
        if self.flips.is_empty() {
            return;
        }
        let flips = std::mem::take(&mut self.flips);
        let mut col = std::mem::take(&mut self.col_bfrt);
        for &(j, change) in &flips {
            self.flip_bound(j);
            self.collect_column(j, change, &mut col);
        }
        self.ftran(&mut col);
        self.col_bfrt = col;
        self.flips = flips;
    }

    fn update_ftran(&mut self) {
        let q = self.variable_in.unwrap();
        let mut col = std::mem::take(&mut self.col_aq);
        col.iter_mut().for_each(|x| *x = 0.0);
        self.collect_column(q, 1.0, &mut col);
        self.ftran(&mut col);
        self.col_aq = col;
        self.alpha_col = self.col_aq[self.row_out.unwrap()];
    }

    fn update_ftran_dse(&mut self) {
        let mut tau = std::mem::take(&mut self.col_dse);
        tau.copy_from_slice(&self.row_ep);
        self.ftran(&mut tau);
        self.col_dse = tau;
    }

    /// HEkkDual::updateVerify / HEkk::reinvertOnNumericalTrouble
    fn update_verify(&mut self) -> bool {
        let (ac, ar) = (self.alpha_col.abs(), self.alpha_row.abs());
        let measure = (ac - ar).abs() / ac.min(ar);
        let updates = self.factor.as_ref().map_or(0, |f| f.num_updates());
        if (measure > NUMERICAL_TROUBLE_TOLERANCE || !measure.is_finite()) && updates > 0 {
            if self.pivot_threshold < MAX_PIVOT_THRESHOLD && updates < 10 {
                self.pivot_threshold = (self.pivot_threshold * 5.0).min(MAX_PIVOT_THRESHOLD);
            }
            self.rebuild_reason = Rebuild::PossiblySingular;
            return true;
        }
        if !measure.is_finite() || ac == 0.0 {
            self.rebuild_reason = Rebuild::PossiblySingular;
            return true;
        }
        false
    }

    /// HEkkDual::updateDual
    fn update_dual(&mut self) {
        let q = self.variable_in.unwrap();
        if self.theta_dual == 0.0 {
            // shiftCost
            let amount = -self.work_dual[q];
            if amount != 0.0 {
                if self.work_shift[q] == 0.0 {
                    self.num_shifts += 1;
                }
                self.work_shift[q] += amount;
            }
        } else {
            for &(j, alpha) in &self.pack {
                self.work_dual[j] -= self.theta_dual * alpha;
            }
        }
        self.work_dual[q] = 0.0;
        let out = self.variable_out;
        self.work_dual[out] = -self.theta_dual;
        // shiftBack
        if self.work_shift[out] != 0.0 {
            self.work_dual[out] -= self.work_shift[out];
            self.work_shift[out] = 0.0;
            self.num_shifts -= 1;
        }
    }

    /// HEkkDual::updatePrimal, with the DSE weight update
    /// (HEkk::updateDualSteepestEdgeWeights).
    fn update_primal(&mut self) -> bool {
        let p = self.row_out.unwrap();
        for i in 0..self.m {
            if self.col_bfrt[i] != 0.0 {
                self.base_value[i] -= self.col_bfrt[i];
            }
        }
        let bound = if self.delta_primal < 0.0 { self.base_lower[p] } else { self.base_upper[p] };
        self.theta_primal = (self.base_value[p] - bound) / self.alpha_col;
        let mut excessive = false;
        for i in 0..self.m {
            let a = self.col_aq[i];
            if a != 0.0 {
                self.base_value[i] -= self.theta_primal * a;
            }
            if self.base_value[i].abs() >= EXCESSIVE_PRIMAL_VALUE {
                excessive = true;
            }
        }
        if excessive {
            self.rebuild_reason = Rebuild::ExcessivePrimal;
            return false;
        }
        let new_pivotal_weight = self.edge_weight[p] / (self.alpha_col * self.alpha_col);
        let kai = -2.0 / self.alpha_col;
        for i in 0..self.m {
            let aa = self.col_aq[i];
            if aa != 0.0 {
                let w = self.edge_weight[i] + aa * (new_pivotal_weight * aa + kai * self.col_dse[i]);
                self.edge_weight[i] = w.max(MIN_DUAL_STEEPEST_EDGE_WEIGHT);
            }
        }
        self.edge_weight[p] = new_pivotal_weight;
        self.compute_infeasibilities();
        true
    }

    /// HEkk::updatePivots plus the factor update.
    fn update_pivots(&mut self) {
        let p = self.row_out.unwrap();
        let q = self.variable_in.unwrap();
        let out = self.basic_index[p];
        self.basic_index[p] = q;
        self.nonbasic_flag[q] = false;
        self.nonbasic_move[q] = 0;
        self.base_lower[p] = self.work_lower[q];
        self.base_upper[p] = self.work_upper[q];
        self.nonbasic_flag[out] = true;
        if self.work_lower[out] == self.work_upper[out] {
            self.work_value[out] = self.work_lower[out];
            self.nonbasic_move[out] = 0;
        } else if self.move_out == -1 {
            self.work_value[out] = self.work_lower[out];
            self.nonbasic_move[out] = 1;
        } else {
            self.work_value[out] = self.work_upper[out];
            self.nonbasic_move[out] = -1;
        }
        if is_inf(self.work_lower[out]) && is_inf(self.work_upper[out]) {
            self.free_list.push(out);
        }
        self.base_value[p] = self.work_value[q] + self.theta_primal;
        self.work_infeasibility[p] = self.primal_infeasibility(p).powi(2);
        let col = std::mem::take(&mut self.col_aq);
        let f = self.factor.as_mut().unwrap();
        f.update(&col, p);
        let updates = f.num_updates();
        self.col_aq = col;
        if updates >= UPDATE_LIMIT {
            self.rebuild_reason = Rebuild::UpdateLimit;
        }
    }

    // --------------------------------------------------------------- result

    fn finish(mut self, status: LpStatus) -> LpSolution {
        let mut x = vec![0.0; self.n];
        let has_values = matches!(status, LpStatus::Optimal) && self.factor.is_some();
        if has_values {
            // Recompute primal values from scratch with true bounds.
            if self.factor.as_ref().is_some_and(|f| f.num_updates() > 0) && self.factorize() {
                self.compute_primal();
            }
            let mut pos = vec![usize::MAX; self.n + self.m];
            for (p, &v) in self.basic_index.iter().enumerate() {
                pos[v] = p;
            }
            for (j, xj) in x.iter_mut().enumerate() {
                *xj = if self.nonbasic_flag[j] { self.work_value[j] } else { self.base_value[pos[j]] };
            }
        }
        let objective = self.lp.objective(&x);
        let mut col_dual = Vec::new();
        if has_values {
            // y = B^-T c_B with the true costs, then d = c - A^T y.
            let mut y: Vec<f64> =
                self.basic_index.iter().map(|&v| if v < self.n { self.lp.col_cost[v] } else { 0.0 }).collect();
            self.btran(&mut y);
            col_dual = (0..self.n)
                .map(|j| {
                    let mut d = self.lp.col_cost[j];
                    for k in self.lp.a_start[j]..self.lp.a_start[j + 1] {
                        d -= self.lp.a_value[k] * y[self.lp.a_index[k]];
                    }
                    d
                })
                .collect();
        }
        let basis = (self.factor.is_some() && self.basic_index.len() == self.m).then(|| Basis {
            basic_index: self.basic_index.clone(),
            nonbasic_move: self.nonbasic_move.clone(),
            dual_edge_weight: self.edge_weight.clone(),
        });
        LpSolution { status, col_value: x, objective, basis, iterations: self.iteration_count, col_dual }
    }
}

/// The basis matrix's columns (structural columns of `lp`, or unit columns
/// for logicals), column-wise: starts, row indices, and values.
fn basis_columns(lp: &Lp, basic_index: &[usize]) -> (Vec<usize>, Vec<usize>, Vec<f64>) {
    let n = lp.num_col;
    let mut start = Vec::with_capacity(basic_index.len() + 1);
    let (mut index, mut value) = (Vec::new(), Vec::new());
    start.push(0);
    for &v in basic_index {
        if v < n {
            index.extend_from_slice(&lp.a_index[lp.a_start[v]..lp.a_start[v + 1]]);
            value.extend_from_slice(&lp.a_value[lp.a_start[v]..lp.a_start[v + 1]]);
        } else {
            index.push(v - n);
            value.push(1.0);
        }
        start.push(index.len());
    }
    (start, index, value)
}

/// Rows of the simplex tableau for `basis`, a basis of `lp`. For each basic
/// variable v in `vars`, returns the coefficients alpha over the nonbasic
/// variables with x_v + sum(alpha_k * x_k) = 0, using the [A I] x = 0
/// convention of this module (variables n + i are the row logicals). Returns
/// None if the basis matrix is singular or some v is not basic.
pub fn tableau_rows(lp: &Lp, basis: &Basis, vars: &[usize]) -> Option<Vec<Vec<(usize, f64)>>> {
    const DROP: f64 = 1e-11;
    let (n, m) = (lp.num_col, lp.num_row);
    if basis.basic_index.len() != m {
        return None;
    }
    let (start, index, value) = basis_columns(lp, &basis.basic_index);
    let built = Factor::build(m, &start, &index, &value, PIVOT_THRESHOLD);
    if !built.singular_cols.is_empty() {
        return None;
    }
    let mut position = vec![usize::MAX; n + m];
    for (j, &v) in basis.basic_index.iter().enumerate() {
        position[v] = built.position[j];
    }
    let mut out = Vec::with_capacity(vars.len());
    for &v in vars {
        let p = *position.get(v)?;
        if p == usize::MAX {
            return None;
        }
        // Row p of B^-1 is (B^-T e_p)^T, and x_B = -B^-1 N x_N.
        let mut rho = vec![0.0; m];
        rho[p] = 1.0;
        built.factor.btran(&mut rho);
        let mut row = Vec::new();
        for k in (0..n).filter(|&k| position[k] == usize::MAX) {
            let a: f64 = (lp.a_start[k]..lp.a_start[k + 1]).map(|q| rho[lp.a_index[q]] * lp.a_value[q]).sum();
            if a.abs() > DROP {
                row.push((k, a));
            }
        }
        for (i, &r) in rho.iter().enumerate() {
            if position[n + i] == usize::MAX && r.abs() > DROP {
                row.push((n + i, r));
            }
        }
        out.push(row);
    }
    Some(out)
}

enum Phase1 {
    Phase2,
    DualInfeasible,
    Stop(LpStatus),
}

enum Phase2 {
    Optimal,
    BackToPhase1,
    Stop(LpStatus),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LinearModel, Row, Sense};

    /// (coefficients, lower, upper)
    type TestRow = (Vec<(usize, f64)>, f64, f64);

    fn model(obj: Vec<f64>, lower: Vec<f64>, upper: Vec<f64>, rows: Vec<TestRow>, sense: Sense) -> LinearModel {
        let n = obj.len();
        LinearModel {
            col_lower: lower,
            col_upper: upper,
            col_integer: vec![false; n],
            col_domain: vec![None; n],
            rows: rows.into_iter().map(|(coefs, lower, upper)| Row { coefs, lower, upper }).collect(),
            obj,
            obj_offset: 0.0,
            sense,
            want_all: false,
            vars: vec![],
            violated_constant_row: None,
            uses_cp_kinds: false,
            scheduling: Vec::new(),
            nonlinear: None,
            col_start: Vec::new(),
        }
    }

    /// min x + 2y + 3z s.t. x + y >= 1, y + z >= 1, x, y, z >= 0: the
    /// optimum is y = 1 (cost 2), with reduced costs c - A^T y for the duals
    /// of the two rows; at the optimum, the basic y has 0, and x and z have
    /// cost minus their rows' duals, which are >= 0 since x, z are at their
    /// lower bounds.
    #[test]
    fn reduced_costs_at_an_optimum() {
        let lp = Lp::new(
            vec![1.0, 2.0, 3.0],
            vec![0.0; 3],
            vec![f64::INFINITY; 3],
            vec![(vec![(0, 1.0), (1, 1.0)], 1.0, f64::INFINITY), (vec![(1, 1.0), (2, 1.0)], 1.0, f64::INFINITY)],
        );
        let s = solve(&lp, &lp.col_lower, &lp.col_upper, None, &Options::default());
        assert_eq!(s.status, LpStatus::Optimal);
        assert!((s.objective - 2.0).abs() < 1e-9);
        let d = &s.col_dual;
        assert!(d[1].abs() < 1e-9, "{d:?}");
        assert!(d[0] >= -1e-9 && d[2] >= -1e-9, "{d:?}");
        // Duals y1 + y2 = 2 (y's cost), d_x = 1 - y1, d_z = 3 - y2: d_x + d_z = 2.
        assert!((d[0] + d[2] - 2.0).abs() < 1e-9, "{d:?}");
    }

    fn run(m: &LinearModel) -> LpSolution {
        let lp = Lp::from_model(m, false);
        solve(&lp, &lp.col_lower, &lp.col_upper, None, &Options::default())
    }

    const INF: f64 = f64::INFINITY;

    #[test]
    fn production_example() {
        // max 45c + 80t: 3c + 5t <= 150, c + 2t <= 50, 0 <= c <= 40, 0 <= t <= 30
        let m = model(
            vec![45.0, 80.0],
            vec![0.0, 0.0],
            vec![40.0, 30.0],
            vec![(vec![(0, 3.0), (1, 5.0)], -INF, 150.0), (vec![(0, 1.0), (1, 2.0)], -INF, 50.0)],
            Sense::Maximize,
        );
        let s = run(&m);
        assert_eq!(s.status, LpStatus::Optimal);
        assert!((s.col_value[0] - 40.0).abs() < 1e-7 && (s.col_value[1] - 5.0).abs() < 1e-7);
        assert!((s.objective + 2200.0).abs() < 1e-6);
    }

    #[test]
    fn needs_phase1_free_and_equality() {
        // min x + y, x free, y >= 0: x - y == 3, x + 2y >= 0  => x=2, y=-1? no, y>=0: x=3,y=0 obj 3
        // but x + 2y >= 0 not binding; with x free min x+y s.t. x = 3 + y -> 3 + 2y, y=0
        let m = model(
            vec![1.0, 1.0],
            vec![-INF, 0.0],
            vec![INF, INF],
            vec![(vec![(0, 1.0), (1, -1.0)], 3.0, 3.0), (vec![(0, 1.0), (1, 2.0)], 0.0, INF)],
            Sense::Minimize,
        );
        let s = run(&m);
        assert_eq!(s.status, LpStatus::Optimal);
        assert!((s.objective - 3.0).abs() < 1e-7, "{}", s.objective);
    }

    #[test]
    fn detects_infeasible() {
        // x + y <= 1, x + y >= 2
        let m = model(
            vec![1.0, 1.0],
            vec![0.0, 0.0],
            vec![INF, INF],
            vec![(vec![(0, 1.0), (1, 1.0)], -INF, 1.0), (vec![(0, 1.0), (1, 1.0)], 2.0, INF)],
            Sense::Minimize,
        );
        assert_eq!(run(&m).status, LpStatus::Infeasible);
    }

    #[test]
    fn detects_unbounded_or_infeasible() {
        // max x + y, x - y <= 1, x,y >= 0
        let m = model(
            vec![1.0, 1.0],
            vec![0.0, 0.0],
            vec![INF, INF],
            vec![(vec![(0, 1.0), (1, -1.0)], -INF, 1.0)],
            Sense::Maximize,
        );
        assert_eq!(run(&m).status, LpStatus::UnboundedOrInfeasible);
    }

    #[test]
    fn warm_start_after_bound_change() {
        let m = model(
            vec![45.0, 80.0],
            vec![0.0, 0.0],
            vec![40.0, 30.0],
            vec![(vec![(0, 3.0), (1, 5.0)], -INF, 150.0), (vec![(0, 1.0), (1, 2.0)], -INF, 50.0)],
            Sense::Maximize,
        );
        let lp = Lp::from_model(&m, false);
        let s = solve(&lp, &lp.col_lower, &lp.col_upper, None, &Options::default());
        let upper = vec![40.0, 4.0];
        let s2 = solve(&lp, &lp.col_lower, &upper, s.basis.as_ref(), &Options::default());
        assert_eq!(s2.status, LpStatus::Optimal);
        // t <= 4: c = 40 (c + 2t <= 50 ok), obj 1800 + 320
        assert!((s2.objective + 2120.0).abs() < 1e-6, "{}", s2.objective);
    }
}
