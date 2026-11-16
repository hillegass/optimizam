//! The model builder: variables, constraints, and objectives in Rust.

use super::expression::{Constraint, Expression, Number, Variable};
use super::family::{ConstraintFamily, Element, Key, Set, VarFamily, key_of};
use super::{Conflict, Error, Options, SolveResult};
use crate::model::Sense;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT_MODEL_ID: AtomicU64 = AtomicU64::new(1);

/// An optimization model built in Rust: variables, constraints, and an
/// objective.
///
/// ```
/// use optimizam::{Model, Options, Status, constraint, sum, var};
///
/// // A small knapsack: pick items of the most value within a weight of 10.
/// let (values, weights) = ([10, 13, 7, 8], [5, 6, 3, 4]);
/// let mut m = Model::new();
/// let take = m.add_vars(4, var().binary().name("take"));
/// m.add(constraint!(sum((0..4).map(|i| weights[i] * take[i])) <= 10));
/// m.maximize(sum((0..4).map(|i| values[i] * take[i])));
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
    /// What the model is called and what it is, for readers
    /// ([`Model::set_model_name`], [`Model::set_description`]).
    pub(crate) name: Option<Arc<str>>,
    pub(crate) description: Option<Arc<str>>,
    pub(crate) families: Vec<Family>,
    pub(crate) num_cols: usize,
    pub(crate) intervals: Vec<IntervalSpec>,
    pub(crate) items: Vec<Item>,
    pub(crate) sense: Sense,
    pub(crate) objective: Expression,
    pub(crate) all_solutions: bool,
    /// Find at most this many solutions (`solution_limit`).
    pub(crate) solution_limit: Option<usize>,
    /// Bounds set with `set_bounds`, by column (over the spec's).
    pub(crate) bounds: BTreeMap<usize, (f64, f64)>,
    /// Starting values set with `set_start` and `start_from`, by column
    /// (over the spec's).
    pub(crate) starts: BTreeMap<usize, f64>,
    /// What each constraint is called, beside `items`: None for one without
    /// a name, which goes by its place (`constraints[k]`).
    pub(crate) names: Vec<Option<Name>>,
    /// What reading a problem file left ([`Model::from_json`]): None for a
    /// model built in Rust.
    pub(crate) source: Option<Arc<Source>>,
}

/// What a model read from a problem file keeps of the file.
#[derive(Debug)]
pub(crate) struct Source {
    /// What the file says that it probably doesn't mean (`lint.rs`, and a
    /// `forall` that matched nothing).
    pub(crate) warnings: Vec<super::Warning>,
    /// The constraints' names in the file's order, so that one whose
    /// `forall` matched nothing is still listed by [`Model::check`].
    pub(crate) declared: Vec<String>,
}

/// What a constraint is called: the family it belongs to, and its key
/// there (empty for a constraint named by itself). In messages and results
/// it is written `family` or `family[north,3]`.
#[derive(Clone, Debug)]
pub(crate) struct Name {
    pub family: Arc<str>,
    pub key: Vec<Element>,
}

impl Name {
    /// `family`, or `family[north,3]`.
    pub fn label(&self) -> String {
        label_of(&self.family, &self.key)
    }
}

