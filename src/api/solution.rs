//! Results of a solve.

use super::builder::{ConstraintId, Model};
use super::constraints::Table;
use super::explain::Conflict;
use super::expression::{Expression, Owner, Variable};
use super::family::{Element, Key, VarFamily};
use super::progress::StopReason;
use crate::model::{LinearModel, Sense, VarInfo};
use crate::problem::{Elem, VarType};
use crate::result::{ConstraintOut, Details, Outcome, SolutionOut, Statistics, Status};
use crate::solve::{Duals, Engine};
use std::collections::HashMap;
use std::ops::Index;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Maps the problem's variables to columns.
pub(crate) struct Layout {
    pub(crate) vars: Vec<VarInfo>,
    /// The builder model the result belongs to (None for problem files).
    model: Option<u64>,
    /// The number of the problem's own columns (encodings add others).
    columns: usize,
    sense: Sense,
    /// What result files leave out: the zero entries of indexed variables,
    /// and the constraints with slack.
    omit_zeros: bool,
    omit_slack: bool,
    /// Each variable's column by key, made when a value is first asked for
    /// by name ([`Solution::get`]).
    by_key: Vec<OnceLock<HashMap<Vec<Elem>, usize>>>,
}

impl Layout {
    pub(crate) fn new(model: &LinearModel, model_id: Option<u64>, options: &super::Options) -> Arc<Layout> {
        let vars = model.vars.clone();
        // The problem's own columns come first; encodings add the others.
        let columns = vars.iter().flat_map(|v| v.elements.iter().map(|e| e.1 + 1)).max().unwrap_or(0);
        let (omit_zeros, omit_slack) = options.omissions();
        let by_key = vars.iter().map(|_| OnceLock::new()).collect();
        Arc::new(Layout { vars, model: model_id, columns, sense: model.sense, omit_zeros, omit_slack, by_key })
    }

    /// A solution from the model's columns (integer values rounded).
    pub(crate) fn solution(self: &Arc<Self>, x: &[f64], objective: Option<f64>) -> Solution {
        let mut values = x[..self.columns.min(x.len())].to_vec();
        for v in &self.vars {
            if v.var_type != VarType::Continuous {
                for &(_, col) in &v.elements {
                    values[col] = values[col].round();
                }
            }
        }
        Solution { values, objective, layout: self.clone(), constraints: None }
    }
}

impl std::fmt::Debug for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.vars.iter().map(|v| &v.name)).finish()
    }
}

/// What a solve found: a [`Status`], the solutions, and a bound on the
/// optimal objective.
#[derive(Clone, Debug)]
pub struct SolveResult {
    status: Status,
    solutions: Vec<Solution>,
    objective_bound: Option<f64>,
    stop_reason: Option<StopReason>,
    conflict: Option<Conflict>,
    unbounded: Vec<String>,
    /// The engine the problem went to (none if it needed none), the
    /// threads its search could use, and its node count.
    engine: Option<Engine>,
    threads: usize,
    nodes: Option<u64>,
    elapsed: Duration,
    /// The constraints, if they are reported, and a linear program's
    /// reduced costs by column.
    table: Option<Arc<Table>>,
    reduced_costs: Option<Vec<f64>>,
    layout: Arc<Layout>,
}

/// The sense as result files write it.
fn sense_name(sense: Sense) -> &'static str {
    match sense {
        Sense::Minimize => "minimize",
        Sense::Maximize => "maximize",
        Sense::Satisfy => "satisfy",
    }
}

/// `|objective - bound| / max(1, |objective|)`.
fn relative_gap(objective: Option<f64>, bound: Option<f64>) -> Option<f64> {
    let (objective, bound) = (objective?, bound?);
    Some((objective - bound).abs() / objective.abs().max(1.0))
}

