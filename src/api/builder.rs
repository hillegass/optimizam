//! The model builder: variables, constraints, and objectives in Rust.

use super::expression::{Constraint, Expression, Variable};
use super::family::{Element, Set, VarFamily, key_of};
use super::{Conflict, Error, Options, SolveResult};
use crate::model::Sense;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT_MODEL_ID: AtomicU64 = AtomicU64::new(1);

/// An optimization model built in Rust: variables, constraints, and an
/// objective.
///
/// ```
/// use optimizam::{Model, Options, Status, constraint, var};
///
/// // A small knapsack: pick items of the most value within a weight of 10.
/// let (values, weights) = ([10, 13, 7, 8], [5, 6, 3, 4]);
/// let mut m = Model::new();
/// let take = m.add_vars(4, var().binary().name("take"));
/// m.add(constraint!((0..4).map(|i| weights[i] * take[i]).sum::<optimizam::Expression>() <= 10));
/// m.maximize((0..4).map(|i| values[i] * take[i]).sum::<optimizam::Expression>());
///
/// let result = m.solve(&Options::default())?;
/// assert_eq!(result.status(), Status::Complete);
/// assert_eq!(result.objective(), Some(21.0));
/// let best = result.best().unwrap();
/// assert_eq!((best[take[1]], best[take[3]]), (1.0, 1.0));
/// # Ok::<(), optimizam::Error>(())
/// ```
///
/// Without [`minimize`](Model::minimize) or [`maximize`](Model::maximize),
/// the model is a satisfaction problem: any solution will do.
#[derive(Clone, Debug)]
pub struct Model {
    pub(crate) id: u64,
    pub(crate) families: Vec<Family>,
    pub(crate) num_cols: usize,
    pub(crate) intervals: Vec<IntervalSpec>,
    pub(crate) items: Vec<Item>,
    pub(crate) sense: Sense,
    pub(crate) objective: Expression,
    pub(crate) all_solutions: bool,
    /// Bounds set with `set_bounds`, by column (over the spec's).
    pub(crate) bounds: BTreeMap<usize, (f64, f64)>,
    /// Labels of constraints that aren't `expr` constraints (for messages;
    /// `#k` otherwise).
    pub(crate) labels: BTreeMap<usize, String>,
}

/// The kind of a variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VarKind {
    Continuous,
    Integer,
    Binary,
}

/// What [`Model::add_var`] creates: a variable's kind, bounds, name, and
/// starting value. Made with [`var`] and chained methods.
///
/// ```
/// use optimizam::var;
///
/// let flow = var().min(0.0).name("flow");               // continuous, x >= 0
/// let count = var().integer().bounds(0, 10);           // integer in 0..=10
/// let open = var().binary();                           // 0 or 1
/// let size = var().integer().domain([0, 4, 8, 16]);    // one of these values
/// let x = var().bounds(-1.0, 1.0).start(0.5);          // a start for nonlinear solves
/// ```
#[derive(Clone, Debug)]
#[must_use = "a variable spec does nothing until it is passed to `Model::add_var`"]
pub struct VarSpec {
    pub(crate) kind: VarKind,
    pub(crate) lower: Option<f64>,
    pub(crate) upper: Option<f64>,
    pub(crate) name: Option<String>,
    pub(crate) start: Option<f64>,
    /// The allowed values, as sorted, disjoint inclusive ranges.
    pub(crate) domain: Option<Vec<(i64, i64)>>,
    /// The set of a symbolic variable (one with a string element).
    pub(crate) values: Option<Set>,
}

/// A continuous variable with no bounds, to refine with [`VarSpec`]'s
/// methods.
pub fn var() -> VarSpec {
    VarSpec { kind: VarKind::Continuous, lower: None, upper: None, name: None, start: None, domain: None, values: None }
}

impl VarSpec {
    /// A continuous (real-valued) variable. This is the default.
    pub fn continuous(mut self) -> Self {
        self.kind = VarKind::Continuous;
        self
    }

    /// An integer variable. The CP engine needs finite bounds on every
    /// variable.
    pub fn integer(mut self) -> Self {
        self.kind = VarKind::Integer;
        self
    }

