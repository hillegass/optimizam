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
    shown: Vec<Shown>,
}

/// How many members of a constraint, or elements of a variable, a check
/// writes out.
const SHOWN: usize = 5;

/// What a name of the model comes to, written out: the first members of the
/// constraints of that name, or the first elements of the variable of that
/// name, and how many there are in all. From [`Check::shown`], for the names
/// given to [`Options::show`].
///
/// ```
/// use optimizam::{Model, Options, constraint, var};
/// # fn main() -> Result<(), optimizam::Error> {
/// let mut m = Model::new();
/// let x = m.add_vars(3, var().integer().bounds(0, 5).name("x"));
/// for k in 0..2 {
///     m.add_indexed("step", k, constraint!(x[k + 1] >= x[k] + 1));
/// }
/// let check = m.check(&Options::default().show("step").show("x"))?;
/// let [step, x] = check.shown() else { panic!() };
/// assert_eq!((step.count(), step.is_variable()), (2, false));
/// assert_eq!(step.instances()[0], ("step[0]".to_string(), "-x[0] + x[1] >= 1".to_string()));
/// assert_eq!(x.instances()[2], ("x[2]".to_string(), "integer, lower 0, upper 5".to_string()));
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct Shown {
    name: String,
    variable: bool,
    count: usize,
    instances: Vec<(String, String)>,
}

impl Shown {
    /// The name that was asked for.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this is a variable's elements (and not a constraint's
    /// members).
    pub fn is_variable(&self) -> bool {
        self.variable
    }

    /// How many members the constraint has, or elements the variable: 0 for
    /// a constraint of a problem file whose `forall` matched nothing.
    pub fn count(&self) -> usize {
        self.count
    }

    /// The first of them, each by its name and as text. A constraint is
    /// written as a problem file would write it, with its variables by name
    /// and its numbers worked out (`make['north', 2] + make['south', 2] <=
    /// 40`), a long sum or list cut short; a variable's element as its type
    /// and what bounds it (`integer, lower 0, upper 12`).
    pub fn instances(&self) -> &[(String, String)] {
        &self.instances
    }
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

    /// What the names given to [`Options::show`] come to, in the order they
    /// were given (a name that a constraint and a variable share: the
    /// constraint, then the variable). Empty when none was given.
    pub fn shown(&self) -> &[Shown] {
        &self.shown
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
        let text = |s: &str| serde_json::Value::from(s).to_string();
        let shown: Vec<String> = self
            .shown
            .iter()
            .map(|s| {
                let instances: Vec<String> =
                    s.instances.iter().map(|(name, what)| format!("        {}: {}", text(name), text(what))).collect();
                let instances =
                    if instances.is_empty() { "{}".to_string() } else { format!("{{\n{}\n      }}", instances.join(",\n")) };
                format!(
                    "    {{\n      \"name\": {},\n      \"kind\": \"{}\",\n      \"count\": {},\n      \"instances\": {}\n    }}",
                    text(&s.name),
                    if s.variable { "variable" } else { "constraint" },
                    s.count,
                    instances
                )
            })
            .collect();
        // Only a check that was asked to show something says what it shows.
        let shown =
            if shown.is_empty() { String::new() } else { format!(",\n  \"shown\": [\n{}\n  ]", shown.join(",\n")) };
        format!(
            "{{\n  \"variables\": {},\n  \"constraints\": {},\n  \"columns\": {},\n  \"rows\": {},\n  \"engine\": \"{}\",\n  \"warnings\": {}{}\n}}\n",
            counts(&self.variables),
            counts(&self.constraints),
            self.columns,
            self.rows,
            self.engine.map_or("presolve", Engine::name),
            warnings,
            shown
        )
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
    // Each constraint counts under its family's name (the instances of
    // `cap` under `cap`), or by itself under its place.
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for name in declared.unwrap_or_default() {
        if counts.insert(name.clone(), 0).is_none() {
            order.push(name);
        }
    }
    for i in (0..model.items.len()).filter(|&i| !matches!(model.items[i], Item::Removed)) {
        let name = model.constraint_family_of(i).map_or_else(|| format!("constraints[{i}]"), str::to_string);
        match counts.get_mut(&name) {
            Some(n) => *n += 1,
            None => {
                counts.insert(name.clone(), 1);
                order.push(name);
            }
        }
    }
    let constraints: Vec<(String, usize)> =
        order.into_iter().map(|name| (counts[&name], name)).map(|(n, name)| (name, n)).collect();
    let mut shown = Vec::new();
    for name in options.shows() {
        let before = shown.len();
        // The constraints of that name: a family's members, one without a
        // name by its place, or one member by its own name.
        let place = name.strip_prefix("constraints[").and_then(|k| k.strip_suffix(']')).and_then(|k| k.parse().ok());
        let live = |i: &usize| !matches!(model.items[*i], Item::Removed);
        let mut members: Vec<usize> = (0..model.items.len())
            .filter(live)
            .filter(|&i| match model.constraint_family_of(i) {
                Some(family) => family == name,
                None => place == Some(i),
            })
            .collect();
        if members.is_empty() {
            members.extend(model.constraint_named(name).map(|id| id.0));
        }
        if !members.is_empty() || constraints.iter().any(|(family, _)| family == name) {
            let first = &members[..members.len().min(SHOWN)];
            let texts = super::export::constraint_texts(model, first)?;
            let mut instances: Vec<(String, String)> = Vec::new();
            for (&i, text) in first.iter().zip(texts) {
                // (Two constraints may share a name, which is a warning.)
                let label = model.constraint_label(i);
                let again =
                    instances.iter().filter(|(l, _)| *l == label || l.starts_with(&format!("{label} ("))).count();
                instances.push((if again == 0 { label } else { format!("{label} ({})", again + 1) }, text));
            }
            shown.push(Shown { name: name.clone(), variable: false, count: members.len(), instances });
        }
        if let Some(f) = model.families.iter().find(|f| f.name() == *name) {
            let mut instances = Vec::new();
            for k in 0..f.len.min(SHOWN) {
                instances
                    .push((super::lower::col_name(model, f.first + k), super::export::variable_text(model, f, k)?));
            }
            shown.push(Shown { name: name.clone(), variable: true, count: f.len, instances });
        }
        if shown.len() == before {
            let names: Vec<String> =
                constraints.iter().map(|c| c.0.clone()).chain(model.families.iter().map(|f| f.name())).collect();
            let meant = crate::problem::closest(name, names.iter().map(String::as_str))
                .map_or(String::new(), |m| format!("; did you mean {m}?"));
            return Err(Error::new(
                super::ErrorKind::Invalid,
                format!("nothing to show for {name}: no constraint or variable has that name{meant}"),
            ));
        }
    }
    Ok(Check {
        variables: model.families.iter().map(|f| (f.name(), f.len)).collect(),
        constraints,
        columns: lowered.num_cols(),
        rows: lowered.rows.len() + lowered.nonlinear.as_ref().map_or(0, |nl| nl.rows.len()),
        engine,
        warnings,
        shown,
    })
}
