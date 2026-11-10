//! CDCL search over the integer trail (lazy clause generation).
//!
//! Propagation runs clauses and linear constraints to a fixed point. A
//! conflict is a set of true literals that can't all hold. Conflict analysis
//! (1-UIP) repeatedly replaces the latest literal of the conflict level by its
//! explanation until one literal of that level remains, learns the clause
//! "not all of these", backjumps to the second-highest level in it, and
//! asserts the negated UIP. Branching picks the most active open variable
//! (VSIDS) and tries its saved value, the smallest at first. With scheduling
//! constraints, until the first solution, it first starts the present task
//! that can start earliest (ties: end earliest) as early as possible. Restarts follow
//! the Luby sequence, and learned clauses with high LBD are deleted when
//! there are too many.

use super::clauses::ClauseStore;
use super::domains::{Domains, Lit, Reason};
use super::linear::LinearStore;
use super::scheduling::{Resource, Term};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, DefaultHasher};
use std::sync::Arc;

/// A hash map with a fixed hasher, so that iteration order (which shapes
/// learned clauses) and so the whole search are the same on every run.
type FixedMap<K, V> = HashMap<K, V, BuildHasherDefault<DefaultHasher>>;
use std::time::Instant;

const RESTART_UNIT: u64 = 100;
const VAR_DECAY: f64 = 0.95;
const CLAUSE_DECAY: f64 = 0.999;
/// A search that shares what it learns tells the others its clauses of at
/// most this LBD, or of at most `SHARE_LEN` literals, if they have at most
/// `SHARE_MAX_LEN` (and every bound it learns at level 0).
const SHARE_LBD: u32 = 3;
const SHARE_LEN: usize = 3;
const SHARE_MAX_LEN: usize = 30;
const FIRST_REDUCE: usize = 2000;
/// Learned clauses with at most this LBD are never deleted.
const KEEP_LBD: u32 = 2;

pub enum Search {
    /// Every variable is fixed and no constraint is violated.
    Solution,
    /// No solution remains.
    Exhausted,
    TimeLimit,
    /// The conflict limit was reached.
    ConflictLimit,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub decisions: u64,
    pub conflicts: u64,
    pub restarts: u64,
    pub learned: u64,
    pub solutions: u64,
    pub deleted: u64,
}

pub struct Solver {
    pub dom: Domains,
    pub lin: LinearStore,
    pub clauses: ClauseStore,
    qhead: usize,
    lin_queue: Vec<u32>,
    lin_queued: Vec<bool>,
    resources: Vec<Resource>,
    /// For each variable: the resources that read its bounds.
    res_watch: Vec<Vec<u32>>,
    res_queue: Vec<u32>,
    res_queued: Vec<bool>,
    /// Explanations of the trail entries with `Reason::Stored`.
    reasons: Vec<Vec<Lit>>,
    /// `reasons.len()` when each decision level after 0 began.
    reason_marks: Vec<usize>,
    /// Distinct tasks of the resources, for scheduling branching: (start, end, presence).
    branch_tasks: Vec<(Term, Term, Option<u32>)>,
    activity: Vec<f64>,
    var_inc: f64,
    clause_inc: f64,
    heap: Heap,
    /// Preferred value of each variable (phase saving).
    phase: Vec<i64>,
    conflicts_at_restart: u64,
    /// Whether to restart (on the Luby schedule).
    pub restarts: bool,
    /// The Luby schedule's unit, in conflicts, and the decay of variable
    /// activities per conflict: the defaults, or what a portfolio worker
    /// sets to search differently from the others.
    pub restart_unit: u64,
    pub var_decay: f64,
    /// Whether to branch on tasks (the earliest start first) until the
    /// first solution, when there are scheduling constraints.
    pub task_branching: bool,
    /// In a portfolio: whether to put the learned clauses worth telling the
    /// other searches in `outbox` (with their LBD); and the clauses the
    /// others have told, which join this search at its next restart.
    pub share: bool,
    pub outbox: Vec<(Vec<Lit>, u32)>,
    pub inbox: Vec<(Arc<[Lit]>, u32)>,
    luby_index: u64,
    max_learned: usize,
    learned_live: usize,
    /// No solution exists (a conflict at level 0).
    unsat: bool,
    pub stats: Stats,
}

