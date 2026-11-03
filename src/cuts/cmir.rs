//! Complemented mixed-integer rounding (c-MIR) cuts on aggregated rows, after
//! Marchand and Wolsey, "Aggregation and mixed integer rounding to solve MIPs"
//! (2001), as in HiGHS's `HighsPathSeparator` and `HighsCutGeneration`.
//!
//! Aggregation, two kinds:
//! - Paths: starting from a row (in `<=` form; cut rows from earlier rounds
//!   included), try to separate a cut. If none is found, pick the continuous
//!   variable of the aggregate that is farthest from its bounds at the LP
//!   point, and eliminate it by adding a multiple of the tightest other model
//!   row that contains it. Repeat up to `MAX_AGGREGATION` rows.
//! - Projections: from a few tight rows mixing integer and continuous
//!   columns, eliminate every continuous column that some row can eliminate,
//!   even at its bound, up to `MAX_LONG_AGGREGATION` rows. This projects the
//!   continuous columns out and finds global counting arguments that no short
//!   path reaches, e.g. summing every demand and capacity row of a facility
//!   location model gives `module_size * sum(modules) >= total demand`.
//!
//! Separation of one aggregate `sum(a_j x_j) <= b`:
//! 1. Continuous columns are substituted by their closest bound: a simple
//!    bound (x = l + x' or x = u - x') or a variable bound on a binary
//!    (x = d y - x' for x <= d y, x = d y + x' for x >= d y), with x' >= 0.
//!    Terms c x' with c > 0 are dropped (a relaxation); the others form
//!    -s, s >= 0.
//!    An integer column with a tight variable upper bound x <= d y (y binary,
//!    d a positive integer, x >= 0) is also substituted, x = d y - x', where
//!    x' in [0, d] is integer, so the rounding still applies to it.
//! 2. Integer columns are complemented to z >= 0 at their closer bound.
//!    The row is now sum(alpha_j z_j) - s <= beta.
//! 3. For a divisor delta, with f0 = frac(beta / delta) and
//!    f_j = frac(alpha_j / delta), the MIR inequality is
//!    `sum((floor(alpha_j / delta) + max(0, f_j - f0) / (1 - f0)) z_j)`
//!    `- s / (delta (1 - f0)) <= floor(beta / delta)`.
//!    Divisors tried: |alpha_j| of integer columns strictly between their
//!    bounds, then the best one halved up to three times. Then complementing
//!    each such column the other way is tried, kept if it helps.
//! 4. The cut is mapped back to the original columns.
//!
//! Variable bounds come from model rows `a x + b y <= 0` (or `>= 0`) with y
//! binary and x continuous, such as `serve[f,c] <= demand[c] * open[f]`, or
//! x integer as described above, such as `modules[f] <= 4 * open[f]`.

use super::{Cut, Round, Source, finish};
use crate::lp::Lp;

/// Most model rows in one path aggregation.
const MAX_AGGREGATION: usize = 6;
/// Most rows in one projection, and projections per round.
const MAX_LONG_AGGREGATION: usize = 200;
const MAX_LONG_STARTS: usize = 3;
/// The fractional part of beta / delta must lie in [this, 1 - this].
const MIN_FRACTIONALITY: f64 = 0.01;
/// Most divisors tried, besides halvings.
const MAX_DIVISORS: usize = 8;
/// Distances to a bound below this count as at the bound.
const MIN_DISTANCE: f64 = 1e-6;
/// Coefficients below this in an aggregate count as zero.
const ZERO: f64 = 1e-9;

#[derive(Clone, Copy, Debug)]
pub(super) struct VarBound {
    y: usize,
    d: f64,
}

/// Variable upper and lower bounds on continuous columns, from model rows.
pub(super) struct VariableBounds {
    vub: Vec<Option<VarBound>>,
    vlb: Vec<Option<VarBound>>,
    /// Rows that only state a variable bound; used by substitution, so they
    /// are neither aggregation starts nor eliminated through.
    is_bound_row: Vec<bool>,
}

