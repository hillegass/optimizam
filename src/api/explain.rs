//! Why a model has no solution, or no best one.
//!
//! - [`obvious`] reads off the model what needs no search: a variable whose
//!   bounds or domain leave it no value, and a constraint that can't hold
//!   within its variables' bounds (a constant one such as `0 >= 1` among
//!   them). Every infeasible result gets these.
//! - [`conflict`] goes on, when asked, with a deletion filter: it removes
//!   the constraints one group at a time and keeps a group out when the rest
//!   is still infeasible. What remains can't hold together, and each of its
//!   members is needed for that. It costs a solve per group, so it is opt-in
//!   and shares the solve's time limit.
//! - [`unbounded_variables`] lists the variables that an unbounded objective
//!   can run away along: usually one of them is missing a bound.
//!
//! All of it works on the builder [`Model`], above the engines, so it
//! applies to every engine and to problem files alike.

use super::builder::{ConstraintId, Item, Model, VarKind};
use super::error::Error;
use super::expression::Mode;
use super::lower::{col_bounds, col_name};
use super::options::Options;
use super::warning::{Warning, WarningKind};
use crate::model::Sense;
use crate::result::Status;
use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Instant;

/// Slack allowed when rounding an integer variable's bounds inward.
const INTEGRALITY: f64 = 1e-9;

/// Constraints that can't hold together, given the variables' bounds and
/// types: why a model is infeasible.
///
/// A solve reports one when the cause can be read off the model (see
/// [`SolveResult::conflict`](crate::SolveResult::conflict)); with
/// [`Options::explain`], or from [`Model::conflict`], it comes from a search
/// that removes constraints while the rest stays infeasible.
///
/// ```
/// use optimizam::{Model, Options, constraint, var};
///
/// let mut m = Model::new();
/// let x = m.add_var(var().integer().bounds(0, 10).name("x"));
/// let y = m.add_var(var().integer().bounds(0, 10).name("y"));
/// m.add(constraint!(x + y <= 4).name("budget"));
/// m.add(constraint!(x >= 3).name("x_floor"));
/// m.add(constraint!(y >= 3).name("y_floor"));
/// m.add(constraint!(x - y <= 5).name("balance"));
///
/// let conflict = m.conflict(&Options::default())?.expect("the model is infeasible");
/// assert_eq!(conflict.constraints(), ["budget", "x_floor", "y_floor"]);
/// assert!(conflict.is_minimal());
/// // Relax one of them, and the rest can hold.
/// m.remove(conflict.constraint_ids()[0]);
/// assert!(m.conflict(&Options::default())?.is_none());
/// # Ok::<(), optimizam::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    constraints: Vec<String>,
    ids: Vec<ConstraintId>,
    variables: Vec<String>,
    minimal: bool,
}

impl Conflict {
    fn new(model: &Model, ids: Vec<ConstraintId>, variables: Vec<String>, minimal: bool) -> Conflict {
        let constraints = ids.iter().map(|id| model.constraint_label(id.0)).collect();
        Conflict { constraints, ids, variables, minimal }
    }

    /// The names of the constraints that can't hold together, in the
    /// model's order: a constraint's own name (an instance of an indexed
    /// constraint of a problem file is named like `capacity[north]`), or
    /// its place if it has none: `constraints[3]`, counting from 0 (in a
    /// problem file's list of constraints, or in the order a model's were
    /// added).
    pub fn constraints(&self) -> &[String] {
        &self.constraints
    }

    /// The same constraints as ids of the model, for
    /// [`Model::remove`]. (For a problem file: of
    /// [`Problem::to_model`](crate::Problem::to_model)'s model.)
    pub fn constraint_ids(&self) -> &[ConstraintId] {
        &self.ids
    }

    /// Variables whose own bounds or domain leave them no value (written
    /// like `x` or `x[north,3]`). Each is a cause by itself.
    pub fn variables(&self) -> &[String] {
        &self.variables
    }

    /// Whether every member is needed: without any one of them, the rest can
    /// hold. False when the search for a smaller set was cut short, and when
    /// several members are each a cause alone.
    pub fn is_minimal(&self) -> bool {
        self.minimal
    }
}

/// What can be read off the model without a search: the variables with no
/// value left, or else the constraints that can't hold within their
/// variables' bounds.
pub(crate) fn obvious(model: &Model) -> Option<Conflict> {
    let mut bounds = Vec::with_capacity(model.num_cols);
    let mut variables = Vec::new();
    for f in &model.families {
        for k in 0..f.len {
            let (mut lo, mut hi, _) = col_bounds(model, f, k).ok()?;
            if f.spec(k).kind != VarKind::Continuous {
                (lo, hi) = ((lo - INTEGRALITY).ceil(), (hi + INTEGRALITY).floor());
            }
            if lo > hi {
                variables.push(col_name(model, f.first + k));
            }
            bounds.push((lo, hi));
        }
    }
    let mut ids = Vec::new();
    // (With a variable that has no value, a range of activities means nothing.)
    for (i, item) in model.items.iter().enumerate().filter(|_| variables.is_empty()) {
        let Item::Expr(c) = item else { continue };
        if !c.literals.is_empty() || c.element.is_some() || c.mode == Mode::Differs || !c.expr.is_affine() {
            continue;
        }
        let (mut least, mut most) = (c.expr.constant, c.expr.constant);
        for (&col, &a) in &c.expr.linear {
            let (lo, hi) = bounds[col];
            if a > 0.0 {
                (least, most) = (least + a * lo, most + a * hi);
            } else if a < 0.0 {
                (least, most) = (least + a * hi, most + a * lo);
            }
        }
        let slack = |bound: f64| 1e-9 * (1.0 + bound.abs());
        if least > c.upper + slack(c.upper) || most < c.lower - slack(c.lower) {
            ids.push(ConstraintId(i));
        }
    }
    let members = variables.len() + ids.len();
    (members > 0).then(|| Conflict::new(model, ids, variables, members == 1))
}

