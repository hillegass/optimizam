//! Cutting planes at the root node.
//!
//! Each round solves the root LP, removes cuts that are no longer binding,
//! and collects candidate cuts from two separators:
//! - [`cmir`]: complemented mixed-integer rounding and lifted cover
//!   inequalities on aggregations of model rows (Marchand and Wolsey; HiGHS's
//!   `HighsPathSeparator` and `HighsCutGeneration`).
//! - [`gmi`]: Gomory mixed-integer cuts from rows of the optimal tableau.
//!
//! Candidates are cleaned up numerically ([`finish`]), and the most
//! efficacious, mutually non-parallel ones are added to the LP as rows. Rounds
//! stop when the bound has stalled for a few of them. Those last rounds are
//! then undone, and every cut whose logical is basic is removed: the bound
//! doesn't need it, and it would slow each node's LP. The cuts that remain
//! are permanent rows, so every branch-and-bound node uses them. All cuts are valid for every
//! integer-feasible point, not just for the optimal ones.
//!
//! The search calls [`separate_root`] again when it restarts from tighter
//! root bounds: the cuts already in the LP stay, subject to the same removal,
//! and new ones are separated with the new bounds.

mod cmir;
mod gmi;

use crate::lp::Lp;
use crate::simplex::{self, Basis, LpStatus, Options};
use std::cell::RefCell;
use std::time::Instant;

const MAX_ROUNDS: usize = 50;
/// Rounds stop after this many in a row without progress. A degenerate LP
/// often needs a few rounds before its bound moves.
const STALL_ROUNDS: usize = 5;
/// Progress is a gain in the bound, since the last round that made progress,
/// of this much relative to max(1, |bound|), and of this share of everything
/// the cuts have gained so far (rounds tail off long before they stall).
const MIN_ROUND_GAIN: f64 = 1e-5;
const MIN_GAIN_SHARE: f64 = 0.001;
const MAX_CUTS_PER_ROUND: usize = 100;
/// Minimum violation at the LP point, divided by the cut's Euclidean norm.
const MIN_EFFICACY: f64 = 1e-4;
/// Maximum ratio of the largest to the smallest absolute coefficient.
const MAX_DYNAMISM: f64 = 1e6;
/// Coefficients below this, relative to the largest, are dropped (with the
/// right-hand side relaxed to stay valid).
const DROP_RELATIVE: f64 = 1e-9;
/// Most nonzeros in a cut: this fraction of the columns, plus an allowance
/// (without which a small problem with dense rows would get no cuts).
const MAX_DENSITY: f64 = 0.2;
const DENSITY_ALLOWANCE: usize = 100;
/// Cuts whose cosine with an already selected cut exceeds this are skipped.
const MAX_PARALLELISM: f64 = 0.99;
/// A cut whose slack exceeds this (relative) at the LP point is removed.
const INACTIVE_SLACK: f64 = 1e-6;

/// What the cut loop did, for tuning (printed with `OPTIMIZAM_DEBUG`).
#[derive(Debug, Default)]
pub struct Stats {
    pub rounds: usize,
    pub cmir_added: usize,
    pub gmi_added: usize,
    pub removed: usize,
    pub initial_objective: f64,
    pub final_objective: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Cmir,
    Gmi,
}

/// A cut `coefs . x >= rhs` over the structural columns, sorted by column.
struct Cut {
    coefs: Vec<(usize, f64)>,
    rhs: f64,
    efficacy: f64,
    source: Source,
}

/// What the separators see in one round.
struct Round<'a> {
    lp: &'a Lp,
    /// Rows `0..num_model_rows` are the model's; later rows are cuts.
    num_model_rows: usize,
    integer: &'a [bool],
    /// Root column bounds, integer columns rounded.
    lower: &'a [f64],
    upper: &'a [f64],
    /// The LP solution.
    x: &'a [f64],
    /// A zero per column, for [`finish`] to add up a cut's terms in.
    scratch: &'a RefCell<Vec<f64>>,
}