/// One solution: a value for every variable.
///
/// Index it with a [`Variable`] of the model (`solution[x]`); integer and
/// binary variables have integral values. Without the model's variables at
/// hand (a problem file's, say), look a value up by name and key with
/// [`get`](Solution::get), or go through them all with
/// [`iter`](Solution::iter). Printed (`println!("{solution}")`), a solution
/// is its objective and one line for each variable's element.
///
/// ```
/// use optimizam::{Model, Options, Set, constraint, sum, var};
///
/// let plants = Set::new("PLANTS", ["north", "south"]);
/// let mut m = Model::new();
/// let make = m.add_family("make", &[&plants, &Set::range("DAYS", 1..=2)], var().integer().bounds(0, 5));
/// m.add(constraint!(make[("north", 1)] + make[("south", 1)] <= 6));
/// m.maximize(sum(make.vars()));
/// let result = m.solve(&Options::default())?;
/// let best = result.best().unwrap();
///
/// assert_eq!(best.get("make", ("north", 2)), Some(5.0));
/// let made: f64 = best.family(&make).map(|(_key, value)| value).sum();
/// assert_eq!(made, 16.0);
/// let busy = best.iter().filter(|entry| entry.value() == 5.0).count();
/// assert!(busy >= 2);
/// assert!(best.to_string().starts_with("objective = 16\nmake[north,1] = "));
/// # Ok::<(), optimizam::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Solution {
    values: Vec<f64>,
    objective: Option<f64>,
    layout: Arc<Layout>,
    /// The reported constraints and the slack this solution leaves in each.
    constraints: Option<(Arc<Table>, Vec<Option<f64>>)>,
}

impl SolveResult {
    /// `stop_reason`: why the search was cut short, if it was; ignored for
    /// proven outcomes.
    pub(crate) fn new(
        model: &LinearModel,
        outcome: Outcome,
        layout: Arc<Layout>,
        stop_reason: Option<StopReason>,
    ) -> SolveResult {
        let solutions = outcome
            .solutions
            .iter()
            .map(|x| {
                let x = model.settled(x);
                layout.solution(&x, (model.sense != Sense::Satisfy).then(|| model.objective_value(&x)))
            })
            .collect();
        let proven = matches!(outcome.status, Status::Complete | Status::Infeasible | Status::Unbounded);
        let (engine, nodes) = crate::config::run();
        SolveResult {
            status: outcome.status,
            solutions,
            objective_bound: outcome.objective_bound,
            stop_reason: stop_reason.filter(|_| !proven),
            conflict: None,
            unbounded: Vec::new(),
            engine,
            threads: crate::config::threads(),
            // The local nonlinear solve has no tree to count.
            nodes: (engine != Some(Engine::Local)).then_some(nodes),
            elapsed: Duration::ZERO,
            table: None,
            reduced_costs: None,
            layout,
        }
    }

    /// Records the model's constraints with each solution's slacks, and a
    /// linear program's duals.
    pub(crate) fn set_constraints(&mut self, model: &Model, table: Table, duals: Option<Duals>) {
        let table = Arc::new(table);
        for solution in &mut self.solutions {
            solution.constraints = Some((table.clone(), table.slacks(model, &solution.values)));
        }
        self.table = Some(table);
        self.reduced_costs = duals.map(|d| d.cols);
    }

    /// Records how long the solve took, all told.
    pub(crate) fn set_elapsed(&mut self, elapsed: Duration) {
        self.elapsed = elapsed;
    }

    /// Records why the problem is infeasible.
    pub(crate) fn set_conflict(&mut self, conflict: Option<Conflict>) {
        self.conflict = conflict;
    }

    /// Records the variables an unbounded objective can run away along.
    pub(crate) fn set_unbounded(&mut self, variables: Vec<String>) {
        self.unbounded = variables;
    }

    /// How the solve ended.
    pub fn status(&self) -> Status {
        self.status
    }

    /// The solutions found: none, one, or (when every solution was
    /// requested) all of them, in the order found.
    pub fn solutions(&self) -> &[Solution] {
        &self.solutions
    }

    /// The first solution, which is the best one found when optimizing.
    pub fn best(&self) -> Option<&Solution> {
        self.solutions.first()
    }

