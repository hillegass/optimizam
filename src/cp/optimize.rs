//! The optimizing search: clause learning with `objective <= best - 1` as a
//! constraint, tightened at each better solution, alternating with large
//! neighborhood search on the best solution once there is one. The main
//! search alone can prove optimality; LNS finds better solutions sooner.
//!
//! Alone, it is the whole CP optimization. In a portfolio (see
//! `portfolio`), every thread runs one, each with its own settings, and they
//! exchange solutions and clauses at their pauses.

use super::lns::{self, Lns};
use super::portfolio::{self, Shared};
use super::solver::{Search, Solver};
use super::{End, IntModel, objective_value, solver_for};
use std::time::Instant;

/// Conflicts of the main search between LNS phases.
const MAIN_SLICE: u64 = 1000;
/// The least an LNS phase gets, as a share of what it gets at most.
const MIN_LNS_SHARE: f64 = 0.125;
/// How many times `MAIN_SLICE` an LNS phase gets at most: in a portfolio's
/// improving searches, which leave the proof to the others while LNS pays.
const IMPROVER_WEIGHT: f64 = 4.0;

/// Told what a search finds (the calling thread reports it; workers are
/// [`Quiet`]).
pub(super) trait Watch {
    /// A better solution became this search's cutoff.
    fn better(&mut self, value: i64, x: &[i64]);
    /// A moment when the search's bounds at level 0 are worth reporting.
    fn pause(&mut self, solver: &Solver, best: Option<i64>);
}

pub(super) struct Quiet;

impl Watch for Quiet {
    fn better(&mut self, _value: i64, _x: &[i64]) {}
    fn pause(&mut self, _solver: &Solver, _best: Option<i64>) {}
}

pub(super) struct Optimizer<'a> {
    m: &'a IntModel,
    deadline: Option<Instant>,
    /// In a portfolio: what the searches share, and this one's number.
    team: Option<(&'a Shared, u64)>,
    pub solver: Solver,
    /// The objective's constraint.
    objective: u32,
    pub best: Option<(i64, Vec<i64>)>,
    pub lns: Lns,
    /// How many times `MAIN_SLICE` an LNS phase gets at most (0: no LNS),
    /// and the share of that it gets now: halved after a phase without an
    /// improvement (down to `MIN_LNS_SHARE`), doubled after one.
    lns_weight: f64,
    lns_share: f64,
    /// The main search's conflicts since the last LNS phase.
    since_lns: u64,
    /// How many of the shared clauses this search has heard.
    seen: usize,
}

impl<'a> Optimizer<'a> {
    pub fn new(m: &'a IntModel, deadline: Option<Instant>, team: Option<(&'a Shared, u64)>) -> Self {
        let (mut solver, objective) = solver_for(m, Some(i64::MAX / 4), &[], &[]);
        let id = team.map_or(0, |t| t.1);
        if team.is_some() {
            portfolio::diversify(&mut solver, id);
        }
        // Of the workers, every other one improves solutions.
        let lns_weight = match (crate::config::lns(), id) {
            (false, _) => 0.0,
            (true, 0) => 1.0,
            (true, id) if id % 2 == 0 => IMPROVER_WEIGHT,
            _ => 0.0,
        };
        Optimizer {
            m,
            deadline,
            team,
            solver,
            objective: objective.expect("an objective"),
            best: None,
            lns: Lns::new(m, id),
            lns_weight,
            lns_share: 1.0,
            since_lns: 0,
            seen: 0,
        }
    }

    /// Searches until the best solution is proven optimal (by this search or
    /// another of its team) or the limits are reached.
    pub fn run(&mut self, watch: &mut dyn Watch) -> End {
        // In a portfolio, the search pauses more often, to hear of the
        // others' solutions; LNS still gets its turn every MAIN_SLICE.
        let pause = self.team.map(|_| portfolio::SLICE);
        loop {
            self.hear(watch);
            let slice = match (self.best.is_some() && self.lns_weight > 0.0, pause) {
                (true, None) => Some(MAIN_SLICE),
                (_, pause) => pause,
            };
            match self.solver.search(self.deadline, slice) {
                Search::Solution => {
                    let x = self.solver.values();
                    let value = objective_value(self.m, &x);
                    self.adopt(value, x, watch);
                    watch.pause(&self.solver, self.best.as_ref().map(|b| b.0));
                }
                Search::Exhausted => {
                    if let Some((shared, _)) = self.team {
                        shared.finish();
                    }
                    return End::Complete;
                }
                Search::TimeLimit if self.team.is_some_and(|t| t.0.finished()) => return End::Complete,
                Search::TimeLimit => return End::TimeLimit,
                Search::ConflictLimit => {
                    self.since_lns += slice.unwrap_or(MAIN_SLICE);
                    if self.best.is_some() && self.lns_weight > 0.0 && self.since_lns >= MAIN_SLICE {
                        self.since_lns = 0;
                        self.improve(watch);
                    }
                }
            }
        }
    }

    /// An LNS phase on the best solution.
    fn improve(&mut self, watch: &mut dyn Watch) {
        watch.pause(&self.solver, self.best.as_ref().map(|b| b.0));
        let until = self.lns.conflicts + (MAIN_SLICE as f64 * self.lns_weight * self.lns_share) as u64;
        let mut any = false;
        while self.lns.conflicts < until && !crate::config::expired(self.deadline) {
            self.hear(watch);
            let (value, x) = self.best.as_ref().unwrap();
            if let Some(better) = self.lns.round(self.m, x, *value, self.deadline, lns::CONFLICTS) {
                self.solver.set_phase(&better.x);
                self.adopt(better.value, better.x, watch);
                any = true;
            }
        }
        self.lns_share = if any { (self.lns_share * 2.0).min(1.0) } else { (self.lns_share / 2.0).max(MIN_LNS_SHARE) };
    }

    /// In a portfolio: exchanges clauses with the other searches, and takes
    /// over their best solution if it is better than this one's.
    fn hear(&mut self, watch: &mut dyn Watch) {
        let Some((shared, id)) = self.team else { return };
        shared.exchange(id, &mut self.solver, &mut self.seen);
        let known = self.best.as_ref().map_or(i64::MAX, |b| b.0);
        if let Some((value, x)) = shared.better_than(known) {
            self.solver.set_phase(&x);
            self.adopt(value, x, watch);
        }
    }

    /// Makes a better solution the best one, and the search's cutoff.
    fn adopt(&mut self, value: i64, x: Vec<i64>, watch: &mut dyn Watch) {
        watch.better(value, &x);
        if let Some((shared, _)) = self.team {
            shared.offer(value, &x);
        }
        self.best = Some((value, x));
        self.solver.lin.cons[self.objective as usize].rhs = value - 1;
        self.solver.restart();
        self.solver.requeue(self.objective);
        self.solver.propagate_root();
    }
}
