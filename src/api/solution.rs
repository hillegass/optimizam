//! Results of a solve.

use super::expression::{Expression, Owner, Variable};
use crate::model::{LinearModel, Sense, VarInfo};
use crate::problem::{Elem, VarType};
use crate::result::{Outcome, Status};
use std::ops::Index;
use std::sync::Arc;

/// Maps the problem's variables to columns.
struct Layout {
    vars: Vec<VarInfo>,
    /// The builder model the result belongs to (None for problem files).
    model: Option<u64>,
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
    pub(crate) fn new(model: &LinearModel, outcome: Outcome, model_id: Option<u64>) -> SolveResult {
        let vars = model.vars.clone();
        // The problem's own columns come first; encodings add the others.
        let n = vars.iter().flat_map(|v| v.elements.iter().map(|e| e.1 + 1)).max().unwrap_or(0);
        let layout = Arc::new(Layout { vars, model: model_id });
        let solutions = outcome
            .solutions
            .iter()
            .map(|x| {
                let objective = (model.sense != Sense::Satisfy).then(|| model.objective_value(x));
                let mut values = x[..n.min(x.len())].to_vec();
                for v in &layout.vars {
                    if v.var_type != VarType::Continuous {
                        for &(_, col) in &v.elements {
                            values[col] = values[col].round();
                        }
                    }
                }
                Solution { values, objective, layout: layout.clone() }
            })
            .collect();
        SolveResult { status: outcome.status, solutions, objective_bound: outcome.objective_bound, layout }
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

    /// The result in the JSON result format (`docs/result-format.md`), as
    /// the `optimizam` command writes it.
    pub fn to_json(&self) -> String {
        let solutions: Vec<(Option<f64>, &[f64])> =
            self.solutions.iter().map(|s| (s.objective, s.values.as_slice())).collect();
        crate::result::to_json(&self.layout.vars, self.status, &solutions, self.objective_bound)
    }
}

impl Solution {
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
