//! The CP engine: lazy clause generation in the style of OR-Tools CP-SAT,
//! for problems whose variables are all integer with finite bounds.
//!
//! - `domains`: the integer trail (bound literals, reasons, levels).
//! - `linear`: linear constraints with exact integer arithmetic.
//! - `clauses`: learned and blocking clauses with two watched literals.
//! - `solver`: CDCL search (1-UIP learning, VSIDS, Luby restarts).
//! - `scheduling`: native `no_overlap` and `cumulative` propagators.
//! - `alldiff`: a native bounds-consistent `all_different` propagator.
//! - `optimize`: the optimizing search, which alternates the CDCL search
//!   with `lns`, large neighborhood search on the best solution.
//! - `portfolio`: with more than one thread, searches with different
//!   settings side by side, sharing solutions and clauses.
//!
//! The engine works on the presolved problem. Rows and the objective are
//! scaled to integer coefficients (decimal scale factors), and row bounds are
//! rounded inward, which is exact because the left side is an integer.
//! Problems with continuous variables, infinite integer bounds, or
//! coefficients that don't scale to integers are not supported.

mod alldiff;
mod clauses;
mod domains;
mod linear;
mod lns;
mod optimize;
mod portfolio;
mod scheduling;
mod solver;

use crate::encode::Affine;
use crate::milp::Milp;
use domains::Domains;
use linear::LinearStore;
use optimize::{Optimizer, Watch};
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

/// The constraints that the engine propagates natively, beside the problem's
/// rows: scheduling constraints, and `all_different` instances (their items)
/// that aren't encoded.
#[derive(Default)]
pub struct Native {
    pub scheduling: Vec<Scheduling>,
    pub all_different: Vec<Vec<Affine>>,
}

pub struct Settings<'a> {
    pub want_all: bool,
    pub satisfy: bool,
    pub deadline: Option<Instant>,
    /// Where improving solutions are reported (in the problem's columns).
    pub report: Option<&'a dyn crate::config::Report>,
    /// With `want_all`: stop at this many solutions.
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// Everything requested was found and proven.
    Complete,
    TimeLimit,
    /// As many solutions as `limit` asked for were found; there may be more.
    SolutionLimit,
}

