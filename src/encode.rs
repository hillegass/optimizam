//! Linear encodings of the format 1.1 constraint kinds, so that both engines
//! can solve them. They add columns (which don't appear in results) and rows
//! to the model.
//!
//! The common tool is the value encoding of an integer column x: a binary
//! `[x = v]` per value v, with `sum_v [x = v] = 1` and `sum_v v [x = v] = x`.
//! It is built once per column and shared. It lets the CP engine remove
//! values from inside a domain, which bound literals alone can't express.
//!
//! - `all_different`: for each value, at most one item takes it (an item over
//!   several columns first gets a column equal to it).
//! - `table`: a binary per distinct allowed tuple, exactly one chosen, and each
//!   variable equal to the chosen tuple's entry. Forbidden tuples: for each,
//!   not every variable takes its entry.
//! - `element`: a selector binary per index combination of the array's
//!   comprehension, exactly one chosen; each index equals the chosen
//!   combination's element, and the value equals the chosen item (exactly
//!   for constant items, with big-M rows when items contain variables).
//! - `enforce_if`: with L the number of false literals (each literal is a
//!   binary, its negation, or a value binary `[x = v]`), the row is relaxed by
//!   M L on each side, M from the row's activity bounds.
//! - `max` (and `min`, mirrored): value >= every item, and a selector z_i for
//!   the item that attains the maximum (sum z = 1) with
//!   value <= item_i + M_i (1 - z_i).
//! - `abs`: value >= of and value >= -of, and a sign binary s with
//!   value <= of + M (1 - s) and value <= -of + M s.
//!
//! Ties would leave selectors free, so `"all"` would report a solution more
//! than once. For integer-valued items, tie-breaking rows make them unique:
//! `max` selects the first maximal item (z_i = 1 forces earlier items to be
//! at most item_i - 1), and `abs` has s = 1 exactly when of >= 0. Without
//! integer values, `"all"` with `min`/`max`/`abs` is an error.
//!
//! Intervals ([start, start + duration), present when their presence binary
//! is 1; the duration is at least 0 when present):
//! - `no_overlap`: for each pair, an order binary o: o = 1 means a ends by the
//!   time b starts; o = 0 means b ends by the time a starts and a ends after
//!   b starts; o is 0 unless both are present. Each holds only when both are
//!   present (as enforce_if). So o is exactly "a ends by the time b starts",
//!   two zero-length intervals may share a point, and a zero-length interval
//!   can't lie strictly inside another.
//! - `cumulative`: time-indexed. For each time t and interval i, a binary
//!   r = [start <= t] and [end > t] and present, linearized exactly from the
//!   value binaries of the start (and of the end, for a variable duration);
//!   then sum(demand r) <= capacity at every t. Zero-length intervals never
//!   run. A variable demand gets an exact product column.
//!
//! All of these are exact for integer-feasible points, and every added
//! binary is determined by the original columns, so `"all"` enumerates each
//! solution once.

use crate::model::{INF, Row};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Most values one column may have when encoded as binaries.
const MAX_ENCODED_VALUES: usize = 10_000;
/// Most columns the encodings together may add.
const MAX_ENCODED_TOTAL: usize = 2_000_000;
/// An `all_different` with an item over several columns with at least this
/// many values is left to the CP engine's Hall-interval propagator
/// (`cp/alldiff.rs`), and encoded only for the MILP engine. Such an item
/// needs a column of its own with its own value encoding, which is large
/// and propagates weakly through the row that defines it. Items over one
/// column share that column's value encoding, which is stronger than bound
/// reasoning (n-queens is much slower without it).
const ALLDIFF_ENCODED_VALUES: f64 = 32.0;

/// A constant plus constants times columns.
#[derive(Clone, Debug, Default)]
pub struct Affine {
    pub coefs: Vec<(usize, f64)>,
    pub constant: f64,
}

/// An `enforce_if` literal.
#[derive(Clone, Copy, Debug)]
pub enum Literal {
    /// A binary column, or (negated) its complement.
    Col { col: usize, negated: bool },
    /// `[x = value]`, or (with `equal == false`) its complement.
    Value { col: usize, value: i64, equal: bool },
}

/// A format 1.1 constraint instance, evaluated to columns and integers
/// (positions for symbolic sets).
#[derive(Clone)]
pub enum Global {
    AllDifferent {
        label: String,
        items: Vec<Affine>,
    },
    Table {
        label: String,
        vars: Vec<usize>,
        tuples: Vec<Vec<i64>>,
        forbidden: bool,
    },
    /// `value == item[k]` for the combination k whose index values equal `index`.
    Element {
        label: String,
        index: Vec<Affine>,
        combos: Vec<(Vec<i64>, Affine)>,
        value: Affine,
    },
    Enforced {
        label: String,
        coefs: Vec<(usize, f64)>,
        lower: f64,
        upper: f64,
        literals: Vec<Literal>,
    },
    /// `value == max(items)` (or, with `maximize == false`, `min(items)`).
    MinMax {
        label: String,
        value: Affine,
        items: Vec<Affine>,
        maximize: bool,
    },
    /// `value == |of|`.
    Abs {
        label: String,
        value: Affine,
        of: Affine,
    },
    /// An interval's duration is at least 0 when it is present.
    IntervalDuration {
        interval: Interval,
    },
    NoOverlap {
        label: String,
        intervals: Vec<Interval>,
    },
    Cumulative {
        label: String,
        intervals: Vec<Interval>,
        demands: Vec<Affine>,
        capacity: Affine,
    },
}