    /// The best solution's objective value (None for satisfaction problems,
    /// or when nothing was found).
    pub fn objective(&self) -> Option<f64> {
        self.best().and_then(|s| s.objective)
    }

    /// A bound on the optimal objective, in the problem's sense (a lower
    /// bound when minimizing), when the search stopped with one: with
    /// [`Status::NotProvenOptimal`] or [`Status::LocallyOptimal`] it says how
    /// much better a solution could be, and with [`Status::Timeout`] how good
    /// any solution could be. None once the optimum is proven.
    pub fn objective_bound(&self) -> Option<f64> {
        self.objective_bound
    }

    /// How far from optimal the best solution could be:
    /// `|objective - bound| / max(1, |objective|)`, when there is a solution
    /// and a bound. None once the optimum is proven (no bound is reported
    /// then), and for results without a solution.
    pub fn gap(&self) -> Option<f64> {
        relative_gap(self.objective(), self.objective_bound)
    }

    /// The engine the problem went to (what [`Engine::Auto`] chose): never
    /// `Auto` itself, and None when the problem was settled before any
    /// engine ran (by presolve, or by a constraint that can't hold).
    pub fn engine(&self) -> Option<Engine> {
        self.engine
    }

    /// How long the solve took, from reading the model to the answer (and
    /// the search for a conflict, with [`Options::explain`](crate::Options::explain)).
    /// With a time limit it can be somewhat more than the limit.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// The size of the search, in what the engine branches on: for
    /// [`Engine::Milp`] the branch-and-bound nodes (over all threads and
    /// restarts), for [`Engine::Cp`] the conflicts of the calling thread's
    /// searches, for [`Engine::Global`] the boxes. Zero when no engine ran;
    /// None for [`Engine::Local`], which has no tree.
    pub fn nodes(&self) -> Option<u64> {
        self.nodes
    }

    /// Why the search stopped before it finished, if it did: a time limit,
    /// a [`CancelToken`](crate::CancelToken), a progress callback's
    /// [`Control::Stop`](crate::Control::Stop), or one of the solver's own
    /// limits. None when it ran to its end, which includes proven outcomes
    /// ([`Status::Complete`], [`Status::Infeasible`], [`Status::Unbounded`])
    /// even if a limit was reached just as the proof finished, and searches
    /// that end without a proof on their own (such as
    /// [`Engine::Local`]'s solve).
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
    }

    /// A constraint's dual value (its shadow price) in a linear program
    /// solved with [`Options::constraints`](crate::Options::constraints):
    /// how much the optimal objective would change per unit that the
    /// constraint's right side rises, in the problem's own sense. So for a
    /// binding `<=` capacity in a maximization it is what one more unit of
    /// capacity is worth; a constraint with slack has dual 0.
    ///
    /// None without that option, for a problem that isn't a linear program
    /// (an integer variable, a nonlinear term) or wasn't solved to an
    /// optimum, and for a constraint that isn't a comparison of variables.
    /// At a degenerate optimum duals aren't unique: these are the ones of
    /// the simplex method's final basis.
    pub fn dual(&self, id: ConstraintId) -> Option<f64> {
        let table = self.table.as_ref()?;
        table.duals.as_ref()?[table.position(id.0)?]
    }

    /// A variable's reduced cost in a linear program solved with
    /// [`Options::constraints`](crate::Options::constraints): how much the
    /// optimal objective would change per unit that the variable is moved
    /// off the bound it sits at, in the problem's own sense (0 for a
    /// variable strictly between its bounds). None when
    /// [`dual`](SolveResult::dual) would be.
    ///
    /// # Panics
    ///
    /// If `v` belongs to another model.
    pub fn reduced_cost(&self, v: Variable) -> Option<f64> {
        assert!(self.layout.model == Some(v.model), "the variable belongs to another model");
        self.reduced_costs.as_ref().map(|d| d[v.col])
    }

    /// Why the problem is infeasible ([`Status::Infeasible`]), when that is
    /// known: constraints that can't hold together, or variables whose
    /// bounds leave them no value.
    ///
    /// Without [`Options::explain`](crate::Options::explain) only causes
    /// that can be read off the model are reported: a variable with crossed
    /// bounds or an empty domain, and a constraint that can't hold within
    /// its variables' bounds (`0 >= 1` among them, as an empty sum gives).
    /// With it, a search for a small set of conflicting constraints follows.
    pub fn conflict(&self) -> Option<&Conflict> {
        self.conflict.as_ref()
    }

    /// For an unbounded problem ([`Status::Unbounded`]): the variables with
    /// an objective coefficient and no bound in the direction that improves
    /// the objective, written like `x` or `x[north,3]`. One of them is
    /// usually missing a bound. (Empty if the objective is unbounded only
    /// through its nonlinear terms.)
    pub fn unbounded_variables(&self) -> &[String] {
        &self.unbounded
    }

    /// The result in the JSON result format (`docs/result-format.md`), as
    /// the `optimizam` command-line tool writes it.
    pub fn to_json(&self) -> String {
        let solutions: Vec<SolutionOut> = self.solutions.iter().map(|s| s.out(self.reduced_costs.as_deref())).collect();
        let conflict = self.conflict.as_ref().map(|c| crate::result::Conflict {
            constraints: c.constraints(),
            variables: c.variables(),
            minimal: c.is_minimal(),
        });
        let details = Details {
            sense: sense_name(self.layout.sense),
            omit_zeros: self.layout.omit_zeros,
            omit_slack: self.layout.omit_slack,
            objective_bound: self.objective_bound,
            gap: self.gap(),
            stop_reason: self.stop_reason.map(StopReason::as_str),
            conflict,
            unbounded: &self.unbounded,
            statistics: Statistics {
                engine: Some(self.engine.map_or("presolve", Engine::name)),
                threads: Some(self.threads),
                time: self.elapsed.as_secs_f64(),
                nodes: self.nodes,
            },
        };
        crate::result::to_json(&self.layout.vars, self.status, &solutions, &details)
    }
}

