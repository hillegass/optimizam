//! The CP engine: lazy clause generation in the style of OR-Tools CP-SAT,
//! for problems whose variables are all integer with finite bounds.
//!
//! - `domains`: the integer trail (bound literals, reasons, levels).
//! - `linear`: linear constraints with exact integer arithmetic.
//! - `clauses`: learned and blocking clauses with two watched literals.
//! - `solver`: CDCL search (1-UIP learning, VSIDS, Luby restarts).
//!
//! The engine works on the presolved problem. Rows and the objective are
//! scaled to integer coefficients (decimal scale factors), and row bounds are
//! rounded inward, which is exact because the left side is an integer.
//! Problems with continuous variables, infinite integer bounds, or
//! coefficients that don't scale to integers are not supported.

mod clauses;
mod domains;
mod linear;
mod solver;

use crate::mip::Mip;
use domains::Domains;
use linear::LinearStore;
use solver::{Search, Solver};
use std::time::Instant;

/// Scale factors tried to make a row's coefficients integral.
const SCALES: [f64; 9] = [1.0, 2.0, 4.0, 8.0, 10.0, 100.0, 1e3, 1e4, 1e6];
/// Largest magnitude of a bound or scaled coefficient.
const MAX_MAGNITUDE: f64 = 1e12;

pub struct Settings {
    pub want_all: bool,
    pub satisfy: bool,
    pub deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// Everything requested was found and proven.
    Complete,
    TimeLimit,
}

pub struct CpResult {
    pub end: End,
    /// Solutions found, in the problem's columns (with `want_all`: every one
    /// found so far; otherwise at most the best one).
    pub solutions: Vec<Vec<f64>>,
    /// For optimization: whether the best solution is proven optimal (with
    /// `want_all`, enumeration of the ties may still be incomplete).
    pub optimum_proven: bool,
}

/// An exact integer version of the problem.
struct IntModel {
    lb: Vec<i64>,
    ub: Vec<i64>,
    ranges: Vec<Option<Vec<(i64, i64)>>>,
    /// Rows as `sum(a x) <= rhs`.
    rows: Vec<(Vec<(u32, i64)>, i64)>,
    /// Minimization objective (scaled), if any.
    objective: Option<Vec<(u32, i64)>>,
    /// A constant row was violated: no solution.
    infeasible: bool,
}

fn integral(v: f64) -> bool {
    v.is_finite() && v.abs() <= MAX_MAGNITUDE && (v - v.round()).abs() <= 1e-9 * v.abs().max(1.0)
}

/// Scales coefficients to integers, or None.
fn scale(coefs: &[(usize, f64)]) -> Option<(f64, Vec<(u32, i64)>)> {
    let s = SCALES.into_iter().find(|&s| coefs.iter().all(|&(_, a)| integral(a * s)))?;
    Some((s, coefs.iter().filter(|c| c.1 != 0.0).map(|&(j, a)| (j as u32, (a * s).round() as i64)).collect()))
}

fn build(mip: &Mip) -> Result<IntModel, String> {
    let lp = &mip.lp;
    let n = lp.num_col;
    let (mut lb, mut ub, mut ranges) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    for j in 0..n {
        if !mip.integer[j] {
            return Err("it has a continuous variable".into());
        }
        let (l, u) = (lp.col_lower[j], lp.col_upper[j]);
        if !(l.is_finite() && u.is_finite()) || l.abs() > MAX_MAGNITUDE || u.abs() > MAX_MAGNITUDE {
            return Err("an integer variable has an infinite (or huge) bound".into());
        }
        lb.push((l - 1e-6).ceil() as i64);
        ub.push((u + 1e-6).floor() as i64);
        ranges.push(mip.domain[j].clone());
    }
    let mut rows = Vec::new();
    let mut infeasible = false;
    for i in 0..lp.num_row {
        let coefs: Vec<(usize, f64)> =
            (lp.ar_start[i]..lp.ar_start[i + 1]).map(|k| (lp.ar_index[k], lp.ar_value[k])).collect();
        let (s, terms) = scale(&coefs).ok_or("a row's coefficients don't scale to integers")?;
        let (lo, up) = (lp.row_lower[i] * s, lp.row_upper[i] * s);
        if terms.is_empty() {
            infeasible |= lo > 1e-9 || up < -1e-9;
            continue;
        }
        if up.is_finite() {
            if up.abs() > MAX_MAGNITUDE {
                return Err("a row bound is too large".into());
            }
            rows.push((terms.clone(), (up + 1e-9).floor() as i64));
        }
        if lo.is_finite() {
            if lo.abs() > MAX_MAGNITUDE {
                return Err("a row bound is too large".into());
            }
            rows.push((terms.iter().map(|&(j, a)| (j, -a)).collect(), -((lo - 1e-9).ceil() as i64)));
        }
    }
    let costs: Vec<(usize, f64)> =
        lp.col_cost.iter().enumerate().filter(|c| *c.1 != 0.0).map(|(j, &c)| (j, c)).collect();
    let objective = if costs.is_empty() {
        None
    } else {
        Some(scale(&costs).ok_or("the objective's coefficients don't scale to integers")?.1)
    };
    Ok(IntModel { lb, ub, ranges, rows, objective, infeasible })
}

