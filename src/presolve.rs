//! Presolve: reductions applied to a [`Milp`] before branch and bound.
//!
//! A removed row is implied by what remains. A removed column has a fixed
//! value, or (in an LP) a value that an equality row gives it from other
//! columns' values. So restoring a solution means re-inserting the fixed
//! values and then working the substitutions back, last to first
//! ([`Postsolve::restore`]).
//!
//! Each pass applies, until nothing changes:
//! - Integer bounds are rounded (and snapped to the column's domain).
//! - Fixed columns are substituted into their rows and removed.
//! - Empty rows are dropped, or prove infeasibility.
//! - Singleton rows become column bounds.
//! - Activity bounds over the column bounds show a row to be infeasible,
//!   redundant (dropped), or forcing (every column fixed at the bound that
//!   attains it).
//! - Implied bounds: each row bounds each of its columns given the others'
//!   bounds. Integer bounds are rounded; a finite continuous bound is only
//!   tightened when the gain is significant.
//! - Congruences on equality rows over integer columns: after scaling
//!   decimal coefficients to integers, a column k can only take values with
//!   a_k x_k = b modulo the gcd of the other coefficients. For columns with a
//!   small range, the values that fail are cut from the bounds; none left
//!   proves infeasibility. For example `2x - 4y + 8.25z = 9.5` with z binary
//!   is infeasible, which branch and bound alone can't show when x and y are
//!   unbounded.
//! - Coefficient tightening on binary columns in one-sided rows: in
//!   `make - M * on <= 0` with `make <= U < M`, M becomes U. This keeps every
//!   integer-feasible point and strengthens the LP relaxation.
//! - Substitutions, in an LP only (HiGHS's doubleton equations and free
//!   column substitution): an equality row gives one of its columns in
//!   terms of the others, x_s = (b - sum a_k x_k) / a_s, and that takes the
//!   column out of every other row and of the objective. If the column's
//!   bounds are implied by the row and the other columns' bounds, the row
//!   goes with it; otherwise the row stays, as a range on the other
//!   columns that stands for those bounds. A column is substituted when
//!   the equality has two columns, or when the row goes and the entries
//!   added are about as many as those removed. Measured on Netlib (see
//!   CLAUDE.md): a slack column whose bounds aren't implied is better left
//!   alone (dfl001 took 50% more iterations with its rows as ranges), and
//!   so are the continuous columns of a MILP, whose rows the cuts read
//!   (lot sizing needed twice the nodes).
//! - Implied bounds aren't applied in an LP: see `process_row`.
//! - Dual reductions (only with `Options::dual`): a column whose cost does
//!   not favor moving away from a finite bound, and that no row prevents
//!   from moving to it (no "locks" in that direction), is fixed at that bound.
//!   This keeps at least one optimal solution, but not necessarily all, so
//!   it is off when every optimal solution is wanted.
//!
//! The primal reductions keep the integer-feasible set exactly: a point is
//! feasible for the reduced problem (restored) if and only if it is feasible
//! for the original.

use crate::lp::{Lp, SparseRow};
use crate::milp::{Domain, Milp, ceil_allowed, floor_allowed};

const MAX_PASSES: usize = 50;
/// Feasibility tolerance, relative to max(1, |bound|).
const FEASIBILITY: f64 = 1e-7;
/// A row counts as redundant when its activity range is inside its bounds to this (relative) tolerance.
const REDUNDANCY: f64 = 1e-9;
/// Tolerance when rounding an implied bound on an integer column.
const INTEGRAL: f64 = 1e-6;
/// A finite continuous bound is tightened only by more than this fraction of max(1, range).
const CONTINUOUS_MIN_GAIN: f64 = 1e-3;
/// Implied bounds beyond this magnitude are ignored as numerically meaningless.
const MAX_BOUND: f64 = 1e15;
/// A coefficient is tightened only by more than this, relative to max(1, |a|).
const COEFFICIENT_MIN_GAIN: f64 = 1e-6;
/// Columns with at most this many values get the congruence check.
const CONGRUENCE_MAX_RANGE: f64 = 64.0;
/// A column is substituted through an equality row only if its coefficient
/// there is at least this fraction of the row's largest and of the column's
/// largest, so that the multipliers stay moderate.
const SUBSTITUTION_PIVOT: f64 = 1e-3;
/// A substitution that removes its row may add this many entries more than
/// it removes.
const SUBSTITUTION_FILL: usize = 3;
/// Entries a substitution leaves smaller than this are cancellations.
const CANCELLED: f64 = 1e-12;
/// Scale factors tried to make a row's decimal coefficients integral.
const INTEGER_SCALES: [f64; 9] = [1.0, 2.0, 4.0, 8.0, 10.0, 100.0, 1e3, 1e4, 1e6];

pub struct Options {
    /// Allow dual reductions (which may remove some, but not all, optimal solutions).
    pub dual: bool,
    /// Columns that constraints outside the rows also use (the CP engine's
    /// native scheduling constraints), so that dual reductions can't fix
    /// them. Empty means none.
    pub frozen: Vec<bool>,
}

/// What presolve did (printed with `OPTIMIZAM_DEBUG`).
#[derive(Debug, Default)]
pub struct Stats {
    pub passes: usize,
    pub removed_rows: usize,
    pub removed_cols: usize,
    pub substituted_cols: usize,
    pub tightened_bounds: usize,
    pub tightened_coefs: usize,
    pub forcing_rows: usize,
    pub dual_fixed: usize,
}