/// What a solve would report as an obvious conflict, as warnings to give
/// before it starts.
pub(crate) fn obvious_warnings(model: &Model) -> Vec<Warning> {
    let Some(conflict) = obvious(model) else { return Vec::new() };
    let variables =
        conflict.variables().iter().map(|v| format!("variable {v} has no value within its bounds or domain"));
    let constraints =
        conflict.constraints().iter().map(|c| format!("constraint {c} can't hold within its variables' bounds"));
    variables
        .chain(constraints)
        .map(|what| Warning::new(WarningKind::Infeasible, None, format!("{what}: the problem is infeasible")))
        .collect()
}

/// The variables with an objective coefficient and no bound in the
/// direction that improves the objective (written like `x[north,3]`).
pub(crate) fn unbounded_variables(model: &Model) -> Vec<String> {
    let down = match model.sense {
        Sense::Minimize => 1.0,
        Sense::Maximize => -1.0,
        Sense::Satisfy => return Vec::new(),
    };
    let free = |col: usize, a: f64| {
        let (f, k) = model.family_of(col);
        let Ok((lo, hi, _)) = col_bounds(model, f, k) else { return false };
        if a * down > 0.0 { lo == f64::NEG_INFINITY } else { hi == f64::INFINITY }
    };
    let coefs = model.objective.linear.iter().filter(|c| *c.1 != 0.0);
    coefs.filter(|&(&col, &a)| free(col, a)).map(|(&col, _)| col_name(model, col)).collect()
}

/// The model's constraints in groups to try removing together: those whose
/// names differ only in a subscript (the instances of one indexed constraint
/// of a problem file), each other constraint alone. In the model's order.
fn groups(model: &Model) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (i, item) in model.items.iter().enumerate() {
        if matches!(item, Item::Removed) {
            continue;
        }
        let label = model.constraint_label(i);
        let family = label.split('[').next().unwrap_or_default().to_string();
        let g = *by_name.entry(family).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[g].push(i);
    }
    groups
}

/// A set of the model's constraints that can't hold together: the obvious
/// one if there is one, otherwise what a deletion filter leaves, searching
/// until `deadline`. None if the model isn't infeasible (it is solved first
/// to find out, unless `known_infeasible`), if that can't be shown in time,
/// or if there was no time to try leaving anything out (the whole model
/// would be the answer, which says nothing).
pub(crate) fn conflict(
    model: &Model,
    options: &Options,
    deadline: Option<Instant>,
    known_infeasible: bool,
) -> Result<Option<Conflict>, Error> {
    if let Some(found) = obvious(model) {
        return Ok(Some(found));
    }
    let options = options.for_trials(deadline);
    let mut work = model.clone();
    work.satisfy();
    work.all_solutions(false);
    // Whether a model has no solution: None if the solve couldn't say.
    let infeasible = |m: &Model| -> Result<Option<bool>, Error> {
        let result = super::solve_with(&options, None, || Ok(Cow::Borrowed(m)))?;
        Ok(match result.status() {
            Status::Infeasible => Some(true),
            _ if result.best().is_some() => Some(false),
            _ => None,
        })
    };
    if !known_infeasible && infeasible(&work)? != Some(true) {
        return Ok(None);
    }

    let (mut minimal, mut tried) = (true, false);
    // Takes the constraints out; puts them back unless the rest is still
    // infeasible. Returns whether they stayed out.
    let mut remove = |work: &mut Model, ids: &[usize]| -> Result<bool, Error> {
        if options.expired(deadline) {
            minimal = false;
            return Ok(false);
        }
        let taken: Vec<Item> = ids.iter().map(|&i| std::mem::replace(&mut work.items[i], Item::Removed)).collect();
        let verdict = infeasible(work)?;
        tried |= verdict.is_some();
        if verdict != Some(true) {
            for (&i, item) in ids.iter().zip(taken) {
                work.items[i] = item;
            }
            minimal &= verdict.is_some();
        }
        Ok(verdict == Some(true))
    };
    // Whole groups first, then the members of the groups that are needed.
    let groups = groups(&work);
    let mut needed = Vec::new();
    for group in &groups {
        if !remove(&mut work, group)? && group.len() > 1 {
            needed.push(group);
        }
    }
    for group in needed {
        for &i in group {
            remove(&mut work, &[i])?;
        }
    }
    let left = (0..work.items.len()).filter(|&i| !matches!(work.items[i], Item::Removed)).map(ConstraintId).collect();
    Ok(tried.then(|| Conflict::new(model, left, Vec::new(), minimal)))
}