/// An interval instance: [start, start + duration), present when the
/// presence column (if any) is 1.
#[derive(Clone, Debug)]
pub struct Interval {
    pub label: String,
    pub start: Affine,
    pub duration: Affine,
    pub presence: Option<usize>,
}

impl Interval {
    fn end(&self) -> Affine {
        let mut coefs: BTreeMap<usize, f64> = BTreeMap::new();
        for &(j, v) in self.start.coefs.iter().chain(&self.duration.coefs) {
            *coefs.entry(j).or_default() += v;
        }
        Affine {
            coefs: coefs.into_iter().filter(|c| c.1 != 0.0).collect(),
            constant: self.start.constant + self.duration.constant,
        }
    }

    fn presence_literals(&self) -> Vec<Literal> {
        self.presence.map(|col| Literal::Col { col, negated: false }).into_iter().collect()
    }
}

/// A 0/1 quantity built from binaries: a constant, or the sum of some binaries
/// (or, negated, one minus that sum).
#[derive(Clone, Debug)]
enum Indicator {
    Const(bool),
    Sum { bools: Vec<usize>, negated: bool },
}

/// Most time steps a cumulative may span.
const MAX_HORIZON: f64 = 10_000.0;

/// The model's column arrays, which encodings append to.
pub struct Columns<'a> {
    pub lower: &'a mut Vec<f64>,
    pub upper: &'a mut Vec<f64>,
    pub integer: &'a mut Vec<bool>,
    pub domain: &'a mut Vec<Option<Vec<(i64, i64)>>>,
}

impl Columns<'_> {
    fn push(&mut self, lo: f64, hi: f64) -> Result<usize, String> {
        if self.lower.len() >= MAX_ENCODED_TOTAL {
            return Err("the format 1.1 constraints would need too many extra variables".into());
        }
        self.lower.push(lo);
        self.upper.push(hi);
        self.integer.push(true);
        self.domain.push(None);
        Ok(self.lower.len() - 1)
    }

    fn binary(&mut self) -> Result<usize, String> {
        self.push(0.0, 1.0)
    }

    fn allows(&self, col: usize, v: i64) -> bool {
        let v_f = v as f64;
        v_f >= self.lower[col] - 1e-9
            && v_f <= self.upper[col] + 1e-9
            && self.domain[col].as_ref().is_none_or(|r| r.iter().any(|&(a, b)| a <= v && v <= b))
    }

    /// The values an integer column can take, or an error if there are too many.
    fn values(&self, col: usize, label: &str) -> Result<Vec<i64>, String> {
        let (lo, hi) = (self.lower[col], self.upper[col]);
        if !(lo.is_finite() && hi.is_finite()) || hi - lo >= MAX_ENCODED_VALUES as f64 {
            return Err(format!(
                "constraint {label}: its variables need finite bounds with at most {MAX_ENCODED_VALUES} values"
            ));
        }
        let (lo, hi) = (lo.ceil() as i64, hi.floor() as i64);
        Ok((lo..=hi).filter(|&v| self.allows(col, v)).collect())
    }

    /// The range of an affine expression over the column bounds.
    fn range(&self, a: &Affine) -> (f64, f64) {
        let (mut lo, mut hi) = (a.constant, a.constant);
        for &(j, v) in &a.coefs {
            let (p, q) = (v * self.lower[j], v * self.upper[j]);
            lo += p.min(q);
            hi += p.max(q);
        }
        (lo, hi)
    }
}

/// Builds encodings, sharing each column's value encoding.
pub struct Encoder<'a> {
    pub cols: Columns<'a>,
    pub rows: &'a mut Vec<Row>,
    values: HashMap<usize, Vec<(i64, usize)>>,
    /// Every solution is wanted, so auxiliary columns must be unique.
    want_all: bool,
}

fn integral(v: f64) -> bool {
    v.fract() == 0.0 && v.abs() < 1e15
}

fn row(coefs: BTreeMap<usize, f64>, lower: f64, upper: f64) -> Row {
    Row { coefs: coefs.into_iter().filter(|c| c.1 != 0.0).collect(), lower, upper }
}

impl<'a> Encoder<'a> {
    pub fn new(cols: Columns<'a>, rows: &'a mut Vec<Row>, want_all: bool) -> Self {
        Encoder { cols, rows, values: HashMap::new(), want_all }
    }