/// Maps a solution of the reduced problem back to the original columns.
pub struct Postsolve {
    /// Original column of each reduced column.
    kept: Vec<usize>,
    /// Values of the removed columns (indexed by original column).
    value: Vec<f64>,
    /// The columns substituted out, in the order they were.
    substitutions: Vec<Substitution>,
    /// Objective contribution of the removed columns, in the LP's costs.
    pub offset: f64,
}

/// A column removed by substitution: x[col] = constant + sum of coef *
/// x[other] over `terms` (original columns, some of which later
/// substitutions may have removed in turn).
struct Substitution {
    col: usize,
    constant: f64,
    terms: Vec<(usize, f64)>,
}

impl Postsolve {
    /// A presolve that changed nothing in a problem with `n` columns.
    fn identity(n: usize) -> Postsolve {
        Postsolve { kept: (0..n).collect(), value: vec![0.0; n], substitutions: Vec::new(), offset: 0.0 }
    }

    pub fn restore(&self, x: &[f64]) -> Vec<f64> {
        let mut full = self.value.clone();
        for (k, &j) in self.kept.iter().enumerate() {
            full[j] = x[k];
        }
        for s in self.substitutions.iter().rev() {
            full[s.col] = s.constant + s.terms.iter().map(|&(k, c)| c * full[k]).sum::<f64>();
        }
        full
    }

    /// Where an original column went: its reduced column, or its fixed
    /// value. (Not for substituted columns, which only LPs have.)
    pub fn column(&self, j: usize) -> Result<usize, f64> {
        debug_assert!(self.substitutions.iter().all(|s| s.col != j));
        self.kept.binary_search(&j).map_err(|_| self.value[j])
    }
}

// Built once per solve, so the size difference between variants doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum Presolved {
    Infeasible,
    Reduced(Milp, Postsolve),
}

struct Infeasible;

fn tol(scale: f64, rel: f64) -> f64 {
    rel * scale.abs().max(1.0)
}

struct State {
    rows: Vec<Vec<(usize, f64)>>,
    lo: Vec<f64>,
    up: Vec<f64>,
    row_alive: Vec<bool>,
    cost: Vec<f64>,
    cl: Vec<f64>,
    cu: Vec<f64>,
    integer: Vec<bool>,
    domain: Vec<Domain>,
    col_alive: Vec<bool>,
    /// No column is integer: the problem is an LP.
    linear: bool,
    substitutions: Vec<Substitution>,
    offset: f64,
    changed: bool,
    stats: Stats,
}

/// Activity range of a row over the column bounds: finite parts and counts of
/// infinite contributions.
struct Activity {
    min: f64,
    min_inf: usize,
    max: f64,
    max_inf: usize,
}

impl Activity {
    fn min(&self) -> f64 {
        if self.min_inf > 0 { f64::NEG_INFINITY } else { self.min }
    }
    fn max(&self) -> f64 {
        if self.max_inf > 0 { f64::INFINITY } else { self.max }
    }
}

impl State {
    fn binary(&self, j: usize) -> bool {
        self.integer[j] && self.cl[j] == 0.0 && self.cu[j] == 1.0
    }

    /// The column's (min, max) contribution a * x over its bounds.
    fn contribution(&self, j: usize, a: f64) -> (f64, f64) {
        let (l, u) = (self.cl[j], self.cu[j]);
        if a > 0.0 { (a * l, a * u) } else { (a * u, a * l) }
    }

    fn activity(&self, i: usize) -> Activity {
        let mut act = Activity { min: 0.0, min_inf: 0, max: 0.0, max_inf: 0 };
        for &(j, a) in &self.rows[i] {
            let (lo, hi) = self.contribution(j, a);
            if lo.is_finite() {
                act.min += lo
            } else {
                act.min_inf += 1
            }
            if hi.is_finite() { act.max += hi } else { act.max_inf += 1 }
        }
        act
    }

    fn remove_row(&mut self, i: usize) {
        self.row_alive[i] = false;
        self.stats.removed_rows += 1;
        self.changed = true;
    }

    /// Tightens column j to [l, u] where that is an improvement worth making.
    /// `exact` forces continuous bounds to be applied (for singleton rows).
    fn tighten(&mut self, j: usize, mut l: f64, mut u: f64, exact: bool) -> Result<(), Infeasible> {
        if l.abs() > MAX_BOUND && l.is_finite() {
            l = f64::NEG_INFINITY;
        }
        if u.abs() > MAX_BOUND && u.is_finite() {
            u = f64::INFINITY;
        }
        if self.integer[j] {
            l = ceil_allowed(&self.domain[j], l - INTEGRAL);
            u = floor_allowed(&self.domain[j], u + INTEGRAL);
        }
        let (ol, ou) = (self.cl[j], self.cu[j]);
        let range = if ol.is_finite() && ou.is_finite() { ou - ol } else { 0.0 };
        let worth = |old: f64, new: f64, raise: bool| {
            let gain = if raise { new - old } else { old - new };
            // NaN when both bounds are the same infinity.
            if gain.is_nan() || gain <= 0.0 {
                return false;
            }
            exact || self.integer[j] || !old.is_finite() || gain > CONTINUOUS_MIN_GAIN * range.max(1.0)
        };
        if worth(ol, l, true) {
            self.cl[j] = l;
            self.stats.tightened_bounds += 1;
            self.changed = true;
        }
        if worth(ou, u, false) {
            self.cu[j] = u;
            self.stats.tightened_bounds += 1;
            self.changed = true;
        }
        let (l, u) = (self.cl[j], self.cu[j]);
        if l > u {
            if l - u > tol(l.abs().max(u.abs()), FEASIBILITY) || self.integer[j] {
                return Err(Infeasible);
            }
            // Crossed within tolerance: fix at the midpoint.
            let mid = 0.5 * (l + u);
            (self.cl[j], self.cu[j]) = (mid, mid);
        }
        Ok(())
    }