pub struct CpResult {
    pub end: End,
    /// Solutions found, in the problem's columns (with `want_all`: every one
    /// found so far; otherwise at most the best one).
    pub solutions: Vec<Vec<f64>>,
    /// For optimization: whether the best solution is proven optimal (with
    /// `want_all`, enumeration of the ties may still be incomplete).
    pub optimum_proven: bool,
    /// For optimization stopped early: a lower bound on the optimum, in the
    /// problem's costs.
    pub bound: Option<f64>,
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
    /// The objective's terms are the problem's costs times this.
    objective_scale: f64,
    /// Scheduling constraints (over the columns and extra variables).
    resources: Vec<(Vec<Task>, Option<Term>)>,
    /// `all_different` instances for the native propagator.
    all_different: Vec<Vec<Term>>,
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

fn build(milp: &Milp, native: &Native) -> Result<IntModel, String> {
    let lp = &milp.lp;
    let n = lp.num_col;
    let (mut lb, mut ub, mut ranges) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    for j in 0..n {
        if !milp.integer[j] {
            return Err("it has a continuous variable".into());
        }
        let (l, u) = (lp.col_lower[j], lp.col_upper[j]);
        if !(l.is_finite() && u.is_finite()) || l.abs() > MAX_MAGNITUDE || u.abs() > MAX_MAGNITUDE {
            return Err("an integer variable has an infinite (or huge) bound".into());
        }
        lb.push((l - 1e-6).ceil() as i64);
        ub.push((u + 1e-6).floor() as i64);
        ranges.push(milp.domain[j].clone());
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
    let (objective_scale, objective) = if costs.is_empty() {
        (1.0, None)
    } else {
        let (s, terms) = scale(&costs).ok_or("the objective's coefficients don't scale to integers")?;
        (s, Some(terms))
    };
    let mut m = IntModel {
        lb,
        ub,
        ranges,
        rows,
        objective,
        objective_scale,
        infeasible,
        resources: Vec::new(),
        all_different: Vec::new(),
        num_cols: n,
    };
    for s in &native.scheduling {
        m.add_scheduling(s)?;
    }
    for items in &native.all_different {
        let items = items.iter().map(|item| m.term(item)).collect::<Result<_, _>>()?;
        m.all_different.push(items);
    }
    Ok(m)
}

/// Whether the CP engine can take this (presolved) problem; Err says why not.
pub fn applicable(milp: &Milp, native: &Native) -> Result<(), String> {
    build(milp, native).map(|_| ())
}

/// A solver for the model, with the objective as an extra constraint
/// `objective <= bound` (returned as its id) when given, extra rows, and
/// variables fixed to values.
fn solver_for(
    m: &IntModel,
    objective_bound: Option<i64>,
    extra_rows: &[(Vec<(u32, i64)>, i64)],
    fixed: &[(u32, i64)],
) -> (Solver, Option<u32>) {
    let (mut lb, mut ub) = (m.lb.clone(), m.ub.clone());
    for &(j, v) in fixed {
        // The value is feasible, so it lies in the domain.
        (lb[j as usize], ub[j as usize]) = (v, v);
    }
    let dom = Domains::new(lb, ub, m.ranges.clone());
    let mut lin = LinearStore::new(m.lb.len());
    for (terms, rhs) in m.rows.iter().chain(extra_rows) {
        lin.add(terms.clone(), *rhs, &dom);
    }
    let obj = match (&m.objective, objective_bound) {
        (Some(terms), Some(b)) => Some(lin.add(terms.clone(), b, &dom)),
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
    for items in &m.all_different {
        solver.add_resource(Resource { tasks: Vec::new(), kind: Kind::AllDifferent { items: items.clone() } });
    }
    (solver, obj)
}

fn objective_value(m: &IntModel, x: &[i64]) -> i64 {
    m.objective.as_ref().map_or(0, |t| t.iter().map(|&(j, a)| a * x[j as usize]).sum())
}

pub fn solve(milp: &Milp, native: &Native, settings: &Settings) -> Result<CpResult, String> {
    let m = build(milp, native)?;
    if m.infeasible {
        return Ok(CpResult { end: End::Complete, solutions: vec![], optimum_proven: false, bound: None });
    }
    let optimize = !settings.satisfy && m.objective.is_some();
    // With more than one thread, a portfolio (see `portfolio`), except to
    // enumerate solutions, which is one search's job.
    let workers = if optimize || !settings.want_all { crate::config::threads().saturating_sub(1) } else { 0 };
    let shared = portfolio::Shared::new();
    let run = Run { m: &m, settings, shared: (workers > 0).then_some(&shared), started: Instant::now() };
    // Every thread's limit checks also stop when one of them has finished.
    let (first, _) = crate::config::halting(&shared.halt, || {
        let deadline = settings.deadline;
        let worker = |id| portfolio::work(&m, &shared, deadline, id as u64, optimize);
        crate::config::team(workers + 1, worker, || {
            let result = if optimize { run.optimize() } else { run.satisfy() };
            shared.stop();
            result
        })
    });
    crate::config::count_nodes(shared.conflicts.load(std::sync::atomic::Ordering::Relaxed));
    match first {
        // All optima: a fresh search (the clauses learned while optimizing
        // assumed a better objective) for every solution with objective <=
        // the optimum.
        Phase::Optimum(value, x) => Ok(run.enumerate_optima(value, &x)),
        Phase::Done(result) => Ok(result),
    }
}

/// What the first phase of a solve leaves: the result, or the proven optimum
/// whose ties are still to be enumerated.
enum Phase {
    Done(CpResult),
    Optimum(i64, Vec<i64>),
}

/// One solve: the model, what is wanted, and, in a portfolio, what the
/// workers share with this (the calling) thread's search.
struct Run<'a> {
    m: &'a IntModel,
    settings: &'a Settings<'a>,
    shared: Option<&'a portfolio::Shared>,
    started: Instant,
}

impl Run<'_> {
    /// Values of the problem's columns (without the extra variables).
    fn columns(&self, x: &[i64]) -> Vec<f64> {
        x[..self.m.num_cols].iter().map(|&v| v as f64).collect()
    }

    /// A search of this thread has ended: counts its conflicts as the
    /// solve's nodes, and logs its statistics.
    fn log(&self, s: &Solver, what: &str) {
        crate::config::count_nodes(s.stats.conflicts);
        if crate::config::debug() {
            crate::config::debug_log!("cp ({what}): {:?}, {:.2}s", s.stats, self.started.elapsed().as_secs_f64());
        }
    }

    /// Whether a worker has finished the job.
    fn finished(&self) -> bool {
        self.shared.is_some_and(|sh| sh.finished())
    }

    /// One solution, or every solution.
    fn satisfy(&self) -> Phase {
        let settings = self.settings;
        let (mut s, _) = solver_for(self.m, None, &[], &[]);
        // Enumerating explores everything anyway, and after a restart the
        // search runs into the solutions it has already blocked again
        // (queens_12_all: 192k conflicts instead of 281k, 11.8 s instead of 22.5 s).
        s.restarts = !settings.want_all;
        let mut solutions = Vec::new();
        let end = loop {
            match s.search(settings.deadline, None) {
                Search::Solution => {
                    let x = self.columns(&s.values());
                    if let Some(r) = settings.report.filter(|_| solutions.is_empty()) {
                        r.solution(&x);
                    }
                    solutions.push(x);
                    if !settings.want_all {
                        break End::Complete;
                    }
                    if settings.limit == Some(solutions.len()) {
                        break End::SolutionLimit;
                    }
                    s.block_solution();
                }
                Search::Exhausted => break End::Complete,
                // A worker's solution (or its proof that there is none)
                // stops this search as a limit would.
                Search::TimeLimit if self.finished() => {
                    if let Some((_, x)) = self.shared.and_then(|sh| sh.better_than(i64::MAX)) {
                        let x = self.columns(&x);
                        if let Some(r) = settings.report {
                            r.solution(&x);
                        }
                        solutions.push(x);
                    }
                    break End::Complete;
                }
                Search::TimeLimit => break End::TimeLimit,
                Search::ConflictLimit => unreachable!(),
            }
        };
        self.log(&s, "satisfy");
        Phase::Done(CpResult { end, solutions, optimum_proven: end == End::Complete, bound: None })
    }

    /// The best solution, proven if the limits allow (see `optimize`).
    fn optimize(&self) -> Phase {
        let (m, settings) = (self.m, self.settings);
        let mut search = Optimizer::new(m, settings.deadline, self.shared.map(|sh| (sh, 0)));
        let mut watch = Reporter { run: self };
        let end = search.run(&mut watch);
        // The solution a worker found last, perhaps.
        let known = search.best.as_ref().map_or(i64::MAX, |b| b.0);
        if let Some((value, x)) = self.shared.and_then(|sh| sh.better_than(known)) {
            watch.better(value, &x);
            search.best = Some((value, x));
        }
        self.log(&search.solver, "optimize");
        if crate::config::debug() {
            let lns = &search.lns;
            crate::config::debug_log!(
                "cp lns: {} rounds, {} improvements, {} conflicts, {:.2}s ({})",
                lns.rounds,
                lns.improvements,
                lns.conflicts,
                lns.seconds,
                lns.summary()
            );
        }
        crate::config::count_nodes(search.lns.conflicts);
        let best_value = search.best.as_ref().map(|b| b.0);
        let bound = (end == End::TimeLimit).then(|| self.lower_bound(&search.solver, best_value));
        let Some((best_value, best_x)) = search.best else {
            return Phase::Done(CpResult { end, solutions: vec![], optimum_proven: false, bound });
        };
        if end == End::TimeLimit || !settings.want_all {
            let optimum_proven = end == End::Complete;
            return Phase::Done(CpResult { end, solutions: vec![self.columns(&best_x)], optimum_proven, bound });
        }
        Phase::Optimum(best_value, best_x)
    }

    /// A lower bound on the optimum, in the problem's costs: the objective's
    /// smallest value at level 0 bounds every solution better than the best
    /// one, which bounds itself.
    fn lower_bound(&self, s: &Solver, best: Option<i64>) -> f64 {
        let root = s.root_min(self.m.objective.as_ref().unwrap());
        let b = best.map_or(root, |v| root.min(v as i128));
        b as f64 / self.m.objective_scale
    }

    /// Every solution with objective at most `best_value`, the optimum,
    /// which `best_x` attains.
    fn enumerate_optima(&self, best_value: i64, best_x: &[i64]) -> CpResult {
        let (mut s, _) = solver_for(self.m, Some(best_value), &[], &[]);
        s.restarts = false; // enumerating, as in `satisfy`
        let mut solutions = Vec::new();
        let end = loop {
            match s.search(self.settings.deadline, None) {
                Search::Solution => {
                    solutions.push(self.columns(&s.values()));
                    if self.settings.limit == Some(solutions.len()) {
                        break End::SolutionLimit;
                    }
                    s.block_solution();
                }
                Search::Exhausted => break End::Complete,
                Search::TimeLimit => break End::TimeLimit,
                Search::ConflictLimit => unreachable!(),
            }
        };
        self.log(&s, "enumerate optima");
        if solutions.is_empty() {
            solutions.push(self.columns(best_x));
        }
        CpResult { end, solutions, optimum_proven: true, bound: None }
    }
}

/// Reports the calling thread's search to the solve's observer and log.
struct Reporter<'a> {
    run: &'a Run<'a>,
}