impl VariableBounds {
    pub(super) fn detect(lp: &Lp, integer: &[bool], lower: &[f64], upper: &[f64]) -> Self {
        let n = lp.num_col;
        let (mut vub, mut vlb) = (vec![None; n], vec![None; n]);
        let mut is_bound_row = vec![false; lp.num_row];
        let binary = |j: usize| integer[j] && lower[j] == 0.0 && upper[j] == 1.0;
        // x may be continuous, or a non-binary integer >= 0 with x <= d y for
        // a positive integer d (checked below).
        let fits = |x: usize, y: usize| binary(y) && (!integer[x] || (!binary(x) && lower[x] == 0.0));
        for i in 0..lp.num_row {
            let (s, e) = (lp.ar_start[i], lp.ar_start[i + 1]);
            if e - s != 2 {
                continue;
            }
            let (j0, a0, j1, a1) = (lp.ar_index[s], lp.ar_value[s], lp.ar_index[s + 1], lp.ar_value[s + 1]);
            let (x, a, y, b) = if fits(j0, j1) {
                (j0, a0, j1, a1)
            } else if fits(j1, j0) {
                (j1, a1, j0, a0)
            } else {
                continue;
            };
            // a x + b y <= 0  is  x <= d y (a > 0) or x >= d y (a < 0), d = -b / a.
            let d = -b / a;
            let vb = Some(VarBound { y, d });
            let int_ok = !integer[x] || (d > 0.0 && d.fract() == 0.0);
            // (is_upper, is_lower) bound statements made by this row.
            let ub = (lp.row_upper[i] == 0.0 && a > 0.0) || (lp.row_lower[i] == 0.0 && a < 0.0);
            let lb = (lp.row_upper[i] == 0.0 && a < 0.0) || (lp.row_lower[i] == 0.0 && a > 0.0);
            if ub && int_ok {
                vub[x] = vb;
                is_bound_row[i] = true;
            }
            if lb && !integer[x] {
                vlb[x] = vb;
                is_bound_row[i] = true;
            }
        }
        VariableBounds { vub, vlb, is_bound_row }
    }
}

/// How a continuous column was replaced by x' >= 0.
#[derive(Clone, Copy, Debug)]
enum Sub {
    Lower,      // x = l + x'
    Upper,      // x = u - x'
    Vub(usize), // x = d y - x', with d from vub
    Vlb(usize), // x = d y + x', with d from vlb
}

/// An aggregate row `coef . x <= rhs`, stored densely with a list of touched columns.
struct Aggregate {
    coef: Vec<f64>,
    touched: Vec<usize>,
    seen: Vec<bool>,
    rhs: f64,
}

impl Aggregate {
    fn new(n: usize) -> Self {
        Aggregate { coef: vec![0.0; n], touched: Vec::new(), seen: vec![false; n], rhs: 0.0 }
    }

    fn clear(&mut self) {
        for &j in &self.touched {
            self.coef[j] = 0.0;
            self.seen[j] = false;
        }
        self.touched.clear();
        self.rhs = 0.0;
    }

    /// Adds `mult` times row i, whose bound `rhs` is the side being used.
    fn add_row(&mut self, lp: &Lp, i: usize, mult: f64, rhs: f64) {
        for k in lp.ar_start[i]..lp.ar_start[i + 1] {
            let j = lp.ar_index[k];
            if !self.seen[j] {
                self.seen[j] = true;
                self.touched.push(j);
            }
            self.coef[j] += mult * lp.ar_value[k];
            if self.coef[j].abs() < ZERO {
                self.coef[j] = 0.0;
            }
        }
        self.rhs += mult * rhs;
    }
}

/// The orientations of row i as `<=` rows: (sign, bound) with sign * row <= sign * bound.
fn orientations(lp: &Lp, i: usize) -> impl Iterator<Item = (f64, f64)> {
    let up = lp.row_upper[i].is_finite().then_some((1.0, lp.row_upper[i]));
    let lo = lp.row_lower[i].is_finite().then_some((-1.0, lp.row_lower[i]));
    up.into_iter().chain(lo)
}