    /// Removes fixed columns, substituting their values into the rows.
    fn remove_fixed_columns(&mut self) {
        let n = self.cl.len();
        let fixed: Vec<bool> = (0..n).map(|j| self.col_alive[j] && self.cl[j] == self.cu[j]).collect();
        if !fixed.iter().any(|&f| f) {
            return;
        }
        for j in (0..n).filter(|&j| fixed[j]) {
            self.col_alive[j] = false;
            self.offset += self.cost[j] * self.cl[j];
            self.stats.removed_cols += 1;
        }
        for i in (0..self.rows.len()).filter(|&i| self.row_alive[i]) {
            if !self.rows[i].iter().any(|&(j, _)| fixed[j]) {
                continue;
            }
            let mut shift = 0.0;
            self.rows[i].retain(|&(j, a)| {
                if fixed[j] {
                    shift += a * self.cl[j];
                    false
                } else {
                    true
                }
            });
            self.lo[i] -= shift;
            self.up[i] -= shift;
        }
        self.changed = true;
    }

    fn process_row(&mut self, i: usize) -> Result<(), Infeasible> {
        let (lo, up) = (self.lo[i], self.up[i]);
        match self.rows[i].len() {
            0 => {
                if lo > tol(lo, FEASIBILITY) || up < -tol(up, FEASIBILITY) {
                    return Err(Infeasible);
                }
                self.remove_row(i);
                return Ok(());
            }
            1 => {
                let (j, a) = self.rows[i][0];
                let (l, u) = if a > 0.0 { (lo / a, up / a) } else { (up / a, lo / a) };
                self.tighten(j, l, u, true)?;
                self.remove_row(i);
                return Ok(());
            }
            _ => {}
        }

        let act = self.activity(i);
        let (amin, amax) = (act.min(), act.max());
        if amin > up + tol(up, FEASIBILITY) || amax < lo - tol(lo, FEASIBILITY) {
            return Err(Infeasible);
        }
        if amin >= lo - tol(lo, REDUNDANCY) && amax <= up + tol(up, REDUNDANCY) {
            self.remove_row(i);
            return Ok(());
        }
        // Forcing: the row can only be satisfied with every column at the
        // bound that attains the activity limit.
        let force_min = up.is_finite() && amin.is_finite() && amin >= up - tol(up, REDUNDANCY);
        let force_max = lo.is_finite() && amax.is_finite() && amax <= lo + tol(lo, REDUNDANCY);
        if force_min || force_max {
            for (j, a) in self.rows[i].clone() {
                let at_lower = (a > 0.0) == force_min;
                let v = if at_lower { self.cl[j] } else { self.cu[j] };
                self.cl[j] = v;
                self.cu[j] = v;
            }
            self.stats.forcing_rows += 1;
            self.remove_row(i);
            return Ok(());
        }

        if lo == up {
            self.congruences(i)?;
        }

        // Implied bounds. Not for an LP: there they tell the simplex method
        // nothing the rows don't, and every bound they add is one more
        // place for a vertex to be degenerate (dfl001 took 15% more
        // iterations with them).
        if self.linear {
            return Ok(());
        }
        for (j, a) in self.rows[i].clone() {
            if a == 0.0 {
                continue;
            }
            let (cmin, cmax) = self.contribution(j, a);
            let others_min = match (act.min_inf, cmin.is_finite()) {
                (0, _) => act.min - cmin,
                (1, false) => act.min,
                _ => f64::NEG_INFINITY,
            };
            let others_max = match (act.max_inf, cmax.is_finite()) {
                (0, _) => act.max - cmax,
                (1, false) => act.max,
                _ => f64::INFINITY,
            };
            let (mut l, mut u) = (f64::NEG_INFINITY, f64::INFINITY);
            if up.is_finite() && others_min.is_finite() {
                let b = (up - others_min) / a;
                if a > 0.0 { u = b } else { l = b }
            }
            if lo.is_finite() && others_max.is_finite() {
                let b = (lo - others_max) / a;
                if a > 0.0 { l = l.max(b) } else { u = u.min(b) }
            }
            self.tighten(j, l, u, false)?;
        }

        self.tighten_coefficients(i);
        Ok(())
    }

    /// The congruence check on an equality row whose columns are all integer.
    fn congruences(&mut self, i: usize) -> Result<(), Infeasible> {
        let row = &self.rows[i];
        if !row.iter().all(|&(j, _)| self.integer[j]) {
            return Ok(());
        }
        let b = self.lo[i];
        let near_int = |v: f64| v.abs() < 1e15 && (v - v.round()).abs() <= 1e-9 * v.abs().max(1.0);
        let Some(scale) = INTEGER_SCALES.into_iter().find(|&s| row.iter().all(|&(_, a)| near_int(a * s))) else {
            return Ok(());
        };
        let coefs: Vec<i64> = row.iter().map(|&(_, a)| (a * scale).round() as i64).collect();
        let rhs = b * scale;
        if !near_int(rhs) {
            return Err(Infeasible); // integer left side, fractional right side
        }
        let rhs = rhs.round() as i64;
        // gcd of all coefficients except k, from prefix and suffix gcds.
        let k_len = coefs.len();
        let (mut prefix, mut suffix) = (vec![0i64; k_len + 1], vec![0i64; k_len + 1]);
        for k in 0..k_len {
            prefix[k + 1] = gcd(prefix[k], coefs[k].abs());
            suffix[k_len - 1 - k] = gcd(suffix[k_len - k], coefs[k_len - 1 - k].abs());
        }
        if rhs % prefix[k_len] != 0 {
            return Err(Infeasible);
        }
        for k in 0..k_len {
            let g = gcd(prefix[k], suffix[k + 1]);
            let j = self.rows[i][k].0;
            let (l, u) = (self.cl[j], self.cu[j]);
            if g <= 1 || !(l.is_finite() && u.is_finite()) || u - l >= CONGRUENCE_MAX_RANGE {
                continue;
            }
            let ok = |v: i64| (coefs[k] * v - rhs).rem_euclid(g) == 0 && in_domain_or_any(&self.domain[j], v);
            let values: Vec<i64> = (l as i64..=u as i64).filter(|&v| ok(v)).collect();
            let (Some(&first), Some(&last)) = (values.first(), values.last()) else { return Err(Infeasible) };
            self.tighten(j, first as f64, last as f64, true)?;
        }
        Ok(())
    }

