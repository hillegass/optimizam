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
    /// Where the last search for a replacement watch stopped (in 2..len):
    /// the next one starts there and wraps around, so long clauses with many
    /// false literals aren't rescanned from the start every time (Gent 2013).
    search: u32,
}

pub struct ClauseStore {
    pub clauses: Vec<Clause>,
    /// For each variable: the clauses watching a literal that becomes false
    /// when its lower bound rises ([0], literals `[x <= v]`) or its upper bound
    /// falls ([1], literals `[x >= v]`), with that literal's value v, so a
    /// watch whose literal still holds is skipped without reading the clause,
    /// and a blocker: another literal of the clause, which if true satisfies
    /// it, so the clause isn't read either (MiniSat). In enumeration most
    /// falsified watches are of satisfied clauses (queens_12_all: 88%).
    watch: Vec<[Vec<Watch>; 2]>,
}

/// A watch: the clause, the watched literal's value, and the blocker literal
/// (its variable, with its side in the top bit, and its value).
#[derive(Clone, Copy)]
struct Watch {
    id: u32,
    blocker_var: u32,
    val: i64,
    blocker_val: i64,
}

impl Watch {
    fn new(id: u32, val: i64, blocker: Lit) -> Watch {
        Watch { id, blocker_var: blocker.var | (blocker.upper as u32) << 31, val, blocker_val: blocker.val }
    }

    fn blocker(&self) -> Lit {
        Lit { var: self.blocker_var & !(1 << 31), upper: self.blocker_var >> 31 == 1, val: self.blocker_val }
    }
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
        for (k, &l) in lits[..2].iter().enumerate() {
            self.watch[l.var()][watch_side(l)].push(Watch::new(id, l.val, lits[1 - k]));
        }
        self.clauses.push(Clause { lits, learned, lbd, activity: 0.0, deleted: false, search: 2 });
        id
    }

    /// Visits the clauses watching a literal that the change to `var`'s lower
    /// (`upper == false`) or upper bound falsified. Err carries a conflict:
    /// literals, all true, whose conjunction the clause forbids.
    pub fn propagate(&mut self, var: usize, upper: bool, dom: &mut Domains) -> Result<(), Vec<Lit>> {
        let side = if upper { 1 } else { 0 };
        let bound = if upper { dom.ub(var) } else { dom.lb(var) };
        let mut list = std::mem::take(&mut self.watch[var][side]);
        let mut result = Ok(());
        // Compacted in place: entries [0, kept) stay.
        let mut kept = 0;
        for i in 0..list.len() {
            let w = list[i];
            let (id, val) = (w.id, w.val);
            // [x <= val] is false once lb > val; [x >= val] once ub < val.
            let falsified = if upper { val > bound } else { val < bound };
            if result.is_err() || !falsified || dom.is_true(w.blocker()) {
                list[kept] = w;
                kept += 1;
                continue;
            }
            let clause = &mut self.clauses[id as usize];
            if clause.deleted {
                continue; // drop the stale watch
            }
            // Make lits[1] the falsified watched literal.
            let watched = Lit { var: var as u32, upper: side == 0, val };
            if clause.lits[0] == watched {
                clause.lits.swap(0, 1);
            }
            debug_assert_eq!(clause.lits[1], watched);
            let first = clause.lits[0];
            if dom.is_true(first) {
                list[kept] = Watch::new(id, val, first);
                kept += 1;
                continue;
            }
            // Look for a replacement watch.
            let (n, from) = (clause.lits.len(), (clause.search as usize).clamp(2, clause.lits.len().max(2)));
            if let Some(k) = (from..n).chain(2..from).find(|&k| !dom.is_false(clause.lits[k])) {
                clause.search = k as u32;
                clause.lits.swap(1, k);
                let l = clause.lits[1];
                self.watch[l.var()][watch_side(l)].push(Watch::new(id, l.val, first));
                continue;
            }
            list[kept] = Watch::new(id, val, first);
            kept += 1;
            if dom.is_false(first) || dom.set(first, Reason::Clause(id)).is_err() {
                result = Err(clause.lits.iter().map(|l| l.negate()).collect());
            }
        }
        list.truncate(kept);
        // Watches added to this list during the loop (a replacement on the same
        // variable and side) are in self.watch[var][side].
        list.append(&mut self.watch[var][side]);
        self.watch[var][side] = list;
        result
    }

    /// Drops the watches of deleted clauses (a watch whose blocker is true is
    /// skipped without reading its clause, so it wouldn't be dropped otherwise).
    pub fn purge_deleted(&mut self) {
        let clauses = &self.clauses;
        for sides in &mut self.watch {
            for list in sides {
                list.retain(|w| !clauses[w.id as usize].deleted);
            }
        }
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
