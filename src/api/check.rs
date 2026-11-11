//! What a model comes to, without solving it.

use super::builder::{Item, Model};
use super::error::Error;
use super::options::Options;
use super::warning::Warning;
use crate::solve::Engine;
use std::collections::HashMap;

/// What a model comes to, without solving it: how many variables and
/// constraints each name stands for, the size of the solver's model, the
/// engine that would solve it, and the warnings. From [`Model::check`]
/// and `optimizam --check`: the
/// way to catch a `forall` that came to nothing, or to far too much, before
/// a long solve.
#[derive(Clone, Debug)]
pub struct Check {
    variables: Vec<(String, usize)>,
    constraints: Vec<(String, usize)>,
    columns: usize,
    rows: usize,
    engine: Option<Engine>,
    warnings: Vec<Warning>,
}

impl Check {
    /// Each variable's name and how many elements it has (1 for a scalar),
    /// in the model's order.
    pub fn variables(&self) -> &[(String, usize)] {
        &self.variables
    }

    /// Each constraint's name and how many constraints it stands for: the
    /// instances of an indexed constraint of a problem file count under its
    /// name, and one whose `forall` matched nothing has 0. A constraint
    /// without a name goes by its place, `constraints[3]`.
    pub fn constraints(&self) -> &[(String, usize)] {
        &self.constraints
    }

    /// The columns of the solver's model: the variables' elements, and what
    /// the constraint kinds and `abs`, `min`, and `max` terms add.
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// The rows of the solver's model, before presolve.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The engine that would solve the model with the options given (what
    /// [`Engine::Auto`] picks); None if presolve settles it.
    pub fn engine(&self) -> Option<Engine> {
        self.engine
    }

    /// What the model says that it probably doesn't mean.
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// The check as JSON, as `optimizam --check` prints it.
    pub fn to_json(&self) -> String {
        let counts = |items: &[(String, usize)]| {
            let fields: Vec<String> =
                items.iter().map(|(name, n)| format!("    {}: {n}", serde_json::Value::from(name.as_str()))).collect();
            if fields.is_empty() { "{}".to_string() } else { format!("{{\n{}\n  }}", fields.join(",\n")) }
        };
        let warnings: Vec<String> = self
            .warnings
            .iter()
            .map(|w| {
                let mut warning = serde_json::json!({ "kind": w.kind().as_str(), "message": w.message() });
                if let Some(place) = w.place() {
                    warning["place"] = place.into();
                }
                format!("    {warning}")
            })
            .collect();
        let warnings = if warnings.is_empty() { "[]".to_string() } else { format!("[\n{}\n  ]", warnings.join(",\n")) };
        format!(
            "{{\n  \"variables\": {},\n  \"constraints\": {},\n  \"columns\": {},\n  \"rows\": {},\n  \"engine\": \"{}\",\n  \"warnings\": {}\n}}\n",
            counts(&self.variables),
            counts(&self.constraints),
            self.columns,
            self.rows,
            self.engine.map_or("presolve", Engine::name),
            warnings
        )
    }
}

/// The name that a constraint's label counts under: the label up to its
/// subscript (`cap[north]` is one of `cap`; `constraints[3][north]`, an
/// instance of a nameless constraint, one of `constraints[3]`).
fn family(label: &str) -> &str {
    let named = |from: usize| label[from..].find('[').map_or(label.len(), |k| from + k);
    match label.strip_prefix("constraints[").and_then(|rest| rest.find(']')) {
        Some(close) => &label[..named("constraints[".len() + close + 1)],
        None => &label[..named(0)],
    }
}

/// Checks `model`. `declared`: the constraints' names in a problem file's
/// order, where one with no instance still counts (as 0).
pub(crate) fn check(
    model: &Model,
    options: &Options,
    warnings: Vec<Warning>,
    declared: Option<Vec<String>>,
) -> Result<Check, Error> {
    // Lowered as for a solve, which then stops where it hands the problem
    // to an engine.
    let mut config = options.config();
    config.plan = true;
    let (lowered, engine) = crate::config::scoped(config, || {
        let lowered = super::lower::lower(model).map_err(Error::from_solver)?;
        crate::solve::solve(&lowered, None, options.engine_choice()).map_err(Error::from_solver)?;
        Ok::<_, Error>((lowered, crate::config::run().0))
    })?;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<String> = declared.unwrap_or_default();
    let labels: Vec<String> = (0..model.items.len())
        .filter(|&i| !matches!(model.items[i], Item::Removed))
        .map(|i| model.constraint_label(i))
        .collect();
    for label in &labels {
        let name = family(label);
        *counts.entry(name).or_default() += 1;
        if !order.iter().any(|n| n == name) {
            order.push(name.to_string());
        }
    }
    let mut seen = std::collections::HashSet::new();
    order.retain(|name| seen.insert(name.clone()));
    let constraints = order.into_iter().map(|name| (counts.get(name.as_str()).copied().unwrap_or(0), name));
    Ok(Check {
        variables: model.families.iter().map(|f| (f.name(), f.len)).collect(),
        constraints: constraints.map(|(n, name)| (name, n)).collect(),
        columns: lowered.num_cols(),
        rows: lowered.rows.len() + lowered.nonlinear.as_ref().map_or(0, |nl| nl.rows.len()),
        engine,
        warnings,
    })
}