    /// Coefficient tightening for binary columns in a one-sided row.
    fn tighten_coefficients(&mut self, i: usize) {
        let (lo, up) = (self.lo[i], self.up[i]);
        // Work on the row as sign * row <= b.
        let (sign, b) = match (lo.is_finite(), up.is_finite()) {
            (false, true) => (1.0, up),
            (true, false) => (-1.0, -lo),
            _ => return,
        };
        let act = self.activity(i);
        let mut maxact = if sign > 0.0 { act.max() } else { -act.min() };
        // An infinite range gives nothing; a redundant row is removed next pass instead.
        if !maxact.is_finite() || maxact <= b + tol(b, REDUNDANCY) {
            return;
        }
        let mut b = b;
        for k in 0..self.rows[i].len() {
            let (j, a0) = self.rows[i][k];
            if !self.binary(j) {
                continue;
            }
            let a = sign * a0;
            if a > 0.0 && maxact - a < b {
                // With x = 0 the row is redundant; lower a and b by the slack.
                let d = b - (maxact - a);
                if d > COEFFICIENT_MIN_GAIN * a.abs().max(1.0) {
                    self.rows[i][k].1 = sign * (a - d);
                    b -= d;
                    maxact -= d;
                    self.stats.tightened_coefs += 1;
                    self.changed = true;
                }
            } else if a < 0.0 && maxact < b - a {
                // With x = 1 the row is redundant; raise a by the slack.
                let d = (b - a) - maxact;
                if d > COEFFICIENT_MIN_GAIN * a.abs().max(1.0) {
                    self.rows[i][k].1 = sign * (a + d);
                    self.stats.tightened_coefs += 1;
                    self.changed = true;
                }
            }
        }
        if sign > 0.0 {
            self.up[i] = b
        } else {
            self.lo[i] = -b
        }
        self.rows[i].retain(|&(_, a)| a != 0.0);
    }

    /// Duality fixing: fix a column at a finite bound when its cost does not
    /// favor leaving that bound and no row locks it there.
    fn dual_fixing(&mut self, frozen: &[bool]) {
        let n = self.cl.len();
        let (mut up_locks, mut down_locks) = (vec![0usize; n], vec![0usize; n]);
        for i in (0..self.rows.len()).filter(|&i| self.row_alive[i]) {
            let (has_lo, has_up) = (self.lo[i].is_finite(), self.up[i].is_finite());
            for &(j, a) in &self.rows[i] {
                // Increasing x raises the activity when a > 0.
                let (raises_hit, lowers_hit) = if a > 0.0 { (has_up, has_lo) } else { (has_lo, has_up) };
                up_locks[j] += raises_hit as usize;
                down_locks[j] += lowers_hit as usize;
            }
        }
        for j in 0..n {
            if !self.col_alive[j] || self.cl[j] >= self.cu[j] || frozen.get(j) == Some(&true) {
                continue;
            }
            let c = self.cost[j];
            let v = if c >= 0.0 && down_locks[j] == 0 && self.cl[j].is_finite() {
                self.cl[j]
            } else if c <= 0.0 && up_locks[j] == 0 && self.cu[j].is_finite() {
                self.cu[j]
            } else {
                continue;
            };
            self.cl[j] = v;
            self.cu[j] = v;
            self.stats.dual_fixed += 1;
            self.changed = true;
        }
    }