    /// A binary variable: 0 or 1 (its bounds are ignored).
    pub fn binary(mut self) -> Self {
        self.kind = VarKind::Binary;
        self
    }

    /// The lower bound (default: none).
    pub fn min(mut self, lower: impl Into<f64>) -> Self {
        self.lower = Some(lower.into());
        self
    }

    /// The upper bound (default: none).
    pub fn max(mut self, upper: impl Into<f64>) -> Self {
        self.upper = Some(upper.into());
        self
    }

    /// Both bounds.
    pub fn bounds(self, lower: impl Into<f64>, upper: impl Into<f64>) -> Self {
        self.min(lower).max(upper)
    }

    /// The values an integer variable may take (instead of bounds), such as
    /// `[0, 4, 8, 16]`. Makes the variable integer.
    pub fn domain(self, values: impl IntoIterator<Item = i64>) -> Self {
        let mut values: Vec<i64> = values.into_iter().collect();
        values.sort_unstable();
        values.dedup();
        self.domain_ranges(crate::model::to_ranges(&values))
    }

    /// A domain given as sorted, disjoint inclusive ranges (as problem files
    /// write it), so that wide ranges aren't listed value by value. Makes
    /// the variable integer.
    pub(crate) fn domain_ranges(mut self, ranges: Vec<(i64, i64)>) -> Self {
        self.kind = VarKind::Integer;
        self.domain = Some(ranges);
        self
    }

    /// The elements of `set` as the variable's values. For a set of integers,
    /// that is [`domain`](VarSpec::domain) with them. A set with a string
    /// element makes the variable symbolic: its value is an element, read
    /// with [`Solution::element`](crate::Solution::element) and tested with
    /// [`Variable::is`] in [`Constraint::only_if`](crate::Constraint::only_if).
    /// Symbolic variables can't be used in arithmetic; besides literals, they
    /// can be [`all_different`](Model::all_different) (over the same set).
    ///
    /// ```
    /// use optimizam::{Model, Options, Set, var};
    ///
    /// let colors = Set::new("COLORS", ["red", "green", "blue"]);
    /// let mut m = Model::new();
    /// let a = m.add_var(var().values(&colors).name("a"));
    /// let b = m.add_var(var().values(&colors).name("b"));
    /// m.all_different([a, b]);
    /// let result = m.solve(&Options::default())?;
    /// let best = result.best().unwrap();
    /// assert_ne!(best.element(a), best.element(b));
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn values(mut self, set: &Set) -> Self {
        if set.has_strings() {
            self.kind = VarKind::Integer;
            self.domain = None;
            self.values = Some(set.clone());
            self
        } else {
            self.values = None;
            self.domain(set.elements().iter().map(|e| match e {
                Element::Int(i) => *i,
                Element::Str(_) => unreachable!(),
            }))
        }
    }

    /// The variable's name, used in results, error messages, and the
    /// exported problem: an identifier (letters, digits, and underscores, not
    /// starting with a digit). Unnamed variables are called `_x` followed by
    /// their position.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// A starting value for the nonlinear engine's local solves (it doesn't
    /// restrict the variable).
    pub fn start(mut self, value: impl Into<f64>) -> Self {
        self.start = Some(value.into());
        self
    }
}

/// Variables declared together: one by [`Model::add_var`], a numbered
/// family by [`Model::add_vars`], or a family over sets by
/// [`Model::add_family`].
#[derive(Clone, Debug)]
pub(crate) struct Family {
    /// The family's name (for `add_var`/`add_vars`: the spec's).
    pub name: Option<String>,
    /// One spec for every variable, or one per variable.
    pub specs: Vec<VarSpec>,
    pub first: usize,
    pub len: usize,
    pub shape: Shape,
}

#[derive(Clone, Debug)]
pub(crate) enum Shape {
    Scalar,
    /// `add_vars`: keyed 0, 1, ...
    Range,
    Sets(Vec<Set>),
}