/// Runs root cut rounds, adding the cuts to `lp`, whose rows from
/// `num_model_rows` on are cuts of earlier calls. Returns the final optimal
/// basis of the extended LP, to warm-start the root node, or None if the
/// last solve did not end optimal (the root node then finds out why).
pub fn separate_root(
    lp: &mut Lp,
    num_model_rows: usize,
    integer: &[bool],
    lower: &[f64],
    upper: &[f64],
    deadline: Option<Instant>,
    stats: &mut Stats,
) -> Option<Basis> {
    let n = lp.num_col;
    let bounds = cmir::VariableBounds::detect(lp, integer, lower, upper);
    let scratch = RefCell::new(vec![0.0; n]);
    let mut basis: Option<Basis> = None;
    // The LP, basis, and bound of the last round that made progress. The
    // rounds after it are undone at the end: their cuts would only make the
    // node LPs slower.
    let mut kept: Option<(Lp, Basis, f64)> = None;
    let (mut stalled, mut rounds) = (0, 0);
    loop {
        let opts = Options { deadline, ..Options::default() };
        let sol = simplex::solve(lp, lower, upper, basis.as_ref(), &opts);
        if sol.status != LpStatus::Optimal {
            return None;
        }
        let b = sol.basis?;
        if stats.rounds == 0 {
            stats.initial_objective = sol.objective;
        }
        let gain = sol.objective - kept.as_ref().map_or(f64::NEG_INFINITY, |k| k.2);
        let tail = MIN_GAIN_SHARE * (sol.objective - stats.initial_objective);
        if gain >= tail.max(MIN_ROUND_GAIN * sol.objective.abs().max(1.0)) {
            stalled = 0;
            stats.final_objective = sol.objective;
            kept = Some((lp.clone(), b.clone(), sol.objective));
        } else {
            stalled += 1;
        }

        let mut chosen = Vec::new();
        let mut b = b;
        if stalled < STALL_ROUNDS && rounds < MAX_ROUNDS {
            b = remove_inactive(lp, num_model_rows, b, Some(&sol.col_value), stats);
            let round = Round { lp, num_model_rows, integer, lower, upper, x: &sol.col_value, scratch: &scratch };
            let mut pool = cmir::separate(&round, &bounds);
            pool.extend(gmi::separate(&round, &b));
            chosen = select(pool, n);
        }
        if chosen.is_empty() {
            if stalled > 0 {
                let (kept_lp, kept_basis, _) = kept?;
                *lp = kept_lp;
                b = kept_basis;
            }
            // The tree gets only the cuts that define this vertex.
            return Some(remove_inactive(lp, num_model_rows, b, None, stats));
        }
        rounds += 1;
        stats.rounds += 1;
        stats.cmir_added += chosen.iter().filter(|c| c.source == Source::Cmir).count();
        stats.gmi_added += chosen.iter().filter(|c| c.source == Source::Gmi).count();

        let added = chosen.len();
        let m_old = lp.num_row;
        let rows: Vec<_> = chosen.into_iter().map(|c| (c.coefs, c.rhs, f64::INFINITY)).collect();
        lp.add_rows(&rows);
        // The new rows' logicals start basic.
        b.basic_index.extend((m_old..lp.num_row).map(|i| n + i));
        b.nonbasic_move.extend(std::iter::repeat_n(0, added));
        b.dual_edge_weight.extend(std::iter::repeat_n(1.0, added));
        basis = Some(b);
    }
}

/// Removes cut rows whose logical is basic, and returns the basis restricted
/// to the remaining rows (still optimal, with the same bound). Given the LP
/// solution `x`, only the ones slack there go: a tight one may bind again once
/// the next round's cuts move the solution.
fn remove_inactive(lp: &mut Lp, num_model_rows: usize, basis: Basis, x: Option<&[f64]>, stats: &mut Stats) -> Basis {
    let (n, m) = (lp.num_col, lp.num_row);
    let mut is_basic = vec![false; n + m];
    for &v in &basis.basic_index {
        is_basic[v] = true;
    }
    let keep: Vec<bool> = (0..m)
        .map(|i| {
            if i < num_model_rows || !is_basic[n + i] {
                return true;
            }
            let Some(x) = x else { return false };
            let ax: f64 = (lp.ar_start[i]..lp.ar_start[i + 1]).map(|k| lp.ar_value[k] * x[lp.ar_index[k]]).sum();
            let lo = lp.row_lower[i];
            ax - lo <= INACTIVE_SLACK * lo.abs().max(1.0)
        })
        .collect();
    let removed = keep.iter().filter(|&&k| !k).count();
    if removed == 0 {
        return basis;
    }
    stats.removed += removed;
    lp.retain_rows(&keep);
    let mut new_row = vec![usize::MAX; m];
    let mut next = 0;
    for i in 0..m {
        if keep[i] {
            new_row[i] = next;
            next += 1;
        }
    }
    let renumber = |v: usize| if v < n { Some(v) } else { (new_row[v - n] != usize::MAX).then(|| n + new_row[v - n]) };
    let (mut basic_index, mut dual_edge_weight) = (Vec::new(), Vec::new());
    for (p, &v) in basis.basic_index.iter().enumerate() {
        if let Some(w) = renumber(v) {
            basic_index.push(w);
            dual_edge_weight.push(basis.dual_edge_weight[p]);
        }
    }
    let nonbasic_move =
        basis.nonbasic_move.iter().enumerate().filter(|&(v, _)| renumber(v).is_some()).map(|(_, &mv)| mv).collect();
    Basis::new(basic_index, nonbasic_move, dual_edge_weight)
}