impl Solver {
    pub fn new(dom: Domains, lin: LinearStore) -> Self {
        let n = dom.num_vars();
        let phase = (0..n).map(|v| dom.lb(v)).collect();
        let lin_queued = vec![true; lin.cons.len()];
        let lin_queue = (0..lin.cons.len() as u32).collect();
        Solver {
            clauses: ClauseStore::new(n),
            dom,
            lin,
            qhead: 0,
            lin_queue,
            lin_queued,
            resources: Vec::new(),
            res_watch: vec![Vec::new(); n],
            res_queue: Vec::new(),
            res_queued: Vec::new(),
            reasons: Vec::new(),
            reason_marks: Vec::new(),
            branch_tasks: Vec::new(),
            activity: vec![0.0; n],
            var_inc: 1.0,
            clause_inc: 1.0,
            heap: Heap::new(n),
            phase,
            conflicts_at_restart: 0,
            restarts: true,
            restart_unit: RESTART_UNIT,
            var_decay: VAR_DECAY,
            task_branching: true,
            share: false,
            outbox: Vec::new(),
            inbox: Vec::new(),
            luby_index: 0,
            max_learned: FIRST_REDUCE,
            learned_live: 0,
            unsat: false,
            stats: Stats::default(),
        }
    }

    /// Adds a scheduling propagator (before searching).
    pub fn add_resource(&mut self, r: Resource) {
        let id = self.resources.len() as u32;
        for v in r.vars() {
            self.res_watch[v as usize].push(id);
        }
        for t in &r.tasks {
            let key = |s: &Term| (s.var, s.coef, s.offset);
            if t.start.var.is_some()
                && !self.branch_tasks.iter().any(|b| key(&b.0) == key(&t.start) && b.2 == t.presence)
            {
                self.branch_tasks.push((t.start, t.end, t.presence));
            }
        }
        self.resources.push(r);
        self.res_queue.push(id);
        self.res_queued.push(true);
    }

    /// Queues linear constraint `c` for propagation (after its rhs changed).
    pub fn requeue(&mut self, c: u32) {
        if !self.lin_queued[c as usize] {
            self.lin_queued[c as usize] = true;
            self.lin_queue.push(c);
        }
    }

    fn propagate(&mut self) -> Result<(), Vec<Lit>> {
        loop {
            while self.qhead < self.dom.trail.len() {
                let idx = self.qhead;
                let lit = self.dom.trail[idx].lit;
                self.qhead += 1;
                let (var, upper) = (lit.var(), lit.upper);
                // The linear activities first: every processed entry must be in
                // them, even if a clause conflict follows.
                let (queued, queue) = (&mut self.lin_queued, &mut self.lin_queue);
                self.lin.apply(idx, &self.dom, |c| {
                    if !queued[c as usize] {
                        queued[c as usize] = true;
                        queue.push(c);
                    }
                });
                self.clauses.propagate(var, upper, &mut self.dom)?;
                for &r in &self.res_watch[var] {
                    if !self.res_queued[r as usize] {
                        self.res_queued[r as usize] = true;
                        self.res_queue.push(r);
                    }
                }
            }
            // Linear constraints first: they are cheaper.
            if let Some(c) = self.lin_queue.pop() {
                self.lin_queued[c as usize] = false;
                self.lin.propagate(c, &mut self.dom)?;
                continue;
            }
            let Some(r) = self.res_queue.pop() else { return Ok(()) };
            self.res_queued[r as usize] = false;
            self.resources[r as usize].propagate(&mut self.dom, &mut self.reasons)?;
        }
    }

    fn clear_queue(&mut self) {
        for c in self.lin_queue.drain(..) {
            self.lin_queued[c as usize] = false;
        }
        for r in self.res_queue.drain(..) {
            self.res_queued[r as usize] = false;
        }
    }

