//! The CP engine: lazy clause generation in the style of OR-Tools CP-SAT,
//! for problems whose variables are all integer with finite bounds.
//!
//! - `domains`: the integer trail (bound literals, reasons, levels).
//! - `linear`: linear constraints with exact integer arithmetic.
//! - `clauses`: learned and blocking clauses with two watched literals.
//! - `solver`: CDCL search (1-UIP learning, VSIDS, Luby restarts).
//! - `scheduling`: native `no_overlap` and `cumulative` propagators.
//!
//! The engine works on the presolved problem. Rows and the objective are
//! scaled to integer coefficients (decimal scale factors), and row bounds are
//! rounded inward, which is exact because the left side is an integer.
//! Problems with continuous variables, infinite integer bounds, or
//! coefficients that don't scale to integers are not supported.

mod clauses;
mod domains;
mod linear;
mod scheduling;
mod solver;

use crate::encode::Affine;
use crate::mip::Mip;
use domains::Domains;
use linear::LinearStore;
use scheduling::{Kind, Resource, Task, Term};
use solver::{Search, Solver};
use std::collections::BTreeMap;
use std::time::Instant;

/// Scale factors tried to make a row's coefficients integral.
const SCALES: [f64; 9] = [1.0, 2.0, 4.0, 8.0, 10.0, 100.0, 1e3, 1e4, 1e6];
/// Largest magnitude of a bound or scaled coefficient.
const MAX_MAGNITUDE: f64 = 1e12;

/// A task of a scheduling constraint, over the problem's columns.
pub struct TaskDef {
    pub start: Affine,
    pub duration: Affine,
    /// A binary column; None if always present.
    pub presence: Option<usize>,
    /// Ignored for `no_overlap`.
    pub demand: Affine,
}

/// A `no_overlap` (no capacity) or `cumulative` constraint, which the engine
/// propagates natively.
pub struct Scheduling {
    pub tasks: Vec<TaskDef>,
    pub capacity: Option<Affine>,
}

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
    /// Scheduling constraints (over the columns and extra variables).
    resources: Vec<(Vec<Task>, Option<Term>)>,
    /// The problem's columns; variables after them are extra.
    num_cols: usize,
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

impl IntModel {
    fn new_var(&mut self, lo: i64, hi: i64) -> Result<Term, String> {
        if lo.abs() as f64 > MAX_MAGNITUDE || hi.abs() as f64 > MAX_MAGNITUDE {
            return Err("a scheduling quantity has a huge range".into());
        }
        self.lb.push(lo);
        self.ub.push(hi);
        self.ranges.push(None);
        Ok(Term::of(self.lb.len() - 1, 1, 0))
    }

    /// A new variable z with rows z = sum(terms) + constant.
    fn define(&mut self, terms: &[(u32, i64)], constant: i64) -> Result<Term, String> {
        let (mut lo, mut hi) = (constant as i128, constant as i128);
        for &(j, a) in terms {
            let (p, q) = (a as i128 * self.lb[j as usize] as i128, a as i128 * self.ub[j as usize] as i128);
            lo += p.min(q);
            hi += p.max(q);
        }
        let big = MAX_MAGNITUDE as i128;
        if lo.abs() > big || hi.abs() > big {
            return Err("a scheduling quantity has a huge range".into());
        }
        let z = self.new_var(lo as i64, hi as i64)?;
        let zv = z.var.unwrap();
        let mut row: Vec<(u32, i64)> = vec![(zv, 1)];
        row.extend(terms.iter().map(|&(j, a)| (j, -a)));
        self.rows.push((row.clone(), constant));
        self.rows.push((row.into_iter().map(|(j, a)| (j, -a)).collect(), -constant));
        Ok(z)
    }

    /// An affine expression of the columns as a term (with a new variable if
    /// it has several columns).
    fn term(&mut self, a: &Affine) -> Result<Term, String> {
        let int = |v: f64| integral(v).then_some(v.round() as i64).ok_or("scheduling values must be integers");
        let constant = int(a.constant)?;
        let terms: Vec<(u32, i64)> = a
            .coefs
            .iter()
            .filter(|c| c.1 != 0.0)
            .map(|&(j, v)| Ok((j as u32, int(v)?)))
            .collect::<Result<_, &str>>()?;
        match terms.as_slice() {
            [] => Ok(Term::constant(constant)),
            &[(j, c)] if c == 1 || c == -1 => Ok(Term::of(j as usize, c, constant)),
            _ => self.define(&terms, constant),
        }
    }