    /// Substitutes columns out of an LP through equality rows (see the
    /// module's notes).
    fn substitute_columns(&mut self, frozen: &[bool]) {
        let n = self.cl.len();
        // Each column's rows, and how many of them. Rows that have lost the
        // column since stay listed, so a listing is checked before use.
        let mut rows_of: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut count = vec![0usize; n];
        for i in (0..self.rows.len()).filter(|&i| self.row_alive[i]) {
            for &(j, _) in &self.rows[i] {
                rows_of[j].push(i);
                count[j] += 1;
            }
        }
        let mut slot = vec![usize::MAX; n];
        for i in 0..self.rows.len() {
            if !self.row_alive[i] || self.lo[i] != self.up[i] || self.rows[i].len() < 2 {
                continue;
            }
            let b = self.lo[i];
            let len = self.rows[i].len();
            let largest = self.rows[i].iter().fold(0.0f64, |m, e| m.max(e.1.abs()));
            // The column with the fewest rows whose substitution is stable
            // and adds little fill.
            let act = self.activity(i);
            let mut best: Option<(usize, usize, f64, bool)> = None; // count, column, coefficient, the row goes
            for &(s, a) in &self.rows[i] {
                if frozen.get(s) == Some(&true) || a.abs() < SUBSTITUTION_PIVOT * largest {
                    continue;
                }
                // Are the column's bounds implied by the row and the other
                // columns' bounds? Then the row goes with the column.
                let (cmin, cmax) = self.contribution(s, a);
                let others_min = match (act.min_inf, cmin.is_finite()) {
                    (0, _) => act.min - cmin,
                    (1, false) => act.min,
                    _ => f64::NEG_INFINITY,
                };
                let others_max = match (act.max_inf, cmax.is_finite()) {
                    (0, _) => act.max - cmax,
                    (1, false) => act.max,
                    _ => f64::INFINITY,
                };
                let (from, to) = ((b - others_max) / a, (b - others_min) / a);
                let (implied_lo, implied_hi) = if a > 0.0 { (from, to) } else { (to, from) };
                let free = implied_lo >= self.cl[s] - tol(self.cl[s], REDUNDANCY)
                    && implied_hi <= self.cu[s] + tol(self.cu[s], REDUNDANCY);
                // Each other row of the column loses its entry and gains up
                // to len - 1; the row itself goes only if the column is free.
                let others = count[s] - 1;
                let fill_ok = len == 2 || (free && others * (len - 2) <= len + SUBSTITUTION_FILL);
                if !fill_ok || best.is_some_and(|(c, ..)| c <= count[s]) {
                    continue;
                }
                // The coefficient mustn't be small beside the column's others.
                let in_column = rows_of[s]
                    .iter()
                    .filter(|&&r| r != i && self.row_alive[r])
                    .filter_map(|&r| self.rows[r].iter().find(|e| e.0 == s))
                    .fold(0.0f64, |m, e| m.max(e.1.abs()));
                if a.abs() >= SUBSTITUTION_PIVOT * in_column {
                    best = Some((count[s], s, a, free));
                }
            }
            let Some((_, s, a, free)) = best else { continue };

            let terms: Vec<(usize, f64)> = self.rows[i].iter().copied().filter(|e| e.0 != s).collect();
            // The other rows: row r loses a_rs x_s and gains a_rs / a times
            // (b minus the rest of row i).
            for r in std::mem::take(&mut rows_of[s]) {
                if r == i || !self.row_alive[r] {
                    continue;
                }
                let Some(at) = self.rows[r].iter().position(|e| e.0 == s) else { continue };
                let f = self.rows[r].swap_remove(at).1 / a;
                for (k, e) in self.rows[r].iter().enumerate() {
                    slot[e.0] = k;
                }
                for &(k, a_k) in &terms {
                    if slot[k] == usize::MAX {
                        slot[k] = self.rows[r].len();
                        self.rows[r].push((k, 0.0));
                        rows_of[k].push(r);
                        count[k] += 1;
                    }
                    self.rows[r][slot[k]].1 -= f * a_k;
                }
                for e in &self.rows[r] {
                    slot[e.0] = usize::MAX;
                }
                self.rows[r].retain(|e| {
                    let keep = e.1.abs() > CANCELLED;
                    if !keep {
                        count[e.0] -= 1;
                    }
                    keep
                });
                self.lo[r] -= f * b;
                self.up[r] -= f * b;
            }
            // The objective.
            let f = self.cost[s] / a;
            for &(k, a_k) in &terms {
                self.cost[k] -= f * a_k;
            }
            self.offset += f * b;
            // Row i: gone, or the column's bounds as a range on the others.
            let (l, u) = (self.cl[s], self.cu[s]);
            self.rows[i].retain(|e| e.0 != s);
            if free {
                for &(k, _) in &terms {
                    count[k] -= 1;
                }
                self.remove_row(i);
            } else {
                (self.lo[i], self.up[i]) = if a > 0.0 { (b - a * u, b - a * l) } else { (b - a * l, b - a * u) };
            }
            self.col_alive[s] = false;
            count[s] = 0;
            self.stats.removed_cols += 1;
            self.stats.substituted_cols += 1;
            self.changed = true;
            self.substitutions.push(Substitution {
                col: s,
                constant: b / a,
                terms: terms.iter().map(|&(k, a_k)| (k, -a_k / a)).collect(),
            });
        }
    }

