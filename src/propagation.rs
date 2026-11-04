//! Bound propagation over linear rows: the domain store shared by the MILP
//! search (at every node) and, later, the CP engine.
//!
//! Each row `lo <= a . x <= up` bounds each of its columns given the others'
//! bounds: with the minimum activity of the other columns, `a_j x_j <= up -
//! min(others)`, and with the maximum, `a_j x_j >= lo - max(others)`. A row
//! whose activity range lies outside its bounds proves the node infeasible.
//! Rows are processed from a queue: a bound change queues the rows of its
//! column, until nothing changes or the work budget runs out.
//!
//! Every bound change is recorded on a `Trail` with its reason (the row that
//! implied it), which conflict analysis will use.
//!
//! The arithmetic is floating point with tolerances, as in presolve: integer
//! bounds are rounded (and snapped to the column's domain), a finite
//! continuous bound is only tightened by a significant amount, and implied
//! bounds of huge magnitude are ignored.

use crate::lp::Lp;
use crate::mip::{Domain, ceil_allowed, floor_allowed};

/// Infeasibility tolerance, relative to max(1, |bound|).
const FEASIBILITY: f64 = 1e-6;
/// Tolerance when rounding an implied bound on an integer column.
const INTEGRAL: f64 = 1e-6;
/// A finite continuous bound is tightened only by more than this fraction of max(1, range).
const CONTINUOUS_MIN_GAIN: f64 = 0.05;
/// Implied bounds beyond this magnitude are ignored.
const MAX_BOUND: f64 = 1e9;
/// Row visits per call, as a multiple of the number of rows (plus a constant).
const WORK_FACTOR: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Lower,
    Upper,
}

/// Why a bound changed.
// Decisions and the trail's contents are read by conflict analysis, the next step.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A branching decision (or other change made by the caller).
    Decision,
    /// Implied by this row and the other columns' bounds at the time.
    Row(usize),
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct BoundChange {
    pub col: usize,
    pub side: Side,
    pub old: f64,
    pub new: f64,
    pub reason: Reason,
}

/// The bound changes made so far, in order.
#[derive(Default)]
pub struct Trail {
    pub changes: Vec<BoundChange>,
}

/// Why propagation failed: a row that can't be satisfied, or a column whose
/// bounds crossed.
#[allow(dead_code)] // the contents are read by conflict analysis, the next step
#[derive(Clone, Copy, Debug)]
pub enum Conflict {
    Row(usize),
    Col(usize),
}

/// Where to start: every row, or the rows of the columns that changed.
pub enum Seeds<'a> {
    All,
    Cols(&'a [usize]),
}

pub struct Propagator {
    num_row: usize,
    row_start: Vec<usize>,
    row_index: Vec<usize>,
    row_value: Vec<f64>,
    row_lower: Vec<f64>,
    row_upper: Vec<f64>,
    col_start: Vec<usize>,
    col_rows: Vec<usize>,
    integer: Vec<bool>,
    domain: Vec<Domain>,
}

/// Activity range of a row: finite parts and counts of infinite contributions,
/// and the largest |a_j| * (u_j - l_j), which bounds how much any one column
/// can be tightened.
struct Activity {
    min: f64,
    min_inf: usize,
    max: f64,
    max_inf: usize,
    max_range: f64,
}

impl Propagator {
    pub fn new(lp: &Lp, integer: &[bool], domain: &[Domain]) -> Self {
        Propagator {
            num_row: lp.num_row,
            row_start: lp.ar_start.clone(),
            row_index: lp.ar_index.clone(),
            row_value: lp.ar_value.clone(),
            row_lower: lp.row_lower.clone(),
            row_upper: lp.row_upper.clone(),
            col_start: lp.a_start.clone(),
            col_rows: lp.a_index.clone(),
            integer: integer.to_vec(),
            domain: domain.to_vec(),
        }
    }

    fn row(&self, i: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        (self.row_start[i]..self.row_start[i + 1]).map(|k| (self.row_index[k], self.row_value[k]))
    }

    fn activity(&self, i: usize, lower: &[f64], upper: &[f64]) -> Activity {
        let mut act = Activity { min: 0.0, min_inf: 0, max: 0.0, max_inf: 0, max_range: 0.0 };
        for (j, a) in self.row(i) {
            let (lo, hi) = contribution(a, lower[j], upper[j]);
            if lo.is_finite() {
                act.min += lo
            } else {
                act.min_inf += 1
            }
            if hi.is_finite() {
                act.max += hi
            } else {
                act.max_inf += 1
            }
            act.max_range = act.max_range.max(hi - lo);
        }
        act
    }

