//! Linear constraints `sum(a_i x_i) <= rhs` over integer variables with
//! integer coefficients, propagated on bounds with lazy explanations.
//!
//! With `min = sum(min over the bounds of a_i x_i)` and `slack = rhs - min`,
//! the constraint is violated if `slack < 0`; otherwise each term can grow by
//! at most `slack`: `x_i <= lb_i + floor(slack / a_i)` for `a_i > 0`, and
//! `x_i >= ub_i - floor(slack / -a_i)` for `a_i < 0`. Such a bound is
//! explained by the other terms' minimizing bounds at the time it was set,
//! which the trail can read back, so explanations are only built when
//! conflict analysis asks for them. Activities use i128, so they can't
//! overflow.
//!
//! The minimum activity is kept up to date incrementally: the solver adds
//! each trail entry's change to the constraints that watch it as it processes
//! the entry ([`LinearStore::apply`]), and takes it back when it backtracks
//! ([`LinearStore::unapply`]). A constraint whose slack is at least the
//! largest range any of its terms can have (`max_range`, from the initial
//! domains) can't propagate, so it isn't even queued.

use super::domains::{Domains, Lit, Reason};

pub struct Linear {
    pub terms: Vec<(u32, i64)>,
    /// Mutable for the objective constraint, whose bound tightens.
    pub rhs: i64,
    /// The minimum activity over the current bounds of the processed trail.
    min: i128,
    /// The largest |a_i| (ub_i - lb_i) over the initial domains.
    max_range: i128,
}

impl Linear {
    pub fn slack(&self) -> i128 {
        self.rhs as i128 - self.min
    }

    /// Whether the constraint could propagate or be violated now.
    pub fn is_tight(&self) -> bool {
        self.slack() < self.max_range
    }
}

pub struct LinearStore {
    pub cons: Vec<Linear>,
    /// For each variable: the constraints (with the variable's coefficient)
    /// whose minimum activity changes when its lower bound rises ([0]) or its
    /// upper bound falls ([1]).
    watch: Vec<[Vec<(u32, i64)>; 2]>,
}

impl LinearStore {
    pub fn new(num_vars: usize) -> Self {
        LinearStore { cons: Vec::new(), watch: vec![[Vec::new(), Vec::new()]; num_vars] }
    }

    /// Adds a constraint; its activity is computed from `dom`, which must not
    /// have any trail entries yet.
    pub fn add(&mut self, terms: Vec<(u32, i64)>, rhs: i64, dom: &Domains) -> u32 {
        assert!(dom.trail.is_empty(), "constraints are added before the search");
        let id = self.cons.len() as u32;
        let (mut min, mut max_range) = (0i128, 0i128);
        for &(v, a) in &terms {
            self.watch[v as usize][(a < 0) as usize].push((id, a));
            let (lb, ub) = (dom.lb(v as usize) as i128, dom.ub(v as usize) as i128);
            min += a as i128 * if a > 0 { lb } else { ub };
            max_range = max_range.max(a.unsigned_abs() as i128 * (ub - lb));
        }
        self.cons.push(Linear { terms, rhs, min, max_range });
        id
    }

    /// Adds trail entry `idx`'s bound change to the activities of the
    /// constraints that watch it, and calls `tight` for each that could now
    /// propagate.
    pub fn apply(&mut self, idx: usize, dom: &Domains, mut tight: impl FnMut(u32)) {
        let lit = dom.trail[idx].lit;
        let change = (lit.val - dom.replaced_value(idx)) as i128;
        for &(c, a) in &self.watch[lit.var()][lit.upper as usize] {
            let con = &mut self.cons[c as usize];
            con.min += a as i128 * change;
            if con.is_tight() {
                tight(c);
            }
        }
    }

    /// Takes back [`LinearStore::apply`] of trail entry `idx` (before it is popped).
    pub fn unapply(&mut self, idx: usize, dom: &Domains) {
        let lit = dom.trail[idx].lit;
        let change = (lit.val - dom.replaced_value(idx)) as i128;
        for &(c, a) in &self.watch[lit.var()][lit.upper as usize] {
            self.cons[c as usize].min -= a as i128 * change;
        }
    }