impl Family {
    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("_x{}", self.first))
    }

    /// The spec of the family's `k`th variable.
    pub fn spec(&self, k: usize) -> &VarSpec {
        &self.specs[if self.specs.len() == 1 { 0 } else { k }]
    }

    /// Reported as a table (not a scalar).
    pub fn indexed(&self) -> bool {
        !matches!(self.shape, Shape::Scalar)
    }

    /// The key of the family's `k`th variable.
    pub fn key(&self, k: usize) -> Vec<Element> {
        match &self.shape {
            Shape::Scalar => vec![],
            Shape::Range => vec![Element::from(k)],
            Shape::Sets(sets) => key_of(sets, k),
        }
    }

    /// The index sets (`add_vars`: the range, named `_{name}_index`).
    pub fn sets(&self) -> Vec<Set> {
        match &self.shape {
            Shape::Scalar => vec![],
            Shape::Range => vec![Set::range(format!("_{}_index", self.name()), 0..self.len as i64)],
            Shape::Sets(sets) => sets.clone(),
        }
    }
}

/// An interval of time `[start, start + duration)`, for
/// [`Model::no_overlap`] and [`Model::cumulative`]. Created by
/// [`Model::interval`] or [`Model::optional_interval`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IntervalVar {
    pub(crate) model: u64,
    pub(crate) index: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct IntervalSpec {
    /// The label in messages (`_iv{k}` without one).
    pub name: Option<String>,
    pub start: Expression,
    pub duration: Expression,
    pub presence: Option<Variable>,
}

/// Identifies a constraint of a [`Model`], in the order they were added.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConstraintId(pub(crate) usize);

#[derive(Clone, Debug)]
pub(crate) enum Item {
    Expr(Constraint),
    AllDifferent(Vec<Expression>),
    Table {
        vars: Vec<Variable>,
        tuples: Vec<Vec<Element>>,
        forbidden: bool,
    },
    /// `value == items[index]`: each item with its key, one element per index
    /// expression (an integer, or for a symbolic index an element of its set).
    Element {
        index: Vec<Expression>,
        items: Vec<(Vec<Element>, Expression)>,
        value: Expression,
    },
    MinMax {
        value: Expression,
        items: Vec<Expression>,
        maximize: bool,
    },
    Abs {
        value: Expression,
        of: Expression,
    },
    NoOverlap(Vec<IntervalVar>),
    Cumulative {
        intervals: Vec<IntervalVar>,
        demands: Vec<Expression>,
        capacity: Expression,
    },
    /// Removed with `Model::remove` (kept so that later ids stay valid).
    Removed,
}

impl Default for Model {
    fn default() -> Self {
        Model::new()
    }
}

impl Model {
    /// An empty model.
    pub fn new() -> Model {
        Model {
            id: NEXT_MODEL_ID.fetch_add(1, Ordering::Relaxed),
            families: Vec::new(),
            num_cols: 0,
            intervals: Vec::new(),
            items: Vec::new(),
            sense: Sense::Satisfy,
            objective: Expression::default(),
            all_solutions: false,
            bounds: BTreeMap::new(),
            labels: BTreeMap::new(),
        }
    }

    /// Adds a variable.
    pub fn add_var(&mut self, spec: VarSpec) -> Variable {
        let name = spec.name.clone();
        self.push_family(name, vec![spec], 1, Shape::Scalar)[0]
    }

    /// Adds `n` variables with the same kind and bounds. With a name, they
    /// are reported as one variable indexed `0..n` (results show
    /// `"take": {"0": 1, "1": 0, ...}`, and
    /// [`Solution::get`](crate::Solution::get) finds `take[3]` as
    /// `get("take", &["3"])`).
    pub fn add_vars(&mut self, n: usize, spec: VarSpec) -> Vec<Variable> {
        let name = spec.name.clone();
        self.push_family(name, vec![spec], n, Shape::Range)
    }

    /// Adds a variable for every combination of elements of `sets` (one per
    /// element for one set), all made from `spec`, as one family named
    /// `name`: results report it as a table keyed by element, one level per
    /// set. See [`VarFamily`] for an example. (The spec's own name is
    /// ignored.)
    pub fn add_family(&mut self, name: impl Into<String>, sets: &[&Set], spec: VarSpec) -> VarFamily {
        self.family_from(name.into(), sets, vec![spec])
    }