    /// Propagates the rows from `seeds` to a fixed point (or the work
    /// budget), tightening `lower` and `upper` in place and recording each
    /// change on `trail`. Err means the bounds admit no feasible point.
    pub fn propagate(
        &self,
        lower: &mut [f64],
        upper: &mut [f64],
        seeds: Seeds,
        trail: &mut Trail,
    ) -> Result<(), Conflict> {
        let mut queued = vec![false; self.num_row];
        let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let push_col = |j: usize, queue: &mut std::collections::VecDeque<usize>, queued: &mut Vec<bool>| {
            for &i in &self.col_rows[self.col_start[j]..self.col_start[j + 1]] {
                if !queued[i] {
                    queued[i] = true;
                    queue.push_back(i);
                }
            }
        };
        match seeds {
            Seeds::All => {
                queue.extend(0..self.num_row);
                queued.iter_mut().for_each(|q| *q = true);
            }
            Seeds::Cols(cols) => {
                for &j in cols {
                    push_col(j, &mut queue, &mut queued);
                }
            }
        }
        let mut budget = WORK_FACTOR * self.num_row + 100;
        while let Some(i) = queue.pop_front() {
            queued[i] = false;
            if budget == 0 {
                break;
            }
            budget -= 1;
            let (lo, up) = (self.row_lower[i], self.row_upper[i]);
            let act = self.activity(i, lower, upper);
            let (amin, amax) = (
                if act.min_inf > 0 { f64::NEG_INFINITY } else { act.min },
                if act.max_inf > 0 { f64::INFINITY } else { act.max },
            );
            if amin > up + tol(up) || amax < lo - tol(lo) {
                return Err(Conflict::Row(i));
            }
            // A column can only be tightened by a side whose slack is below its
            // own range |a_j| (u_j - l_j); if no column's range reaches either
            // slack, the row implies nothing.
            let up_slack = if up.is_finite() { up - amin } else { f64::INFINITY };
            let lo_slack = if lo.is_finite() { amax - lo } else { f64::INFINITY };
            if act.max_range <= up_slack && act.max_range <= lo_slack {
                continue;
            }
            for (j, a) in self.row(i) {
                if lower[j] == upper[j] {
                    continue;
                }
                let (cmin, cmax) = contribution(a, lower[j], upper[j]);
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
                if self.tighten(j, l, u, lower, upper, Reason::Row(i), trail)? {
                    push_col(j, &mut queue, &mut queued);
                }
            }
        }
        Ok(())
    }

    /// Tightens column j toward [l, u] where worthwhile; returns whether it changed.
    #[allow(clippy::too_many_arguments)]
    fn tighten(
        &self,
        j: usize,
        mut l: f64,
        mut u: f64,
        lower: &mut [f64],
        upper: &mut [f64],
        reason: Reason,
        trail: &mut Trail,
    ) -> Result<bool, Conflict> {
        if l.abs() > MAX_BOUND {
            l = f64::NEG_INFINITY;
        }
        if u.abs() > MAX_BOUND {
            u = f64::INFINITY;
        }
        let (ol, ou) = (lower[j], upper[j]);
        if self.integer[j] {
            l = ceil_allowed(&self.domain[j], l - INTEGRAL);
            u = floor_allowed(&self.domain[j], u + INTEGRAL);
        } else {
            // Relax a derived continuous bound slightly against rounding error.
            l -= 1e-9 * l.abs().max(1.0);
            u += 1e-9 * u.abs().max(1.0);
        }
        let range = if ol.is_finite() && ou.is_finite() { ou - ol } else { 0.0 };
        let worth = |gain: f64, old: f64| {
            gain > 0.0 && (self.integer[j] || !old.is_finite() || gain > CONTINUOUS_MIN_GAIN * range.max(1.0))
        };
        let mut changed = false;
        if worth(l - ol, ol) {
            trail.changes.push(BoundChange { col: j, side: Side::Lower, old: ol, new: l, reason });
            lower[j] = l;
            changed = true;
        }
        if worth(ou - u, ou) {
            trail.changes.push(BoundChange { col: j, side: Side::Upper, old: ou, new: u, reason });
            upper[j] = u;
            changed = true;
        }
        if lower[j] > upper[j] {
            if lower[j] - upper[j] > tol(lower[j].abs().max(upper[j].abs())) || self.integer[j] {
                return Err(Conflict::Col(j));
            }
            // Crossed within tolerance: meet in the middle.
            let mid = 0.5 * (lower[j] + upper[j]);
            (lower[j], upper[j]) = (mid, mid);
        }
        Ok(changed)
    }
}

