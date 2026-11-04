//! CDCL search over the integer trail (lazy clause generation).
//!
//! Propagation runs clauses and linear constraints to a fixed point. A
//! conflict is a set of true literals that can't all hold. Conflict analysis
//! (1-UIP) repeatedly replaces the latest literal of the conflict level by its
//! explanation until one literal of that level remains, learns the clause
//! "not all of these", backjumps to the second-highest level in it, and
//! asserts the negated UIP. Branching picks the most active open variable
//! (VSIDS) and tries its saved value, the smallest at first. Restarts follow
//! the Luby sequence, and learned clauses with high LBD are deleted when
//! there are too many.

use super::clauses::ClauseStore;
use super::domains::{Domains, Lit, Reason};
use super::linear::LinearStore;
use std::collections::HashMap;
use std::time::Instant;

const RESTART_UNIT: u64 = 100;
const VAR_DECAY: f64 = 0.95;
const CLAUSE_DECAY: f64 = 0.999;
const FIRST_REDUCE: usize = 2000;
/// Learned clauses with at most this LBD are never deleted.
const KEEP_LBD: u32 = 2;

pub enum Search {
    /// Every variable is fixed and no constraint is violated.
    Solution,
    /// No solution remains.
    Exhausted,
    TimeLimit,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub decisions: u64,
    pub conflicts: u64,
    pub restarts: u64,
    pub learned: u64,
    pub deleted: u64,
}

pub struct Solver {
    pub dom: Domains,
    pub lin: LinearStore,
    pub clauses: ClauseStore,
    qhead: usize,
    lin_queue: Vec<u32>,
    lin_queued: Vec<bool>,
    activity: Vec<f64>,
    var_inc: f64,
    clause_inc: f64,
    heap: Heap,
    /// Preferred value of each variable (phase saving).
    phase: Vec<i64>,
    conflicts_at_restart: u64,
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
            activity: vec![0.0; n],
            var_inc: 1.0,
            clause_inc: 1.0,
            heap: Heap::new(n),
            phase,
            conflicts_at_restart: 0,
            luby_index: 0,
            max_learned: FIRST_REDUCE,
            learned_live: 0,
            unsat: false,
            stats: Stats::default(),
        }
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
                let lit = self.dom.trail[self.qhead].lit;
                self.qhead += 1;
                let (var, upper) = (lit.var(), lit.upper);
                self.clauses.propagate(var, upper, &mut self.dom)?;
                for &c in self.lin.watchers(var, upper) {
                    if !self.lin_queued[c as usize] {
                        self.lin_queued[c as usize] = true;
                        self.lin_queue.push(c);
                    }
                }
            }
            let Some(c) = self.lin_queue.pop() else { return Ok(()) };
            self.lin_queued[c as usize] = false;
            self.lin.propagate(c, &mut self.dom)?;
        }
    }

    fn clear_queue(&mut self) {
        for c in self.lin_queue.drain(..) {
            self.lin_queued[c as usize] = false;
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
        self.dom.backtrack_to(level);
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
        // The conflict set: for each (variable, side), the strongest literal.
        let mut set: HashMap<(u32, bool), i64> = HashMap::new();
        let add = |set: &mut HashMap<(u32, bool), i64>, dom: &Domains, l: Lit| {
            if dom.lit_level(l) == 0 {
                return;
            }
            let e = set.entry((l.var, l.upper)).or_insert(l.val);
            *e = if l.upper { (*e).min(l.val) } else { (*e).max(l.val) };
        };
        for l in conflict {
            add(&mut set, &self.dom, l);
        }
        loop {
            let lits: Vec<(Lit, usize, usize)> = set
                .iter()
                .map(|(&(var, upper), &val)| {
                    let l = Lit { var, upper, val };
                    let e = self.dom.entry_for(l).expect("conflict literals are true");
                    (l, e, self.dom.trail[e].level as usize)
                })
                .collect();
            let max_level = lits.iter().map(|t| t.2).max()?;
            if max_level == 0 {
                return None;
            }
            if max_level < self.dom.level() {
                // The conflict already holds at a lower level: analyze it there.
                self.backtrack(max_level);
                continue;
            }
            let at_level: Vec<&(Lit, usize, usize)> = lits.iter().filter(|t| t.2 == max_level).collect();
            if at_level.len() == 1 {
                let uip = at_level[0].0;
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
            let &&(lit, idx, _) = at_level.iter().max_by_key(|t| t.1).unwrap();
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
            return self.dom.set(first, Reason::Decision).map(|_| ()).map_err(|_| violated);
        }
        let levels: std::collections::HashSet<usize> =
            learned[1..].iter().map(|l| self.dom.lit_level(l.negate())).collect();
        let id = self.clauses.add(learned, true, levels.len() as u32 + 1);
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
        self.var_inc /= VAR_DECAY;
        self.clause_inc /= CLAUSE_DECAY;
        if self.stats.conflicts - self.conflicts_at_restart >= RESTART_UNIT * luby(self.luby_index) {
            self.luby_index += 1;
            self.conflicts_at_restart = self.stats.conflicts;
            self.stats.restarts += 1;
            self.backtrack(0);
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
        self.max_learned += self.max_learned / 10;
    }

    fn decide(&mut self) -> Option<Lit> {
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

    /// Searches until a solution, exhaustion, or the deadline.
    pub fn search(&mut self, deadline: Option<Instant>) -> Search {
        if self.unsat {
            return Search::Exhausted;
        }
        let mut steps = 0u32;
        loop {
            if let Err(conflict) = self.propagate() {
                if !self.resolve(conflict) {
                    return Search::Exhausted;
                }
                continue;
            }
            steps = steps.wrapping_add(1);
            if steps % 64 == 0 && deadline.is_some_and(|d| Instant::now() >= d) {
                return Search::TimeLimit;
            }
            let Some(lit) = self.decide() else { return Search::Solution };
            self.stats.decisions += 1;
            self.dom.push_level();
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

    /// Restarts from level 0 (after tightening a constraint's rhs).
    pub fn restart(&mut self) {
        self.clear_queue();
        self.backtrack(0);
        self.qhead = self.dom.trail.len();
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