pub(super) fn separate(round: &Round, bounds: &VariableBounds) -> Vec<Cut> {
    let lp = round.lp;
    let mut cuts = Vec::new();
    let mut agg = Aggregate::new(lp.num_col);
    let mut used: Vec<usize> = Vec::new();
    for start in 0..lp.num_row {
        if start < round.num_model_rows && bounds.is_bound_row[start] {
            continue;
        }
        for (sign, bound) in orientations(lp, start) {
            agg.clear();
            used.clear();
            agg.add_row(lp, start, sign, bound);
            used.push(start);
            for _ in 0..MAX_AGGREGATION {
                if let Some(cut) = separate_aggregate(round, bounds, &agg) {
                    cuts.push(cut);
                    break;
                }
                let Some(j) = farthest_continuous(round, bounds, &agg) else { break };
                let Some((row, mult, rhs)) = eliminating_row(round, bounds, &agg, j, &used) else { break };
                agg.add_row(lp, row, mult, rhs);
                agg.coef[j] = 0.0;
                used.push(row);
            }
        }
    }

    // Projections.
    let mut stuck: Vec<usize> = Vec::new();
    let mut long_starts = 0;
    for start in 0..round.num_model_rows {
        if long_starts >= MAX_LONG_STARTS {
            break;
        }
        let cols = || (lp.ar_start[start]..lp.ar_start[start + 1]).map(|k| lp.ar_index[k]);
        if bounds.is_bound_row[start] || !cols().any(|j| round.integer[j]) || cols().all(|j| round.integer[j]) {
            continue;
        }
        let activity: f64 =
            (lp.ar_start[start]..lp.ar_start[start + 1]).map(|k| lp.ar_value[k] * round.x[lp.ar_index[k]]).sum();
        let Some((sign, bound)) = orientations(lp, start)
            .find(|&(sign, bound)| sign * (bound - activity) <= MIN_DISTANCE * bound.abs().max(1.0))
        else {
            continue;
        };
        long_starts += 1;
        agg.clear();
        used.clear();
        stuck.clear();
        agg.add_row(lp, start, sign, bound);
        used.push(start);
        while used.len() < MAX_LONG_AGGREGATION {
            let next = agg
                .touched
                .iter()
                .filter(|&&j| agg.coef[j] != 0.0 && !round.integer[j] && !stuck.contains(&j))
                .map(|&j| (j, closest_bound(round, bounds, j).map_or(f64::INFINITY, |(d, _)| d)))
                .fold(
                    None,
                    |best: Option<(usize, f64)>, (j, d)| {
                        if best.is_none_or(|(_, bd)| d > bd) { Some((j, d)) } else { best }
                    },
                );
            let Some((j, _)) = next else { break };
            match eliminating_row(round, bounds, &agg, j, &used) {
                Some((row, mult, rhs)) => {
                    agg.add_row(lp, row, mult, rhs);
                    agg.coef[j] = 0.0;
                    used.push(row);
                }
                None => stuck.push(j),
            }
        }
        if let Some(cut) = separate_aggregate(round, bounds, &agg) {
            cuts.push(cut);
        }
    }
    cuts
}

/// Distance of continuous column j from its closest bound at the LP point,
/// and that bound.
fn closest_bound(round: &Round, bounds: &VariableBounds, j: usize) -> Option<(f64, Sub)> {
    let x = round.x;
    let mut best: Option<(f64, Sub)> = None;
    let consider = |dist: f64, sub: Sub, best: &mut Option<(f64, Sub)>| {
        // Ties prefer a variable bound (listed first), which keeps the binary in the cut.
        if best.is_none_or(|(d, _)| dist < d - MIN_DISTANCE) {
            *best = Some((dist.max(0.0), sub));
        }
    };
    if let Some(vb) = bounds.vub[j] {
        consider(vb.d * x[vb.y] - x[j], Sub::Vub(vb.y), &mut best);
    }
    if let Some(vb) = bounds.vlb[j] {
        consider(x[j] - vb.d * x[vb.y], Sub::Vlb(vb.y), &mut best);
    }
    if round.lower[j].is_finite() {
        consider(x[j] - round.lower[j], Sub::Lower, &mut best);
    }
    if round.upper[j].is_finite() {
        consider(round.upper[j] - x[j], Sub::Upper, &mut best);
    }
    best
}