impl Watch for Reporter<'_> {
    fn better(&mut self, value: i64, x: &[i64]) {
        let run = self.run;
        if crate::config::debug() {
            crate::config::debug_log!("cp: objective {value} at {:.2}s", run.started.elapsed().as_secs_f64());
        }
        if let Some(r) = run.settings.report {
            r.solution(&run.columns(x));
        }
    }

    fn pause(&mut self, solver: &Solver, best: Option<i64>) {
        if let Some(r) = self.run.settings.report {
            r.bound(self.run.lower_bound(solver, best));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lp::{Lp, SparseRow};

    fn binaries(n: usize, rows: Vec<SparseRow>, cost: Vec<f64>) -> Milp {
        Milp { lp: Lp::new(cost, vec![0.0; n], vec![1.0; n], rows), integer: vec![true; n], domain: vec![None; n] }
    }

    /// The n-queens model of examples/four_queens.json, on binaries q[r][c].
    fn queens(n: usize) -> Milp {
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
            let r = solve(
                &queens(n),
                &Native::default(),
                &Settings { want_all: true, satisfy: true, deadline: None, report: None, limit: None },
            )
            .unwrap();
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
            &Native::default(),
            &Settings { want_all: false, satisfy: true, deadline: None, report: None, limit: None },
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

    /// A random problem on three integers in small ranges, with its costs
    /// and every feasible point.
    fn random_problem(rng: &mut Rng) -> (Milp, Vec<f64>, Vec<Vec<f64>>) {
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
        let milp = Milp {
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
        (milp, cost, feasible)
    }

    fn sorted(mut v: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    }

    /// On random small integer problems, the CP engine agrees with brute force:
    /// the set of all feasible points, and the optimum with all its ties.
    #[test]
    fn agrees_with_brute_force() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for case in 0..400 {
            let (milp, cost, feasible) = random_problem(&mut rng);
            let all = solve(
                &milp,
                &Native::default(),
                &Settings { want_all: true, satisfy: true, deadline: None, report: None, limit: None },
            )
            .unwrap();
            assert_eq!(sorted(all.solutions), sorted(feasible.clone()), "case {case}: all feasible points");
            let obj = |x: &Vec<f64>| x.iter().zip(&cost).map(|(a, b)| a * b).sum::<f64>();
            let best = feasible.iter().map(obj).fold(f64::INFINITY, f64::min);
            let ties: Vec<Vec<f64>> = feasible.iter().filter(|x| obj(x) == best).cloned().collect();
            let opt = solve(
                &milp,
                &Native::default(),
                &Settings { want_all: true, satisfy: false, deadline: None, report: None, limit: None },
            )
            .unwrap();
            if cost.iter().all(|&c| c == 0.0) {
                continue; // treated as satisfy
            }
            assert_eq!(sorted(opt.solutions), sorted(ties), "case {case}: all optima");
        }
    }

    /// With several threads, the portfolio's answers are the same: the
    /// optimum (with all its ties, which one search then enumerates), and
    /// one feasible point or the proof that there is none.
    #[test]
    fn portfolio_agrees_with_brute_force() {
        let config =
            crate::config::Config { presolve: true, symmetry: true, lns: true, log: None, cancel: None, threads: 4 };
        crate::config::scoped(config, || {
            let mut rng = Rng(0x9e3779b97f4a7c15);
            for case in 0..150 {
                let (milp, cost, feasible) = random_problem(&mut rng);
                let one = solve(
                    &milp,
                    &Native::default(),
                    &Settings { want_all: false, satisfy: true, deadline: None, report: None, limit: None },
                )
                .unwrap();
                assert_eq!(one.end, End::Complete, "case {case}");
                assert_eq!(one.solutions.len(), usize::from(!feasible.is_empty()), "case {case}: one solution");
                assert!(one.solutions.iter().all(|x| feasible.contains(x)), "case {case}: a feasible point");
                if cost.iter().all(|&c| c == 0.0) {
                    continue; // treated as satisfy
                }
                let obj = |x: &Vec<f64>| x.iter().zip(&cost).map(|(a, b)| a * b).sum::<f64>();
                let best = feasible.iter().map(obj).fold(f64::INFINITY, f64::min);
                let ties: Vec<Vec<f64>> = feasible.iter().filter(|x| obj(x) == best).cloned().collect();
                let opt = solve(
                    &milp,
                    &Native::default(),
                    &Settings { want_all: true, satisfy: false, deadline: None, report: None, limit: None },
                )
                .unwrap();
                assert_eq!(sorted(opt.solutions), sorted(ties), "case {case}: all optima");
            }
        });
    }
}
