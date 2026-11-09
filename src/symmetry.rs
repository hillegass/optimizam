//! Value symmetry: classes of columns whose values can be permuted in any
//! solution without changing feasibility or the objective (like the colors
//! of a graph coloring), so that value precedence can keep one solution of
//! each set of relabelings.
//!
//! A class is a set C of integer columns, linked by `all_different`
//! instances whose items are plain columns of C, with the same bounds and
//! domain, no objective cost, and no other constraint than:
//!
//! - single-column rows that fix a value: that value is *pinned* (never
//!   permuted); a single-column row that only narrows the bounds breaks the
//!   symmetry, so the columns aren't a class;
//! - "max" rows `y >= x + d`, with the same y (outside C) and d for every x
//!   in C.
//!
//! The interchangeable values V are the domain minus the pinned ones, in
//! increasing order. Relabeling V in order of first occurrence (along the
//! columns of C in their order) maps any solution to one whose V-values
//! appear in that order: `all_different` over C holds after any permutation
//! of values, pinned values don't move, and the largest value used doesn't
//! grow (the m values of V in use become the m smallest of V), so every
//! max row still holds. Value precedence (`Encoder::value_precedence`)
//! requires that order, which keeps at least one optimal solution. It
//! removes solutions, so it isn't used when all solutions are wanted.

use crate::encode::{Affine, Global, Interval};
use crate::model::Row;
use std::collections::BTreeMap;

/// Most values a class may have (the precedence rows grow with it).
const MAX_VALUES: i64 = 1000;

#[derive(Debug, PartialEq)]
pub struct ValueClass {
    /// In increasing order (the order of first occurrence).
    pub cols: Vec<usize>,
    /// The interchangeable values, in increasing order.
    pub values: Vec<i64>,
}

pub struct Columns<'a> {
    pub lower: &'a [f64],
    pub upper: &'a [f64],
    pub integer: &'a [bool],
    pub domain: &'a [Option<Vec<(i64, i64)>>],
}

fn plain(a: &Affine) -> Option<usize> {
    match a.coefs.as_slice() {
        &[(j, v)] if v == 1.0 && a.constant == 0.0 => Some(j),
        _ => None,
    }
}

