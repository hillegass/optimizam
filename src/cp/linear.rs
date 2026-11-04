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

use super::domains::{Domains, Lit, Reason};

pub struct Linear {
    pub terms: Vec<(u32, i64)>,
    /// Mutable for the objective constraint, whose bound tightens.
    pub rhs: i64,
}

pub struct LinearStore {
    pub cons: Vec<Linear>,
    /// For each variable: the constraints whose minimum activity changes when
    /// its lower bound rises ([0]) or its upper bound falls ([1]).
    watch: Vec<[Vec<u32>; 2]>,
}

impl LinearStore {
    pub fn new(num_vars: usize) -> Self {
        LinearStore { cons: Vec::new(), watch: vec![[Vec::new(), Vec::new()]; num_vars] }
    }

    pub fn add(&mut self, terms: Vec<(u32, i64)>, rhs: i64) -> u32 {
        let id = self.cons.len() as u32;
        for &(v, a) in &terms {
            self.watch[v as usize][(a < 0) as usize].push(id);
        }
        self.cons.push(Linear { terms, rhs });
        id
    }

    /// The constraints to revisit after a change to `var`'s lower bound
    /// (`upper == false`) or upper bound.
    pub fn watchers(&self, var: usize, upper: bool) -> &[u32] {
        &self.watch[var][upper as usize]
    }

    /// Propagates constraint `c`. Err carries a conflict: literals, all true
    /// now, that together violate the constraint.
    pub fn propagate(&self, c: u32, dom: &mut Domains) -> Result<(), Vec<Lit>> {
        let con = &self.cons[c as usize];
        let mut min: i128 = 0;
        for &(v, a) in &con.terms {
            let b = if a > 0 { dom.lb(v as usize) } else { dom.ub(v as usize) };
            min += a as i128 * b as i128;
        }
        let slack = con.rhs as i128 - min;
        if slack < 0 {
            return Err(self.explain(c, None, dom.trail.len(), dom));
        }
        for &(v, a) in &con.terms {
            let var = v as usize;
            let (lb, ub) = (dom.lb(var), dom.ub(var));
            if (a.unsigned_abs() as i128) * ((ub - lb) as i128) <= slack {
                continue;
            }
            let lit = if a > 0 {
                Lit::le(var, (lb as i128 + slack / a as i128) as i64)
            } else {
                Lit::ge(var, (ub as i128 - slack / (-(a as i128))) as i64)
            };
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

    /// x + 2y <= 6 with x, y in [0, 5]: y <= 3; after x >= 2, y <= 2 explained by [x >= 2].
    #[test]
    fn propagates_and_explains() {
        let mut dom = Domains::new(vec![0, 0], vec![5, 5], vec![None, None]);
        let mut store = LinearStore::new(2);
        let c = store.add(vec![(0, 1), (1, 2)], 6);
        store.propagate(c, &mut dom).unwrap();
        assert_eq!(dom.ub(1), 3);
        dom.push_level();
        dom.set(Lit::ge(0, 2), Reason::Decision).unwrap();
        store.propagate(c, &mut dom).unwrap();
        assert_eq!(dom.ub(1), 2);
        let pos = dom.entry_for(Lit::le(1, 2)).unwrap();
        assert_eq!(store.explain(c, Some(1), pos, &dom), vec![Lit::ge(0, 2)]);
        dom.set(Lit::ge(1, 2), Reason::Decision).unwrap();
        dom.set(Lit::ge(0, 3), Reason::Decision).unwrap();
        let conflict = store.propagate(c, &mut dom).unwrap_err();
        assert!(conflict.contains(&Lit::ge(0, 3)) && conflict.contains(&Lit::ge(1, 2)), "{conflict:?}");
    }
}