    fn pass(&mut self, opts: &Options) -> Result<(), Infeasible> {
        for j in 0..self.cl.len() {
            if self.col_alive[j] && self.integer[j] {
                let (l, u) = (self.cl[j], self.cu[j]);
                self.tighten(j, l, u, true)?;
            }
        }
        self.remove_fixed_columns();
        for i in 0..self.rows.len() {
            if self.row_alive[i] {
                self.process_row(i)?;
            }
        }
        if opts.dual {
            self.dual_fixing(&opts.frozen);
        }
        self.remove_fixed_columns();
        if self.linear {
            self.substitute_columns(&opts.frozen);
        }
        Ok(())
    }
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn in_domain_or_any(domain: &Domain, v: i64) -> bool {
    domain.as_ref().is_none_or(|ranges| ranges.iter().any(|&(a, b)| a <= v && v <= b))
}

pub fn presolve(milp: &Milp, opts: &Options, stats: &mut Stats) -> Presolved {
    let lp = &milp.lp;
    if !crate::config::presolve() {
        return Presolved::Reduced(milp.clone(), Postsolve::identity(lp.num_col));
    }
    let rows: Vec<Vec<(usize, f64)>> = (0..lp.num_row)
        .map(|i| {
            (lp.ar_start[i]..lp.ar_start[i + 1])
                .map(|k| (lp.ar_index[k], lp.ar_value[k]))
                .filter(|&(_, a)| a != 0.0)
                .collect()
        })
        .collect();
    let mut st = State {
        rows,
        lo: lp.row_lower.clone(),
        up: lp.row_upper.clone(),
        row_alive: vec![true; lp.num_row],
        cost: lp.col_cost.clone(),
        cl: lp.col_lower.clone(),
        cu: lp.col_upper.clone(),
        integer: milp.integer.clone(),
        domain: milp.domain.clone(),
        col_alive: vec![true; lp.num_col],
        linear: !milp.integer.iter().any(|&i| i),
        substitutions: Vec::new(),
        offset: 0.0,
        changed: true,
        stats: Stats::default(),
    };
    while st.changed && st.stats.passes < MAX_PASSES {
        st.changed = false;
        st.stats.passes += 1;
        if st.pass(opts).is_err() {
            *stats = st.stats;
            return Presolved::Infeasible;
        }
    }
    // Rows emptied by the last substitution still need their check.
    for i in 0..st.rows.len() {
        if st.row_alive[i] && st.rows[i].is_empty() && st.process_row(i).is_err() {
            *stats = st.stats;
            return Presolved::Infeasible;
        }
    }

    let kept: Vec<usize> = (0..lp.num_col).filter(|&j| st.col_alive[j]).collect();
    let mut new_index = vec![usize::MAX; lp.num_col];
    for (k, &j) in kept.iter().enumerate() {
        new_index[j] = k;
    }
    let rows: Vec<SparseRow> = (0..st.rows.len())
        .filter(|&i| st.row_alive[i])
        .map(|i| (st.rows[i].iter().map(|&(j, a)| (new_index[j], a)).collect(), st.lo[i], st.up[i]))
        .collect();
    let pick = |v: &[f64]| kept.iter().map(|&j| v[j]).collect::<Vec<f64>>();
    let reduced = Milp {
        lp: Lp::new(pick(&st.cost), pick(&st.cl), pick(&st.cu), rows),
        integer: kept.iter().map(|&j| st.integer[j]).collect(),
        domain: kept.iter().map(|&j| st.domain[j].clone()).collect(),
    };
    let value = (0..lp.num_col).map(|j| if st.col_alive[j] { 0.0 } else { st.cl[j] }).collect();
    *stats = st.stats;
    let post = Postsolve { kept, value, substitutions: st.substitutions, offset: st.offset };
    Presolved::Reduced(reduced, post)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn milp(cost: Vec<f64>, lower: Vec<f64>, upper: Vec<f64>, integer: Vec<bool>, rows: Vec<SparseRow>) -> Milp {
        let n = cost.len();
        Milp { lp: Lp::new(cost, lower, upper, rows), integer, domain: vec![None; n] }
    }

    fn feasible(m: &Milp, x: &[f64]) -> bool {
        let lp = &m.lp;
        (0..lp.num_col).all(|j| x[j] >= lp.col_lower[j] - 1e-9 && x[j] <= lp.col_upper[j] + 1e-9)
            && (0..lp.num_row).all(|i| {
                let ax: f64 = (lp.ar_start[i]..lp.ar_start[i + 1]).map(|k| lp.ar_value[k] * x[lp.ar_index[k]]).sum();
                ax >= lp.row_lower[i] - 1e-9 && ax <= lp.row_upper[i] + 1e-9
            })
    }

    fn reduce(m: &Milp, dual: bool) -> Option<(Milp, Postsolve, Stats)> {
        let mut stats = Stats::default();
        match presolve(m, &Options { dual, frozen: vec![] }, &mut stats) {
            Presolved::Infeasible => None,
            Presolved::Reduced(r, p) => Some((r, p, stats)),
        }
    }

    struct Rng(u64);
    impl Rng {
        fn int(&mut self, lo: i64, hi: i64) -> i64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            lo + (self.0 % (hi - lo + 1) as u64) as i64
        }
    }

    fn describe(m: &Milp) -> String {
        let lp = &m.lp;
        let rows: Vec<String> = (0..lp.num_row)
            .map(|i| {
                let terms: Vec<String> = (lp.ar_start[i]..lp.ar_start[i + 1])
                    .map(|k| format!("{}*x{}", lp.ar_value[k], lp.ar_index[k]))
                    .collect();
                format!("{} <= {} <= {}", lp.row_lower[i], terms.join(" + "), lp.row_upper[i])
            })
            .collect();
        format!(
            "cost {:?}, lower {:?}, upper {:?}, rows [{}]",
            lp.col_cost,
            lp.col_lower,
            lp.col_upper,
            rows.join("; ")
        )
    }

    /// Each column joins the row with probability 2/3, with a coefficient in [-5, 5].
    fn random_coefs(rng: &mut Rng, n: usize) -> Vec<(usize, f64)> {
        let mut coefs = Vec::new();
        for j in 0..n {
            if rng.int(0, 2) > 0 {
                coefs.push((j, rng.int(-5, 5) as f64));
            }
        }
        coefs
    }

    /// Calls `f` for every integer point of the box.
    fn each_point(lower: &[f64], upper: &[f64], f: &mut dyn FnMut(&[f64])) {
        let mut x: Vec<f64> = lower.to_vec();
        loop {
            f(&x);
            let mut k = 0;
            while k < x.len() {
                if x[k] < upper[k] {
                    x[k] += 1.0;
                    break;
                }
                x[k] = lower[k];
                k += 1;
            }
            if k == x.len() {
                return;
            }
        }
    }