/// Every column that a constraint instance mentions.
fn global_columns(g: &Global) -> Vec<usize> {
    let affine = |a: &Affine| a.coefs.iter().map(|c| c.0).collect::<Vec<_>>();
    let interval = |iv: &Interval| {
        let mut cols = affine(&iv.start);
        cols.extend(affine(&iv.duration));
        cols.extend(iv.presence);
        cols
    };
    match g {
        Global::AllDifferent { items, .. } => items.iter().flat_map(affine).collect(),
        Global::Table { vars, .. } => vars.clone(),
        Global::Element { index, combos, value, .. } => {
            index.iter().chain(combos.iter().map(|c| &c.1)).chain([value]).flat_map(affine).collect()
        }
        Global::Enforced { coefs, literals, .. } => {
            let mut cols: Vec<usize> = coefs.iter().map(|c| c.0).collect();
            cols.extend(literals.iter().map(|l| match *l {
                crate::encode::Literal::Col { col, .. } | crate::encode::Literal::Value { col, .. } => col,
            }));
            cols
        }
        Global::MinMax { value, items, .. } => items.iter().chain([value]).flat_map(affine).collect(),
        Global::Abs { value, of, .. } => [value, of].into_iter().flat_map(affine).collect(),
        Global::IntervalDuration { interval: iv } => interval(iv),
        Global::NoOverlap { intervals, .. } => intervals.iter().flat_map(interval).collect(),
        Global::Cumulative { intervals, demands, capacity, .. } => {
            intervals.iter().flat_map(interval).chain(demands.iter().chain([capacity]).flat_map(affine)).collect()
        }
    }
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// What a row says about the class columns it mentions.
enum RowUse {
    /// The row doesn't mention a class candidate.
    None,
    /// Fixes column x to a value.
    Pin(usize, i64),
    /// Holds for every value of x in its bounds.
    Redundant,
    /// `y - x >= d`, as (x, y, d).
    Max(usize, usize, f64),
    /// Anything else: x's class isn't symmetric.
    Breaks(Vec<usize>),
}

fn classify(row: &Row, candidate: &[bool], cols: &Columns) -> RowUse {
    let mine: Vec<(usize, f64)> = row.coefs.iter().copied().filter(|c| candidate[c.0] && c.1 != 0.0).collect();
    if mine.is_empty() {
        return RowUse::None;
    }
    let breaks = RowUse::Breaks(mine.iter().map(|c| c.0).collect());
    match (row.coefs.as_slice(), mine.as_slice()) {
        (&[(x, a)], _) => {
            let (mut l, mut u) = (row.lower / a, row.upper / a);
            if a < 0.0 {
                (l, u) = (u, l);
            }
            if l == u && l.fract() == 0.0 {
                RowUse::Pin(x, l as i64)
            } else if l <= cols.lower[x] && u >= cols.upper[x] {
                RowUse::Redundant
            } else {
                breaks
            }
        }
        (&[_, _], &[(x, a)]) => {
            let &(y, b) = row.coefs.iter().find(|c| c.0 != x).unwrap();
            // a x + b y in [lower, upper], with b = -a: as y - x >= d.
            if b != -a {
                return breaks;
            }
            let (lo, up) = if b > 0.0 { (row.lower / b, row.upper / b) } else { (row.upper / b, row.lower / b) };
            // b (y - x) in [lower, upper]: y - x in [lo, up] (bounds swapped for b < 0).
            if up == f64::INFINITY && lo.is_finite() { RowUse::Max(x, y, lo) } else { breaks }
        }
        _ => breaks,
    }
}

/// The value symmetry classes of a model (before its constraint instances
/// are encoded): the rows, the instances, the objective, and the columns.
pub fn value_classes(rows: &[Row], globals: &[Global], obj: &[f64], cols: &Columns) -> Vec<ValueClass> {
    let n = cols.lower.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let mut candidate = vec![false; n];
    let mut broken = vec![false; n];
    for g in globals {
        let plain_items = match g {
            Global::AllDifferent { items, .. } => items.iter().map(plain).collect::<Option<Vec<usize>>>(),
            _ => None,
        };
        match plain_items {
            Some(items) if items.len() >= 2 => {
                for &j in &items {
                    candidate[j] = true;
                    let (a, b) = (find(&mut parent, items[0]), find(&mut parent, j));
                    parent[b] = a;
                }
            }
            _ => global_columns(g).into_iter().for_each(|j| broken[j] = true),
        }
    }
    for (j, &c) in obj.iter().enumerate() {
        if c != 0.0 {
            broken[j] = true;
        }
    }
    let mut pinned: BTreeMap<usize, Vec<i64>> = BTreeMap::new();
    let mut maxes: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for row in rows {
        match classify(row, &candidate, cols) {
            RowUse::None | RowUse::Redundant => {}
            RowUse::Pin(x, v) => pinned.entry(find(&mut parent, x)).or_default().push(v),
            RowUse::Max(x, y, d) if !candidate[y] => maxes[x].push((y, d)),
            RowUse::Max(x, y, _) => [x, y].into_iter().for_each(|j| broken[j] = true),
            RowUse::Breaks(js) => js.into_iter().for_each(|j| broken[j] = true),
        }
    }

    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for j in (0..n).filter(|&j| candidate[j]) {
        members.entry(find(&mut parent, j)).or_default().push(j);
    }
    let mut classes = Vec::new();
    for (root, cs) in members {
        let first = cs[0];
        let same = |j: usize| {
            cols.integer[j]
                && cols.lower[j] == cols.lower[first]
                && cols.upper[j] == cols.upper[first]
                && cols.domain[j] == cols.domain[first]
        };
        if cs.len() < 2 || cs.iter().any(|&j| broken[j] || !same(j)) {
            continue;
        }
        let max_rows = |j: usize| {
            let mut m = maxes[j].clone();
            m.sort_by(|a, b| a.partial_cmp(b).unwrap());
            m.dedup();
            m
        };
        let common = max_rows(first);
        if cs.iter().any(|&j| max_rows(j) != common) {
            continue;
        }
        let (lo, hi) = (cols.lower[first], cols.upper[first]);
        if !(lo.is_finite() && hi.is_finite()) || hi - lo >= MAX_VALUES as f64 {
            continue;
        }
        let pins = pinned.get(&root).cloned().unwrap_or_default();
        let allowed = |v: i64| cols.domain[first].as_ref().is_none_or(|r| r.iter().any(|&(a, b)| a <= v && v <= b));
        let values: Vec<i64> = (lo as i64..=hi as i64).filter(|&v| allowed(v) && !pins.contains(&v)).collect();
        if values.len() >= 2 {
            classes.push(ValueClass { cols: cs, values });
        }
    }
    classes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alldiff(items: &[usize]) -> Global {
        Global::AllDifferent {
            label: "d".into(),
            items: items.iter().map(|&j| Affine { coefs: vec![(j, 1.0)], constant: 0.0 }).collect(),
        }
    }

    fn row(coefs: &[(usize, f64)], lower: f64, upper: f64) -> Row {
        Row { coefs: coefs.to_vec(), lower, upper }
    }

    /// A coloring of a triangle x0 x1 x2 in 1..4, with used (x3, 1..4) >= every color.
    fn classes(rows: &[Row], obj: &[f64]) -> Vec<ValueClass> {
        let (lower, upper) = (vec![1.0; 4], vec![4.0; 4]);
        let globals = [alldiff(&[0, 1]), alldiff(&[1, 2]), alldiff(&[0, 2])];
        let cols = Columns { lower: &lower, upper: &upper, integer: &[true; 4], domain: &[None, None, None, None] };
        value_classes(rows, &globals, obj, &cols)
    }

    #[test]
    fn coloring_with_a_pinned_color() {
        let mut rows: Vec<Row> = (0..3).map(|j| row(&[(3, 1.0), (j, -1.0)], 0.0, f64::INFINITY)).collect();
        rows.push(row(&[(0, 1.0)], 1.0, 1.0));
        let obj = [0.0, 0.0, 0.0, 1.0];
        assert_eq!(classes(&rows, &obj), vec![ValueClass { cols: vec![0, 1, 2], values: vec![2, 3, 4] }]);
    }

    #[test]
    fn what_breaks_the_symmetry() {
        let max: Vec<Row> = (0..3).map(|j| row(&[(3, 1.0), (j, -1.0)], 0.0, f64::INFINITY)).collect();
        // A cost on a color.
        assert!(classes(&max, &[0.0, 1.0, 0.0, 1.0]).is_empty());
        // A bound on one color that doesn't fix it.
        let mut rows = max.clone();
        rows.push(row(&[(1, 1.0)], f64::NEG_INFINITY, 2.0));
        assert!(classes(&rows, &[0.0; 4]).is_empty());
        // The max row for only some of the colors.
        assert!(classes(&max[..2], &[0.0; 4]).is_empty());
        // Any other row over the colors.
        let mut rows = max.clone();
        rows.push(row(&[(0, 1.0), (1, 1.0)], f64::NEG_INFINITY, 5.0));
        assert!(classes(&rows, &[0.0; 4]).is_empty());
        // Without those, the class is all four values.
        assert_eq!(classes(&max, &[0.0; 4]).len(), 1);
    }
}