    /// The term a + b.
    fn sum(&mut self, a: Term, b: Term) -> Result<Term, String> {
        match (a.var, b.var) {
            (None, _) => Ok(b.plus(a.offset)),
            (_, None) => Ok(a.plus(b.offset)),
            (Some(x), Some(y)) => {
                let mut terms: BTreeMap<u32, i64> = BTreeMap::new();
                *terms.entry(x).or_default() += a.coef;
                *terms.entry(y).or_default() += b.coef;
                let terms: Vec<(u32, i64)> = terms.into_iter().filter(|t| t.1 != 0).collect();
                let constant = a.offset + b.offset;
                match terms.as_slice() {
                    [] => Ok(Term::constant(constant)),
                    &[(j, c)] if c == 1 || c == -1 => Ok(Term::of(j as usize, c, constant)),
                    _ => self.define(&terms, constant),
                }
            }
        }
    }

    /// Requires `t >= 0` (a demand); false if that is impossible.
    fn at_least_zero(&mut self, t: Term) -> bool {
        match t.var {
            None => t.offset >= 0,
            Some(x) => {
                let x = x as usize;
                if t.coef > 0 {
                    self.lb[x] = self.lb[x].max(-t.offset);
                } else {
                    self.ub[x] = self.ub[x].min(t.offset);
                }
                self.lb[x] <= self.ub[x]
            }
        }
    }

    fn add_scheduling(&mut self, s: &Scheduling) -> Result<(), String> {
        let mut tasks = Vec::new();
        for t in &s.tasks {
            let start = self.term(&t.start)?;
            let duration = self.term(&t.duration)?;
            let end = self.sum(start, duration)?;
            let demand = if s.capacity.is_some() { self.term(&t.demand)? } else { Term::constant(1) };
            if !self.at_least_zero(demand) {
                self.infeasible = true;
            }
            if let Some(p) = t.presence
                && !(self.lb[p] >= 0 && self.ub[p] <= 1)
            {
                return Err("an interval's presence must be binary".into());
            }
            tasks.push(Task { start, end, duration, demand, presence: t.presence.map(|p| p as u32) });
        }
        let capacity = s.capacity.as_ref().map(|c| self.term(c)).transpose()?;
        self.resources.push((tasks, capacity));
        Ok(())
    }
}

fn build(mip: &Mip, scheduling: &[Scheduling]) -> Result<IntModel, String> {
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
    let mut m = IntModel { lb, ub, ranges, rows, objective, infeasible, resources: Vec::new(), num_cols: n };
    for s in scheduling {
        m.add_scheduling(s)?;
    }
    Ok(m)
}

/// Whether the CP engine can take this (presolved) problem; Err says why not.
pub fn applicable(mip: &Mip, scheduling: &[Scheduling]) -> Result<(), String> {
    build(mip, scheduling).map(|_| ())
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
    let mut solver = Solver::new(dom, lin);
    for (tasks, capacity) in &m.resources {
        let kind = match capacity {
            None => Kind::NoOverlap,
            Some(c) => Kind::Cumulative { capacity: *c },
        };
        solver.add_resource(Resource { tasks: tasks.clone(), kind });
    }
    (solver, obj)
}

fn objective_value(m: &IntModel, x: &[i64]) -> i64 {
    m.objective.as_ref().map_or(0, |t| t.iter().map(|&(j, a)| a * x[j as usize]).sum())
}

pub fn solve(mip: &Mip, scheduling: &[Scheduling], settings: &Settings) -> Result<CpResult, String> {
    let m = build(mip, scheduling)?;
    // Values of the problem's columns (without the extra variables).
    let to_f64 = |x: &[i64]| -> Vec<f64> { x[..m.num_cols].iter().map(|&v| v as f64).collect() };
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
            let r = solve(&queens(n), &[], &Settings { want_all: true, satisfy: true, deadline: None }).unwrap();
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
            &[],
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
            let all = solve(&mip, &[], &Settings { want_all: true, satisfy: true, deadline: None }).unwrap();
            assert_eq!(sorted(all.solutions), sorted(feasible.clone()), "case {case}: all feasible points");
            let obj = |x: &Vec<f64>| x.iter().zip(&cost).map(|(a, b)| a * b).sum::<f64>();
            let best = feasible.iter().map(obj).fold(f64::INFINITY, f64::min);
            let ties: Vec<Vec<f64>> = feasible.iter().filter(|x| obj(x) == best).cloned().collect();
            let opt = solve(&mip, &[], &Settings { want_all: true, satisfy: false, deadline: None }).unwrap();
            if cost.iter().all(|&c| c == 0.0) {
                continue; // treated as satisfy
            }
            assert_eq!(sorted(opt.solutions), sorted(ties), "case {case}: all optima");
        }
    }
}