    /// Without dual reductions, presolve keeps the integer-feasible set exactly.
    #[test]
    fn primal_reductions_keep_the_feasible_set() {
        let mut rng = Rng(0x853c49e6748fea9b);
        let mut reduced_something = 0;
        for _ in 0..400 {
            let n = 4;
            let lower: Vec<f64> = (0..n).map(|_| rng.int(-2, 0) as f64).collect();
            let upper: Vec<f64> = lower.iter().map(|l| l + rng.int(0, 3) as f64).collect();
            let rows: Vec<SparseRow> = (0..rng.int(1, 4))
                .map(|_| {
                    let coefs = random_coefs(&mut rng, n);
                    let b = rng.int(-6, 10) as f64;
                    match rng.int(0, 3) {
                        0 => (coefs, b, b),
                        1 => (coefs, b, f64::INFINITY),
                        _ => (coefs, f64::NEG_INFINITY, b),
                    }
                })
                .collect();
            let cost: Vec<f64> = (0..n).map(|_| rng.int(-3, 3) as f64).collect();
            let m = milp(cost, lower.clone(), upper.clone(), vec![true; n], rows);
            let mut original = Vec::new();
            each_point(&lower, &upper, &mut |x| {
                if feasible(&m, x) {
                    original.push(x.to_vec());
                }
            });
            match reduce(&m, false) {
                None => assert!(
                    original.is_empty(),
                    "presolve says infeasible, but {original:?} are feasible: {}",
                    describe(&m)
                ),
                Some((r, post, stats)) => {
                    if stats.removed_rows + stats.removed_cols + stats.tightened_coefs > 0 {
                        reduced_something += 1;
                    }
                    let mut restored = Vec::new();
                    if r.lp.num_col == 0 {
                        restored.push(post.restore(&[]));
                    } else {
                        each_point(&r.lp.col_lower, &r.lp.col_upper, &mut |x| {
                            if feasible(&r, x) {
                                restored.push(post.restore(x));
                            }
                        });
                    }
                    for x in &restored {
                        assert!(feasible(&m, x), "restored point {x:?} infeasible for the original");
                    }
                    restored.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    original.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    assert_eq!(restored, original);
                }
            }
        }
        assert!(reduced_something > 100, "presolve reduced only {reduced_something} problems");
    }

    /// With dual reductions, the optimal value is unchanged.
    #[test]
    fn dual_reductions_keep_the_optimum() {
        let mut rng = Rng(0x2545f4914f6cdd1d);
        for _ in 0..400 {
            let n = 4;
            let lower = vec![0.0; n];
            let upper: Vec<f64> = (0..n).map(|_| rng.int(1, 3) as f64).collect();
            let rows: Vec<SparseRow> = (0..rng.int(1, 3))
                .map(|_| {
                    let coefs = random_coefs(&mut rng, n);
                    (coefs, f64::NEG_INFINITY, rng.int(-2, 10) as f64)
                })
                .collect();
            let cost: Vec<f64> = (0..n).map(|_| rng.int(-3, 3) as f64).collect();
            let m = milp(cost, lower.clone(), upper.clone(), vec![true; n], rows);
            let best = |m: &Milp, post: Option<&Postsolve>| {
                let mut best = f64::INFINITY;
                each_point(&m.lp.col_lower, &m.lp.col_upper, &mut |x| {
                    if feasible(m, x) {
                        best = best.min(m.lp.objective(x) + post.map_or(0.0, |p| p.offset));
                    }
                });
                best
            };
            let original = best(&m, None);
            match reduce(&m, true) {
                None => assert!(original.is_infinite(), "presolve says infeasible: {}", describe(&m)),
                Some((r, post, _)) => {
                    let reduced =
                        if r.lp.num_col == 0 { m.lp.objective(&post.restore(&[])) } else { best(&r, Some(&post)) };
                    assert_eq!(reduced, original, "{}", describe(&m));
                }
            }
        }
    }

