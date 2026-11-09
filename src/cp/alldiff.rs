//! A native bounds-consistent `all_different` propagator (Hall intervals).
//!
//! If k items all lie in [L, U] and k = U - L + 1, they take every value of
//! [L, U] (a Hall interval), so every other item lies outside it: an item
//! whose lower bound is in [L, U] must be above U. With k > U - L + 1 there
//! is no solution. Running this on the items and on their negations (which
//! pulls upper bounds down) reaches bounds consistency at the fixed point.
//!
//! For each candidate right end U (an item's upper bound), the items with
//! upper bound at most U are swept in decreasing order of lower bound: after
//! the items with lower bound at least L, the count is exactly the number of
//! items inside [L, U]. That is O(n^2) per call after one sort. The
//! explanation of a pushed bound is the Hall set's bounds ([t >= L] and
//! [t <= U] for each member) and the pushed item's own [t >= L].
//!
//! The value encoding of `all_different` (`encode.rs`) is exact on its own;
//! this propagator either replaces it (for items with large domains, where
//! the encoding is big) or adds bound reasoning to it.

use super::domains::{Domains, Lit};
use super::scheduling::{Term, set_ge};

/// Hall-interval pruning of the items' lower bounds; Err is a conflict.
fn lower_bounds(items: &[Term], dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>) -> Result<(), Vec<Lit>> {
    let bounds: Vec<(i64, i64)> = items.iter().map(|t| (t.lb(dom), t.ub(dom))).collect();
    let mut by_lb: Vec<usize> = (0..items.len()).collect();
    by_lb.sort_by_key(|&i| std::cmp::Reverse(bounds[i].0));
    let mut ends: Vec<i64> = bounds.iter().map(|b| b.1).collect();
    ends.sort_unstable();
    ends.dedup();
    let why = |members: &[usize], lo: i64, hi: i64| -> Vec<Lit> {
        members.iter().flat_map(|&i| [items[i].ge(lo), items[i].le(hi)]).flatten().collect()
    };
    for &hi in &ends {
        let mut members: Vec<usize> = Vec::new();
        let inside: Vec<usize> = by_lb.iter().copied().filter(|&i| bounds[i].1 <= hi).collect();
        for (k, &i) in inside.iter().enumerate() {
            members.push(i);
            let lo = bounds[i].0;
            // Count all the items with this lower bound before checking.
            if inside.get(k + 1).is_some_and(|&j| bounds[j].0 == lo) {
                continue;
            }
            let size = (hi - lo + 1).max(0) as usize;
            if members.len() > size {
                return Err(why(&members, lo, hi));
            }
            if members.len() < size {
                continue;
            }
            // [lo, hi] is a Hall interval: push the others' lower bounds past it.
            for (j, t) in items.iter().enumerate() {
                let (lb, ub) = bounds[j];
                if ub > hi && lb >= lo && lb <= hi {
                    let mut reason = why(&members, lo, hi);
                    reason.extend(t.ge(lo));
                    set_ge(dom, reasons, *t, hi + 1, reason)?;
                }
            }
        }
    }
    Ok(())
}

/// Propagates `all_different` over the items.
pub fn propagate(items: &[Term], dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>) -> Result<(), Vec<Lit>> {
    lower_bounds(items, dom, reasons)?;
    let negated: Vec<Term> = items.iter().map(|t| t.neg()).collect();
    lower_bounds(&negated, dom, reasons)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(n: usize) -> Vec<Term> {
        (0..n).map(|v| Term::of(v, 1, 0)).collect()
    }

    /// x, y in [1, 2] form a Hall interval, so z in [1, 3] must be 3.
    #[test]
    fn hall_interval_pushes_the_others() {
        let mut dom = Domains::new(vec![1, 1, 1], vec![2, 2, 3], vec![None; 3]);
        let mut reasons = Vec::new();
        propagate(&vars(3), &mut dom, &mut reasons).unwrap();
        assert_eq!((dom.lb(2), dom.ub(2)), (3, 3));
        // Pushed from below by the Hall set's bounds and z's own [z >= 1] (root, so dropped later).
        assert!(reasons[0].contains(&Lit::le(0, 2)) && reasons[0].contains(&Lit::le(1, 2)), "{:?}", reasons[0]);
    }

    /// Three items in [1, 2] can't all differ.
    #[test]
    fn pigeons_conflict() {
        let mut dom = Domains::new(vec![1; 3], vec![2; 3], vec![None; 3]);
        assert!(propagate(&vars(3), &mut dom, &mut Vec::new()).is_err());
    }

    /// Upper bounds through the negated items: x, y in [4, 5], z in [3, 5] gives z = 3.
    #[test]
    fn upper_bounds_by_mirroring() {
        let mut dom = Domains::new(vec![4, 4, 3], vec![5, 5, 5], vec![None; 3]);
        propagate(&vars(3), &mut dom, &mut Vec::new()).unwrap();
        assert_eq!((dom.lb(2), dom.ub(2)), (3, 3));
    }

    /// Offsets and negated terms: x - 1 and 3 - y differ from the constant 1.
    #[test]
    fn affine_items_and_constants() {
        // x in [2, 3] so x - 1 in [1, 2]; y in [1, 2] so 3 - y in [1, 2]; with the constant 1,
        // three items in [1, 2]: a conflict.
        let mut dom = Domains::new(vec![2, 1], vec![3, 2], vec![None; 2]);
        let items = [Term::of(0, 1, -1), Term::of(1, -1, 3), Term::constant(1)];
        assert!(propagate(&items, &mut dom, &mut Vec::new()).is_err());
        // Without the constant: a Hall interval [1, 2] over both, nothing to push.
        let mut dom = Domains::new(vec![2, 1], vec![3, 2], vec![None; 2]);
        propagate(&items[..2], &mut dom, &mut Vec::new()).unwrap();
        assert_eq!((dom.lb(0), dom.ub(0), dom.lb(1), dom.ub(1)), (2, 3, 1, 2));
    }
}