    pub fn backtrack(&mut self, level: usize) {
        if level >= self.dom.level() {
            return;
        }
        let start = self.dom.level_start(level);
        for e in &self.dom.trail[start..] {
            let v = e.lit.var();
            if self.dom.is_fixed(v) {
                self.phase[v] = self.dom.lb(v);
            }
        }
        let popped: Vec<usize> = self.dom.trail[start..].iter().map(|e| e.lit.var()).collect();
        // Take the processed entries back out of the linear activities.
        for idx in (start..self.qhead.min(self.dom.trail.len())).rev() {
            self.lin.unapply(idx, &self.dom);
        }
        self.dom.backtrack_to(level);
        self.reasons.truncate(self.reason_marks[level]);
        self.reason_marks.truncate(level);
        for v in popped {
            self.heap.insert(v, &self.activity);
        }
        self.qhead = self.qhead.min(self.dom.trail.len());
    }

    fn explain(&self, idx: usize) -> Vec<Lit> {
        let e = self.dom.trail[idx];
        match e.reason {
            Reason::Decision => Vec::new(),
            Reason::Clause(id) => self.clauses.explain(id, e.lit, &self.dom),
            Reason::Linear(c) => self.lin.explain(c, Some(e.lit.var()), idx, &self.dom),
            Reason::Stored(k) => self.reasons[k as usize].clone(),
        }
    }

    fn bump_var(&mut self, v: usize) {
        self.activity[v] += self.var_inc;
        if self.activity[v] > 1e100 {
            self.activity.iter_mut().for_each(|a| *a *= 1e-100);
            self.var_inc *= 1e-100;
        }
        self.heap.increased(v, &self.activity);
    }

    /// 1-UIP analysis of a conflict (true literals that can't all hold).
    /// Returns the learned clause (asserting literal first) and the backjump
    /// level, or None if the conflict holds at level 0.
    fn analyze(&mut self, conflict: Vec<Lit>) -> Option<(Vec<Lit>, usize)> {
        // The conflict set: for each (variable, side), the strongest literal,
        // with the trail entry that made it true and that entry's level.
        // Literals true at level 0 are left out.
        type Set = FixedMap<(u32, bool), (i64, usize, usize)>;
        let mut set: Set = FixedMap::default();
        let add = |set: &mut Set, dom: &Domains, l: Lit| {
            let Some(e) = dom.entry_for(l) else { return };
            let level = dom.trail[e].level as usize;
            if level == 0 {
                return;
            }
            let cur = set.entry((l.var, l.upper)).or_insert((l.val, e, level));
            if if l.upper { l.val < cur.0 } else { l.val > cur.0 } {
                *cur = (l.val, e, level);
            }
        };
        for l in conflict {
            add(&mut set, &self.dom, l);
        }
        loop {
            let max_level = set.values().map(|t| t.2).max()?;
            if max_level < self.dom.level() {
                // The conflict already holds at a lower level: analyze it there
                // (the entries up to that level stay where they are).
                self.backtrack(max_level);
                continue;
            }
            let (mut count, mut latest) = (0, None);
            for (&(var, upper), &(val, e, level)) in &set {
                if level == max_level {
                    count += 1;
                    if latest.is_none_or(|(_, le)| e > le) {
                        latest = Some((Lit { var, upper, val }, e));
                    }
                }
            }
            if count == 1 {
                let lits: Vec<(Lit, usize, usize)> =
                    set.iter().map(|(&(var, upper), &(val, e, level))| (Lit { var, upper, val }, e, level)).collect();
                let uip = latest.unwrap().0;
                let mut others: Vec<(Lit, usize)> = lits.iter().filter(|t| t.0 != uip).map(|t| (t.0, t.2)).collect();
                others.sort_by_key(|o| std::cmp::Reverse(o.1));
                let backjump = others.first().map_or(0, |o| o.1);
                let mut learned = vec![uip.negate()];
                learned.extend(others.iter().map(|o| o.0.negate()));
                for l in &learned {
                    self.bump_var(l.var());
                }
                return Some((learned, backjump));
            }
            // Resolve the latest literal of the conflict level with its explanation.
            let (lit, idx) = latest.unwrap();
            set.remove(&(lit.var, lit.upper));
            if let Reason::Clause(id) = self.dom.trail[idx].reason {
                let c = &mut self.clauses.clauses[id as usize];
                c.activity += self.clause_inc;
            }
            for l in self.explain(idx) {
                add(&mut set, &self.dom, l);
            }
            self.bump_var(lit.var());
        }
    }

