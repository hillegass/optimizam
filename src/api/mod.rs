//! The public API: the model builder, problem files, options, and results.

pub(crate) mod builder;
pub(crate) mod constraints;
pub(crate) mod convert;
pub(crate) mod error;
pub(crate) mod explain;
pub(crate) mod export;
pub(crate) mod expression;
pub(crate) mod family;
pub(crate) mod lint;
pub(crate) mod lower;
pub(crate) mod options;
pub(crate) mod progress;
pub(crate) mod solution;

pub use builder::{ConstraintId, IntervalVar, Model, VarSpec, var};
pub use error::{Error, ErrorKind};
pub use explain::Conflict;
pub use expression::{Constraint, Expression, Literal, Variable};
pub use family::{Element, Key, KeyPart, Set, VarFamily};
pub use options::{CancelToken, Options};
pub use progress::{Control, Event, Progress, StopReason};
pub use solution::{Solution, SolveResult};

use crate::result::Status;
use crate::solve::Engine;
use solution::Layout;
use std::borrow::Cow;
use std::path::Path;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Instant;

/// Solves the model that `source` makes, with `options` installed, and says
/// why if it has no solution or no best one (`explain.rs`).
pub(crate) fn solve_with<'a>(
    options: &Options,
    model_id: Option<u64>,
    source: impl FnOnce() -> Result<Cow<'a, Model>, String>,
) -> Result<SolveResult, Error> {
    let start = Instant::now();
    let deadline = options.deadline_from(start);
    let (mut result, source) = crate::config::scoped(options.config(), || {
        let source = source().map_err(Error::from_solver)?;
        let model = lower::lower(&source).map_err(Error::from_solver)?;
        let layout = Layout::new(&model, model_id);
        if let Some(callback) = options.progress_callback() {
            crate::config::observe(Rc::new(progress::Observer::new(callback, &model, layout.clone(), start)));
        }
        // A linear program whose duals are wanted is solved for them directly.
        let engine = options.engine_choice();
        let for_duals = options.reports_constraints() && matches!(engine, Engine::Auto | Engine::Milp);
        let (outcome, duals) = match for_duals.then(|| crate::solve::linear_program(&model, deadline)).flatten() {
            Some((outcome, duals)) => (outcome, Some(duals)),
            None => (crate::solve::solve(&model, deadline, engine).map_err(Error::from_solver)?, None),
        };
        let mut result = SolveResult::new(&model, outcome, layout, crate::config::stop_reason());
        if options.reports_constraints() {
            result.set_constraints(&source, constraints::Table::new(&source, &model, duals.as_ref()), duals);
        }
        Ok::<_, Error>((result, source))
    })?;
    match result.status() {
        _ if options.is_trial() => {}
        Status::Infeasible if options.explains() => {
            result.set_conflict(explain::conflict(&source, options, deadline, true)?);
        }
        Status::Infeasible => result.set_conflict(explain::obvious(&source)),
        Status::Unbounded => result.set_unbounded(explain::unbounded_variables(&source)),
        _ => {}
    }
    result.set_elapsed(start.elapsed());
    Ok(result)
}

/// A problem in the JSON problem format (`docs/problem-format.md`, also in
/// [`crate::format::problem`]), as the `optimizam` command reads it.
///
/// ```
/// use optimizam::{Options, Problem, Status};
///
/// let problem = Problem::from_json(r#"{
///     "version": "1.0",
///     "sets": { "ITEMS": ["a", "b", "c"] },
///     "parameters": {
///         "value":  { "index": ["ITEMS"], "values": { "a": 10, "b": 13, "c": 7 } },
///         "weight": { "index": ["ITEMS"], "values": { "a": 5, "b": 6, "c": 3 } }
///     },
///     "variables": [{ "name": "take", "type": "binary", "index": ["ITEMS"] }],
///     "constraints": [{ "name": "capacity", "expr": "sum(i in ITEMS) weight[i] * take[i] <= 9" }],
///     "solve": { "method": "maximize", "objective": "sum(i in ITEMS) value[i] * take[i]" }
/// }"#)?;
/// let result = problem.solve(&Options::default())?;
/// assert_eq!(result.status(), Status::Complete);
/// assert_eq!(result.objective(), Some(20.0));
/// assert_eq!(result.best().unwrap().get("take", &["b"]), Some(1.0));
/// println!("{}", result.to_json());
/// # Ok::<(), optimizam::Error>(())
/// ```
#[derive(Debug)]
pub struct Problem {
    inner: crate::problem::Problem,
    /// The problem as a model, with its warnings: made when first needed.
    converted: OnceLock<Result<(Model, Vec<String>), String>>,
}