    /// The value binaries `(v, [col = v])` of an integer column, built on first use.
    fn value_bools(&mut self, col: usize, label: &str) -> Result<Vec<(i64, usize)>, String> {
        if let Some(b) = self.values.get(&col) {
            return Ok(b.clone());
        }
        if !self.cols.integer[col] {
            return Err(format!("constraint {label}: needs integer variables"));
        }
        let mut bools = Vec::new();
        for v in self.cols.values(col, label)? {
            bools.push((v, self.cols.binary()?));
        }
        self.rows.push(Row { coefs: bools.iter().map(|&(_, b)| (b, 1.0)).collect(), lower: 1.0, upper: 1.0 });
        let mut link: BTreeMap<usize, f64> = bools.iter().map(|&(v, b)| (b, v as f64)).collect();
        link.insert(col, -1.0);
        self.rows.push(row(link, 0.0, 0.0));
        self.values.insert(col, bools.clone());
        Ok(bools)
    }

    /// Encodes one instance. Returns its label if it is infeasible on its face.
    pub fn encode(&mut self, g: &Global) -> Result<Option<String>, String> {
        match g {
            Global::AllDifferent { label, items } => self.all_different(label, items),
            Global::Table { label, vars, tuples, forbidden } => self.table(label, vars, tuples, *forbidden),
            Global::Element { label, index, combos, value } => self.element(label, index, combos, value),
            Global::Enforced { label, coefs, lower, upper, literals } => {
                self.enforced(label, coefs, *lower, *upper, literals)
            }
            Global::MinMax { label, value, items, maximize } => {
                if *maximize {
                    self.max(label, value, items)
                } else {
                    // min(items) = value  is  max(-items) = -value.
                    let items: Vec<Affine> = items.iter().map(negate).collect();
                    self.max(label, &negate(value), &items)
                }
            }
            Global::Abs { label, value, of } => self.abs(label, value, of),
            Global::IntervalDuration { interval } => {
                let d = &interval.duration;
                self.enforced(&interval.label, &d.coefs, -d.constant, INF, &interval.presence_literals())
            }
            Global::NoOverlap { label, intervals } => self.no_overlap(label, intervals),
            Global::Cumulative { label, intervals, demands, capacity } => {
                self.cumulative(label, intervals, demands, capacity)
            }
        }
    }

    /// Value precedence over a value symmetry class (`symmetry.rs`): value
    /// `values[k]` may appear at a column only if `values[k - 1]` appears at
    /// an earlier one, as `[x_i = v_k] <= sum(j < i) [x_j = v_(k-1)]`.
    pub fn value_precedence(&mut self, cols: &[usize], values: &[i64]) -> Result<(), String> {
        let mut bools: Vec<HashMap<i64, usize>> = Vec::new();
        for &c in cols {
            bools.push(self.value_bools(c, "value precedence")?.into_iter().collect());
        }
        for k in 1..values.len() {
            for (i, b) in bools.iter().enumerate() {
                let Some(&own) = b.get(&values[k]) else { continue };
                let mut coefs: BTreeMap<usize, f64> = BTreeMap::from([(own, 1.0)]);
                for earlier in &bools[..i] {
                    if let Some(&e) = earlier.get(&values[k - 1]) {
                        *coefs.entry(e).or_default() -= 1.0;
                    }
                }
                self.rows.push(row(coefs, -INF, 0.0));
            }
        }
        Ok(())
    }

    fn check_all_different(&self, label: &str, items: &[Affine]) -> Result<(), String> {
        for item in items {
            if !integral(item.constant) || item.coefs.iter().any(|&(j, v)| !integral(v) || !self.cols.integer[j]) {
                return Err(format!(
                    "constraint {label}: all_different items must be integer-valued (integer variables, integer coefficients)"
                ));
            }
        }
        Ok(())
    }

    /// Whether an `all_different` instance should be left unencoded, for the
    /// CP engine's native propagator: when an item over several columns has
    /// many values (see `ALLDIFF_ENCODED_VALUES`). Items with infinite bounds
    /// are encoded, which reports the error. Checks the items first.
    pub fn defer_all_different(&self, g: &Global) -> Result<bool, String> {
        let Global::AllDifferent { label, items } = g else { return Ok(false) };
        self.check_all_different(label, items)?;
        let range = |a: &Affine| {
            let (lo, hi) = self.cols.range(a);
            hi - lo
        };
        Ok(items.iter().all(|a| range(a).is_finite())
            && items.iter().any(|a| a.coefs.len() > 1 && range(a) >= ALLDIFF_ENCODED_VALUES))
    }