    /// Like [`add_family`](Model::add_family), with each variable's spec
    /// from `spec(key)`, for bounds or starts that differ by element. Every
    /// spec must have the same kind and domain (checked when the model is
    /// solved or exported).
    ///
    /// ```
    /// use optimizam::{Model, Set, var};
    ///
    /// let plants = Set::new("PLANTS", ["north", "south"]);
    /// let capacity = |p: &str| if p == "north" { 40.0 } else { 25.0 };
    /// let mut m = Model::new();
    /// let make = m.add_family_with("make", &[&plants], |key| {
    ///     var().bounds(0.0, capacity(&key[0].to_string()))
    /// });
    /// # let _ = make;
    /// ```
    pub fn add_family_with(
        &mut self,
        name: impl Into<String>,
        sets: &[&Set],
        mut spec: impl FnMut(&[Element]) -> VarSpec,
    ) -> VarFamily {
        let owned: Vec<Set> = sets.iter().map(|&s| s.clone()).collect();
        let n = owned.iter().map(Set::len).product();
        let specs = (0..n).map(|k| spec(&key_of(&owned, k))).collect();
        self.family_from(name.into(), sets, specs)
    }

    pub(crate) fn family_from(&mut self, name: String, sets: &[&Set], specs: Vec<VarSpec>) -> VarFamily {
        let owned: Vec<Set> = sets.iter().map(|&s| s.clone()).collect();
        let n = owned.iter().map(Set::len).product();
        let shape = if owned.is_empty() { Shape::Scalar } else { Shape::Sets(owned.clone()) };
        let vars = self.push_family(Some(name.clone()), specs, n, shape);
        VarFamily::new(&name, owned, vars)
    }

    fn push_family(&mut self, name: Option<String>, specs: Vec<VarSpec>, n: usize, shape: Shape) -> Vec<Variable> {
        let first = self.num_cols;
        if n > 0 {
            self.families.push(Family { name, specs, first, len: n, shape });
        }
        self.num_cols += n;
        (first..first + n).map(|col| Variable { model: self.id, col }).collect()
    }

    /// The family named `name` (made by any of the `add_*` methods; one
    /// from [`add_var`](Model::add_var) has no index sets, and is indexed
    /// with `()`), or None.
    pub fn family(&self, name: &str) -> Option<VarFamily> {
        let f = self.families.iter().find(|f| f.name.as_deref() == Some(name))?;
        let vars = (f.first..f.first + f.len).map(|col| Variable { model: self.id, col }).collect();
        Some(VarFamily::new(name, f.sets(), vars))
    }

    /// The variable named `name`, made by [`add_var`](Model::add_var), or
    /// None.
    pub fn var_named(&self, name: &str) -> Option<Variable> {
        let f = self.families.iter().find(|f| f.name.as_deref() == Some(name) && !f.indexed())?;
        Some(Variable { model: self.id, col: f.first })
    }

    /// The first constraint named `name` (with
    /// [`Constraint::name`](crate::Constraint::name)) that hasn't been
    /// removed, or None.
    pub fn constraint_named(&self, name: &str) -> Option<ConstraintId> {
        self.items
            .iter()
            .position(|item| matches!(item, Item::Expr(c) if c.name.as_deref() == Some(name)))
            .map(ConstraintId)
    }

    /// Removes a constraint. Other constraints keep their ids; removing one
    /// twice does nothing.
    pub fn remove(&mut self, id: ConstraintId) {
        if let Some(item) = self.items.get_mut(id.0) {
            *item = Item::Removed;
        }
    }

    /// Changes a variable's bounds (replacing those of its spec; a binary
    /// variable's are intersected with 0..=1, a domain's applied to its
    /// values), for example to fix it: `set_bounds(x, 3, 3)`.
    ///
    /// # Panics
    ///
    /// If `v` belongs to another model.
    pub fn set_bounds(&mut self, v: Variable, lower: impl Into<f64>, upper: impl Into<f64>) {
        assert!(v.model == self.id && v.col < self.num_cols, "the variable belongs to another model");
        self.bounds.insert(v.col, (lower.into(), upper.into()));
    }