impl Solution {
    /// The solution as a result file has it.
    fn out<'a>(&'a self, reduced_costs: Option<&'a [f64]>) -> SolutionOut<'a> {
        let constraints = self.constraints.as_ref().map(|(table, slacks)| {
            let dual = |k: usize| table.duals.as_ref().and_then(|d| d[k]);
            let constraint = |k: usize| {
                let (family, key) = &table.names[k];
                ConstraintOut { family, key, slack: slacks[k], dual: dual(k) }
            };
            (0..slacks.len()).map(constraint).collect()
        });
        SolutionOut { objective: self.objective, values: &self.values, constraints, reduced_costs }
    }

    /// The slack this solution leaves in a constraint, when the solve was
    /// asked to report constraints
    /// ([`Options::constraints`](crate::Options::constraints)): how far the
    /// constraint's left side is from its right side (for a constraint
    /// with two bounds, from the nearer one). 0 means it is tight.
    ///
    /// None without that option, for a constraint that isn't a comparison
    /// (`all_different`, `no_overlap`, ...), and for one whose `only_if`
    /// literals don't hold in this solution, which it then doesn't bind.
    pub fn slack(&self, id: ConstraintId) -> Option<f64> {
        let (table, slacks) = self.constraints.as_ref()?;
        slacks[table.position(id.0)?]
    }

    /// A result file with just this solution, for a solve still running.
    pub(crate) fn result_json(
        &self,
        layout: &Layout,
        status: Status,
        bound: Option<f64>,
        gap: Option<f64>,
        elapsed: Duration,
    ) -> String {
        let details = Details {
            sense: sense_name(layout.sense),
            omit_zeros: layout.omit_zeros,
            omit_slack: layout.omit_slack,
            objective_bound: bound,
            gap,
            statistics: Statistics { time: elapsed.as_secs_f64(), ..Statistics::default() },
            ..Details::default()
        };
        crate::result::to_json(&layout.vars, status, &[self.out(None)], &details)
    }

