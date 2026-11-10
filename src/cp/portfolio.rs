//! A parallel portfolio, in the manner of OR-Tools CP-SAT: beside the
//! search that `solve` runs on the calling thread, workers on other threads
//! search the same model in their own ways, and all share what they find.
//!
//! Every search is an [`Optimizer`](super::optimize::Optimizer) (or, to
//! find one solution, a plain clause-learning search) with settings of its
//! own: restart schedule, activity decay, the order and values it starts
//! with, and how much of its time goes to large neighborhood search.
//!
//! They share:
//! - the best solution: each search takes a better one over as its objective
//!   cutoff at its next pause, and LNS improves whichever is best;
//! - learned clauses that are short or of low LBD, and bounds learned at
//!   level 0, which join a search at its next restart. A clause learned
//!   under one cutoff may cut off solutions that only a weaker cutoff
//!   allows, and those are no better than a solution already found;
//! - the end: the first search to exhaust its space has proven the best
//!   solution optimal (its cutoff can't be weaker than that solution's
//!   objective, or its space would hold that solution), and halts the rest.
//!
//! The calling thread reports progress, since the observer lives there;
//! workers only offer solutions. A run with workers isn't deterministic:
//! which search finds or learns what first depends on timing.

use super::domains::Lit;
use super::optimize::{Optimizer, Quiet};
use super::solver::{Search, Solver};
use super::{IntModel, solver_for};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Conflicts between a search's looks at what the others have found.
pub(super) const SLICE: u64 = 200;
/// The clause pool stops growing at this many literals.
const POOL_LITERALS: usize = 20_000_000;

/// What the searches of a portfolio share.
pub(super) struct Shared {
    /// The best solution found: its objective (0 without one) and values.
    best: Mutex<Option<(i64, Vec<i64>)>>,
    /// Its objective, or `i64::MAX` while there is none: for a cheap look.
    value: AtomicI64,
    pool: Mutex<Pool>,
    /// Set when one search has finished the job for all (and at the end, to
    /// stop the workers): every search's limit check then says stop.
    pub halt: Arc<AtomicBool>,
    finished: AtomicBool,
}

/// The clauses the searches have told each other.
#[derive(Default)]
struct Pool {
    clauses: Vec<Told>,
    /// Their literals, in all.
    literals: usize,
}

struct Told {
    /// The search that learned it.
    by: u64,
    clause: Arc<[Lit]>,
    lbd: u32,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            best: Mutex::new(None),
            value: AtomicI64::new(i64::MAX),
            pool: Mutex::new(Pool::default()),
            halt: Arc::new(AtomicBool::new(false)),
            finished: AtomicBool::new(false),
        }
    }

    /// Offers a solution; true if it is better than the best so far.
    pub fn offer(&self, value: i64, x: &[i64]) -> bool {
        let mut best = self.best.lock().unwrap();
        if best.as_ref().is_some_and(|b| b.0 <= value) {
            return false;
        }
        *best = Some((value, x.to_vec()));
        self.value.store(value, Ordering::Release);
        true
    }

    /// The best solution, if it is better than `known`.
    pub fn better_than(&self, known: i64) -> Option<(i64, Vec<i64>)> {
        if self.value.load(Ordering::Acquire) >= known {
            return None;
        }
        self.best.lock().unwrap().clone().filter(|b| b.0 < known)
    }

    /// Records that a search has finished the job (exhausted its space, or
    /// found the one solution wanted), and stops the others.
    pub fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        self.halt.store(true, Ordering::Release);
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Stops the workers (the calling thread's search has ended).
    pub fn stop(&self) {
        self.halt.store(true, Ordering::Release);
    }

    /// Search `id` tells its new clauses and hears the others' since the
    /// first `seen` (which is updated).
    pub fn exchange(&self, id: u64, solver: &mut Solver, seen: &mut usize) {
        let mut pool = self.pool.lock().unwrap();
        solver.inbox.extend(pool.clauses[*seen..].iter().filter(|t| t.by != id).map(|t| (t.clause.clone(), t.lbd)));
        for (clause, lbd) in solver.outbox.drain(..) {
            if pool.literals + clause.len() <= POOL_LITERALS {
                pool.literals += clause.len();
                pool.clauses.push(Told { by: id, clause: clause.into(), lbd });
            }
        }
        *seen = pool.clauses.len();
    }
}

/// Makes search number `id` (0 is the calling thread's, which keeps the
/// defaults) different from the others: its restart schedule, its
/// activities' decay, and the order and values it starts with.
pub(super) fn diversify(solver: &mut Solver, id: u64) {
    solver.share = true;
    if id == 0 {
        return;
    }
    // Quick restarts, the default, rare restarts, ...
    const RESTART_UNITS: [u64; 4] = [30, 100, 1000, 300];
    const DECAYS: [f64; 3] = [0.95, 0.99, 0.9];
    let variant = (id / 2) as usize;
    solver.restart_unit = RESTART_UNITS[variant % RESTART_UNITS.len()];
    solver.var_decay = DECAYS[variant % DECAYS.len()];
    solver.task_branching = variant % 5 != 4;
    solver.diversify(id, variant % 4 == 3);
}

/// Worker `id`'s part (the calling thread's search is the first, 0). With
/// `optimize`, it minimizes the model's objective; otherwise the first
/// solution any search finds ends the job.
pub(super) fn work(m: &IntModel, shared: &Shared, deadline: Option<Instant>, id: u64, optimize: bool) {
    if !optimize {
        return find_one(m, shared, deadline, id);
    }
    let mut search = Optimizer::new(m, deadline, Some((shared, id)));
    search.run(&mut Quiet);
    if crate::config::debug() {
        let (stats, lns) = (&search.solver.stats, &search.lns);
        crate::config::debug_log!(
            "cp worker {id}: {stats:?}; lns: {} rounds, {} improvements, {} conflicts",
            lns.rounds,
            lns.improvements,
            lns.conflicts
        );
    }
}

/// A worker's search for one solution.
fn find_one(m: &IntModel, shared: &Shared, deadline: Option<Instant>, id: u64) {
    let (mut solver, _) = solver_for(m, None, &[], &[]);
    diversify(&mut solver, id);
    let mut seen = 0;
    loop {
        shared.exchange(id, &mut solver, &mut seen);
        match solver.search(deadline, Some(SLICE)) {
            Search::Solution => {
                shared.offer(0, &solver.values());
                shared.finish();
                return;
            }
            Search::Exhausted => {
                shared.finish();
                return;
            }
            Search::TimeLimit => return,
            Search::ConflictLimit => {}
        }
    }
}