    /// Learns a clause from analysis: backjump and assert its first literal.
    fn learn(&mut self, learned: Vec<Lit>, backjump: usize) -> Result<(), Vec<Lit>> {
        self.backtrack(backjump);
        let first = learned[0];
        // If the asserting literal can't be set, the whole clause is violated.
        let violated: Vec<Lit> = learned.iter().map(|l| l.negate()).collect();
        if learned.len() == 1 {
            if self.share {
                self.outbox.push((learned, 1));
            }
            return self.dom.set(first, Reason::Decision).map(|_| ()).map_err(|_| violated);
        }
        let levels: std::collections::HashSet<usize> =
            learned[1..].iter().map(|l| self.dom.lit_level(l.negate())).collect();
        let lbd = levels.len() as u32 + 1;
        if self.share && learned.len() <= SHARE_MAX_LEN && (lbd <= SHARE_LBD || learned.len() <= SHARE_LEN) {
            self.outbox.push((learned.clone(), lbd));
        }
        let id = self.clauses.add(learned, true, lbd);
        self.stats.learned += 1;
        self.learned_live += 1;
        self.dom.set(first, Reason::Clause(id)).map(|_| ()).map_err(|_| violated)
    }

    /// Handles a conflict: analysis, learning, restarts and clause deletion.
    /// Returns false if no solution remains.
    fn resolve(&mut self, mut conflict: Vec<Lit>) -> bool {
        loop {
            self.stats.conflicts += 1;
            self.clear_queue();
            let Some((learned, backjump)) = self.analyze(conflict) else {
                self.unsat = true;
                return false;
            };
            match self.learn(learned, backjump) {
                Ok(()) => break,
                Err(c) => conflict = c,
            }
        }
        self.var_inc /= self.var_decay;
        self.clause_inc /= CLAUSE_DECAY;
        if self.restarts
            && self.stats.conflicts - self.conflicts_at_restart >= self.restart_unit * luby(self.luby_index)
        {
            self.luby_index += 1;
            self.conflicts_at_restart = self.stats.conflicts;
            self.stats.restarts += 1;
            self.backtrack(0);
            if !self.take_in() {
                self.unsat = true;
                return false;
            }
        }
        if self.learned_live > self.max_learned {
            self.reduce();
        }
        true
    }

    /// Deletes the less useful half of the learned clauses (high LBD, low
    /// activity), except those with LBD <= KEEP_LBD and those that are the
    /// reason for a current trail entry.
    fn reduce(&mut self) {
        let locked: std::collections::HashSet<u32> = self
            .dom
            .trail
            .iter()
            .filter_map(|e| if let Reason::Clause(id) = e.reason { Some(id) } else { None })
            .collect();
        let mut candidates: Vec<u32> = (0..self.clauses.clauses.len() as u32)
            .filter(|&id| {
                let c = &self.clauses.clauses[id as usize];
                c.learned && !c.deleted && c.lbd > KEEP_LBD && !locked.contains(&id)
            })
            .collect();
        candidates.sort_by(|&a, &b| {
            let (ca, cb) = (&self.clauses.clauses[a as usize], &self.clauses.clauses[b as usize]);
            cb.lbd.cmp(&ca.lbd).then(ca.activity.total_cmp(&cb.activity))
        });
        for &id in &candidates[..candidates.len() / 2] {
            self.clauses.clauses[id as usize].deleted = true;
            self.learned_live -= 1;
            self.stats.deleted += 1;
        }
        self.clauses.purge_deleted();
        self.max_learned += self.max_learned / 10;
    }

    /// Scheduling branching: the present task with an open start that can
    /// start earliest (then end earliest) starts as early as it can.
    fn decide_task(&self) -> Option<Lit> {
        let mut best: Option<(i64, i64, Lit)> = None;
        for &(start, end, presence) in &self.branch_tasks {
            let var = start.var? as usize;
            if self.dom.is_fixed(var) || presence.is_some_and(|p| self.dom.lb(p as usize) < 1) {
                continue;
            }
            let (est, ect) = (start.lb(&self.dom), end.lb(&self.dom));
            if best.is_none_or(|b| (est, ect) < (b.0, b.1)) {
                best = Some((est, ect, start.le(est).unwrap()));
            }
        }
        best.map(|b| b.2)
    }