/// Whether the CP engine can take this (presolved) problem; Err says why not.
pub fn applicable(mip: &Mip) -> Result<(), String> {
    build(mip).map(|_| ())
}

/// A solver for the model, with the objective as an extra constraint
/// `objective <= bound` (returned as its id) when given.
fn solver_for(m: &IntModel, objective_bound: Option<i64>) -> (Solver, Option<u32>) {
    let dom = Domains::new(m.lb.clone(), m.ub.clone(), m.ranges.clone());
    let mut lin = LinearStore::new(m.lb.len());
    for (terms, rhs) in &m.rows {
        lin.add(terms.clone(), *rhs);
    }
    let obj = match (&m.objective, objective_bound) {
        (Some(terms), Some(b)) => Some(lin.add(terms.clone(), b)),
        _ => None,
    };
    (Solver::new(dom, lin), obj)
}

fn objective_value(m: &IntModel, x: &[i64]) -> i64 {
    m.objective.as_ref().map_or(0, |t| t.iter().map(|&(j, a)| a * x[j as usize]).sum())
}

fn to_f64(x: &[i64]) -> Vec<f64> {
    x.iter().map(|&v| v as f64).collect()
}

pub fn solve(mip: &Mip, settings: &Settings) -> Result<CpResult, String> {
    let m = build(mip)?;
    let debug = std::env::var_os("OPTIMIZAM_DEBUG").is_some();
    let started = Instant::now();
    let report = |s: &Solver, what: &str| {
        if debug {
            eprintln!("cp ({what}): {:?}, {:.2}s", s.stats, started.elapsed().as_secs_f64());
        }
    };
    let none = |end| Ok(CpResult { end, solutions: vec![], optimum_proven: false });
    if m.infeasible {
        return none(End::Complete);
    }
    let optimize = !settings.satisfy && m.objective.is_some();

    // Satisfy (or an optimization problem whose objective is constant).
    if !optimize {
        let (mut s, _) = solver_for(&m, None);
        let mut solutions = Vec::new();
        let end = loop {
            match s.search(settings.deadline) {
                Search::Solution => {
                    solutions.push(to_f64(&s.values()));
                    if !settings.want_all {
                        break End::Complete;
                    }
                    s.block_solution();
                }
                Search::Exhausted => break End::Complete,
                Search::TimeLimit => break End::TimeLimit,
            }
        };
        report(&s, "satisfy");
        return Ok(CpResult { end, solutions, optimum_proven: end == End::Complete });
    }

    // Optimize: after each solution, require a strictly better objective.
    let (mut s, obj) = solver_for(&m, Some(i64::MAX / 4));
    let obj = obj.unwrap();
    let mut best: Option<(i64, Vec<i64>)> = None;
    let end = loop {
        match s.search(settings.deadline) {
            Search::Solution => {
                let x = s.values();
                let value = objective_value(&m, &x);
                best = Some((value, x));
                s.lin.cons[obj as usize].rhs = value - 1;
                s.restart();
                s.requeue(obj);
                s.propagate_root();
            }
            Search::Exhausted => break End::Complete,
            Search::TimeLimit => break End::TimeLimit,
        }
    };
    report(&s, "optimize");
    let Some((best_value, best_x)) = best else { return none(end) };
    if end == End::TimeLimit || !settings.want_all {
        return Ok(CpResult { end, solutions: vec![to_f64(&best_x)], optimum_proven: end == End::Complete });
    }

    // All optima: a fresh search (learned clauses above assumed a better
    // objective) for every solution with objective <= the optimum.
    let (mut s, _) = solver_for(&m, Some(best_value));
    let mut solutions = Vec::new();
    let end = loop {
        match s.search(settings.deadline) {
            Search::Solution => {
                solutions.push(to_f64(&s.values()));
                s.block_solution();
            }
            Search::Exhausted => break End::Complete,
            Search::TimeLimit => break End::TimeLimit,
        }
    };
    report(&s, "enumerate optima");
    if solutions.is_empty() {
        solutions.push(to_f64(&best_x));
    }
    Ok(CpResult { end, solutions, optimum_proven: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lp::{Lp, SparseRow};

    fn binaries(n: usize, rows: Vec<SparseRow>, cost: Vec<f64>) -> Mip {
        Mip { lp: Lp::new(cost, vec![0.0; n], vec![1.0; n], rows), integer: vec![true; n], domain: vec![None; n] }
    }

    /// The n-queens model of examples/four_queens.json, on binaries q[r][c].
    fn queens(n: usize) -> Mip {
        let var = |r: usize, c: usize| r * n + c;
        let mut rows: Vec<SparseRow> = Vec::new();
        for r in 0..n {
            rows.push(((0..n).map(|c| (var(r, c), 1.0)).collect(), 1.0, 1.0));
            rows.push(((0..n).map(|c| (var(c, r), 1.0)).collect(), 1.0, 1.0));
        }
        for d in -(n as i64 - 1)..n as i64 {
            let cells: Vec<(usize, f64)> = (0..n)
                .flat_map(|r| (0..n).map(move |c| (r, c)))
                .filter(|&(r, c)| r as i64 - c as i64 == d)
                .map(|(r, c)| (var(r, c), 1.0))
                .collect();
            rows.push((cells, f64::NEG_INFINITY, 1.0));
        }
        for s in 0..2 * n - 1 {
            let cells: Vec<(usize, f64)> = (0..n)
                .flat_map(|r| (0..n).map(move |c| (r, c)))
                .filter(|&(r, c)| r + c == s)
                .map(|(r, c)| (var(r, c), 1.0))
                .collect();
            rows.push((cells, f64::NEG_INFINITY, 1.0));
        }
        binaries(n * n, rows, vec![0.0; n * n])
    }

    #[test]
    fn counts_queens_solutions() {
        for (n, count) in [(1, 1), (2, 0), (3, 0), (4, 2), (5, 10), (6, 4), (7, 40), (8, 92)] {
            let r = solve(&queens(n), &Settings { want_all: true, satisfy: true, deadline: None }).unwrap();
            assert_eq!((r.end, r.solutions.len()), (End::Complete, count), "{n}-queens");
            let mut distinct = r.solutions.clone();
            distinct.sort_by(|a, b| a.partial_cmp(b).unwrap());
            distinct.dedup();
            assert_eq!(distinct.len(), count, "{n}-queens solutions repeat");
        }
    }

    /// Pigeonhole: 6 pigeons, 5 holes is infeasible (needs real search).
    #[test]
    fn pigeonhole_is_infeasible() {
        let (p, h) = (6, 5);
        let var = |i: usize, j: usize| i * h + j;
        let mut rows: Vec<SparseRow> = Vec::new();
        for i in 0..p {
            rows.push(((0..h).map(|j| (var(i, j), 1.0)).collect(), 1.0, f64::INFINITY));
        }
        for j in 0..h {
            rows.push(((0..p).map(|i| (var(i, j), 1.0)).collect(), f64::NEG_INFINITY, 1.0));
        }
        let r = solve(
            &binaries(p * h, rows, vec![0.0; p * h]),
            &Settings { want_all: false, satisfy: true, deadline: None },
        )
        .unwrap();
        assert_eq!((r.end, r.solutions.len()), (End::Complete, 0));
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

    /// On random small integer problems, the CP engine agrees with brute force:
    /// the set of all feasible points, and the optimum with all its ties.
    #[test]
    fn agrees_with_brute_force() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for case in 0..400 {
            let n = 3;
            let lower: Vec<f64> = (0..n).map(|_| rng.int(-2, 1) as f64).collect();
            let upper: Vec<f64> = lower.iter().map(|l| l + rng.int(0, 3) as f64).collect();
            let rows: Vec<SparseRow> = (0..rng.int(1, 4))
                .map(|_| {
                    let mut coefs = Vec::new();
                    for j in 0..n {
                        let a = rng.int(-4, 4);
                        if a != 0 {
                            coefs.push((j, a as f64));
                        }
                    }
                    let b = rng.int(-5, 8) as f64;
                    match rng.int(0, 2) {
                        0 => (coefs, b, b),
                        1 => (coefs, b, f64::INFINITY),
                        _ => (coefs, f64::NEG_INFINITY, b),
                    }
                })
                .collect();
            let cost: Vec<f64> = (0..n).map(|_| rng.int(-3, 3) as f64).collect();
            let mip = Mip {
                lp: Lp::new(cost.clone(), lower.clone(), upper.clone(), rows.clone()),
                integer: vec![true; n],
                domain: vec![None; n],
            };
            let mut feasible = Vec::new();
            for a in lower[0] as i64..=upper[0] as i64 {
                for b in lower[1] as i64..=upper[1] as i64 {
                    for c in lower[2] as i64..=upper[2] as i64 {
                        let x = [a as f64, b as f64, c as f64];
                        let ok = rows.iter().all(|(coefs, lo, up)| {
                            let ax: f64 = coefs.iter().map(|&(j, v)| v * x[j]).sum();
                            ax >= *lo && ax <= *up
                        });
                        if ok {
                            feasible.push(x.to_vec());
                        }
                    }
                }
            }
            let sorted = |mut v: Vec<Vec<f64>>| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                v
            };
            let all = solve(&mip, &Settings { want_all: true, satisfy: true, deadline: None }).unwrap();
            assert_eq!(sorted(all.solutions), sorted(feasible.clone()), "case {case}: all feasible points");
            let obj = |x: &Vec<f64>| x.iter().zip(&cost).map(|(a, b)| a * b).sum::<f64>();
            let best = feasible.iter().map(obj).fold(f64::INFINITY, f64::min);
            let ties: Vec<Vec<f64>> = feasible.iter().filter(|x| obj(x) == best).cloned().collect();
            let opt = solve(&mip, &Settings { want_all: true, satisfy: false, deadline: None }).unwrap();
            if cost.iter().all(|&c| c == 0.0) {
                continue; // treated as satisfy
            }
            assert_eq!(sorted(opt.solutions), sorted(ties), "case {case}: all optima");
        }
    }
}