    fn all_different(&mut self, label: &str, items: &[Affine]) -> Result<Option<String>, String> {
        self.check_all_different(label, items)?;
        // Each item as (column, coefficient, constant), or a constant.
        let mut terms: Vec<Result<(usize, i64, i64), i64>> = Vec::new();
        for item in items {
            terms.push(match item.coefs.len() {
                0 => Err(item.constant as i64),
                1 => Ok((item.coefs[0].0, item.coefs[0].1 as i64, item.constant as i64)),
                _ => {
                    // z = item, then encode z.
                    let (lo, hi) = self.cols.range(item);
                    let z = self.cols.push(lo, hi)?;
                    let mut coefs: BTreeMap<usize, f64> = item.coefs.iter().map(|&(j, v)| (j, -v)).collect();
                    *coefs.entry(z).or_default() += 1.0;
                    self.rows.push(row(coefs, item.constant, item.constant));
                    Ok((z, 1, 0))
                }
            });
        }
        let mut constants: BTreeMap<i64, i64> = BTreeMap::new();
        for t in &terms {
            if let Err(c) = t {
                *constants.entry(*c).or_default() += 1;
            }
        }
        if constants.values().any(|&k| k > 1) {
            return Ok(Some(label.to_string()));
        }
        // For each value: at most one item takes it.
        let mut takers: BTreeMap<i64, BTreeMap<usize, f64>> = BTreeMap::new();
        for &(col, a, c) in terms.iter().flatten() {
            for (w, b) in self.value_bools(col, label)? {
                *takers.entry(a * w + c).or_default().entry(b).or_default() += 1.0;
            }
        }
        for (value, coefs) in takers {
            let fixed = constants.get(&value).copied().unwrap_or(0);
            let count: f64 = coefs.values().sum();
            if count + (fixed as f64) < 2.0 {
                continue;
            }
            self.rows.push(row(coefs, -INF, (1 - fixed) as f64));
        }
        Ok(None)
    }

    fn table(
        &mut self,
        label: &str,
        vars: &[usize],
        tuples: &[Vec<i64>],
        forbidden: bool,
    ) -> Result<Option<String>, String> {
        for &x in vars {
            if !self.cols.integer[x] {
                return Err(format!("constraint {label}: table variables must be integer"));
            }
        }
        // Distinct tuples whose every entry is in its variable's domain.
        let tuples: BTreeSet<&Vec<i64>> =
            tuples.iter().filter(|t| t.iter().zip(vars).all(|(&v, &x)| self.cols.allows(x, v))).collect();
        if forbidden {
            for t in tuples {
                let mut coefs: BTreeMap<usize, f64> = BTreeMap::new();
                for (&v, &x) in t.iter().zip(vars) {
                    let bools = self.value_bools(x, label)?;
                    let b = bools.iter().find(|p| p.0 == v).expect("the value is in the domain").1;
                    *coefs.entry(b).or_default() += 1.0;
                }
                self.rows.push(row(coefs, -INF, vars.len() as f64 - 1.0));
            }
            return Ok(None);
        }
        if tuples.is_empty() {
            return Ok(Some(label.to_string()));
        }
        let mut pick = Vec::new();
        for _ in 0..tuples.len() {
            pick.push(self.cols.binary()?);
        }
        self.rows.push(Row { coefs: pick.iter().map(|&b| (b, 1.0)).collect(), lower: 1.0, upper: 1.0 });
        for (i, &x) in vars.iter().enumerate() {
            let mut coefs: BTreeMap<usize, f64> = BTreeMap::new();
            *coefs.entry(x).or_default() += 1.0;
            for (t, &b) in tuples.iter().zip(&pick) {
                *coefs.entry(b).or_default() -= t[i] as f64;
            }
            self.rows.push(row(coefs, 0.0, 0.0));
        }
        Ok(None)
    }