    fn decide(&mut self) -> Option<Lit> {
        // Scheduling branching finds a good first schedule; after that, VSIDS
        // (with phase saving, which starts from the last solution) improves
        // and proves it, which the fixed order does poorly.
        if self.task_branching
            && self.stats.solutions == 0
            && let Some(lit) = self.decide_task()
        {
            return Some(lit);
        }
        while let Some(v) = self.heap.pop(&self.activity) {
            if self.dom.is_fixed(v) {
                continue;
            }
            let (lb, ub) = (self.dom.lb(v), self.dom.ub(v));
            let p = self.phase[v].clamp(lb, ub);
            let lit = if p == lb {
                Lit::le(v, lb)
            } else if p == ub {
                Lit::ge(v, ub)
            } else {
                Lit::le(v, p)
            };
            self.heap.insert(v, &self.activity); // still open until fixed
            return Some(lit);
        }
        None
    }

    /// Searches until a solution, exhaustion, the deadline, or (if given)
    /// that many more conflicts.
    pub fn search(&mut self, deadline: Option<Instant>, conflict_limit: Option<u64>) -> Search {
        if self.unsat {
            return Search::Exhausted;
        }
        let stop = conflict_limit.map(|l| self.stats.conflicts + l);
        let mut steps = 0u32;
        loop {
            if let Err(conflict) = self.propagate() {
                if !self.resolve(conflict) {
                    return Search::Exhausted;
                }
                if stop.is_some_and(|s| self.stats.conflicts >= s) {
                    return Search::ConflictLimit;
                }
                continue;
            }
            steps = steps.wrapping_add(1);
            if steps % 64 == 0 && crate::config::expired(deadline) {
                return Search::TimeLimit;
            }
            let Some(lit) = self.decide() else {
                self.stats.solutions += 1;
                return Search::Solution;
            };
            self.stats.decisions += 1;
            self.dom.push_level();
            self.reason_marks.push(self.reasons.len());
            self.dom.set(lit, Reason::Decision).expect("a decision on an open variable");
        }
    }

    /// The current values (call after `Search::Solution`).
    pub fn values(&self) -> Vec<i64> {
        (0..self.dom.num_vars()).map(|v| self.dom.lb(v)).collect()
    }

    /// Forbids the current solution (all variables fixed) and resolves the
    /// resulting conflict, so the next `search` looks for a different one.
    ///
    /// The blocking clause negates the decisions only: propagation from them
    /// fixed every variable, so this solution is the only one that satisfies
    /// all of them. That is much shorter than negating every variable's value.
    pub fn block_solution(&mut self) {
        let mut lits: Vec<Lit> = self
            .dom
            .trail
            .iter()
            .filter(|e| e.reason == Reason::Decision && e.level > 0)
            .map(|e| e.lit.negate())
            .collect();
        if lits.is_empty() {
            self.unsat = true; // found without decisions: the only solution
            return;
        }
        // Watch the literals falsified last (the latest decisions), so the
        // clause stays correctly watched after backjumping.
        lits.reverse();
        let conflict: Vec<Lit> = lits.iter().map(|l| l.negate()).collect();
        if lits.len() == 1 {
            self.backtrack(0);
            if self.dom.set(lits[0], Reason::Decision).is_err() {
                self.unsat = true;
            }
            return;
        }
        self.clauses.add(lits, false, 0);
        if !self.resolve(conflict) {
            self.unsat = true;
        }
    }

    /// Sets the preferred values (e.g. to a solution found elsewhere).
    pub fn set_phase(&mut self, x: &[i64]) {
        self.phase.copy_from_slice(x);
    }

