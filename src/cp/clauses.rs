//! Clauses over bound literals (learned clauses and solution-blocking
//! clauses), propagated with two watched literals.
//!
//! A literal `[x >= v]` can only become false when x's upper bound falls,
//! and `[x <= v]` when its lower bound rises, so a watch on a literal is
//! filed under that variable and side. The watched literals are the first
//! two of each clause.

use super::domains::{Domains, Lit, Reason};

pub struct Clause {
    pub lits: Vec<Lit>,
    pub learned: bool,
    /// Literal block distance when learned (distinct decision levels).
    pub lbd: u32,
    pub activity: f64,
    pub deleted: bool,
}

pub struct ClauseStore {
    pub clauses: Vec<Clause>,
    /// For each variable: clauses watching a literal that becomes false when
    /// its lower bound rises ([0]) or its upper bound falls ([1]).
    watch: Vec<[Vec<u32>; 2]>,
}

/// The watch list a literal belongs to: the bound change that can falsify it.
fn watch_side(lit: Lit) -> usize {
    if lit.upper { 0 } else { 1 }
}

impl ClauseStore {
    pub fn new(num_vars: usize) -> Self {
        ClauseStore { clauses: Vec::new(), watch: vec![[Vec::new(), Vec::new()]; num_vars] }
    }

    /// Adds a clause of at least two literals; the first two are watched.
    pub fn add(&mut self, lits: Vec<Lit>, learned: bool, lbd: u32) -> u32 {
        debug_assert!(lits.len() >= 2);
        let id = self.clauses.len() as u32;
        for &l in &lits[..2] {
            self.watch[l.var()][watch_side(l)].push(id);
        }
        self.clauses.push(Clause { lits, learned, lbd, activity: 0.0, deleted: false });
        id
    }

    /// Visits the clauses watching a literal that the change to `var`'s lower
    /// (`upper == false`) or upper bound may have falsified. Err carries a
    /// conflict: literals, all true, whose conjunction the clause forbids.
    pub fn propagate(&mut self, var: usize, upper: bool, dom: &mut Domains) -> Result<(), Vec<Lit>> {
        let side = if upper { 1 } else { 0 };
        let list = std::mem::take(&mut self.watch[var][side]);
        let mut keep = Vec::with_capacity(list.len());
        let mut result = Ok(());
        let mut i = 0;
        while i < list.len() {
            let id = list[i];
            i += 1;
            if result.is_err() {
                keep.push(id);
                continue;
            }
            let clause = &mut self.clauses[id as usize];
            if clause.deleted {
                continue; // drop the stale watch
            }
            // Make lits[1] the watched literal on this variable and side.
            let on_this = |l: Lit| l.var() == var && watch_side(l) == side;
            if on_this(clause.lits[0]) && !on_this(clause.lits[1]) {
                clause.lits.swap(0, 1);
            }
            if !on_this(clause.lits[1]) || !dom.is_false(clause.lits[1]) {
                keep.push(id);
                continue;
            }
            if dom.is_true(clause.lits[0]) {
                keep.push(id);
                continue;
            }
            // Look for a replacement watch.
            if let Some(k) = (2..clause.lits.len()).find(|&k| !dom.is_false(clause.lits[k])) {
                clause.lits.swap(1, k);
                let l = clause.lits[1];
                self.watch[l.var()][watch_side(l)].push(id);
                continue;
            }
            keep.push(id);
            let first = clause.lits[0];
            if dom.is_false(first) || dom.set(first, Reason::Clause(id)).is_err() {
                result = Err(clause.lits.iter().map(|l| l.negate()).collect());
            }
        }
        // Watches added to this list during the loop (a replacement on the same
        // variable and side) are already in self.watch[var][side].
        keep.append(&mut self.watch[var][side]);
        self.watch[var][side] = keep;
        result
    }

    /// The explanation of a literal that clause `id` propagated: the
    /// negations of its other literals (all false at the time).
    pub fn explain(&self, id: u32, lit: Lit, dom: &Domains) -> Vec<Lit> {
        self.clauses[id as usize]
            .lits
            .iter()
            .filter(|l| !(l.var == lit.var && l.upper == lit.upper))
            .map(|l| l.negate())
            .filter(|l| !dom.is_root_true(*l))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [x >= 1] or [y >= 1] or [z <= 0]: after x <= 0 and z >= 1, y >= 1.
    #[test]
    fn unit_propagation() {
        let mut dom = Domains::new(vec![0; 3], vec![1; 3], vec![None; 3]);
        let mut store = ClauseStore::new(3);
        let id = store.add(vec![Lit::ge(0, 1), Lit::ge(1, 1), Lit::le(2, 0)], false, 0);
        dom.push_level();
        dom.set(Lit::le(0, 0), Reason::Decision).unwrap();
        store.propagate(0, true, &mut dom).unwrap();
        assert_eq!(dom.lb(1), 0, "two literals are still open");
        dom.set(Lit::ge(2, 1), Reason::Decision).unwrap();
        store.propagate(2, false, &mut dom).unwrap();
        assert_eq!(dom.lb(1), 1);
        let mut why = store.explain(id, Lit::ge(1, 1), &dom);
        why.sort_by_key(|l| l.var);
        assert_eq!(why, vec![Lit::le(0, 0), Lit::ge(2, 1)]);
        dom.backtrack_to(0);
        dom.push_level();
        dom.set(Lit::le(0, 0), Reason::Decision).unwrap();
        dom.set(Lit::le(1, 0), Reason::Decision).unwrap();
        dom.set(Lit::ge(2, 1), Reason::Decision).unwrap();
        let mut result = Ok(());
        for (v, up) in [(0, true), (1, true), (2, false)] {
            result = result.and(store.propagate(v, up, &mut dom));
        }
        assert!(result.is_err());
    }
}