fn farthest_continuous(round: &Round, bounds: &VariableBounds, agg: &Aggregate) -> Option<usize> {
    agg.touched
        .iter()
        .filter(|&&j| agg.coef[j] != 0.0 && !round.integer[j])
        .map(|&j| (j, closest_bound(round, bounds, j).map_or(f64::INFINITY, |(d, _)| d)))
        .filter(|&(_, d)| d > MIN_DISTANCE)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// The unused model row that eliminates column j from the aggregate with the
/// least slack at the LP point: (row, multiplier, bound of the side used).
fn eliminating_row(
    round: &Round,
    bounds: &VariableBounds,
    agg: &Aggregate,
    j: usize,
    used: &[usize],
) -> Option<(usize, f64, f64)> {
    let lp = round.lp;
    let a = agg.coef[j];
    let mut best: Option<(f64, usize, usize, f64, f64)> = None; // (slack, nnz, row, mult, rhs)
    for k in lp.a_start[j]..lp.a_start[j + 1] {
        let i = lp.a_index[k];
        if i >= round.num_model_rows || bounds.is_bound_row[i] || used.contains(&i) {
            continue;
        }
        let r = lp.a_value[k];
        let activity: f64 =
            (lp.ar_start[i]..lp.ar_start[i + 1]).map(|q| lp.ar_value[q] * round.x[lp.ar_index[q]]).sum();
        for (sign, bound) in orientations(lp, i) {
            // Add lambda * (sign * row) with lambda > 0 so that j cancels.
            let lambda = -a / (sign * r);
            if lambda <= 0.0 || !(1e-4..=1e4).contains(&lambda.abs()) {
                continue;
            }
            let slack = sign * (bound - activity);
            let nnz = lp.ar_start[i + 1] - lp.ar_start[i];
            if best.is_none_or(|(s, z, ..)| (slack, nnz) < (s, z)) {
                best = Some((slack, nnz, i, lambda * sign, bound));
            }
        }
    }
    best.map(|(_, _, i, mult, rhs)| (i, mult, rhs))
}

/// An integer term of the transformed row: alpha * z with z >= 0. The term's
/// variable w is column `col`, or, with `slack = Some((y, d))`, the integer
/// slack w = d y - x of column x = `col` under its variable upper bound.
struct IntTerm {
    col: usize,
    slack: Option<(usize, f64)>,
    alpha: f64,
    /// z = hi - w (true) or w - lo (false).
    complemented: bool,
    z: f64,
    lo: f64,
    hi: f64,
}

/// A continuous column with negative coefficient in the transformed row: c * x', c < 0.
struct ContTerm {
    col: usize,
    sub: Sub,
    c: f64,
}

/// The MIR inequality for a divisor, as (coefficient per integer term,
/// coefficient on s, rhs), and its efficacy in the transformed space.
fn mir(ints: &[IntTerm], s_norm2: f64, s_value: f64, beta: f64, delta: f64) -> Option<(Vec<f64>, f64, f64, f64)> {
    let b = beta / delta;
    let f0 = b - b.floor();
    if !(MIN_FRACTIONALITY..=1.0 - MIN_FRACTIONALITY).contains(&f0) {
        return None;
    }
    let g: Vec<f64> = ints
        .iter()
        .map(|t| {
            let q = t.alpha / delta;
            let (mut fl, mut f) = (q.floor(), q - q.floor());
            if f > 1.0 - ZERO {
                fl += 1.0;
                f = 0.0;
            }
            fl + (f - f0).max(0.0) / (1.0 - f0)
        })
        .collect();
    let h = 1.0 / (delta * (1.0 - f0));
    let rhs = b.floor();
    let activity: f64 = g.iter().zip(ints).map(|(gi, t)| gi * t.z).sum::<f64>() - h * s_value;
    let norm = (g.iter().map(|v| v * v).sum::<f64>() + h * h * s_norm2).sqrt();
    if norm == 0.0 {
        return None;
    }
    Some((g, h, rhs, (activity - rhs) / norm))
}

fn separate_aggregate(round: &Round, bounds: &VariableBounds, agg: &Aggregate) -> Option<Cut> {
    let (x, lower, upper) = (round.x, round.lower, round.upper);
    let mut beta = agg.rhs;
    // (column, is the slack of its variable upper bound, coefficient)
    let mut int_coef: Vec<(usize, bool, f64)> = Vec::new();
    let mut conts: Vec<ContTerm> = Vec::new();
    let (mut s_value, mut s_norm2) = (0.0, 0.0);

    // 1. Substitute continuous columns.
    for &j in &agg.touched {
        let a = agg.coef[j];
        if a == 0.0 {
            continue;
        }
        if round.integer[j] {
            match bounds.vub[j] {
                Some(vb) if vb.d * x[vb.y] - x[j] <= MIN_DISTANCE => {
                    // x = d y - x'
                    int_coef.push((vb.y, false, a * vb.d));
                    int_coef.push((j, true, -a));
                }
                _ => int_coef.push((j, false, a)),
            }
            continue;
        }
        let (_, sub) = closest_bound(round, bounds, j)?;
        let (c, xp) = match sub {
            Sub::Lower => {
                beta -= a * lower[j];
                (a, x[j] - lower[j])
            }
            Sub::Upper => {
                beta -= a * upper[j];
                (-a, upper[j] - x[j])
            }
            Sub::Vub(y) => {
                let d = bounds.vub[j].unwrap().d;
                int_coef.push((y, false, a * d));
                (-a, d * x[y] - x[j])
            }
            Sub::Vlb(y) => {
                let d = bounds.vlb[j].unwrap().d;
                int_coef.push((y, false, a * d));
                (a, x[j] - d * x[y])
            }
        };
        if c < 0.0 {
            s_value += -c * xp;
            s_norm2 += c * c;
            conts.push(ContTerm { col: j, sub, c });
        }
    }

    // 2. Complement integer columns at their closer bound.
    int_coef.sort_unstable_by_key(|&(j, slack, _)| (j, slack));
    let mut ints: Vec<IntTerm> = Vec::new();
    for (j, is_slack, a) in int_coef {
        if let Some(last) = ints.last_mut().filter(|t| t.col == j && t.slack.is_some() == is_slack) {
            last.alpha += a; // the raw coefficient until complementation below
            continue;
        }
        let slack = is_slack.then(|| bounds.vub[j].map(|vb| (vb.y, vb.d))).flatten();
        ints.push(IntTerm { col: j, slack, alpha: a, complemented: false, z: 0.0, lo: 0.0, hi: 0.0 });
    }
    ints.retain(|t| t.alpha.abs() >= ZERO);
    if ints.is_empty() {
        return None;
    }
    for t in &mut ints {
        let (l, u, v) = match t.slack {
            None => (lower[t.col], upper[t.col], x[t.col]),
            Some((y, d)) => (0.0, d, d * x[y] - x[t.col]),
        };
        if !l.is_finite() && !u.is_finite() {
            return None;
        }
        t.complemented = !l.is_finite() || (u.is_finite() && v > 0.5 * (l + u));
        (t.lo, t.hi) = (l, u);
        if t.complemented {
            beta -= t.alpha * u;
            t.alpha = -t.alpha;
            t.z = u - v;
        } else {
            beta -= t.alpha * l;
            t.z = v - l;
        }
    }

    // 3. Divisor search.
    let between = |t: &IntTerm| t.z > MIN_DISTANCE && t.z < t.hi - t.lo - MIN_DISTANCE;
    let mut divisors: Vec<f64> = Vec::new();
    let mut order: Vec<&IntTerm> = ints.iter().filter(|t| between(t)).collect();
    order.sort_by(|a, b| b.z.total_cmp(&a.z));
    for t in order {
        let d = t.alpha.abs();
        if divisors.len() < MAX_DIVISORS && !divisors.iter().any(|&e| (e - d).abs() <= 1e-9 * d.max(1.0)) {
            divisors.push(d);
        }
    }
    let efficacy = |ints: &[IntTerm], beta: f64, delta: f64| mir(ints, s_norm2, s_value, beta, delta).map(|r| r.3);
    let mut best: Option<(f64, f64)> = None; // (efficacy, delta)
    for &d in &divisors {
        if let Some(e) = efficacy(&ints, beta, d)
            && best.is_none_or(|(b, _)| e > b)
        {
            best = Some((e, d));
        }
    }
    let (_, mut delta) = best?;
    for k in [2.0, 4.0, 8.0] {
        let d = best.unwrap().1 / k;
        if let Some(e) = efficacy(&ints, beta, d)
            && e > best.unwrap().0
        {
            best = Some((e, d));
            delta = d;
        }
    }

    // Try complementing each column strictly between its bounds the other way.
    for idx in 0..ints.len() {
        if !between(&ints[idx]) {
            continue;
        }
        let flip = |t: &mut IntTerm, beta: &mut f64| {
            // alpha z = alpha (range - z'), so the coefficient flips and beta drops by alpha * range.
            let range = t.hi - t.lo;
            *beta -= t.alpha * range;
            t.alpha = -t.alpha;
            t.z = range - t.z;
            t.complemented = !t.complemented;
        };
        let mut trial_beta = beta;
        flip(&mut ints[idx], &mut trial_beta);
        match efficacy(&ints, trial_beta, delta) {
            Some(e) if e > best.unwrap().0 => {
                best = Some((e, delta));
                beta = trial_beta;
            }
            _ => {
                let mut ignored = trial_beta;
                flip(&mut ints[idx], &mut ignored);
            }
        }
    }
    let (g, h, rhs, eff) = mir(&ints, s_norm2, s_value, beta, delta)?;
    if eff <= 0.0 {
        return None;
    }

    // 4. Back to the original columns: sum(g z) - h s <= rhs, as a ">=" cut.
    let mut le: Vec<(usize, f64)> = Vec::new();
    let mut r = rhs;
    for (t, &gi) in ints.iter().zip(&g) {
        // g (hi - w) or g (w - lo): a coefficient on w and a constant.
        let cw = if t.complemented {
            r -= gi * t.hi;
            -gi
        } else {
            r += gi * t.lo;
            gi
        };
        match t.slack {
            None => le.push((t.col, cw)),
            Some((y, d)) => {
                // w = d y - x
                le.push((y, cw * d));
                le.push((t.col, -cw));
            }
        }
    }
    for t in &conts {
        let hc = h * t.c; // coefficient on x', negative
        match t.sub {
            Sub::Lower => {
                le.push((t.col, hc));
                r += hc * lower[t.col];
            }
            Sub::Upper => {
                le.push((t.col, -hc));
                r -= hc * upper[t.col];
            }
            Sub::Vub(y) => {
                let d = bounds.vub[t.col].unwrap().d;
                le.push((y, hc * d));
                le.push((t.col, -hc));
            }
            Sub::Vlb(y) => {
                let d = bounds.vlb[t.col].unwrap().d;
                le.push((t.col, hc));
                le.push((y, -hc * d));
            }
        }
    }
    let ge = le.into_iter().map(|(j, c)| (j, -c)).collect();
    finish(round, ge, -r, Source::Cmir)
}