    /// The value of a variable.
    ///
    /// # Panics
    ///
    /// If `v` belongs to another model.
    pub fn value(&self, v: Variable) -> f64 {
        assert!(self.layout.model == Some(v.model), "the variable belongs to another model");
        self.values[v.col]
    }

    /// The value of an integer or binary variable as an integer (a
    /// continuous variable's value rounded to the nearest one).
    ///
    /// # Panics
    ///
    /// If `v` belongs to another model.
    pub fn int(&self, v: Variable) -> i64 {
        self.value(v).round() as i64
    }

    /// Whether a binary variable is 1 (for any variable: whether its value
    /// rounds to 1).
    ///
    /// # Panics
    ///
    /// If `v` belongs to another model.
    pub fn is_one(&self, v: Variable) -> bool {
        self.int(v) == 1
    }

    /// The value of an expression of the model's variables.
    ///
    /// # Panics
    ///
    /// If the expression has variables of another model.
    pub fn eval(&self, e: &Expression) -> f64 {
        match e.owner {
            Owner::None => {}
            Owner::Model(id) if self.layout.model == Some(id) => {}
            _ => panic!("the expression has variables of another model"),
        }
        e.evaluate(&|col| self.values[col])
    }

    /// A symbolic variable's value: the element of its set
    /// ([`VarSpec::values`](crate::VarSpec::values)), or None if the
    /// variable isn't symbolic.
    ///
    /// # Panics
    ///
    /// If `v` belongs to another model.
    pub fn element(&self, v: Variable) -> Option<Element> {
        assert!(self.layout.model == Some(v.model), "the variable belongs to another model");
        let var = self.layout.vars.iter().find(|var| var.elements.iter().any(|e| e.1 == v.col))?;
        self.element_at(var, v.col)
    }

    /// The objective value (None for satisfaction problems).
    pub fn objective(&self) -> Option<f64> {
        self.objective
    }

    /// A variable's value by its name and its key: `get("make", ("north",
    /// 3))` for `make[north,3]`, `get("budget", ())` for a variable without
    /// index sets, one element for one set (`get("start", 4)`), or a slice
    /// of [`Element`]s for any number (see [`Key`]). None if there is no
    /// such variable or key. An element is what it is in its set: the
    /// integer `3`, not the string `"3"`. Variables from
    /// [`Model::add_vars`](crate::Model::add_vars) are keyed `0, 1, ...`.
    ///
    /// For a symbolic variable, the value is the 1-based position of its
    /// element in the set; see [`get_element`](Solution::get_element).
    pub fn get(&self, name: &str, key: impl Key) -> Option<f64> {
        self.lookup(name, &key).map(|(_, col)| self.values[col])
    }

    /// A symbolic variable's value by its name and key: the element of its
    /// set, or None if there is no such variable or key, or the variable
    /// isn't symbolic.
    pub fn get_element(&self, name: &str, key: impl Key) -> Option<Element> {
        let (var, col) = self.lookup(name, &key)?;
        self.element_at(var, col)
    }

    /// The element a symbolic variable's column stands at.
    fn element_at(&self, var: &VarInfo, col: usize) -> Option<Element> {
        let elems = var.symbolic.as_ref()?;
        let k = (self.values[col].round() as usize).clamp(1, elems.len()) - 1;
        Some(Element::from_elem(&elems[k]))
    }

    fn lookup(&self, name: &str, key: &impl Key) -> Option<(&VarInfo, usize)> {
        let v = self.layout.vars.iter().position(|v| v.name == name)?;
        let var = &self.layout.vars[v];
        let key: Vec<Elem> = key.elements()?.iter().map(Element::to_elem).collect();
        let by_key = self.layout.by_key[v].get_or_init(|| var.elements.iter().cloned().collect());
        by_key.get(&key).map(|&col| (var, col))
    }