    fn element(
        &mut self,
        label: &str,
        index: &[Affine],
        combos: &[(Vec<i64>, Affine)],
        value: &Affine,
    ) -> Result<Option<String>, String> {
        if combos.is_empty() {
            return Ok(Some(label.to_string()));
        }
        let mut select = Vec::new();
        for _ in 0..combos.len() {
            select.push(self.cols.binary()?);
        }
        self.rows.push(Row { coefs: select.iter().map(|&b| (b, 1.0)).collect(), lower: 1.0, upper: 1.0 });
        // Each index equals the chosen combination's element.
        for (d, idx) in index.iter().enumerate() {
            let mut coefs: BTreeMap<usize, f64> = BTreeMap::new();
            for &(j, v) in &idx.coefs {
                *coefs.entry(j).or_default() += v;
            }
            for ((k, _), &s) in combos.iter().zip(&select) {
                *coefs.entry(s).or_default() -= k[d] as f64;
            }
            self.rows.push(row(coefs, -idx.constant, -idx.constant));
        }
        if combos.iter().all(|(_, item)| item.coefs.is_empty()) {
            // value = sum_k item_k s_k, exactly.
            let mut coefs: BTreeMap<usize, f64> = BTreeMap::new();
            for &(j, v) in &value.coefs {
                *coefs.entry(j).or_default() += v;
            }
            for ((_, item), &s) in combos.iter().zip(&select) {
                *coefs.entry(s).or_default() -= item.constant;
            }
            self.rows.push(row(coefs, -value.constant, -value.constant));
            return Ok(None);
        }
        // With variable items: when s_k = 1, value - item_k = 0 (big-M pairs).
        for ((_, item), &s) in combos.iter().zip(&select) {
            let mut diff: BTreeMap<usize, f64> = BTreeMap::new();
            for &(j, v) in &value.coefs {
                *diff.entry(j).or_default() += v;
            }
            for &(j, v) in &item.coefs {
                *diff.entry(j).or_default() -= v;
            }
            let d0 = value.constant - item.constant;
            let d = Affine { coefs: diff.iter().map(|(&j, &v)| (j, v)).collect(), constant: d0 };
            let (lo, hi) = self.cols.range(&d);
            if !(lo.is_finite() && hi.is_finite()) {
                return Err(format!(
                    "constraint {label}: element with variable array items needs finite bounds on the value and items"
                ));
            }
            // d <= hi (1 - s): sum + hi s <= hi - d0
            if hi > 0.0 {
                let mut c = diff.clone();
                *c.entry(s).or_default() += hi;
                self.rows.push(row(c, -INF, hi - d0));
            }
            // d >= lo (1 - s): sum + lo s >= lo - d0
            if lo < 0.0 {
                let mut c = diff.clone();
                *c.entry(s).or_default() += lo;
                self.rows.push(row(c, lo - d0, INF));
            }
        }
        Ok(None)
    }

    fn enforced(
        &mut self,
        label: &str,
        coefs: &[(usize, f64)],
        lower: f64,
        upper: f64,
        literals: &[Literal],
    ) -> Result<Option<String>, String> {
        // Each literal as a binary column b, used positively (b) or negatively (1 - b).
        let mut lits: Vec<(usize, bool)> = Vec::new();
        for &lit in literals {
            match lit {
                Literal::Col { col, negated } => {
                    if !(self.cols.integer[col] && self.cols.lower[col] >= 0.0 && self.cols.upper[col] <= 1.0) {
                        return Err(format!(
                            "constraint {label}: enforce_if literal must be a binary variable or a comparison"
                        ));
                    }
                    lits.push((col, !negated));
                }
                Literal::Value { col, value, equal } => {
                    if !self.cols.allows(col, value) {
                        if equal {
                            return Ok(None); // never true: the constraint never applies
                        }
                        continue; // always true
                    }
                    let bools = self.value_bools(col, label)?;
                    let b = bools.iter().find(|p| p.0 == value).expect("the value is in the domain").1;
                    lits.push((b, equal));
                }
            }
        }
        if lits.is_empty() {
            // Every literal always holds: a plain row.
            self.rows.push(Row { coefs: coefs.to_vec(), lower, upper });
            return Ok(None);
        }
        let act = Affine { coefs: coefs.to_vec(), constant: 0.0 };
        let (amin, amax) = self.cols.range(&act);
        let positives = lits.iter().filter(|l| l.1).count() as f64;
        let base: BTreeMap<usize, f64> = coefs.iter().copied().collect();
        // L = sum over positive literals of (1 - b) + sum over negative literals of b.
        // a x <= up + M L  is  a x + M sum_pos b - M sum_neg b <= up + M #pos.
        if upper.is_finite() {
            let m = amax - upper;
            if m > 0.0 {
                if !m.is_finite() {
                    return Err(format!("constraint {label}: needs finite bounds on the variables of its rows"));
                }
                let mut c = base.clone();
                for &(b, pos) in &lits {
                    *c.entry(b).or_default() += if pos { m } else { -m };
                }
                self.rows.push(row(c, -INF, upper + m * positives));
            }
        }
        // a x >= lo - M L  is  a x - M sum_pos b + M sum_neg b >= lo - M #pos.
        if lower.is_finite() {
            let m = lower - amin;
            if m > 0.0 {
                if !m.is_finite() {
                    return Err(format!("constraint {label}: needs finite bounds on the variables of its rows"));
                }
                let mut c = base;
                for &(b, pos) in &lits {
                    *c.entry(b).or_default() += if pos { -m } else { m };
                }
                self.rows.push(row(c, lower - m * positives, INF));
            }
        }
        Ok(None)
    }

    /// Whether an affine expression takes only integer values.
    fn integer_valued(&self, a: &Affine) -> bool {
        integral(a.constant) && a.coefs.iter().all(|&(j, v)| integral(v) && self.cols.integer[j])
    }

    /// Bounds of an affine expression, which must be finite.
    fn finite_range(&self, a: &Affine, label: &str, what: &str) -> Result<(f64, f64), String> {
        let (lo, hi) = self.cols.range(a);
        if lo.is_finite() && hi.is_finite() {
            Ok((lo, hi))
        } else {
            Err(format!("constraint {label}: {what} needs finite bounds on its variables"))
        }
    }

