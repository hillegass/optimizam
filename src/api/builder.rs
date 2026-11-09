//! The model builder: variables, constraints, and objectives in Rust.

use super::expression::{Constraint, Expression, Variable};
use super::{Error, Options, SolveResult};
use crate::model::Sense;
use std::sync::atomic::{AtomicU64, Ordering};

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
    pub(crate) domain: Option<Vec<i64>>,
}

/// A continuous variable with no bounds, to refine with [`VarSpec`]'s
/// methods.
pub fn var() -> VarSpec {
    VarSpec { kind: VarKind::Continuous, lower: None, upper: None, name: None, start: None, domain: None }
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
    pub fn domain(mut self, values: impl IntoIterator<Item = i64>) -> Self {
        let mut values: Vec<i64> = values.into_iter().collect();
        values.sort_unstable();
        values.dedup();
        self.kind = VarKind::Integer;
        self.domain = Some(values);
        self
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

/// Variables declared together: one by [`Model::add_var`], or a numbered
/// family by [`Model::add_vars`].
#[derive(Clone, Debug)]
pub(crate) struct Family {
    pub spec: VarSpec,
    pub first: usize,
    pub len: usize,
    /// Declared with `add_vars`: reported as a table keyed 0, 1, ...
    pub indexed: bool,
}

impl Family {
    pub fn name(&self) -> String {
        self.spec.name.clone().unwrap_or_else(|| format!("_x{}", self.first))
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
    Table { vars: Vec<Variable>, tuples: Vec<Vec<i64>>, forbidden: bool },
    Element { index: Expression, items: Vec<Expression>, value: Expression },
    MinMax { value: Expression, items: Vec<Expression>, maximize: bool },
    Abs { value: Expression, of: Expression },
    NoOverlap(Vec<IntervalVar>),
    Cumulative { intervals: Vec<IntervalVar>, demands: Vec<Expression>, capacity: Expression },
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
        }
    }

    /// Adds a variable.
    pub fn add_var(&mut self, spec: VarSpec) -> Variable {
        self.add_family(spec, 1, false)[0]
    }

    /// Adds `n` variables with the same kind and bounds. With a name, they
    /// are reported as one variable indexed `0..n` (results show
    /// `"take": {"0": 1, "1": 0, ...}`, and
    /// [`Solution::get`](crate::Solution::get) finds `take[3]` as
    /// `get("take", &["3"])`).
    pub fn add_vars(&mut self, n: usize, spec: VarSpec) -> Vec<Variable> {
        self.add_family(spec, n, true)
    }

    fn add_family(&mut self, spec: VarSpec, n: usize, indexed: bool) -> Vec<Variable> {
        let first = self.num_cols;
        if n > 0 {
            self.families.push(Family { spec, first, len: n, indexed });
        }
        self.num_cols += n;
        (first..first + n).map(|col| Variable { model: self.id, col }).collect()
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

    /// The items take pairwise different values. Items are integer-valued
    /// affine expressions (integer or binary variables with integer
    /// coefficients).
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
        let tuples = tuples.into_iter().map(|t| t.as_ref().to_vec()).collect();
        self.push(Item::Table { vars: vars.to_vec(), tuples, forbidden: false })
    }

    /// The variables, in order, equal none of the tuples.
    pub fn forbidden_table(
        &mut self,
        vars: &[Variable],
        tuples: impl IntoIterator<Item: AsRef<[i64]>>,
    ) -> ConstraintId {
        let tuples = tuples.into_iter().map(|t| t.as_ref().to_vec()).collect();
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
        let items = items.into_iter().map(Into::into).collect();
        self.push(Item::Element { index: index.into(), items, value: value.into() })
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
        self.intervals.push(IntervalSpec { start: start.into(), duration: duration.into(), presence: None });
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
        let spec = IntervalSpec { start: start.into(), duration: duration.into(), presence: Some(presence) };
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
        super::solve_with(options, Some(self.id), || super::lower::lower(self))
    }

    /// The model in the JSON problem format (format 1.2), to solve with the
    /// `optimizam` command or to attach to a bug report. Each variable
    /// keeps its name; intervals or `element` items that the format can't
    /// express directly get auxiliary variables, whose names start with `_`.
    pub fn to_json(&self) -> Result<String, Error> {
        super::export::to_json(self)
    }
}
