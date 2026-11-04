//! The integer trail: each variable's current bounds, and every bound change
//! in order, with its reason and decision level (as CP-SAT's `IntegerTrail`).
//!
//! A literal is a bound, `[x >= v]` or `[x <= v]`. Each trail entry makes one
//! literal true and links to the previous entry for the same variable and
//! side, so the bound as of any earlier trail position can be read back
//! (lazy explanations need that), and backtracking restores bounds by
//! following the links.
//!
//! Variables may have a domain with gaps (sorted inclusive ranges); bounds
//! are snapped past the gaps when set.

/// A bound literal: `[var >= val]`, or with `upper`, `[var <= val]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lit {
    pub var: u32,
    pub upper: bool,
    pub val: i64,
}

impl Lit {
    pub fn ge(var: usize, val: i64) -> Lit {
        Lit { var: var as u32, upper: false, val }
    }

    pub fn le(var: usize, val: i64) -> Lit {
        Lit { var: var as u32, upper: true, val }
    }

    /// not [x >= v] is [x <= v - 1], and not [x <= v] is [x >= v + 1].
    pub fn negate(self) -> Lit {
        if self.upper { Lit::ge(self.var as usize, self.val + 1) } else { Lit::le(self.var as usize, self.val - 1) }
    }

    pub fn var(self) -> usize {
        self.var as usize
    }

    /// Whether this literal implies `other` (same variable and side, at least as strong).
    pub fn implies(self, other: Lit) -> bool {
        self.var == other.var
            && self.upper == other.upper
            && if self.upper { self.val <= other.val } else { self.val >= other.val }
    }
}

/// Why a literal became true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Decision,
    Clause(u32),
    Linear(u32),
}

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub lit: Lit,
    /// The previous entry for the same variable and side, or NONE.
    prev: u32,
    pub reason: Reason,
    pub level: u32,
}

pub struct Domains {
    lb: Vec<i64>,
    ub: Vec<i64>,
    initial_lb: Vec<i64>,
    initial_ub: Vec<i64>,
    /// The latest entry for [lower, upper] of each variable.
    last: Vec<[u32; 2]>,
    ranges: Vec<Option<Vec<(i64, i64)>>>,
    pub trail: Vec<Entry>,
    /// Trail length when each decision level after 0 began.
    level_start: Vec<usize>,
}

impl Domains {
    pub fn new(lb: Vec<i64>, ub: Vec<i64>, ranges: Vec<Option<Vec<(i64, i64)>>>) -> Self {
        let n = lb.len();
        Domains {
            initial_lb: lb.clone(),
            initial_ub: ub.clone(),
            lb,
            ub,
            last: vec![[NONE; 2]; n],
            ranges,
            trail: Vec::new(),
            level_start: Vec::new(),
        }
    }

    pub fn num_vars(&self) -> usize {
        self.lb.len()
    }

    pub fn lb(&self, var: usize) -> i64 {
        self.lb[var]
    }

    pub fn ub(&self, var: usize) -> i64 {
        self.ub[var]
    }

    pub fn is_fixed(&self, var: usize) -> bool {
        self.lb[var] == self.ub[var]
    }

    pub fn level(&self) -> usize {
        self.level_start.len()
    }

    pub fn is_true(&self, lit: Lit) -> bool {
        if lit.upper { self.ub[lit.var()] <= lit.val } else { self.lb[lit.var()] >= lit.val }
    }

    pub fn is_false(&self, lit: Lit) -> bool {
        if lit.upper { self.lb[lit.var()] > lit.val } else { self.ub[lit.var()] < lit.val }
    }

    /// Whether the literal holds for every value in the initial domain.
    pub fn is_root_true(&self, lit: Lit) -> bool {
        if lit.upper { self.initial_ub[lit.var()] <= lit.val } else { self.initial_lb[lit.var()] >= lit.val }
    }

    /// The trail length when decision level `level + 1` began.
    pub fn level_start(&self, level: usize) -> usize {
        self.level_start[level]
    }

    pub fn push_level(&mut self) {
        self.level_start.push(self.trail.len());
    }

    /// Snaps a literal's value to the domain: the smallest allowed value at
    /// least v for [x >= v], the largest at most v for [x <= v]. None if there
    /// is no such value.
    fn snap(&self, lit: Lit) -> Option<Lit> {
        let Some(ranges) = &self.ranges[lit.var()] else { return Some(lit) };
        let v = lit.val;
        let snapped = if lit.upper {
            ranges.iter().rev().find(|&&(a, _)| a <= v).map(|&(_, b)| b.min(v))
        } else {
            ranges.iter().find(|&&(_, b)| b >= v).map(|&(a, _)| a.max(v))
        };
        snapped.map(|val| Lit { val, ..lit })
    }