    /// Propagates constraint `c` (whose activity covers the whole trail). Err
    /// carries a conflict: literals, all true now, that together violate the
    /// constraint.
    pub fn propagate(&self, c: u32, dom: &mut Domains) -> Result<(), Vec<Lit>> {
        let con = &self.cons[c as usize];
        debug_assert_eq!(
            con.min,
            con.terms
                .iter()
                .map(|&(v, a)| a as i128 * if a > 0 { dom.lb(v as usize) } else { dom.ub(v as usize) } as i128)
                .sum::<i128>(),
            "incremental activity of constraint {c}"
        );
        let slack = con.slack();
        if slack < 0 {
            return Err(self.explain(c, None, dom.trail.len(), dom));
        }
        if slack >= con.max_range {
            return Ok(());
        }
        for &(v, a) in &con.terms {
            let var = v as usize;
            let (lb, ub) = (dom.lb(var), dom.ub(var));
            if (a.unsigned_abs() as i128) * ((ub - lb) as i128) <= slack {
                continue;
            }
            // The slack is below this term's range, so slack / |a| < ub - lb
            // fits in i64; divide in i64 when the slack does too (i128
            // division is a slow library call).
            let step = match i64::try_from(slack) {
                Ok(s) => s / a.abs(),
                Err(_) => (slack / a.unsigned_abs() as i128) as i64,
            };
            let lit = if a > 0 { Lit::le(var, lb + step) } else { Lit::ge(var, ub - step) };
            if dom.set(lit, Reason::Linear(c)).is_err() {
                // The implied bound crosses the opposite one (possibly after
                // snapping past a domain gap).
                let mut conflict = self.explain(c, Some(var), dom.trail.len(), dom);
                conflict.push(if lit.upper { Lit::ge(var, lb) } else { Lit::le(var, ub) });
                return Err(conflict);
            }
        }
        Ok(())
    }

    /// The other terms' minimizing bounds just before trail position `pos`:
    /// an explanation for a bound that constraint `c` implied on `except` at
    /// `pos` (or, with no exception, for a violation at `pos`). Literals that
    /// the initial domains imply are left out.
    pub fn explain(&self, c: u32, except: Option<usize>, pos: usize, dom: &Domains) -> Vec<Lit> {
        let mut lits = Vec::new();
        for &(v, a) in &self.cons[c as usize].terms {
            let var = v as usize;
            if Some(var) == except {
                continue;
            }
            let lit = if a > 0 {
                Lit::ge(var, dom.bound_at(var, false, pos))
            } else {
                Lit::le(var, dom.bound_at(var, true, pos))
            };
            if !dom.is_root_true(lit) {
                lits.push(lit);
            }
        }
        lits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Processes the trail entries from `from` on, as the solver does.
    fn apply_all(store: &mut LinearStore, dom: &Domains, from: usize) {
        for idx in from..dom.trail.len() {
            store.apply(idx, dom, |_| {});
        }
    }

    /// x + 2y <= 6 with x, y in [0, 5]: y <= 3; after x >= 2, y <= 2 explained by [x >= 2].
    #[test]
    fn propagates_and_explains() {
        let mut dom = Domains::new(vec![0, 0], vec![5, 5], vec![None, None]);
        let mut store = LinearStore::new(2);
        let c = store.add(vec![(0, 1), (1, 2)], 6, &dom);
        store.propagate(c, &mut dom).unwrap();
        assert_eq!(dom.ub(1), 3);
        apply_all(&mut store, &dom, 0);
        dom.push_level();
        let start = dom.trail.len();
        dom.set(Lit::ge(0, 2), Reason::Decision).unwrap();
        apply_all(&mut store, &dom, start);
        let start = dom.trail.len();
        store.propagate(c, &mut dom).unwrap();
        apply_all(&mut store, &dom, start);
        assert_eq!(dom.ub(1), 2);
        let pos = dom.entry_for(Lit::le(1, 2)).unwrap();
        assert_eq!(store.explain(c, Some(1), pos, &dom), vec![Lit::ge(0, 2)]);
        let start = dom.trail.len();
        dom.set(Lit::ge(1, 2), Reason::Decision).unwrap();
        dom.set(Lit::ge(0, 3), Reason::Decision).unwrap();
        apply_all(&mut store, &dom, start);
        let conflict = store.propagate(c, &mut dom).unwrap_err();
        assert!(conflict.contains(&Lit::ge(0, 3)) && conflict.contains(&Lit::ge(1, 2)), "{conflict:?}");
        // Taking the level back restores the activity.
        for idx in (dom.level_start(0)..dom.trail.len()).rev() {
            store.unapply(idx, &dom);
        }
        dom.backtrack_to(0);
        assert_eq!(store.cons[0].slack(), 6);
    }
}
