//! The public API: the model builder, problem files, options, and results.

pub(crate) mod builder;
pub(crate) mod check;
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
pub(crate) mod warning;

pub use builder::{ConstraintId, IntervalVar, Model, VarSpec, var};
pub use check::Check;
pub use error::{Error, ErrorKind};
pub use explain::Conflict;
pub use expression::{Constraint, Expression, Literal, Number, Variable, max, min, sum};
pub use family::{Element, Key, KeyPart, Set, VarFamily};
pub use options::{CancelToken, Options};
pub use progress::{Control, Event, Progress, StopReason};
pub use solution::{Solution, SolveResult};
pub use warning::{Warning, WarningKind};

use crate::result::Status;
use crate::solve::Engine;
use solution::Layout;
use std::borrow::Cow;
use std::path::Path;
use std::rc::Rc;
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
        let layout = Layout::new(&model, model_id, options);
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

impl Model {
    /// Reads a problem file's JSON (`docs/problem-format.md`, also in
    /// [`crate::format::problem`]) into a model, to solve as it is or to
    /// change first: each variable is a family over its index sets
    /// ([`Model::family`]), each instance of an indexed constraint a
    /// constraint named like `capacity[north]` ([`Model::constraint_named`]).
    /// What the file says that it probably doesn't mean is in
    /// [`Model::warnings`].
    ///
    /// An error says everything that is wrong with the file (up to 20
    /// errors; see [`Error::errors`]). [`Model::to_json`] of the result
    /// writes the expanded problem (one constraint per instance, sums
    /// written out), not the original file.
    ///
    /// ```
    /// use optimizam::{Model, Options, Status};
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
    /// let result = m.solve(&Options::default())?;
    /// assert_eq!(result.status(), Status::Complete);
    /// assert_eq!(result.objective(), Some(6.0));
    /// assert_eq!(result.best().unwrap().get("make", &["south"]), Some(6.0));
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
        let reading = crate::problem::Problem::read(text).map_err(|issues| Error::from_issues(&issues))?;
        if reading.issues.is_empty() {
            return convert::convert(&reading.problem).map_err(|issues| Error::from_issues(&issues));
        }
        // Something is wrong. Say everything that is, in this one reading:
        // where the sections agree with each other, the expressions are read
        // too (with placeholders for what didn't read), and their errors
        // added, but for those that only echo one already found.
        let mut issues = crate::problem::Issues(reading.issues.clone());
        if reading.convertible
            && let Err(more) = convert::convert(&reading.problem)
        {
            issues.0.extend(more.0.into_iter().filter(|issue| !reading.echoes(&issue.text)));
        }
        Err(Error::from_issues(&issues))
    }

    /// Reads a problem file into a model (see [`Model::from_json`]).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Model, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::new(ErrorKind::Io, format!("reading {}: {e}", path.display())))?;
        Model::from_json(&text)
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
                    // Through the public path too, which reads on past what
                    // is wrong (with placeholders) to find every error.
                    let outcome = std::panic::catch_unwind(|| {
                        if let Ok(p) = crate::problem::Problem::from_json(&json) {
                            let _ = crate::model::build(&p);
                        }
                        let _ = super::Model::from_json(&json);
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
