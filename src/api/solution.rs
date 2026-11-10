//! Results of a solve.

use super::explain::Conflict;
use super::expression::{Expression, Owner, Variable};
use super::family::Element;
use super::progress::StopReason;
use crate::model::{LinearModel, Sense, VarInfo};
use crate::problem::{Elem, VarType};
use crate::result::{Details, Outcome, Status};
use std::ops::Index;
use std::sync::Arc;

/// Maps the problem's variables to columns.
pub(crate) struct Layout {
    pub(crate) vars: Vec<VarInfo>,
    /// The builder model the result belongs to (None for problem files).
    model: Option<u64>,
    /// The number of the problem's own columns (encodings add others).
    columns: usize,
}

impl Layout {
    pub(crate) fn new(model: &LinearModel, model_id: Option<u64>) -> Arc<Layout> {
        let vars = model.vars.clone();
        // The problem's own columns come first; encodings add the others.
        let columns = vars.iter().flat_map(|v| v.elements.iter().map(|e| e.1 + 1)).max().unwrap_or(0);
        Arc::new(Layout { vars, model: model_id, columns })
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
        Solution { values, objective, layout: self.clone() }
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
    layout: Arc<Layout>,
}

/// One solution: a value for every variable.
///
/// Index it with a [`Variable`] of the model (`solution[x]`); integer and
/// binary variables have integral values. For problem files, look variables
/// up by name with [`get`](Solution::get).
#[derive(Clone, Debug)]
pub struct Solution {
    values: Vec<f64>,
    objective: Option<f64>,
    layout: Arc<Layout>,
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
            .map(|x| layout.solution(x, (model.sense != Sense::Satisfy).then(|| model.objective_value(x))))
            .collect();
        let proven = matches!(outcome.status, Status::Complete | Status::Infeasible | Status::Unbounded);
        SolveResult {
            status: outcome.status,
            solutions,
            objective_bound: outcome.objective_bound,
            stop_reason: stop_reason.filter(|_| !proven),
            conflict: None,
            unbounded: Vec::new(),
            layout,
        }
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
    /// bound when minimizing), when the solver has one: with
    /// [`Status::NotProvenOptimal`] or [`Status::LocallyOptimal`] it says how
    /// much better a solution could be.
    pub fn objective_bound(&self) -> Option<f64> {
        self.objective_bound
    }

    /// Why the search stopped before it finished, if it did: a time limit,
    /// a [`CancelToken`](crate::CancelToken), a progress callback's
    /// [`Control::Stop`](crate::Control::Stop), or one of the solver's own
    /// limits. None when it ran to its end, which includes proven outcomes
    /// ([`Status::Complete`], [`Status::Infeasible`], [`Status::Unbounded`])
    /// even if a limit was reached just as the proof finished, and searches
    /// that end without a proof on their own (such as
    /// [`Engine::Nlp`](crate::Engine::Nlp)'s local solve).
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
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
    /// the `optimizam` command writes it.
    pub fn to_json(&self) -> String {
        let solutions: Vec<(Option<f64>, &[f64])> =
            self.solutions.iter().map(|s| (s.objective, s.values.as_slice())).collect();
        let conflict = self.conflict.as_ref().map(|c| crate::result::Conflict {
            constraints: c.constraints(),
            variables: c.variables(),
            minimal: c.is_minimal(),
        });
        let details = Details { objective_bound: self.objective_bound, conflict, unbounded: &self.unbounded };
        crate::result::to_json(&self.layout.vars, self.status, &solutions, &details)
    }
}

impl Solution {
    /// A result file with just this solution.
    pub(crate) fn result_json(&self, layout: &Layout, status: Status, bound: Option<f64>) -> String {
        let details = Details { objective_bound: bound, ..Details::default() };
        crate::result::to_json(&layout.vars, status, &[(self.objective, self.values.as_slice())], &details)
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
        let elems = var.symbolic.as_ref()?;
        let k = (self.values[v.col].round() as usize).clamp(1, elems.len()) - 1;
        Some(Element::from_elem(&elems[k]))
    }

    /// The objective value (None for satisfaction problems).
    pub fn objective(&self) -> Option<f64> {
        self.objective
    }

    /// A variable's value by name and index elements, as written in the
    /// problem file (`get("x", &["a", "3"])` for `x["a", 3]`; `&[]` for a
    /// scalar variable). Builder variables from
    /// [`Model::add_vars`](crate::Model::add_vars) are indexed `0, 1, ...`.
    /// For a symbolic variable, the value is the 1-based position of its
    /// element in the set; see [`get_element`](Solution::get_element).
    pub fn get(&self, name: &str, index: &[&str]) -> Option<f64> {
        self.lookup(name, index).map(|(_, col)| self.values[col])
    }

    /// A symbolic variable's value: the element of its set (as written in
    /// the problem file), or None if the variable isn't symbolic.
    pub fn get_element(&self, name: &str, index: &[&str]) -> Option<String> {
        let (var, col) = self.lookup(name, index)?;
        let elems = var.symbolic.as_ref()?;
        let k = (self.values[col].round() as usize).clamp(1, elems.len()) - 1;
        Some(elems[k].key())
    }

    fn lookup(&self, name: &str, index: &[&str]) -> Option<(&VarInfo, usize)> {
        let var = self.layout.vars.iter().find(|v| v.name == name)?;
        let matches = |keys: &[Elem]| keys.len() == index.len() && keys.iter().zip(index).all(|(k, i)| k.key() == *i);
        var.elements.iter().find(|(keys, _)| matches(keys)).map(|&(_, col)| (var, col))
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