impl Problem {
    /// Reads a problem from JSON text.
    pub fn from_json(text: &str) -> Result<Problem, Error> {
        crate::problem::Problem::from_json(text).map(|inner| Problem { inner, converted: OnceLock::new() }).map_err(
            |e| {
                let kind = if e.starts_with("invalid problem file:") { ErrorKind::Parse } else { ErrorKind::Invalid };
                Error::new(kind, e)
            },
        )
    }

    /// The problem's model and warnings, or why it has none.
    fn converted(&self) -> Result<&(Model, Vec<String>), String> {
        self.converted.get_or_init(|| convert::convert(&self.inner)).as_ref().map_err(String::clone)
    }

    /// What the problem file says that it probably doesn't mean. None of
    /// these stops a solve; the `optimizam` command prints them to stderr.
    ///
    /// - A `forall` that matches nothing (its constraint doesn't exist).
    /// - An index of a `sum`, a list, or a `forall` that hides an outer
    ///   index, a parameter, or a variable of the same name.
    /// - Two constraints with one name.
    /// - A parameter that nothing uses; a variable in no constraint and not
    ///   in the objective.
    /// - A variable whose bounds leave it no value, or a constraint that
    ///   can't hold within its variables' bounds: the problem is infeasible
    ///   before any search.
    ///
    /// Empty if the problem has an error, which [`solve`](Problem::solve)
    /// and [`to_model`](Problem::to_model) report.
    ///
    /// ```
    /// use optimizam::Problem;
    ///
    /// let problem = Problem::from_json(r#"{
    ///     "version": "1.0",
    ///     "sets": { "T": { "from": 1, "to": 3 } },
    ///     "parameters": { "bonus": 5 },
    ///     "variables": [{ "name": "y", "type": "binary", "index": ["T"] }],
    ///     "constraints": [{ "name": "late", "forall": "t in T: t > 3", "expr": "y[t] == 0" }],
    ///     "solve": { "method": "maximize", "objective": "sum(t in T) y[t]" }
    /// }"#)?;
    /// assert_eq!(problem.warnings(), [
    ///     "constraint late: its forall (\"t in T: t > 3\") matches nothing, so it adds no constraints",
    ///     "parameter bonus is never used",
    /// ]);
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn warnings(&self) -> Vec<String> {
        self.converted().map(|c| c.1.clone()).unwrap_or_default()
    }

    /// Reads a problem from a JSON file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Problem, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::new(ErrorKind::Io, format!("reading {}: {e}", path.display())))?;
        Problem::from_json(&text)
    }

    /// Solves the problem. As with [`Model::solve`], an infeasible problem
    /// or a time limit is a status, not an error.
    pub fn solve(&self, options: &Options) -> Result<SolveResult, Error> {
        solve_with(options, None, || self.converted().map(|c| Cow::Borrowed(&c.0)))
    }

    /// Why the problem is infeasible: a set of its constraints that can't
    /// hold together, or None if it has a solution (see
    /// [`Model::conflict`]).
    pub fn conflict(&self, options: &Options) -> Result<Option<Conflict>, Error> {
        self.converted().map_err(Error::from_solver)?.0.conflict(options)
    }

    /// The problem as a [`Model`], to change before solving: each variable
    /// is a family over its index sets ([`Model::family`]), each instance
    /// of an indexed constraint a constraint named like `capacity[north]`
    /// ([`Model::constraint_named`]). Solving the model gives the same
    /// answers as solving the problem.
    ///
    /// [`Model::to_json`] of the result writes the expanded problem (one
    /// constraint per instance, sums written out), not the original file.
    pub fn to_model(&self) -> Result<Model, Error> {
        self.converted().map(|c| c.0.clone()).map_err(Error::from_solver)
    }
}

impl Problem {
    /// The problem's model itself, not a copy.
    fn into_model(self) -> Result<Model, Error> {
        self.converted().map_err(Error::from_solver)?;
        Ok(self.converted.into_inner().and_then(Result::ok).expect("just converted").0)
    }
}