    /// The number of variables.
    pub fn num_vars(&self) -> usize {
        self.num_cols
    }

    fn push(&mut self, item: Item) -> ConstraintId {
        self.items.push(item);
        ConstraintId(self.items.len() - 1)
    }

    /// Adds a constraint, made with the [`constraint!`](crate::constraint)
    /// macro or [`Expression::leq`] and its siblings.
    pub fn add(&mut self, constraint: Constraint) -> ConstraintId {
        self.push(Item::Expr(constraint))
    }

    /// The family a column belongs to, and its position in it.
    pub(crate) fn family_of(&self, col: usize) -> (&Family, usize) {
        let k = self.families.partition_point(|f| f.first + f.len <= col);
        let f = &self.families[k];
        (f, col - f.first)
    }

    /// The items take pairwise different values. Items are integer-valued
    /// affine expressions (integer or binary variables with integer
    /// coefficients), or symbolic variables over one set.
    ///
    /// ```
    /// use optimizam::{Model, Options, var};
    ///
    /// // Eight queens: one per column, rows and both diagonals different.
    /// let mut m = Model::new();
    /// let row = m.add_vars(8, var().integer().bounds(0, 7).name("row"));
    /// m.all_different(&row);
    /// m.all_different((0..8).map(|c| row[c] + c as i32));
    /// m.all_different((0..8).map(|c| row[c] - c as i32));
    /// let result = m.solve(&Options::default())?;
    /// assert!(result.best().is_some());
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn all_different(&mut self, items: impl IntoIterator<Item: Into<Expression>>) -> ConstraintId {
        let items = items.into_iter().map(Into::into).collect();
        self.push(Item::AllDifferent(items))
    }

    /// The variables, in order, equal one of the tuples (integer
    /// variables).
    pub fn table(&mut self, vars: &[Variable], tuples: impl IntoIterator<Item: AsRef<[i64]>>) -> ConstraintId {
        let tuples = tuples.into_iter().map(|t| t.as_ref().iter().map(|&v| Element::Int(v)).collect()).collect();
        self.push(Item::Table { vars: vars.to_vec(), tuples, forbidden: false })
    }

    /// A table whose tuples hold elements (for symbolic variables: elements
    /// of their sets; a tuple naming an element outside a variable's values
    /// never matches).
    pub(crate) fn table_of(&mut self, vars: Vec<Variable>, tuples: Vec<Vec<Element>>, forbidden: bool) -> ConstraintId {
        self.push(Item::Table { vars, tuples, forbidden })
    }

    /// `element` with keyed items: `value` equals the item whose key matches
    /// the index expressions (see [`Item::Element`]).
    pub(crate) fn element_keyed(
        &mut self,
        index: Vec<Expression>,
        items: Vec<(Vec<Element>, Expression)>,
        value: Expression,
    ) -> ConstraintId {
        self.push(Item::Element { index, items, value })
    }

    /// Labels a constraint in messages.
    pub(crate) fn label(&mut self, id: ConstraintId, label: String) {
        self.labels.insert(id.0, label);
    }

    /// What the `i`th constraint is called in messages and conflicts: its
    /// name, its label, or `#k` for the model's k-th constraint.
    pub(crate) fn constraint_label(&self, i: usize) -> String {
        let name = match &self.items[i] {
            Item::Expr(c) => c.name.clone(),
            _ => None,
        };
        name.or_else(|| self.labels.get(&i).cloned()).unwrap_or_else(|| format!("#{}", i + 1))
    }

    /// A named interval (the name is its label in messages).
    pub(crate) fn interval_named(
        &mut self,
        name: String,
        start: Expression,
        duration: Expression,
        presence: Option<Variable>,
    ) -> IntervalVar {
        self.intervals.push(IntervalSpec { name: Some(name), start, duration, presence });
        IntervalVar { model: self.id, index: self.intervals.len() - 1 }
    }