    /// Gives the variables small random activities (and, with `high`, their
    /// upper bounds as preferred values), so that this search branches in an
    /// order of its own. Before searching.
    pub fn diversify(&mut self, seed: u64, high: bool) {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        for v in 0..self.activity.len() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            self.activity[v] = (state >> 11) as f64 / (1u64 << 53) as f64 * 1e-3;
            if high {
                self.phase[v] = self.dom.ub(v);
            }
        }
        self.heap = Heap::new(self.activity.len());
        for i in (0..self.heap.items.len() / 2).rev() {
            self.heap.down(i, &self.activity);
        }
    }

    /// The smallest value of `sum(a x)` over the bounds that hold at decision
    /// level 0: implied by the model, the learned clauses, and (when
    /// optimizing) the objective's current rhs, so every solution still
    /// sought has at least this value.
    pub fn root_min(&self, terms: &[(u32, i64)]) -> i128 {
        let level0 = (self.dom.level() > 0).then(|| self.dom.level_start(0));
        let bound = |var: usize, upper: bool| match level0 {
            Some(pos) => self.dom.bound_at(var, upper, pos),
            None if upper => self.dom.ub(var),
            None => self.dom.lb(var),
        };
        terms.iter().map(|&(j, a)| a as i128 * bound(j as usize, a < 0) as i128).sum()
    }

    /// Restarts from level 0 (after tightening a constraint's rhs).
    pub fn restart(&mut self) {
        self.clear_queue();
        self.backtrack(0);
        if !self.take_in() {
            self.unsat = true;
        }
    }

    /// At level 0: adds the clauses in `inbox`, as far as they still say
    /// something here. False if one of them is violated: no solution remains.
    fn take_in(&mut self) -> bool {
        for (clause, lbd) in std::mem::take(&mut self.inbox) {
            if clause.iter().any(|&l| self.dom.is_true(l)) {
                continue;
            }
            let open: Vec<Lit> = clause.iter().copied().filter(|&l| !self.dom.is_false(l)).collect();
            match open.len() {
                0 => return false,
                1 => {
                    if self.dom.set(open[0], Reason::Decision).is_err() {
                        return false;
                    }
                }
                _ => {
                    self.clauses.add(open, true, lbd);
                    self.learned_live += 1;
                }
            }
        }
        true
    }

    /// Propagates at level 0 (e.g. after a restart with a tightened rhs);
    /// records unsatisfiability.
    pub fn propagate_root(&mut self) {
        if self.propagate().is_err() {
            self.unsat = true;
        }
    }
}

/// The Luby sequence 1, 1, 2, 1, 1, 2, 4, 1, 1, 2, ...
fn luby(i: u64) -> u64 {
    let mut i = i + 1;
    loop {
        let k = 64 - i.leading_zeros() as u64; // bits
        if i == (1 << k) - 1 {
            return 1 << (k - 1);
        }
        i -= (1 << (k - 1)) - 1;
    }
}

/// A max-heap of variables by activity, with positions for updates.
struct Heap {
    items: Vec<usize>,
    pos: Vec<usize>,
}

const ABSENT: usize = usize::MAX;

impl Heap {
    fn new(n: usize) -> Self {
        Heap { items: (0..n).collect(), pos: (0..n).collect() }
    }

    fn insert(&mut self, v: usize, act: &[f64]) {
        if self.pos[v] != ABSENT {
            return;
        }
        self.pos[v] = self.items.len();
        self.items.push(v);
        self.up(self.items.len() - 1, act);
    }

    fn increased(&mut self, v: usize, act: &[f64]) {
        if self.pos[v] != ABSENT {
            self.up(self.pos[v], act);
        }
    }

    fn pop(&mut self, act: &[f64]) -> Option<usize> {
        let top = *self.items.first()?;
        let last = self.items.pop().unwrap();
        self.pos[top] = ABSENT;
        if !self.items.is_empty() {
            self.items[0] = last;
            self.pos[last] = 0;
            self.down(0, act);
        }
        Some(top)
    }

    fn up(&mut self, mut i: usize, act: &[f64]) {
        while i > 0 {
            let p = (i - 1) / 2;
            if act[self.items[i]] <= act[self.items[p]] {
                break;
            }
            self.swap(i, p);
            i = p;
        }
    }

    fn down(&mut self, mut i: usize, act: &[f64]) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut best = i;
            if l < self.items.len() && act[self.items[l]] > act[self.items[best]] {
                best = l;
            }
            if r < self.items.len() && act[self.items[r]] > act[self.items[best]] {
                best = r;
            }
            if best == i {
                break;
            }
            self.swap(i, best);
            i = best;
        }
    }

    fn swap(&mut self, a: usize, b: usize) {
        self.items.swap(a, b);
        self.pos[self.items[a]] = a;
        self.pos[self.items[b]] = b;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luby_sequence() {
        let seq: Vec<u64> = (0..15).map(luby).collect();
        assert_eq!(seq, vec![1, 1, 2, 1, 1, 2, 4, 1, 1, 2, 1, 1, 2, 4, 8]);
    }
}