impl Model {
    /// Reads a problem file's JSON into a model (see [`Problem::to_model`]).
    ///
    /// ```
    /// use optimizam::{Model, Options};
    ///
    /// let mut m = Model::from_json(r#"{
    ///     "version": "1.3",
    ///     "sets": { "PLANTS": ["north", "south"] },
    ///     "parameters": { "capacity": { "index": ["PLANTS"], "values": { "north": 4, "south": 6 } } },
    ///     "variables": [{ "name": "make", "type": "integer", "index": ["PLANTS"], "lower": 0, "upper": 10 }],
    ///     "constraints": [
    ///         { "name": "capacity", "forall": "p in PLANTS", "expr": "make[p] <= capacity[p]" },
    ///         { "name": "shut", "expr": "make['north'] == 0" }
    ///     ],
    ///     "solve": { "method": "maximize", "objective": "sum(p in PLANTS) make[p]" }
    /// }"#)?;
    /// assert_eq!(m.solve(&Options::default())?.objective(), Some(6.0));
    ///
    /// // Reopen north, and let south make up to 8.
    /// m.remove(m.constraint_named("shut").unwrap());
    /// let make = m.family("make").unwrap();
    /// m.remove(m.constraint_named("capacity[south]").unwrap());
    /// m.set_bounds(make["south"], 0, 8);
    /// assert_eq!(m.solve(&Options::default())?.objective(), Some(12.0));
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn from_json(text: &str) -> Result<Model, Error> {
        Problem::from_json(text)?.into_model()
    }

    /// Reads a problem file into a model (see [`Problem::to_model`]).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Model, Error> {
        Problem::from_file(path)?.into_model()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// Every string in a problem, with a setter that replaces it.
    fn strings(v: &Value, path: Vec<String>, out: &mut Vec<(Vec<String>, String)>) {
        match v {
            Value::String(s) => out.push((path, s.clone())),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    let mut p = path.clone();
                    p.push(i.to_string());
                    strings(item, p, out);
                }
            }
            Value::Object(map) => {
                for (k, item) in map {
                    let mut p = path.clone();
                    p.push(k.clone());
                    strings(item, p, out);
                }
            }
            _ => {}
        }
    }

    fn set(v: &mut Value, path: &[String], s: &str) {
        let mut cur = v;
        for p in path {
            cur = match cur {
                Value::Array(items) => &mut items[p.parse::<usize>().unwrap()],
                Value::Object(map) => map.get_mut(p).unwrap(),
                _ => unreachable!(),
            };
        }
        *cur = Value::String(s.to_string());
    }

    /// Problem files with any string replaced by another name, a broken
    /// expression, or nonsense are rejected with an error, never a panic.
    #[test]
    fn mutated_problems_never_panic() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut panics = Vec::new();
        let mut cases = 0;
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        files.sort();
        for file in files {
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            if !name.ends_with(".json") || name.ends_with(".result.json") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            if text.len() > 3000 {
                continue;
            }
            let original: Value = serde_json::from_str(&text).unwrap();
            let mut leaves = Vec::new();
            strings(&original, Vec::new(), &mut leaves);
            let mut names: Vec<String> = leaves.iter().map(|l| l.1.clone()).filter(|s| s.len() < 20).collect();
            names.sort();
            names.dedup();
            names.extend(["nosuch".into(), "".into(), "x[".into(), "[a | i in".into(), "1".into(), "-".into()]);
            for (path, value) in &leaves {
                let mut replacements = names.clone();
                replacements.push(value[..value.len() / 2].to_string());
                for r in &replacements {
                    let mut v = original.clone();
                    set(&mut v, path, r);
                    let json = v.to_string();
                    cases += 1;
                    let outcome = std::panic::catch_unwind(|| {
                        if let Ok(p) = crate::problem::Problem::from_json(&json) {
                            let _ = crate::model::build(&p);
                        }
                    });
                    if outcome.is_err() {
                        panics.push(format!("{name}: {} = {r:?}", path.join(".")));
                    }
                }
            }
        }
        assert!(cases > 1000);
        assert!(panics.is_empty(), "{} of {cases} panicked:\n{}", panics.len(), panics.join("\n"));
    }
}
