//! The public API: the model builder, problem files, options, and results.

pub(crate) mod builder;
pub(crate) mod error;
pub(crate) mod export;
pub(crate) mod expression;
pub(crate) mod family;
pub(crate) mod lower;
pub(crate) mod options;
pub(crate) mod progress;
pub(crate) mod solution;

pub use builder::{ConstraintId, IntervalVar, Model, VarSpec, var};
pub use error::{Error, ErrorKind};
pub use expression::{Constraint, Expression, Literal, Variable};
pub use family::{Element, Key, KeyPart, Set, VarFamily};
pub use options::{CancelToken, Options};
pub use progress::{Control, Event, Progress, StopReason};
pub use solution::{Solution, SolveResult};

use crate::model::LinearModel;
use solution::Layout;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

/// Builds the model and solves it with `options` installed.
pub(crate) fn solve_with(
    options: &Options,
    model_id: Option<u64>,
    build: impl FnOnce() -> Result<LinearModel, String>,
) -> Result<SolveResult, Error> {
    let start = Instant::now();
    let deadline = options.deadline_from(start);
    crate::config::scoped(options.config(), || {
        let model = build().map_err(Error::from_solver)?;
        let layout = Layout::new(&model, model_id);
        if let Some(callback) = options.progress_callback() {
            crate::config::observe(Rc::new(progress::Observer::new(callback, &model, layout.clone(), start)));
        }
        let outcome = crate::solve::solve(&model, deadline, options.engine_choice()).map_err(Error::from_solver)?;
        Ok(SolveResult::new(&model, outcome, layout, crate::config::stop_reason()))
    })
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
}

impl Problem {
    /// Reads a problem from JSON text.
    pub fn from_json(text: &str) -> Result<Problem, Error> {
        crate::problem::Problem::from_json(text).map(|inner| Problem { inner }).map_err(|e| {
            let kind = if e.starts_with("invalid problem file:") { ErrorKind::Parse } else { ErrorKind::Invalid };
            Error::new(kind, e)
        })
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
        solve_with(options, None, || crate::model::build(&self.inner))
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