    /// The big-M row `make - 1000 on <= 0` with `make <= 40` from a capacity
    /// row becomes `make - 40 on <= 0`.
    /// On random LPs with equality rows, the reduced problem has the
    /// original's optimum, and its solution restores to a feasible point of
    /// the original with that objective.
    #[test]
    fn substitutions_keep_the_optimum() {
        use crate::simplex::{self, LpStatus};
        const INF: f64 = f64::INFINITY;
        let mut rng = Rng(20261109);
        let solve = |m: &Milp| {
            let s = simplex::solve(&m.lp, &m.lp.col_lower, &m.lp.col_upper, None, &simplex::Options::default());
            assert_eq!(s.status, LpStatus::Optimal, "{}", describe(m));
            s
        };
        let (mut substituted, mut rows_removed) = (0, 0);
        for case in 0..400 {
            let n = rng.int(3, 9) as usize;
            // A point the rows are built around, so the LP is feasible.
            let x0: Vec<f64> = (0..n).map(|_| rng.int(0, 8) as f64 / 2.0).collect();
            let lower: Vec<f64> =
                (0..n).map(|j| if rng.int(0, 3) == 0 { -INF } else { x0[j] - rng.int(0, 3) as f64 }).collect();
            let upper: Vec<f64> =
                (0..n).map(|j| if rng.int(0, 3) == 0 { INF } else { x0[j] + rng.int(0, 3) as f64 }).collect();
            // Bounded: the cost never favors an infinite direction.
            let cost: Vec<f64> = (0..n)
                .map(|j| {
                    let c = rng.int(-3, 3) as f64;
                    if (c > 0.0 && lower[j] == -INF) || (c < 0.0 && upper[j] == INF) { 0.0 } else { c }
                })
                .collect();
            let mut rows: Vec<SparseRow> = Vec::new();
            for _ in 0..rng.int(1, 6) {
                let mut coefs = random_coefs(&mut rng, n);
                coefs.truncate(rng.int(1, 4) as usize);
                let at: f64 = coefs.iter().map(|&(j, a)| a * x0[j]).sum();
                rows.push(match rng.int(0, 4) {
                    0 => (coefs, -INF, at + rng.int(0, 2) as f64),
                    1 => (coefs, at - rng.int(0, 2) as f64, INF),
                    _ => (coefs, at, at),
                });
            }
            let m = milp(cost, lower, upper, vec![false; n], rows);
            let original = solve(&m);
            let (reduced, post, stats) =
                reduce(&m, case % 2 == 0).unwrap_or_else(|| panic!("feasible: {}", describe(&m)));
            substituted += stats.substituted_cols;
            rows_removed += stats.removed_rows;
            let x = post.restore(&solve(&reduced).col_value);
            let tolerance = 1e-6 * (1.0 + original.objective.abs());
            let lp = &m.lp;
            for j in 0..n {
                assert!(
                    x[j] >= lp.col_lower[j] - 1e-6 && x[j] <= lp.col_upper[j] + 1e-6,
                    "case {case}: {}",
                    describe(&m)
                );
            }
            for i in 0..lp.num_row {
                let ax: f64 = (lp.ar_start[i]..lp.ar_start[i + 1]).map(|k| lp.ar_value[k] * x[lp.ar_index[k]]).sum();
                assert!(ax >= lp.row_lower[i] - 1e-6 && ax <= lp.row_upper[i] + 1e-6, "case {case}: {}", describe(&m));
            }
            assert!((lp.objective(&x) - original.objective).abs() <= tolerance, "case {case}: {}", describe(&m));
        }
        assert!(
            substituted > 200 && rows_removed > 200,
            "{substituted} columns substituted, {rows_removed} rows removed"
        );
    }

    #[test]
    fn big_m_is_tightened() {
        // Columns: make (continuous, >= 0), on (binary), other (continuous >= 0).
        let m = milp(
            vec![1.0, 10.0, 1.0],
            vec![0.0, 0.0, 0.0],
            vec![f64::INFINITY, 1.0, f64::INFINITY],
            vec![false, true, false],
            vec![
                (vec![(0, 1.0), (1, -1000.0)], f64::NEG_INFINITY, 0.0),
                (vec![(0, 1.0), (2, 1.0)], f64::NEG_INFINITY, 40.0),
                (vec![(0, 1.0), (2, 1.0)], 30.0, f64::INFINITY),
            ],
        );
        let (r, _, stats) = reduce(&m, false).unwrap();
        assert!(stats.tightened_coefs >= 1, "{stats:?}");
        let lp = &r.lp;
        let row =
            (0..lp.num_row).find(|&i| lp.ar_start[i + 1] - lp.ar_start[i] == 2 && lp.row_upper[i] == 0.0).unwrap();
        let on = (lp.ar_start[row]..lp.ar_start[row + 1]).find(|&k| r.integer[lp.ar_index[k]]).unwrap();
        assert_eq!(lp.ar_value[on], -40.0);
    }

    /// -3 x0 + 2 x2 + 3 x3 - 9 x4 + 8.25 x5 - 3 x7 = 9.5 (x5 binary, x3 and x4
    /// unbounded): times 4, every term but 33 x5 is a multiple of 4, and
    /// 33 x5 = 38 (mod 4) has no binary solution.
    #[test]
    fn congruence_proves_infeasibility() {
        let inf = f64::INFINITY;
        let m = milp(
            vec![0.0; 6],
            vec![0.0, 0.0, 0.0, -3.0, 0.0, 0.0],
            vec![4.0, 8.0, inf, inf, 1.0, 7.0],
            vec![true; 6],
            vec![(vec![(0, -3.0), (1, 2.0), (2, 3.0), (3, -9.0), (4, 8.25), (5, -3.0)], 9.5, 9.5)],
        );
        assert!(reduce(&m, false).is_none());
        // With right-hand side 9.25 (37 after scaling), x5 = 1 works: 33 = 37 (mod 4).
        let mut feasible_rhs = m.clone();
        (feasible_rhs.lp.row_lower[0], feasible_rhs.lp.row_upper[0]) = (9.25, 9.25);
        let (r, post, _) = reduce(&feasible_rhs, false).unwrap();
        let x5 = post.restore(&vec![0.0; r.lp.num_col])[4];
        assert!(r.lp.num_col == 5 && x5 == 1.0, "x5 should be fixed to 1");
    }

    #[test]
    fn detects_infeasibility_and_forcing_rows() {
        // x + y >= 5 with x, y in [0, 2]: infeasible.
        let m = milp(
            vec![0.0; 2],
            vec![0.0; 2],
            vec![2.0; 2],
            vec![true; 2],
            vec![(vec![(0, 1.0), (1, 1.0)], 5.0, f64::INFINITY)],
        );
        assert!(reduce(&m, false).is_none());
        // x + y <= 0 with x, y >= 0 forces both to 0.
        let m = milp(
            vec![-1.0, -1.0],
            vec![0.0; 2],
            vec![5.0; 2],
            vec![false; 2],
            vec![(vec![(0, 1.0), (1, 1.0)], f64::NEG_INFINITY, 0.0)],
        );
        let (r, post, stats) = reduce(&m, false).unwrap();
        assert_eq!((r.lp.num_col, r.lp.num_row, stats.forcing_rows), (0, 0, 1));
        assert_eq!(post.restore(&[]), vec![0.0, 0.0]);
    }
}