    /// The variables, in order, equal none of the tuples.
    pub fn forbidden_table(
        &mut self,
        vars: &[Variable],
        tuples: impl IntoIterator<Item: AsRef<[i64]>>,
    ) -> ConstraintId {
        let tuples = tuples.into_iter().map(|t| t.as_ref().iter().map(|&v| Element::Int(v)).collect()).collect();
        self.push(Item::Table { vars: vars.to_vec(), tuples, forbidden: true })
    }

    /// `value == items[index]`, counting from 0: `index` is an
    /// integer-valued affine expression that takes values `0..items.len()`,
    /// and the items and `value` are affine expressions (items may contain
    /// variables).
    ///
    /// For a two-dimensional table, index it with `n * i + j`.
    pub fn element(
        &mut self,
        index: impl Into<Expression>,
        items: impl IntoIterator<Item: Into<Expression>>,
        value: impl Into<Expression>,
    ) -> ConstraintId {
        let items = items.into_iter().enumerate().map(|(k, e)| (vec![Element::from(k)], e.into())).collect();
        self.push(Item::Element { index: vec![index.into()], items, value: value.into() })
    }

    /// `value == min(items)`, over a non-empty list of affine expressions.
    pub fn min_of(
        &mut self,
        value: impl Into<Expression>,
        items: impl IntoIterator<Item: Into<Expression>>,
    ) -> ConstraintId {
        let items = items.into_iter().map(Into::into).collect();
        self.push(Item::MinMax { value: value.into(), items, maximize: false })
    }

    /// `value == max(items)`, over a non-empty list of affine expressions.
    pub fn max_of(
        &mut self,
        value: impl Into<Expression>,
        items: impl IntoIterator<Item: Into<Expression>>,
    ) -> ConstraintId {
        let items = items.into_iter().map(Into::into).collect();
        self.push(Item::MinMax { value: value.into(), items, maximize: true })
    }

    /// `value == |of|`, for affine expressions.
    pub fn abs_of(&mut self, value: impl Into<Expression>, of: impl Into<Expression>) -> ConstraintId {
        self.push(Item::Abs { value: value.into(), of: of.into() })
    }

    /// An interval `[start, start + duration)`. Start and duration are
    /// integer-valued affine expressions (constants, or integer variables
    /// with integer coefficients); a variable duration is at least 0.
    pub fn interval(&mut self, start: impl Into<Expression>, duration: impl Into<Expression>) -> IntervalVar {
        self.intervals.push(IntervalSpec {
            name: None,
            start: start.into(),
            duration: duration.into(),
            presence: None,
        });
        IntervalVar { model: self.id, index: self.intervals.len() - 1 }
    }

    /// An interval that is present only when the binary variable `presence`
    /// is 1. Absent intervals take no part in
    /// [`no_overlap`](Model::no_overlap) and [`cumulative`](Model::cumulative).
    pub fn optional_interval(
        &mut self,
        start: impl Into<Expression>,
        duration: impl Into<Expression>,
        presence: Variable,
    ) -> IntervalVar {
        let spec =
            IntervalSpec { name: None, start: start.into(), duration: duration.into(), presence: Some(presence) };
        self.intervals.push(spec);
        IntervalVar { model: self.id, index: self.intervals.len() - 1 }
    }

    /// The present intervals don't overlap (one machine runs one task at a
    /// time). Two zero-length intervals may share a point.
    ///
    /// ```
    /// use optimizam::{Model, Options, var};
    ///
    /// // Three jobs on one machine; finish them all as early as possible.
    /// let durations = [3, 2, 4];
    /// let mut m = Model::new();
    /// let start = m.add_vars(3, var().integer().bounds(0, 20).name("start"));
    /// let end = m.add_var(var().integer().bounds(0, 20).name("end"));
    /// let jobs: Vec<_> = (0..3).map(|j| m.interval(start[j], durations[j])).collect();
    /// m.no_overlap(&jobs);
    /// m.max_of(end, (0..3).map(|j| start[j] + durations[j]));
    /// m.minimize(end);
    /// assert_eq!(m.solve(&Options::default())?.objective(), Some(9.0));
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn no_overlap(&mut self, intervals: &[IntervalVar]) -> ConstraintId {
        self.push(Item::NoOverlap(intervals.to_vec()))
    }