    /// Pushes `sum(coefs) + constant` in [lower, upper], with the constant moved.
    fn push_row(&mut self, terms: (BTreeMap<usize, f64>, f64), lower: f64, upper: f64) {
        let (coefs, constant) = terms;
        self.rows.push(row(coefs, lower - constant, upper - constant));
    }

    fn max(&mut self, label: &str, value: &Affine, items: &[Affine]) -> Result<Option<String>, String> {
        if items.is_empty() {
            return Err(format!("constraint {label}: the list of min/max must not be empty"));
        }
        if items.len() == 1 {
            self.push_row(diff(value, &items[0]), 0.0, 0.0);
            return Ok(None);
        }
        // value >= every item.
        for item in items {
            self.push_row(diff(value, item), 0.0, INF);
        }
        let ranges: Vec<(f64, f64)> =
            items.iter().map(|it| self.finite_range(it, label, "min/max")).collect::<Result<_, _>>()?;
        let top = ranges.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max);
        let mut select = Vec::new();
        for _ in items {
            select.push(self.cols.binary()?);
        }
        self.rows.push(Row { coefs: select.iter().map(|&b| (b, 1.0)).collect(), lower: 1.0, upper: 1.0 });
        // value <= item_i + M_i (1 - z_i), M_i = top - lo_i.
        for ((item, &(lo, _)), &z) in items.iter().zip(&ranges).zip(&select) {
            let m = top - lo;
            let (mut coefs, constant) = diff(value, item);
            *coefs.entry(z).or_default() += m;
            self.push_row((coefs, constant), -INF, m);
        }
        // Tie-breaking: z_i = 1 forces item_j <= item_i - 1 for j < i.
        if items.iter().all(|it| self.integer_valued(it)) {
            for i in 0..items.len() {
                for j in 0..i {
                    // item_j - item_i <= -1 + B (1 - z_i), B = hi_j - lo_i + 1.
                    let b = ranges[j].1 - ranges[i].0 + 1.0;
                    if b <= 0.0 {
                        continue; // item_j < item_i always
                    }
                    let (mut coefs, constant) = diff(&items[j], &items[i]);
                    *coefs.entry(select[i]).or_default() += b;
                    self.push_row((coefs, constant), -INF, b - 1.0);
                }
            }
        } else if self.want_all {
            return Err(format!(
                "constraint {label}: min/max with \"all\" needs integer-valued items (so that ties are counted once)"
            ));
        }
        Ok(None)
    }

    fn abs(&mut self, label: &str, value: &Affine, of: &Affine) -> Result<Option<String>, String> {
        let (lo, hi) = self.finite_range(of, label, "abs")?;
        let minus_of = negate(of);
        if lo >= 0.0 {
            self.push_row(diff(value, of), 0.0, 0.0);
            return Ok(None);
        }
        if hi <= 0.0 {
            self.push_row(diff(value, &minus_of), 0.0, 0.0);
            return Ok(None);
        }
        // value >= of and value >= -of.
        self.push_row(diff(value, of), 0.0, INF);
        self.push_row(diff(value, &minus_of), 0.0, INF);
        let s = self.cols.binary()?;
        let m = 2.0 * hi.max(-lo);
        // value - of <= M (1 - s)
        let (mut coefs, constant) = diff(value, of);
        *coefs.entry(s).or_default() += m;
        self.push_row((coefs, constant), -INF, m);
        // value + of <= M s
        let (mut coefs, constant) = diff(value, &minus_of);
        *coefs.entry(s).or_default() -= m;
        self.push_row((coefs, constant), -INF, 0.0);
        // Tie-breaking: s = 1 exactly when of >= 0.
        if self.integer_valued(of) {
            let zero = Affine::default();
            // of >= lo (1 - s): of + lo s >= lo
            let (mut coefs, constant) = diff(of, &zero);
            *coefs.entry(s).or_default() += lo;
            self.push_row((coefs, constant), lo, INF);
            // of <= -1 + (hi + 1) s
            let (mut coefs, constant) = diff(of, &zero);
            *coefs.entry(s).or_default() -= hi + 1.0;
            self.push_row((coefs, constant), -INF, -1.0);
        } else if self.want_all {
            return Err(format!("constraint {label}: abs with \"all\" needs an integer-valued expression"));
        }
        Ok(None)
    }

    /// Pushes `a` in [lower, upper] when all the literals hold.
    fn enforced_affine(
        &mut self,
        label: &str,
        a: (BTreeMap<usize, f64>, f64),
        lower: f64,
        upper: f64,
        literals: &[Literal],
    ) -> Result<Option<String>, String> {
        let (coefs, constant) = a;
        let coefs: Vec<(usize, f64)> = coefs.into_iter().filter(|c| c.1 != 0.0).collect();
        self.enforced(label, &coefs, lower - constant, upper - constant, literals)
    }

    fn no_overlap(&mut self, label: &str, intervals: &[Interval]) -> Result<Option<String>, String> {
        for (i, a) in intervals.iter().enumerate() {
            for b in &intervals[i + 1..] {
                let o = self.cols.binary()?;
                let mut lits: Vec<Literal> = a.presence_literals();
                lits.extend(b.presence_literals());
                let (end_a, end_b) = (a.end(), b.end());
                // o = 1: a ends by the time b starts.
                let mut on = lits.clone();
                on.push(Literal::Col { col: o, negated: false });
                self.enforced_affine(label, diff(&end_a, &b.start), -INF, 0.0, &on)?;
                // o = 0: b ends by the time a starts, and a ends after b starts.
                let mut off = lits.clone();
                off.push(Literal::Col { col: o, negated: true });
                self.enforced_affine(label, diff(&end_b, &a.start), -INF, 0.0, &off)?;
                self.enforced_affine(label, diff(&end_a, &b.start), 1.0, INF, &off)?;
                // o is 0 unless both are present.
                for p in a.presence.iter().chain(&b.presence) {
                    self.rows.push(row(BTreeMap::from([(o, 1.0), (*p, -1.0)]), -INF, 0.0));
                }
            }
        }
        Ok(None)
    }

    /// The column equal to a non-constant affine expression (itself if it is
    /// one column), or None for a constant.
    fn column_of(&mut self, a: &Affine) -> Result<Option<usize>, String> {
        match a.coefs.as_slice() {
            [] => Ok(None),
            [(j, v)] if *v == 1.0 && a.constant == 0.0 => Ok(Some(*j)),
            _ => {
                let (lo, hi) = self.cols.range(a);
                let z = self.cols.push(lo, hi)?;
                let (mut coefs, constant) = diff(&Affine::default(), a);
                *coefs.entry(z).or_default() += 1.0;
                self.push_row((coefs, constant), 0.0, 0.0);
                Ok(Some(z))
            }
        }
    }

    /// [x <= t] for an affine x, given its column (None if constant).
    fn at_most(&mut self, x: &Affine, col: Option<usize>, t: i64, label: &str) -> Result<Indicator, String> {
        let Some(col) = col else { return Ok(Indicator::Const(x.constant <= t as f64)) };
        let bools: Vec<usize> =
            self.value_bools(col, label)?.into_iter().filter(|&(v, _)| v <= t).map(|p| p.1).collect();
        let all = self.values.get(&col).map_or(0, |v| v.len());
        Ok(match bools.len() {
            0 => Indicator::Const(false),
            n if n == all => Indicator::Const(true),
            _ => Indicator::Sum { bools, negated: false },
        })
    }

    fn cumulative(
        &mut self,
        label: &str,
        intervals: &[Interval],
        demands: &[Affine],
        capacity: &Affine,
    ) -> Result<Option<String>, String> {
        self.check_scheduling(&Global::Cumulative {
            label: label.to_string(),
            intervals: intervals.to_vec(),
            demands: demands.to_vec(),
            capacity: capacity.clone(),
        })?;
        // usage[t]: the terms (columns, constant) of the total demand at time t.
        let mut usage: BTreeMap<i64, (BTreeMap<usize, f64>, f64)> = BTreeMap::new();
        for (iv, demand) in intervals.iter().zip(demands) {
            let (dlo, dhi) = self.finite_range(demand, label, "cumulative")?;
            if dlo < 0.0 {
                self.push_row(diff(demand, &Affine::default()), 0.0, INF); // demand >= 0
            }
            if dhi == 0.0 || (iv.duration.coefs.is_empty() && iv.duration.constant <= 0.0) {
                continue; // never uses anything
            }
            let end = iv.end();
            let (slo, _) = self.finite_range(&iv.start, label, "cumulative")?;
            let (_, ehi) = self.finite_range(&end, label, "cumulative")?;
            if ehi - slo > MAX_HORIZON {
                return Err(format!("constraint {label}: cumulative spans more than {MAX_HORIZON} time steps"));
            }
            let start_col = self.column_of(&iv.start)?;
            // For a variable duration, the end gets its own column.
            let end_col = if iv.duration.coefs.is_empty() { None } else { self.column_of(&end)? };
            for t in slo as i64..ehi as i64 {
                // r = [start <= t] and [end > t] and present.
                let started = self.at_most(&iv.start, start_col, t, label)?;
                let running = match end_col {
                    None => {
                        // end > t  is  start > t - duration.
                        let d = iv.duration.constant as i64;
                        negate_indicator(self.at_most(&iv.start, start_col, t - d, label)?)
                    }
                    Some(_) => negate_indicator(self.at_most(&end, end_col, t, label)?),
                };
                let mut factors = vec![started, running];
                if let Some(p) = iv.presence {
                    factors.push(Indicator::Sum { bools: vec![p], negated: false });
                }
                if factors.iter().any(|f| matches!(f, Indicator::Const(false))) {
                    continue;
                }
                factors.retain(|f| !matches!(f, Indicator::Const(true)));
                let r = if factors.is_empty() { None } else { Some(self.and(&factors)?) };
                let entry = usage.entry(t).or_default();
                match (r, demand.coefs.is_empty()) {
                    (None, _) => {
                        for &(j, v) in &demand.coefs {
                            *entry.0.entry(j).or_default() += v;
                        }
                        entry.1 += demand.constant;
                    }
                    (Some(r), true) => *entry.0.entry(r).or_default() += demand.constant,
                    (Some(r), false) => {
                        // w = demand * r: w <= D r, w <= demand, w >= demand - D (1 - r), w >= 0.
                        let w = self.cols.push(0.0, dhi)?;
                        self.rows.push(row(BTreeMap::from([(w, 1.0), (r, -dhi)]), -INF, 0.0));
                        let (mut c, k) = diff(&Affine::default(), demand);
                        *c.entry(w).or_default() += 1.0;
                        self.push_row((c.clone(), k), -INF, 0.0);
                        *c.entry(r).or_default() -= dhi;
                        self.push_row((c, k), -dhi, INF);
                        *entry.0.entry(w).or_default() += 1.0;
                    }
                }
            }
        }
        // At every time: usage <= capacity.
        for (_, (mut coefs, constant)) in usage {
            for &(j, v) in &capacity.coefs {
                *coefs.entry(j).or_default() -= v;
            }
            self.push_row((coefs, constant - capacity.constant), -INF, 0.0);
        }
        Ok(None)
    }

    /// The checks of a `no_overlap` or `cumulative` instance that don't
    /// depend on how it is solved (natively by the CP engine, or encoded).
    pub fn check_scheduling(&self, g: &Global) -> Result<(), String> {
        let Global::Cumulative { label, intervals, demands, .. } = g else { return Ok(()) };
        if intervals.len() != demands.len() {
            return Err(format!("constraint {label}: {} intervals but {} demands", intervals.len(), demands.len()));
        }
        for (iv, demand) in intervals.iter().zip(demands) {
            let (dlo, dhi) = self.finite_range(demand, label, "cumulative")?;
            if !self.integer_valued(demand) || !self.integer_valued(&iv.start) || !self.integer_valued(&iv.duration) {
                return Err(format!(
                    "constraint {label}: cumulative needs integer-valued starts, durations, and demands"
                ));
            }
            if dhi < 0.0 || (demand.coefs.is_empty() && dlo < 0.0) {
                return Err(format!("constraint {label}: a demand must be at least 0"));
            }
        }
        Ok(())
    }

    /// A binary equal to the AND of 0/1 factors (none constant).
    fn and(&mut self, factors: &[Indicator]) -> Result<usize, String> {
        let r = self.cols.binary()?;
        // r <= f for each factor; r >= sum(f) - (k - 1).
        let mut lower: BTreeMap<usize, f64> = BTreeMap::from([(r, 1.0)]);
        let mut lower_const = 0.0;
        for f in factors {
            let Indicator::Sum { bools, negated } = f else { unreachable!() };
            let sign = if *negated { -1.0 } else { 1.0 };
            // f = sum(bools) or 1 - sum(bools).
            let mut c: BTreeMap<usize, f64> = BTreeMap::from([(r, 1.0)]);
            for &b in bools {
                *c.entry(b).or_default() -= sign;
                *lower.entry(b).or_default() -= sign;
            }
            let f_const = if *negated { 1.0 } else { 0.0 };
            self.rows.push(row(c, -INF, f_const)); // r - (f - f_const) <= f_const
            lower_const += f_const;
        }
        // r - sum(f) >= -(k - 1)  with  sum(f) = sum(+-bools) + constants.
        self.rows.push(row(lower, -(factors.len() as f64 - 1.0) + lower_const, INF));
        Ok(r)
    }
}

fn negate_indicator(f: Indicator) -> Indicator {
    match f {
        Indicator::Const(b) => Indicator::Const(!b),
        Indicator::Sum { bools, negated } => Indicator::Sum { bools, negated: !negated },
    }
}

fn negate(a: &Affine) -> Affine {
    Affine { coefs: a.coefs.iter().map(|&(j, v)| (j, -v)).collect(), constant: -a.constant }
}

/// `a - b` as (coefficients, constant).
fn diff(a: &Affine, b: &Affine) -> (BTreeMap<usize, f64>, f64) {
    let mut coefs: BTreeMap<usize, f64> = BTreeMap::new();
    for &(j, v) in &a.coefs {
        *coefs.entry(j).or_default() += v;
    }
    for &(j, v) in &b.coefs {
        *coefs.entry(j).or_default() -= v;
    }
    (coefs, a.constant - b.constant)
}