/// What the member at `key` of the constraints named `family` is called:
/// `family`, or `family[north,3]`.
pub(crate) fn label_of(family: &str, key: &[Element]) -> String {
    use std::fmt::Write;
    let mut label = String::with_capacity(family.len() + 8 * key.len());
    label.push_str(family);
    for (k, e) in key.iter().enumerate() {
        label.push(if k == 0 { '[' } else { ',' });
        write!(label, "{e}").unwrap();
    }
    if !key.is_empty() {
        label.push(']');
    }
    label
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
    pub fn min(mut self, lower: impl Number) -> Self {
        self.lower = Some(lower.to_f64());
        self
    }

    /// The upper bound (default: none).
    pub fn max(mut self, upper: impl Number) -> Self {
        self.upper = Some(upper.to_f64());
        self
    }

    /// Both bounds.
    pub fn bounds(self, lower: impl Number, upper: impl Number) -> Self {
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

    /// A starting value: where to look first. It doesn't restrict the
    /// variable, and a start that is no solution costs little.
    ///
    /// What it does depends on the engine. The MILP engine looks for a
    /// solution with the integer and binary variables at their starts (the
    /// others, and variables without a start, are free in that search),
    /// and begins from it if there is one: starts that are a solution are
    /// the first incumbent. The CP engine branches on the variables with a
    /// start first, toward their starts, until its first solution. The
    /// nonlinear engine begins its local solves at the starts. A linear
    /// program has no use for them.
    ///
    /// [`Model::start_from`](crate::Model::start_from) gives every variable
    /// the value a solution has for it.
    pub fn start(mut self, value: impl Number) -> Self {
        self.start = Some(value.to_f64());
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
            name: None,
            description: None,
            families: Vec::new(),
            num_cols: 0,
            intervals: Vec::new(),
            items: Vec::new(),
            sense: Sense::Satisfy,
            objective: Expression::default(),
            all_solutions: false,
            solution_limit: None,
            bounds: BTreeMap::new(),
            starts: BTreeMap::new(),
            names: Vec::new(),
            source: None,
        }
    }

    /// What the model is called, if it has a name: a problem file's
    /// top-level `name`, or what [`Model::set_model_name`] gave it.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// What the model says it is: a problem file's top-level `description`,
    /// or what [`Model::set_description`] gave it.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Names the model. The name changes nothing about what is solved: it is
    /// written in the problem file ([`Model::to_json`]) and in result files,
    /// so that a result says what it answers. ([`Model::set_name`] names a
    /// constraint.)
    ///
    /// ```
    /// use optimizam::{Model, Options, constraint, var};
    ///
    /// let mut m = Model::new();
    /// m.set_model_name("two shifts");
    /// m.set_description("How many people to put on each shift.");
    /// let day = m.add_var(var().integer().bounds(0, 9).name("day"));
    /// m.add(constraint!(day >= 3));
    /// m.minimize(day);
    /// assert_eq!(m.name(), Some("two shifts"));
    /// assert!(m.to_json()?.contains(r#""name": "two shifts""#));
    /// assert!(m.solve(&Options::default())?.to_json().contains(r#""name": "two shifts""#));
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn set_model_name(&mut self, name: impl Into<String>) {
        self.name = Some(name.into().into());
    }

    /// Says what the model is, for whoever reads the problem file
    /// ([`Model::to_json`] writes it as the top-level `description`).
    pub fn set_description(&mut self, description: impl Into<String>) {
        self.description = Some(description.into().into());
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
    ///     var().bounds(0, capacity(key[0].as_str().unwrap()))
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

    /// The first constraint named `name` that hasn't been removed, or None.
    /// A member of a family is named with its key, as results and messages
    /// write it (`capacity[north]`); [`constraint_family`](Model::constraint_family)
    /// finds it by the key itself.
    pub fn constraint_named(&self, name: &str) -> Option<ConstraintId> {
        let named = |n: &Name| match n.key.is_empty() {
            true => *n.family == *name,
            false => name.strip_prefix(&*n.family).is_some_and(|key| key.starts_with('[')) && n.label() == name,
        };
        let live = |i: &usize| !matches!(self.items[*i], Item::Removed);
        (0..self.items.len()).filter(live).find(|&i| self.names[i].as_ref().is_some_and(named)).map(ConstraintId)
    }

    /// The constraints that go by the name `family` (those added with
    /// [`add_indexed`](Model::add_indexed), the instances of a problem
    /// file's indexed constraint, or one constraint named so by itself) and
    /// haven't been removed, with their keys. None if there are none.
    pub fn constraint_family(&self, family: &str) -> Option<ConstraintFamily> {
        let member = |i: usize| match (&self.items[i], &self.names[i]) {
            (Item::Removed, _) | (_, None) => None,
            (_, Some(name)) => (*name.family == *family).then(|| (name.key.clone(), ConstraintId(i))),
        };
        let members: Vec<_> = (0..self.items.len()).filter_map(member).collect();
        (!members.is_empty()).then(|| ConstraintFamily::new(family, members))
    }

    /// Names a constraint of any kind (replacing the name it had), for
    /// messages, conflicts, and results, and for
    /// [`constraint_named`](Model::constraint_named).
    ///
    /// ```
    /// use optimizam::{Model, var};
    ///
    /// let mut m = Model::new();
    /// let x = m.add_vars(3, var().integer().bounds(1, 3).name("x"));
    /// let distinct = m.all_different(&x);
    /// m.set_name(distinct, "distinct");
    /// assert_eq!(m.constraint_named("distinct"), Some(distinct));
    /// ```
    ///
    /// # Panics
    ///
    /// If the model has no such constraint.
    pub fn set_name(&mut self, id: ConstraintId, name: impl Into<String>) {
        self.set_family(id, &name.into(), ());
    }

    /// Makes a constraint of any kind the member at `key` of the constraints
    /// named `family` (replacing the name it had), as
    /// [`add_indexed`](Model::add_indexed) does for a comparison.
    ///
    /// # Panics
    ///
    /// If the model has no such constraint, or an integer of the key is too
    /// large for an `i64`.
    pub fn set_family(&mut self, id: ConstraintId, family: &str, key: impl Key) {
        let key = key.elements().expect("a key's integer is too large for an i64");
        self.name_item(id, family.into(), key);
    }

    pub(crate) fn name_item(&mut self, id: ConstraintId, family: Arc<str>, key: Vec<Element>) {
        self.names[id.0] = Some(Name { family, key });
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
    pub fn set_bounds(&mut self, v: Variable, lower: impl Number, upper: impl Number) {
        assert!(v.model == self.id && v.col < self.num_cols, "the variable belongs to another model");
        self.bounds.insert(v.col, (lower.to_f64(), upper.to_f64()));
    }

    /// The number of variables.
    pub fn num_vars(&self) -> usize {
        self.num_cols
    }

    fn push(&mut self, item: Item) -> ConstraintId {
        self.items.push(item);
        self.names.push(None);
        ConstraintId(self.items.len() - 1)
    }

    /// Adds a constraint, made with the [`constraint!`](crate::constraint)
    /// macro or [`Expression::leq`] and its siblings.
    pub fn add(&mut self, constraint: impl Into<Constraint>) -> ConstraintId {
        // The model keeps every kind's name in one place.
        let mut constraint = constraint.into();
        let name = constraint.name.take();
        let id = self.push(Item::Expr(constraint));
        if let Some(name) = name {
            self.name_item(id, name.into(), Vec::new());
        }
        id
    }

    /// Adds a constraint as the member at `key` of the constraints named
    /// `family`: what a problem file's indexed constraint comes to, one
    /// member for each value of its `forall`. Members are written
    /// `family[key]` in messages, conflicts, and results;
    /// [`constraint_family`](Model::constraint_family) finds them again, and
    /// [`conflict`](Model::conflict) tries removing a family as a whole
    /// before its members one by one. Any name the constraint has itself is
    /// dropped.
    ///
    /// ```
    /// use optimizam::{Model, Options, Set, constraint, sum, var};
    ///
    /// let plants = Set::new("PLANTS", ["north", "south"]);
    /// let hours = Set::range("HOURS", 1..=3);
    /// let mut m = Model::new();
    /// let on = m.add_family("on", &[&plants, &hours], var().binary());
    /// for h in 1..=3 {
    ///     m.add_indexed("cover", h, constraint!(on[("north", h)] + on[("south", h)] >= 1));
    /// }
    /// m.minimize(sum(on.vars()));
    /// assert_eq!(m.solve(&Options::default())?.objective(), Some(3.0));
    ///
    /// let cover = m.constraint_family("cover").unwrap();
    /// assert_eq!(m.constraint_named("cover[2]"), Some(cover[2]));
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    ///
    /// # Panics
    ///
    /// If an integer of the key is too large for an `i64`.
    pub fn add_indexed(&mut self, family: &str, key: impl Key, constraint: impl Into<Constraint>) -> ConstraintId {
        let id = self.add(constraint);
        self.set_family(id, family, key);
        id
    }

    /// What the model comes to, without solving it: how many variables and
    /// constraints each name stands for, the size of the solver's model, and
    /// the engine that would solve it with these options, and its
    /// [`warnings`](Model::warnings). An error if the model is one that
    /// solving rejects. For a model read from a problem file, a constraint
    /// whose `forall` matched nothing is listed with 0 instances.
    ///
    /// ```
    /// use optimizam::{Engine, Model, Options, constraint, var};
    /// # fn main() -> Result<(), optimizam::Error> {
    /// let mut m = Model::new();
    /// let x = m.add_vars(3, var().integer().bounds(0, 5).name("x"));
    /// m.all_different(&x);
    /// m.add(constraint!(x[0] + x[1] <= 4).name("few"));
    /// let check = m.check(&Options::default())?;
    /// assert_eq!(check.variables(), [("x".to_string(), 3)]);
    /// assert_eq!(check.constraints().len(), 2);
    /// assert_eq!(check.engine(), Some(Engine::Cp));
    /// # Ok(())
    /// # }
    /// ```
    pub fn check(&self, options: &Options) -> Result<super::Check, Error> {
        let declared = self.source.as_ref().map(|s| s.declared.clone());
        super::check::check(self, options, self.warnings(), declared)
    }

    /// What the model holds that it probably doesn't mean. None of these
    /// stops a solve; the `optimizam` command-line tool prints them to
    /// stderr.
    ///
    /// For a model read from a problem file ([`Model::from_json`]), what
    /// the file says:
    ///
    /// - A `forall` that matches nothing (its constraint doesn't exist).
    /// - An index of a `sum`, a list, or a `forall` that hides an outer
    ///   index, a parameter, or a variable of the same name.
    /// - Two constraints with one name.
    /// - A parameter that nothing uses; a variable in no constraint and not
    ///   in the objective.
    ///
    /// And for every model, as it is now: a variable whose bounds leave it
    /// no value, or a constraint that can't hold within its variables'
    /// bounds, so that the problem is infeasible before any search.
    ///
    /// ```
    /// use optimizam::Model;
    ///
    /// let model = Model::from_json(r#"{
    ///     "version": "1.3",
    ///     "sets": { "T": { "from": 1, "to": 3 } },
    ///     "parameters": { "bonus": 5 },
    ///     "variables": [{ "name": "y", "type": "binary", "index": ["T"] }],
    ///     "constraints": [{ "name": "late", "forall": "t in T: t > 3", "expr": "y[t] == 0" }],
    ///     "solve": { "method": "maximize", "objective": "sum(t in T) y[t]" }
    /// }"#)?;
    /// assert_eq!(model.warnings(), [
    ///     "constraint late: its forall (\"t in T: t > 3\") matches nothing, so it adds no constraints",
    ///     "parameter bonus is never used",
    /// ]);
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn warnings(&self) -> Vec<super::Warning> {
        let mut warnings = self.source.as_ref().map(|s| s.warnings.clone()).unwrap_or_default();
        warnings.extend(super::explain::obvious_warnings(self));
        warnings
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
    /// m.all_different((0..8).map(|c| row[c] + c));
    /// m.all_different((0..8).map(|c| row[c] - c));
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

    /// What the `i`th constraint is called in messages, conflicts, and
    /// results: its name (with its key, `capacity[north]`), or its place,
    /// `constraints[i]` (counting from 0, as its id does and as a problem
    /// file's unnamed constraints are called).
    pub(crate) fn constraint_label(&self, i: usize) -> String {
        self.names[i].as_ref().map_or_else(|| format!("constraints[{i}]"), Name::label)
    }

    /// The name of the family the `i`th constraint belongs to, or None for
    /// one without a name (which is a family by itself).
    pub(crate) fn constraint_family_of(&self, i: usize) -> Option<&str> {
        self.names[i].as_ref().map(|name| &*name.family)
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
    /// use optimizam::{Model, Options, max, var};
    ///
    /// // Three jobs on one machine; finish them all as early as possible.
    /// let durations = [3, 2, 4];
    /// let mut m = Model::new();
    /// let start = m.add_vars(3, var().integer().bounds(0, 20).name("start"));
    /// let jobs: Vec<_> = (0..3).map(|j| m.interval(start[j], durations[j])).collect();
    /// m.no_overlap(&jobs);
    /// m.minimize(max((0..3).map(|j| start[j] + durations[j])));
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
    /// Replaces a [`solution_limit`](Model::solution_limit).
    pub fn all_solutions(&mut self, on: bool) {
        self.all_solutions = on;
        self.solution_limit = None;
    }

    /// Find up to `n` solutions (optimal ones, when there is an objective),
    /// instead of one or all of them. Every variable must then be integer
    /// or binary.
    ///
    /// If there are no more than `n`, the result has them all and its status
    /// is [`Status::Complete`](crate::Status::Complete). Otherwise the
    /// search stops at the `n`th: the status is
    /// [`Status::Incomplete`](crate::Status::Incomplete) and the stop reason
    /// [`StopReason::SolutionLimit`](crate::StopReason::SolutionLimit). So
    /// two is enough to learn whether a solution is the only one:
    ///
    /// ```
    /// use optimizam::{Model, Options, Status, constraint, var};
    ///
    /// let mut m = Model::new();
    /// let x = m.add_vars(3, var().integer().bounds(1, 3));
    /// m.all_different(&x);
    /// m.add(constraint!(x[0] <= x[1] - 1));
    /// m.solution_limit(2);
    /// let two = m.solve(&Options::default())?;
    /// assert_eq!((two.status(), two.solutions().len()), (Status::Incomplete, 2));
    ///
    /// m.add(constraint!(x[1] <= x[2] - 1));
    /// let only = m.solve(&Options::default())?;
    /// assert_eq!((only.status(), only.solutions().len()), (Status::Complete, 1));
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    ///
    /// A limit of one differs from the default in that it stops an
    /// enumeration: one solution found and no proof that it is the only one
    /// is `Incomplete`. Zero is an error when the model is solved.
    pub fn solution_limit(&mut self, n: usize) {
        self.all_solutions = false;
        self.solution_limit = Some(n);
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
    /// without each group of constraints in turn (the members of a family,
    /// such as `capacity[north]` and `capacity[south]`, are tried together
    /// first: see [`add_indexed`](Model::add_indexed)), so it can take many
    /// solves; with a time
    /// limit it stops there and reports what it has, which may not be
    /// minimal. [`Options::explain`] does the same after an infeasible solve.
    pub fn conflict(&self, options: &Options) -> Result<Option<Conflict>, Error> {
        let deadline = options.deadline_from(Instant::now());
        super::explain::conflict(self, options, deadline, false)
    }

    /// The model in the JSON problem format (format 1.3), to solve with the
    /// `optimizam` command-line tool or to attach to a bug report. Each variable
    /// keeps its name; intervals or `element` items that the format can't
    /// express directly get auxiliary variables, whose names start with `_`.
    pub fn to_json(&self) -> Result<String, Error> {
        super::export::to_json(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Model, Options};
    use crate::constraint;

    fn solve(m: &Model) -> crate::api::SolveResult {
        m.solve(&Options::default()).unwrap()
    }

    /// Solves the model and its export, which must agree.
    fn agree(m: &Model) -> crate::api::SolveResult {
        let direct = solve(m);
        let json = m.to_json().unwrap();
        let exported = Model::from_json(&json).unwrap().solve(&Options::default()).unwrap();
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
        m.set_name(id, "palette");
        m.add(a.is("green"));
        m.maximize(n);
        let r = agree(&m);
        assert_eq!(r.objective(), Some(2.0));
        // Outside the set, a literal is an error: the element is misspelled.
        let mut bad = Model::new();
        let a = bad.add_var(var().values(&colors).name("a"));
        bad.add(Constraint::from(a.is("purple")).name("pick"));
        let err = bad.solve(&Options::default()).unwrap_err();
        let expected = r#"constraint pick: "purple" isn't one of the values of a ("red", "green", "blue")"#;
        assert_eq!(err.message(), expected);
        assert_eq!(bad.to_json().unwrap_err().message(), expected);
        // On an integer variable, a literal is == (or !=).
        let mut int = Model::new();
        let n = int.add_var(var().integer().bounds(0, 5).name("n"));
        int.add(n.is(4));
        int.maximize(n);
        assert_eq!(agree(&int).objective(), Some(4.0));
        // A numeric table with a string entry is an error naming the label.
        let mut wrong = Model::new();
        let n = wrong.add_var(var().integer().bounds(0, 5).name("n"));
        let id = wrong.table_of(vec![n], vec![vec![Element::from("x")]], false);
        wrong.set_family(id, "tbl", 2);
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