    /// At every time, the demands of the present intervals running then add
    /// up to at most `capacity` (affine expressions; a variable demand is an
    /// integer variable).
    pub fn cumulative(
        &mut self,
        intervals: &[IntervalVar],
        demands: impl IntoIterator<Item: Into<Expression>>,
        capacity: impl Into<Expression>,
    ) -> ConstraintId {
        let demands = demands.into_iter().map(Into::into).collect();
        self.push(Item::Cumulative { intervals: intervals.to_vec(), demands, capacity: capacity.into() })
    }

    /// Minimize `objective` (replacing any earlier objective).
    pub fn minimize(&mut self, objective: impl Into<Expression>) {
        self.sense = Sense::Minimize;
        self.objective = objective.into();
    }

    /// Maximize `objective` (replacing any earlier objective).
    pub fn maximize(&mut self, objective: impl Into<Expression>) {
        self.sense = Sense::Maximize;
        self.objective = objective.into();
    }

    /// Drop the objective: any solution will do.
    pub fn satisfy(&mut self) {
        self.sense = Sense::Satisfy;
        self.objective = Expression::default();
    }

    /// Find every solution (every optimal one, when there is an objective)
    /// instead of one. Every variable must then be integer or binary.
    pub fn all_solutions(&mut self, on: bool) {
        self.all_solutions = on;
    }

    /// Solves the model.
    ///
    /// An infeasible model, or one whose time ran out, is a successful solve
    /// with that [`Status`](crate::Status); errors are for models that are
    /// malformed or that the solver can't take.
    pub fn solve(&self, options: &Options) -> Result<SolveResult, Error> {
        super::solve_with(options, Some(self.id), || Ok(Cow::Borrowed(self)))
    }

    /// Why the model is infeasible: a set of its constraints that can't hold
    /// together (see [`Conflict`]), or None if the model has a solution, or
    /// if neither could be shown within the options' time limit.
    ///
    /// The objective plays no part. The search solves the model, then again
    /// without each group of constraints in turn (constraints named alike
    /// but for a subscript, such as `capacity[north]` and `capacity[south]`,
    /// are tried together first), so it can take many solves; with a time
    /// limit it stops there and reports what it has, which may not be
    /// minimal. [`Options::explain`] does the same after an infeasible solve.
    pub fn conflict(&self, options: &Options) -> Result<Option<Conflict>, Error> {
        let deadline = options.deadline_from(Instant::now());
        super::explain::conflict(self, options, deadline, false)
    }