/// Picks the most efficacious cuts, skipping near-parallel ones.
fn select(mut pool: Vec<Cut>, n: usize) -> Vec<Cut> {
    pool.sort_by(|a, b| b.efficacy.total_cmp(&a.efficacy));
    let mut chosen: Vec<(Cut, f64)> = Vec::new();
    let mut dense = vec![0.0; n];
    for cut in pool {
        if chosen.len() >= MAX_CUTS_PER_ROUND {
            break;
        }
        let norm = cut.coefs.iter().map(|(_, c)| c * c).sum::<f64>().sqrt();
        for &(j, c) in &cut.coefs {
            dense[j] = c;
        }
        let parallel = chosen.iter().any(|(other, other_norm)| {
            let dot: f64 = other.coefs.iter().map(|&(j, c)| c * dense[j]).sum();
            dot / (norm * other_norm) > MAX_PARALLELISM
        });
        for &(j, _) in &cut.coefs {
            dense[j] = 0.0;
        }
        if !parallel {
            chosen.push((cut, norm));
        }
    }
    chosen.into_iter().map(|(c, _)| c).collect()
}

/// Turns raw cut terms `coefs . x >= rhs` (duplicates allowed) into a clean
/// cut, or None if it is numerically unsafe, too dense, or not efficacious
/// at the LP point.
fn finish(round: &Round, coefs: Vec<(usize, f64)>, mut rhs: f64, source: Source) -> Option<Cut> {
    if !rhs.is_finite() {
        return None;
    }
    // Add up the terms of each column (a tableau cut has thousands).
    let mut sum = round.scratch.borrow_mut();
    for &(j, c) in &coefs {
        sum[j] += c;
    }
    let mut merged = coefs;
    merged.retain_mut(|(j, c)| {
        *c = std::mem::take(&mut sum[*j]);
        *c != 0.0
    });
    let max_abs = merged.iter().map(|(_, c)| c.abs()).fold(0.0, f64::max);
    if max_abs == 0.0 || !max_abs.is_finite() {
        return None;
    }

    // Drop tiny coefficients: sum_{others} c x >= rhs - c_j x_j >= rhs - max(c_j x_j).
    let mut kept = merged;
    kept.retain(|&(j, c)| {
        let worst = if c > 0.0 { c * round.upper[j] } else { c * round.lower[j] };
        let drop = c.abs() < DROP_RELATIVE * max_abs && worst.is_finite();
        if drop {
            rhs -= worst;
        }
        !drop
    });
    let min_abs = kept.iter().map(|(_, c)| c.abs()).fold(f64::INFINITY, f64::min);
    let max_nonzeros = (MAX_DENSITY * round.lp.num_col as f64) as usize + DENSITY_ALLOWANCE;
    if kept.is_empty() || max_abs / min_abs > MAX_DYNAMISM || kept.len() > max_nonzeros {
        return None;
    }
    kept.sort_unstable_by_key(|&(j, _)| j);

    // Scale to a largest coefficient of 1, and relax slightly for safety.
    let scale = 1.0 / max_abs;
    kept.iter_mut().for_each(|(_, c)| *c *= scale);
    let mut rhs = rhs * scale;
    rhs -= 1e-9 * rhs.abs().max(1.0);

    let norm = kept.iter().map(|(_, c)| c * c).sum::<f64>().sqrt();
    let activity: f64 = kept.iter().map(|&(j, c)| c * round.x[j]).sum();
    let efficacy = (rhs - activity) / norm;
    (efficacy >= MIN_EFFICACY).then_some(Cut { coefs: kept, rhs, efficacy, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LinearModel, Row, Sense};

    /// A tiny deterministic generator (xorshift).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn int(&mut self, lo: i64, hi: i64) -> i64 {
            lo + (self.next() % (hi - lo + 1) as u64) as i64
        }
    }

    type TestRow = (Vec<(usize, f64)>, f64, f64);

    fn lp_of(obj: Vec<f64>, rows: Vec<TestRow>, integer: Vec<bool>, lower: Vec<f64>, upper: Vec<f64>) -> Lp {
        let n = obj.len();
        let model = LinearModel {
            col_lower: lower,
            col_upper: upper,
            col_integer: integer,
            col_domain: vec![None; n],
            rows: rows.into_iter().map(|(coefs, lower, upper)| Row { coefs, lower, upper }).collect(),
            obj,
            obj_offset: 0.0,
            sense: Sense::Minimize,
            want_all: false,
            solution_limit: None,
            vars: vec![],
            violated_constant_row: None,
            uses_cp_kinds: false,
            scheduling: Vec::new(),
            nonlinear: None,
            col_start: Vec::new(),
        };
        Lp::from_model(&model, false)
    }

    fn activity(lp: &Lp, i: usize, x: &[f64]) -> f64 {
        (lp.ar_start[i]..lp.ar_start[i + 1]).map(|k| lp.ar_value[k] * x[lp.ar_index[k]]).sum()
    }

    fn satisfies(lp: &Lp, i: usize, x: &[f64]) -> bool {
        let ax = activity(lp, i, x);
        ax >= lp.row_lower[i] - 1e-7 && ax <= lp.row_upper[i] + 1e-7
    }

    /// Every integer point of random pure-integer problems satisfies every cut.
    #[test]
    fn cuts_keep_every_integer_point() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        let mut total = 0;
        for _ in 0..300 {
            let n = 3;
            let obj: Vec<f64> = (0..n).map(|_| rng.int(-9, 9) as f64).collect();
            let rows: Vec<_> = (0..3)
                .map(|_| {
                    let coefs: Vec<(usize, f64)> =
                        (0..n).map(|j| (j, rng.int(-6, 6) as f64)).filter(|c| c.1 != 0.0).collect();
                    let rhs = rng.int(0, 20) as f64;
                    if rng.int(0, 3) == 0 { (coefs, rhs, rhs) } else { (coefs, f64::NEG_INFINITY, rhs) }
                })
                .filter(|r| !r.0.is_empty())
                .collect();
            let (lower, upper) = (vec![0.0; n], vec![5.0; n]);
            let mut lp = lp_of(obj, rows, vec![true; n], lower.clone(), upper.clone());
            let m0 = lp.num_row;
            let mut stats = Stats::default();
            let rows = lp.num_row;
            separate_root(&mut lp, rows, &[true; 3], &lower, &upper, None, &mut stats);
            total += stats.cmir_added + stats.gmi_added;
            for a in 0..=5 {
                for b in 0..=5 {
                    for c in 0..=5 {
                        let x = [a as f64, b as f64, c as f64];
                        if (0..m0).all(|i| satisfies(&lp, i, &x)) {
                            for i in m0..lp.num_row {
                                assert!(satisfies(&lp, i, &x), "cut {i} removes integer point {x:?}");
                            }
                        }
                    }
                }
            }
        }
        assert!(total > 50, "only {total} cuts generated");
    }

    /// Mixed-integer validity: for random problems with two integer columns,
    /// one continuous column, and a variable upper bound on it, every integer
    /// assignment that admits a feasible continuous value still admits one
    /// after the cuts.
    #[test]
    fn cuts_keep_every_mixed_integer_point() {
        let mut rng = Rng(0x2545f4914f6cdd1d);
        let mut total = 0;
        for _ in 0..300 {
            // Columns: 0, 1 integer in [0, 4]; 2 binary; 3 continuous in [0, 10] with x3 <= 7 * x2.
            let obj = vec![rng.int(-9, 9) as f64, rng.int(-9, 9) as f64, rng.int(-5, 5) as f64, rng.int(-9, 9) as f64];
            let mut rows: Vec<TestRow> = (0..2)
                .map(|_| {
                    let coefs =
                        vec![(0, rng.int(-6, 6) as f64), (1, rng.int(-6, 6) as f64), (3, rng.int(-6, 6) as f64 + 0.5)];
                    (coefs, f64::NEG_INFINITY, rng.int(0, 20) as f64 + 0.25)
                })
                .collect();
            rows.push((vec![(2, -7.0), (3, 1.0)], f64::NEG_INFINITY, 0.0));
            let integer = vec![true, true, true, false];
            let (lower, upper) = (vec![0.0; 4], vec![4.0, 4.0, 1.0, 10.0]);
            let mut lp = lp_of(obj, rows, integer.clone(), lower.clone(), upper.clone());
            let m0 = lp.num_row;
            let mut stats = Stats::default();
            let rows = lp.num_row;
            separate_root(&mut lp, rows, &integer, &lower, &upper, None, &mut stats);
            total += stats.cmir_added + stats.gmi_added;
            // For fixed integers, each row is an interval constraint on x3;
            // compare the feasible x3 interval with and without cuts.
            let interval = |rows: std::ops::Range<usize>, xi: [f64; 3]| {
                let (mut lo, mut hi) = (0.0f64, 10.0f64);
                for i in rows {
                    let x = [xi[0], xi[1], xi[2], 0.0];
                    let rest = activity(&lp, i, &x);
                    let c3: f64 = (lp.ar_start[i]..lp.ar_start[i + 1])
                        .filter(|&k| lp.ar_index[k] == 3)
                        .map(|k| lp.ar_value[k])
                        .sum();
                    let (rl, ru) = (lp.row_lower[i] - rest, lp.row_upper[i] - rest);
                    if c3 == 0.0 {
                        if rl > 1e-7 || ru < -1e-7 {
                            return None;
                        }
                    } else if c3 > 0.0 {
                        lo = lo.max(rl / c3);
                        hi = hi.min(ru / c3);
                    } else {
                        lo = lo.max(ru / c3);
                        hi = hi.min(rl / c3);
                    }
                }
                (lo <= hi + 1e-7).then_some(())
            };
            for a in 0..=4 {
                for b in 0..=4 {
                    for y in 0..=1 {
                        let xi = [a as f64, b as f64, y as f64];
                        if interval(0..m0, xi).is_some() {
                            assert!(interval(0..lp.num_row, xi).is_some(), "cuts remove integer assignment {xi:?}");
                        }
                    }
                }
            }
        }
        assert!(total > 20, "only {total} cuts generated");
    }

    /// The tableau row reproduces each basic variable's value from the nonbasic values.
    #[test]
    fn tableau_row_matches_solution() {
        let lp = lp_of(
            vec![-5.0, -4.0],
            vec![
                (vec![(0, 6.0), (1, 4.0)], f64::NEG_INFINITY, 24.0),
                (vec![(0, 1.0), (1, 2.0)], f64::NEG_INFINITY, 6.0),
            ],
            vec![true; 2],
            vec![0.0; 2],
            vec![f64::INFINITY; 2],
        );
        let sol = simplex::solve(&lp, &lp.col_lower, &lp.col_upper, None, &Options::default());
        assert_eq!(sol.status, LpStatus::Optimal);
        let basis = sol.basis.unwrap();
        let x = &sol.col_value;
        let mut all = x.clone();
        for i in 0..lp.num_row {
            all.push(-activity(&lp, i, x));
        }
        let rows = simplex::tableau_rows(&lp, &basis, &basis.basic_index).unwrap();
        for (v, row) in basis.basic_index.iter().zip(&rows) {
            let implied: f64 = -row.iter().map(|&(k, a)| a * all[k]).sum::<f64>();
            assert!((implied - all[*v]).abs() < 1e-9, "var {v}: {implied} vs {}", all[*v]);
        }
    }

    /// max 5x + 4y, 6x + 4y <= 24, x + 2y <= 6: LP 21 at (3, 1.5), integer
    /// optimum 20. Cuts move the bound toward 20 without passing it.
    #[test]
    fn cuts_raise_the_bound() {
        let (lower, upper) = (vec![0.0; 2], vec![10.0; 2]);
        let mut lp = lp_of(
            vec![-5.0, -4.0],
            vec![
                (vec![(0, 6.0), (1, 4.0)], f64::NEG_INFINITY, 24.0),
                (vec![(0, 1.0), (1, 2.0)], f64::NEG_INFINITY, 6.0),
            ],
            vec![true; 2],
            lower.clone(),
            upper.clone(),
        );
        let mut stats = Stats::default();
        let rows = lp.num_row;
        separate_root(&mut lp, rows, &[true; 2], &lower, &upper, None, &mut stats);
        assert!((stats.initial_objective + 21.0).abs() < 1e-9);
        assert!(stats.final_objective > -21.0 + 0.1, "{stats:?}");
        assert!(stats.final_objective <= -20.0 + 1e-6, "{stats:?}");
    }
}