    /// Makes `lit` true. Ok(true) if a bound changed, Ok(false) if it already
    /// held, Err if it contradicts the current bounds (nothing changes then).
    pub fn set(&mut self, lit: Lit, reason: Reason) -> Result<bool, ()> {
        let lit = self.snap(lit).ok_or(())?;
        if self.is_true(lit) {
            return Ok(false);
        }
        if self.is_false(lit) {
            return Err(());
        }
        let (var, side) = (lit.var(), lit.upper as usize);
        let idx = self.trail.len() as u32;
        self.trail.push(Entry { lit, prev: self.last[var][side], reason, level: self.level() as u32 });
        self.last[var][side] = idx;
        if lit.upper {
            self.ub[var] = lit.val;
        } else {
            self.lb[var] = lit.val;
        }
        Ok(true)
    }

    /// Undoes every entry made after decision level `level`.
    pub fn backtrack_to(&mut self, level: usize) {
        if level >= self.level() {
            return;
        }
        let keep = self.level_start[level];
        while self.trail.len() > keep {
            let e = self.trail.pop().unwrap();
            let (var, side) = (e.lit.var(), e.lit.upper as usize);
            self.last[var][side] = e.prev;
            let restored = if e.prev == NONE {
                if e.lit.upper { self.initial_ub[var] } else { self.initial_lb[var] }
            } else {
                self.trail[e.prev as usize].lit.val
            };
            if e.lit.upper {
                self.ub[var] = restored;
            } else {
                self.lb[var] = restored;
            }
        }
        self.level_start.truncate(level);
    }

    /// The earliest trail entry that implies the (true) literal, or None if the
    /// initial domain implies it.
    pub fn entry_for(&self, lit: Lit) -> Option<usize> {
        if self.is_root_true(lit) {
            return None;
        }
        let mut idx = self.last[lit.var()][lit.upper as usize];
        let mut found = None;
        while idx != NONE && self.trail[idx as usize].lit.implies(lit) {
            found = Some(idx as usize);
            idx = self.trail[idx as usize].prev;
        }
        found
    }

    /// The decision level at which a true literal became true (0 if the
    /// initial domain implies it).
    pub fn lit_level(&self, lit: Lit) -> usize {
        self.entry_for(lit).map_or(0, |e| self.trail[e].level as usize)
    }

    /// The variable's lower (or, with `upper`, upper) bound just before trail
    /// position `pos`.
    pub fn bound_at(&self, var: usize, upper: bool, pos: usize) -> i64 {
        let mut idx = self.last[var][upper as usize];
        while idx != NONE && idx as usize >= pos {
            idx = self.trail[idx as usize].prev;
        }
        if idx != NONE {
            self.trail[idx as usize].lit.val
        } else if upper {
            self.initial_ub[var]
        } else {
            self.initial_lb[var]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_backtrack_and_history() {
        let mut d = Domains::new(vec![0, 0], vec![10, 10], vec![None, Some(vec![(0, 2), (6, 10)])]);
        d.push_level();
        assert_eq!(d.set(Lit::ge(0, 3), Reason::Decision), Ok(true));
        assert_eq!(d.set(Lit::ge(0, 2), Reason::Decision), Ok(false));
        d.push_level();
        assert_eq!(d.set(Lit::ge(0, 5), Reason::Linear(0)), Ok(true));
        // Snapping past the gap 3..5.
        assert_eq!(d.set(Lit::ge(1, 3), Reason::Linear(0)), Ok(true));
        assert_eq!(d.lb(1), 6);
        assert_eq!(d.set(Lit::le(0, 4), Reason::Linear(0)), Err(()));
        assert_eq!(d.lit_level(Lit::ge(0, 4)), 2);
        assert_eq!(d.lit_level(Lit::ge(0, 3)), 1);
        assert_eq!(d.entry_for(Lit::ge(0, 0)), None);
        assert_eq!(d.bound_at(0, false, 1), 3);
        assert_eq!(d.bound_at(0, false, 0), 0);
        d.backtrack_to(1);
        assert_eq!((d.lb(0), d.lb(1), d.level()), (3, 0, 1));
        d.backtrack_to(0);
        assert_eq!((d.lb(0), d.trail.len()), (0, 0));
    }

    #[test]
    fn negation_and_implication() {
        assert_eq!(Lit::ge(3, 5).negate(), Lit::le(3, 4));
        assert_eq!(Lit::le(3, 4).negate(), Lit::ge(3, 5));
        assert!(Lit::ge(1, 5).implies(Lit::ge(1, 3)));
        assert!(!Lit::ge(1, 3).implies(Lit::ge(1, 5)));
        assert!(Lit::le(1, 2).implies(Lit::le(1, 4)));
    }
}