    /// The model in the JSON problem format (format 1.2, or 1.3 when it
    /// needs string literals for elements), to solve with the
    /// `optimizam` command or to attach to a bug report. Each variable
    /// keeps its name; intervals or `element` items that the format can't
    /// express directly get auxiliary variables, whose names start with `_`.
    pub fn to_json(&self) -> Result<String, Error> {
        super::export::to_json(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Options, Problem};
    use crate::{Status, constraint};

    fn solve(m: &Model) -> crate::api::SolveResult {
        m.solve(&Options::default()).unwrap()
    }

    /// Solves the model and its export, which must agree.
    fn agree(m: &Model) -> crate::api::SolveResult {
        let direct = solve(m);
        let json = m.to_json().unwrap();
        let exported = Problem::from_json(&json).unwrap().solve(&Options::default()).unwrap();
        assert_eq!((exported.status(), exported.objective()), (direct.status(), direct.objective()), "{json}");
        direct
    }

    #[test]
    fn keyed_element_over_symbolic_and_integer_indexes() {
        // cost[machine, size]: machine symbolic, size an integer key 10/20.
        let machines = Set::new("MACHINES", ["lathe", "mill"]);
        let mut m = Model::new();
        let machine = m.add_var(var().values(&machines).name("machine"));
        let size = m.add_var(var().domain([10, 20]).name("size"));
        let cost = m.add_var(var().integer().bounds(0, 100).name("cost"));
        let key = |a: &str, b: i64| vec![Element::from(a), Element::from(b)];
        let items = vec![
            (key("lathe", 10), Expression::from(7.0)),
            (key("lathe", 20), Expression::from(9.0)),
            (key("mill", 10), Expression::from(5.0)),
            (key("mill", 20), Expression::from(12.0)),
        ];
        m.element_keyed(vec![machine.into(), size.into()], items.clone(), cost.into());
        m.add(constraint!(size >= 15));
        m.minimize(cost);
        let r = agree(&m);
        assert_eq!(r.objective(), Some(9.0));
        assert_eq!(r.best().unwrap().element(machine), Some(Element::from("lathe")));
        // A key missing from the product: (mill, 20) gone, so a condition.
        let mut sparse = Model::new();
        let machine = sparse.add_var(var().values(&machines).name("machine"));
        let size = sparse.add_var(var().domain([10, 20]).name("size"));
        let cost = sparse.add_var(var().integer().bounds(0, 100).name("cost"));
        let x = sparse.add_var(var().integer().bounds(0, 3).name("x"));
        let mut items = items[..3].to_vec();
        items[0].1 = x + 4; // a variable item
        sparse.element_keyed(vec![machine.into(), size.into()], items, cost.into());
        sparse.maximize(cost);
        let r = agree(&sparse);
        assert_eq!(r.objective(), Some(9.0));
        let json = sparse.to_json().unwrap();
        assert!(json.contains("_elementpresent") && json.contains("'lathe'"), "{json}");
    }

    #[test]
    fn tables_of_elements_and_equals() {
        let colors = Set::new("COLORS", ["red", "green", "blue"]);
        let mut m = Model::new();
        let a = m.add_var(var().values(&colors).name("a"));
        let n = m.add_var(var().integer().bounds(0, 5).name("n"));
        let e = |s: &str, k: i64| vec![Element::from(s), Element::from(k)];
        // (purple, 5) names an element outside the set: it never matches.
        let id = m.table_of(vec![a, n], vec![e("red", 1), e("green", 2), e("blue", 3), e("purple", 5)], false);
        m.label(id, "palette".into());
        m.add(a.equals("green"));
        m.maximize(n);
        let r = agree(&m);
        assert_eq!(r.objective(), Some(2.0));
        // Outside the set, equals is infeasible.
        let mut bad = Model::new();
        let a = bad.add_var(var().values(&colors).name("a"));
        bad.add(a.equals("purple"));
        assert_eq!(agree(&bad).status(), Status::Infeasible);
        // On an integer variable, equals is ==.
        let mut int = Model::new();
        let n = int.add_var(var().integer().bounds(0, 5).name("n"));
        int.add(n.equals(4));
        int.maximize(n);
        assert_eq!(agree(&int).objective(), Some(4.0));
        // A numeric table with a string entry is an error naming the label.
        let mut wrong = Model::new();
        let n = wrong.add_var(var().integer().bounds(0, 5).name("n"));
        let id = wrong.table_of(vec![n], vec![vec![Element::from("x")]], false);
        wrong.label(id, "tbl[2]".into());
        let err = wrong.solve(&Options::default()).unwrap_err();
        assert!(err.message().contains("constraint tbl[2]") && err.message().contains("numeric"), "{}", err.message());
    }

    #[test]
    fn named_intervals_and_wide_domains() {
        let mut m = Model::new();
        let start = m.add_var(var().integer().bounds(0, 10).name("s"));
        let iv = m.interval_named("op[1,2]".into(), start.into(), Expression::from(-1.0), None);
        let _ = iv;
        let err = m.solve(&Options::default()).unwrap_err();
        assert!(err.message().contains("interval op[1,2]: the duration is negative"), "{}", err.message());
        // A domain of a billion values stays two ranges.
        let mut w = Model::new();
        let x = w.add_var(VarSpec::domain_ranges(var(), vec![(1, 3), (7, 1_000_000_000)]).name("x"));
        w.minimize(x);
        w.add(constraint!(x >= 4));
        assert_eq!(agree(&w).objective(), Some(7.0));
        assert!(w.to_json().unwrap().contains("[[1, 3], [7, 1000000000]]"));
    }
}