    /// Every variable's every element with its value, in the model's order
    /// (a family's elements in the order of its sets, the last varying
    /// fastest): what a caller that doesn't know the model reads a solution
    /// with.
    ///
    /// ```
    /// use optimizam::{Model, Options};
    ///
    /// let model = Model::from_json(r#"{
    ///     "version": "1.3",
    ///     "sets": { "T": ["cut", "weld"], "M": ["lathe", "mill"] },
    ///     "variables": [
    ///         { "name": "on", "type": "integer", "index": ["T"], "domain": "M" },
    ///         { "name": "hours", "type": "integer", "lower": 2, "upper": 9 }
    ///     ],
    ///     "constraints": [{ "all_different": "[on[t] | t in T]" }, { "expr": "on['cut'] == 'mill'" }],
    ///     "solve": { "method": "minimize", "objective": "hours" }
    /// }"#)?;
    /// let result = model.solve(&Options::default())?;
    /// let lines: Vec<String> = result.best().unwrap().iter().map(|entry| entry.to_string()).collect();
    /// assert_eq!(lines, ["on[cut] = mill", "on[weld] = lathe", "hours = 2"]);
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = VarValue<'_>> + '_ {
        let entry = |var, key, col| VarValue { solution: self, var, key, col };
        self.layout.vars.iter().flat_map(move |var| var.elements.iter().map(move |(key, col)| entry(var, key, *col)))
    }

    /// The values of a family's variables, each with its key, in the
    /// family's order.
    ///
    /// # Panics
    ///
    /// If the family belongs to another model.
    pub fn family<'a>(&'a self, family: &'a VarFamily) -> impl Iterator<Item = (Vec<Element>, f64)> + 'a {
        family.iter().map(|(key, v)| (key, self.value(v)))
    }
}

/// One element of one variable in a [`Solution`], from
/// [`Solution::iter`]: the variable's name, the element's key, and its
/// value. Printed, it is `make[north,3] = 12` (for a symbolic variable,
/// `machine[cut] = mill`).
#[derive(Clone, Copy)]
pub struct VarValue<'a> {
    solution: &'a Solution,
    var: &'a VarInfo,
    key: &'a [Elem],
    col: usize,
}

impl VarValue<'_> {
    /// The variable's name.
    pub fn name(&self) -> &str {
        &self.var.name
    }

    /// The element's key: one element for each of the variable's index
    /// sets, none for a variable without them.
    pub fn key(&self) -> Vec<Element> {
        self.key.iter().map(Element::from_elem).collect()
    }

    /// The value (integral for an integer or binary variable; for a
    /// symbolic one, the 1-based position of its element in the set).
    pub fn value(&self) -> f64 {
        self.solution.values[self.col]
    }

    /// A symbolic variable's value, as the element of its set; None for any
    /// other variable.
    pub fn element(&self) -> Option<Element> {
        self.solution.element_at(self.var, self.col)
    }
}

impl std::fmt::Display for VarValue<'_> {
    /// `name[key] = value`, as results and messages write a variable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.var.name)?;
        for (k, e) in self.key.iter().enumerate() {
            write!(f, "{}{}", if k == 0 { '[' } else { ',' }, Element::from_elem(e))?;
        }
        if !self.key.is_empty() {
            f.write_str("]")?;
        }
        match self.element() {
            Some(element) => write!(f, " = {element}"),
            None => write!(f, " = {}", self.value()),
        }
    }
}

impl std::fmt::Debug for VarValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VarValue({self})")
    }
}

impl std::fmt::Display for Solution {
    /// The objective, if there is one, then each variable's element on a
    /// line of its own: `make[north,3] = 12`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut lines =
            self.objective.map(|objective| format!("objective = {objective}")).into_iter().collect::<Vec<_>>();
        lines.extend(self.iter().map(|entry| entry.to_string()));
        f.write_str(&lines.join("\n"))
    }
}

impl Index<Variable> for Solution {
    type Output = f64;

    /// The value of a variable; panics if it belongs to another model.
    fn index(&self, v: Variable) -> &f64 {
        assert!(self.layout.model == Some(v.model), "the variable belongs to another model");
        &self.values[v.col]
    }
}