fn contribution(a: f64, l: f64, u: f64) -> (f64, f64) {
    if a > 0.0 { (a * l, a * u) } else { (a * u, a * l) }
}

fn tol(b: f64) -> f64 {
    FEASIBILITY * b.abs().max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lp::SparseRow;

    fn propagator(rows: Vec<SparseRow>, n: usize, integer: Vec<bool>) -> Propagator {
        let lp = Lp::new(vec![0.0; n], vec![0.0; n], vec![f64::INFINITY; n], rows);
        Propagator::new(&lp, &integer, &vec![None; n])
    }

    /// make <= 50 * on with on fixed to 0 forces make = 0; the cap row then
    /// lets the other product use the whole capacity.
    #[test]
    fn propagates_through_rows() {
        // cols: make, on, other
        let p = propagator(
            vec![(vec![(0, 1.0), (1, -50.0)], f64::NEG_INFINITY, 0.0), (vec![(0, 1.0), (2, 1.0)], 30.0, f64::INFINITY)],
            3,
            vec![false, true, false],
        );
        let (mut lower, mut upper) = (vec![0.0; 3], vec![f64::INFINITY, 0.0, 40.0]);
        let mut trail = Trail::default();
        p.propagate(&mut lower, &mut upper, Seeds::Cols(&[1]), &mut trail).unwrap();
        assert!(upper[0] < 1e-6, "make should be 0, got {}", upper[0]);
        assert!(lower[2] > 30.0 - 1e-6, "other must cover the demand, got {}", lower[2]);
        assert!(trail.changes.iter().all(|c| matches!(c.reason, Reason::Row(_))));
    }

    #[test]
    fn detects_infeasibility() {
        // x + y >= 5 with x, y integer in [0, 2].
        let p = propagator(vec![(vec![(0, 1.0), (1, 1.0)], 5.0, f64::INFINITY)], 2, vec![true, true]);
        let (mut lower, mut upper) = (vec![0.0; 2], vec![2.0; 2]);
        assert!(p.propagate(&mut lower, &mut upper, Seeds::All, &mut Trail::default()).is_err());
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

    /// Propagation never removes an integer-feasible point, and when it reports
    /// a conflict there is none.
    #[test]
    fn keeps_every_feasible_point() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for _ in 0..3000 {
            let n = 3;
            let rows: Vec<SparseRow> = (0..rng.int(1, 4))
                .map(|_| {
                    let mut coefs = Vec::new();
                    for j in 0..n {
                        let a = rng.int(-5, 5);
                        if a != 0 {
                            coefs.push((j, a as f64));
                        }
                    }
                    let b = rng.int(-6, 10) as f64;
                    match rng.int(0, 2) {
                        0 => (coefs, b, b),
                        1 => (coefs, b, f64::INFINITY),
                        _ => (coefs, f64::NEG_INFINITY, b),
                    }
                })
                .collect();
            let lower0: Vec<f64> = (0..n).map(|_| rng.int(-3, 0) as f64).collect();
            let upper0: Vec<f64> = lower0.iter().map(|l| l + rng.int(0, 4) as f64).collect();
            let p = propagator(rows.clone(), n, vec![true; n]);
            let feasible = |x: &[f64]| {
                rows.iter().all(|(coefs, lo, up)| {
                    let ax: f64 = coefs.iter().map(|&(j, a)| a * x[j]).sum();
                    ax >= *lo - 1e-9 && ax <= *up + 1e-9
                })
            };
            let mut points = Vec::new();
            for a in lower0[0] as i64..=upper0[0] as i64 {
                for b in lower0[1] as i64..=upper0[1] as i64 {
                    for c in lower0[2] as i64..=upper0[2] as i64 {
                        let x = [a as f64, b as f64, c as f64];
                        if feasible(&x) {
                            points.push(x);
                        }
                    }
                }
            }
            let (mut lower, mut upper) = (lower0.clone(), upper0.clone());
            match p.propagate(&mut lower, &mut upper, Seeds::All, &mut Trail::default()) {
                Err(_) => assert!(points.is_empty(), "conflict reported, but {points:?} are feasible"),
                Ok(()) => {
                    for x in &points {
                        assert!(
                            (0..n).all(|j| x[j] >= lower[j] && x[j] <= upper[j]),
                            "propagation removed {x:?}: bounds {lower:?} {upper:?}"
                        );
                    }
                }
            }
        }
    }
}
