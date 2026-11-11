//! The problem file format (docs/problem-format.md): read from JSON by hand,
//! so that a structural error says where in the file it is
//! (`variables[1] (carrier): missing "type"`), plus the checks between
//! sections.

use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};

/// The problem format's version, the only one read.
pub(crate) const VERSION: &str = "1.3";

#[derive(Debug)]
pub struct Problem {
    #[allow(dead_code)]
    pub schema: Option<String>,
    pub version: String,
    pub sets: BTreeMap<String, SetDef>,
    pub parameters: BTreeMap<String, ParamDef>,
    pub variables: Vec<VarDef>,
    /// Format 1.1: time intervals for no_overlap and cumulative.
    pub intervals: Vec<IntervalDef>,
    pub constraints: Vec<ConstraintDef>,
    pub solve: Solve,
}

/// A set element: a string or an integer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Elem {
    Int(i64),
    Str(String),
}

impl SetDef {
    /// The keys that a parameter table may have for this set: how its
    /// elements are written as keys.
    fn keys(&self) -> Keys<'_> {
        match self {
            SetDef::List(items) => {
                let mut keys: Vec<String> = items.iter().map(Elem::key).collect();
                keys.sort_unstable();
                Keys::Listed(keys)
            }
            SetDef::Range(r) => Keys::Range(r),
        }
    }

    pub fn contains(&self, e: &Elem) -> bool {
        match (self, e) {
            (SetDef::List(items), e) => items.contains(e),
            (SetDef::Range(r), Elem::Int(i)) => (r.from..=r.to).contains(i),
            (SetDef::Range(_), Elem::Str(_)) => false,
        }
    }

    /// The set's elements, for a message: `1 to 3`, or `gold, silver, gem`.
    pub fn describe(&self) -> String {
        match self {
            SetDef::Range(r) => format!("{} to {}", r.from, r.to),
            SetDef::List(items) => {
                let mut shown: Vec<String> = items.iter().take(8).map(Elem::key).collect();
                if items.len() > 8 {
                    shown.push("...".into());
                }
                shown.join(", ")
            }
        }
    }
}

/// The keys of a set's elements (see [`SetDef::keys`]): sorted, if listed.
enum Keys<'a> {
    Listed(Vec<String>),
    Range(&'a RangeDef),
}

impl Keys<'_> {
    /// The first of a table's keys (which come sorted, as the JSON object
    /// keeps them) that isn't one of these. Tables can have hundreds of
    /// thousands of entries, so this walks the two sorted lists side by
    /// side and hashes nothing.
    fn stranger<'k>(&self, mut table: impl Iterator<Item = &'k String>) -> Option<&'k String> {
        match self {
            Keys::Listed(keys) => {
                let mut keys = keys.iter();
                table.find(|key| !keys.any(|k| k == *key))
            }
            Keys::Range(r) => table.find(|key| {
                // As the element is written: no sign but a minus, no leading zero.
                let digits = key.strip_prefix('-').unwrap_or(key);
                let written = !digits.is_empty() && (digits == "0" || !digits.starts_with('0')) && *key != "-0";
                !(written && key.parse::<i64>().is_ok_and(|i| (r.from..=r.to).contains(&i)))
            }),
        }
    }
}

impl Elem {
    /// The key used for this element in parameter tables and result files.
    pub fn key(&self) -> String {
        match self {
            Elem::Int(i) => i.to_string(),
            Elem::Str(s) => s.clone(),
        }
    }
}

#[derive(Debug)]
pub enum SetDef {
    List(Vec<Elem>),
    Range(RangeDef),
}

#[derive(Debug)]
pub struct RangeDef {
    pub from: i64,
    pub to: i64,
}

#[derive(Debug)]
pub enum ParamDef {
    Scalar(f64),
    Table(TableDef),
}

#[derive(Debug)]
pub struct TableDef {
    #[allow(dead_code)]
    pub description: Option<String>,
    pub index: Vec<String>,
    pub values: serde_json::Map<String, Value>,
    pub default: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarType {
    Continuous,
    Integer,
    Binary,
}

/// A bound, a start, or an interval's field: a number, or an expression in
/// the parameters and the indexes that the item's `index` list names (for
/// an interval's fields, in the variables too).
#[derive(Debug, Clone)]
pub enum Bound {
    Num(f64),
    Expr(String),
}

#[derive(Debug)]
pub struct VarDef {
    pub name: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    pub var_type: VarType,
    /// The sets of the index list.
    pub index: Vec<String>,
    /// The name each entry gives its index (`"t in TASKS"`), for the
    /// bounds' and the start's expressions.
    pub index_names: Vec<Option<String>>,
    pub lower: Option<Bound>,
    pub upper: Option<Bound>,
    pub domain: Option<DomainDef>,
    /// Format 1.2: a starting value for local solvers.
    pub start: Option<Bound>,
}

/// An integer variable's domain: sorted inclusive ranges, or (format 1.1)
/// the name of a set whose elements are the values.
#[derive(Debug)]
pub enum DomainDef {
    Ranges(Vec<[i64; 2]>),
    Set(String),
}

#[derive(Debug)]
pub struct ConstraintDef {
    /// Its place in the file's `constraints` list, counting from 0.
    pub position: usize,
    pub name: Option<String>,
    #[allow(dead_code)]
    pub description: Option<String>,
    pub forall: Option<String>,
    pub expr: Option<String>,
    /// Format 1.1: a list whose items take pairwise different values.
    pub all_different: Option<String>,
    // Format 1.1 kinds that aren't implemented yet: read so they can be
    // rejected with a clear message.
    /// Format 1.1, with expr only: literals under which the relation must hold.
    pub enforce_if: Option<String>,
    /// Format 1.1.
    pub element: Option<ElementDef>,
    /// Format 1.1.
    pub table: Option<TableConstraintDef>,
    /// Format 1.1: a list of intervals that don't overlap.
    pub no_overlap: Option<String>,
    /// Format 1.1: intervals with demands that never exceed a capacity.
    pub cumulative: Option<CumulativeDef>,
}

/// What a constraint is.
/// `element`: value equals the array item selected by the index (or indexes).
#[derive(Debug)]
pub struct ElementDef {
    pub index: OneOrMany,
    pub array: String,
    pub value: String,
}

/// One expression string, or a list of them.
#[derive(Debug)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

/// `table`: the listed variables equal one of the tuples (or, forbidden, none).
#[derive(Debug)]
pub struct TableConstraintDef {
    pub vars: String,
    pub tuples: Vec<Vec<Elem>>,
    pub forbidden: bool,
}

/// An interval: [start, start + duration), present when `presence` is 1.
/// `start` and `duration` are a number or an affine expression (often just
/// the name of a variable or parameter indexed by the same sets as the
/// interval); `presence` is a binary variable.
#[derive(Debug)]
pub struct IntervalDef {
    pub name: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    pub index: Vec<String>,
    pub index_names: Vec<Option<String>>,
    pub start: Bound,
    pub duration: Bound,
    pub presence: Option<String>,
}

#[derive(Debug)]
pub struct CumulativeDef {
    pub intervals: String,
    pub demands: String,
    pub capacity: String,
}

pub enum ConstraintKind<'a> {
    /// A relation, and the `enforce_if` literals if any.
    Expr(&'a str, Option<&'a str>),
    AllDifferent(&'a str),
    Element(&'a ElementDef),
    Table(&'a TableConstraintDef),
    NoOverlap(&'a str),
    Cumulative(&'a CumulativeDef),
}

impl ConstraintDef {
    /// What the constraint is called in messages and results: its name, or
    /// `constraints[k]` for the file's k-th (counting from 0, as in a JSON
    /// path).
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("constraints[{}]", self.position))
    }

    /// The constraint's kind, or an error for a missing or repeated kind.
    pub fn kind(&self) -> Result<ConstraintKind<'_>, String> {
        let label = self.label();
        let kinds = [
            self.expr.is_some(),
            self.all_different.is_some(),
            self.element.is_some(),
            self.table.is_some(),
            self.no_overlap.is_some(),
            self.cumulative.is_some(),
        ];
        match kinds.iter().filter(|&&k| k).count() {
            0 => return Err(format!("constraint {label} has no kind field such as expr")),
            1 => {}
            _ => return Err(format!("constraint {label} has more than one kind field")),
        }
        if self.enforce_if.is_some() && self.expr.is_none() {
            return Err(format!("constraint {label}: enforce_if is only allowed with expr"));
        }
        Ok(if let Some(e) = &self.expr {
            ConstraintKind::Expr(e, self.enforce_if.as_deref())
        } else if let Some(l) = &self.all_different {
            ConstraintKind::AllDifferent(l)
        } else if let Some(e) = &self.element {
            ConstraintKind::Element(e)
        } else if let Some(t) = &self.table {
            ConstraintKind::Table(t)
        } else if let Some(l) = &self.no_overlap {
            ConstraintKind::NoOverlap(l)
        } else {
            ConstraintKind::Cumulative(self.cumulative.as_ref().unwrap())
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Satisfy,
    Minimize,
    Maximize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Solutions {
    #[default]
    One,
    All,
    /// At most this many (format 1.3).
    UpTo(usize),
}

#[derive(Debug)]
pub struct Solve {
    pub method: Method,
    pub objective: Option<String>,
    pub solutions: Solutions,
}

const RESERVED: [&str; 5] = ["sum", "in", "and", "or", "not"];

fn check_identifier(kind: &str, name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok_start = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok_start || !ok_rest {
        return Err(format!("{kind} name {name:?} is not a valid identifier"));
    }
    if RESERVED.contains(&name) {
        return Err(format!("{kind} name {name:?} is a reserved word"));
    }
    Ok(())
}

/// Something wrong with a problem file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Where it is: a path into the file, counting from 0 (`variables[1].type`).
    pub place: Option<String>,
    /// The name of the variable, constraint, or other item it is in.
    pub name: Option<String>,
    /// The message, whole, as it is shown.
    pub text: String,
    /// The file's structure is at fault (it can't be read as a problem
    /// file), not what it says.
    pub structural: bool,
}

/// Everything found wrong in one reading of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issues(pub Vec<Issue>);

impl From<String> for Issues {
    fn from(text: String) -> Issues {
        Issues(vec![Issue { place: None, name: None, text, structural: false }])
    }
}

/// A problem file as far as it could be read: where something was wrong, a
/// placeholder stands (a bound left out, a variable taken as continuous), so
/// that the rest can still be checked and every mistake reported in one run.
pub struct Reading {
    pub problem: Problem,
    /// What was wrong with the structure and with how the sections refer to
    /// each other. Empty for a problem that can be solved as it is.
    pub issues: Vec<Issue>,
    /// Whether the expressions can be read too, for their errors: the
    /// sections agree with each other (with the placeholders in).
    pub convertible: bool,
    /// The names of items that couldn't be read as written. An error that
    /// mentions one would only be an echo of the one already reported.
    pub broken: HashSet<String>,
}

impl Reading {
    /// Whether an error with this text is about something already reported.
    pub fn echoes(&self, text: &str) -> bool {
        self.broken.iter().any(|name| mentions(text, name))
    }
}

/// One of a problem's items, for [`Problem::validate`] to name the ones that
/// fail a check (variables, intervals, and constraints by their place in the
/// problem's lists).
enum Item {
    Version,
    NoVariables,
    Set(String),
    Parameter(String),
    Variable(usize),
    Interval(usize),
    Constraint(usize),
    Solve,
}

/// Whether `name` occurs in `text` as a whole name.
fn mentions(text: &str, name: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|word| word == name)
}

/// The number of single-character edits between two names.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut diagonal = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let substituted = diagonal + usize::from(a[i - 1] != b[j - 1]);
            diagonal = row[j];
            row[j] = substituted.min(row[j] + 1).min(row[j - 1] + 1);
        }
    }
    row[b.len()]
}

/// The candidate that `name` was most likely meant to be: one slip away, or
/// two in a long name ("args" isn't "abs"), and nothing for a name of one or
/// two letters.
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = match name.chars().count() {
        0..=2 => return None, // every other one-letter name is one slip away
        3..=5 => 1,
        _ => 2,
    };
    let near = candidates.into_iter().map(|c| (edit_distance(c, name), c)).filter(|&(d, c)| d <= limit && c != name);
    near.min_by_key(|&(d, _)| d).map(|(_, c)| c)
}

/// `; did you mean X?`, if a candidate is close to `name`.
pub fn did_you_mean<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    closest(name, candidates).map_or(String::new(), |meant| format!("; did you mean {meant}?"))
}

impl Problem {
    /// Reads a problem file that has nothing wrong with it (the first thing
    /// wrong is the error).
    #[cfg(test)]
    pub fn from_json(text: &str) -> Result<Problem, String> {
        let reading = Problem::read(text).map_err(|issues| issues.0[0].text.clone())?;
        match reading.issues.first() {
            Some(issue) => Err(issue.text.clone()),
            None => Ok(reading.problem),
        }
    }

    /// Reads a problem file as far as it goes, noting everything wrong on the
    /// way. An error only if it isn't JSON, or not an object.
    pub fn read(text: &str) -> Result<Reading, Issues> {
        let structural = |text: String| Issue { place: None, name: None, text, structural: true };
        let value: Value =
            serde_json::from_str(text).map_err(|e| Issues(vec![structural(format!("invalid problem file: {e}"))]))?;
        let sink = Sink::default();
        let problem = read_problem(&value, &sink);
        let (mut issues, broken) = (sink.issues.into_inner(), sink.broken.into_inner());
        let Some(mut problem) = problem else { return Err(Issues(issues)) };
        let mut broken = broken;
        // What fails a check is noted and taken out, so that the rest can be
        // read on; then what depended on it fails in turn (and is taken out
        // without a note, being an echo), until what is left is sound.
        loop {
            let (found, invalid) = problem.validate(&broken);
            issues.extend(found);
            if invalid.is_empty() {
                break;
            }
            problem.take_out(invalid, &mut broken);
        }
        let convertible = !problem.variables.is_empty();
        Ok(Reading { problem, issues, convertible, broken })
    }

    /// Checks that JSON Schema would make, plus cross-references between
    /// sections: one error at most for each set, parameter, variable,
    /// interval, and constraint. Returns the errors that don't just echo a
    /// `broken` item, and every item that failed.
    fn validate(&self, broken: &HashSet<String>) -> (Vec<Issue>, Vec<Item>) {
        let (mut issues, mut invalid) = (Vec::new(), Vec::new());
        let mut note = |place: String, name: Option<&str>, item: Item, check: Result<(), String>| {
            let Err(text) = check else { return };
            if !broken.iter().any(|b| mentions(&text, b)) {
                issues.push(Issue { place: Some(place), name: name.map(str::to_string), text, structural: false });
            }
            invalid.push(item);
        };
        note("version".into(), None, Item::Version, self.check_version());
        if self.variables.is_empty() && !broken.contains(VARIABLES) {
            let none = Err("the problem must declare at least one variable".into());
            note("variables".into(), None, Item::NoVariables, none);
        }
        for (name, set) in &self.sets {
            note(format!("sets.{name}"), Some(name), Item::Set(name.clone()), check_set(name, set));
        }
        let mut names = HashSet::new();
        for (name, param) in &self.parameters {
            let check = self.check_parameter(name, param);
            note(format!("parameters.{name}"), Some(name), Item::Parameter(name.clone()), check);
            names.insert(name.as_str());
        }
        for (k, var) in self.variables.iter().enumerate() {
            let fresh = names.insert(var.name.as_str());
            note(format!("variables[{k}]"), Some(&var.name), Item::Variable(k), self.check_variable(var, fresh));
        }
        for (k, iv) in self.intervals.iter().enumerate() {
            let fresh = names.insert(iv.name.as_str());
            note(format!("intervals[{k}]"), Some(&iv.name), Item::Interval(k), self.check_interval(iv, fresh));
        }
        for (k, c) in self.constraints.iter().enumerate() {
            let named = c.name.as_ref().map_or(Ok(()), |name| check_identifier("constraint", name));
            let check = named.and_then(|()| c.kind().map(|_| ()));
            note(format!("constraints[{}]", c.position), c.name.as_deref(), Item::Constraint(k), check);
        }
        if !broken.contains(SOLVE) {
            note("solve".into(), None, Item::Solve, self.check_solve());
        }
        (issues, invalid)
    }

    /// Takes the items that failed a check out of the problem (or puts a
    /// placeholder in their place), and marks their names broken.
    fn take_out(&mut self, invalid: Vec<Item>, broken: &mut HashSet<String>) {
        let (mut variables, mut intervals, mut constraints) = (HashSet::new(), HashSet::new(), HashSet::new());
        for item in invalid {
            match item {
                Item::Version => self.version = VERSION.into(),
                Item::NoVariables => drop(broken.insert(VARIABLES.into())),
                Item::Set(name) => {
                    self.sets.remove(&name);
                    broken.insert(name);
                }
                Item::Parameter(name) => {
                    self.parameters.remove(&name);
                    broken.insert(name);
                }
                Item::Variable(k) => {
                    broken.insert(self.variables[k].name.clone());
                    variables.insert(k);
                }
                Item::Interval(k) => {
                    broken.insert(self.intervals[k].name.clone());
                    intervals.insert(k);
                }
                Item::Constraint(k) => drop(constraints.insert(k)),
                Item::Solve => {
                    broken.insert(SOLVE.into());
                    self.solve = Solve { method: Method::Satisfy, objective: None, solutions: Solutions::One };
                }
            }
        }
        fn keep<T>(items: &mut Vec<T>, out: &HashSet<usize>) {
            let mut k = 0;
            items.retain(|_| {
                k += 1;
                !out.contains(&(k - 1))
            });
        }
        keep(&mut self.variables, &variables);
        keep(&mut self.intervals, &intervals);
        keep(&mut self.constraints, &constraints);
    }

    fn check_version(&self) -> Result<(), String> {
        match self.version.as_str() {
            VERSION => Ok(()),
            v => Err(format!("unsupported version {v:?}; this program reads version \"{VERSION}\"")),
        }
    }

    /// An unknown set, with the one that was probably meant.
    fn unknown_set(&self, set: &str) -> String {
        format!("unknown set {set}{}", did_you_mean(set, self.sets.keys().map(String::as_str)))
    }

    fn check_parameter(&self, name: &str, param: &ParamDef) -> Result<(), String> {
        check_identifier("parameter", name)?;
        if let ParamDef::Table(t) = param {
            for s in &t.index {
                if !self.sets.contains_key(s) {
                    return Err(format!("parameter {name} is indexed by {}", self.unknown_set(s)));
                }
            }
            if t.index.is_empty() {
                return Err(format!("parameter {name} has an empty index list"));
            }
            // Each index set's keys, made once for the whole table.
            let keys: Vec<Keys> = t.index.iter().map(|s| self.sets[s].keys()).collect();
            self.check_keys(name, &t.index, &keys, &t.values)?;
        }
        Ok(())
    }

    /// `fresh`: no parameter or earlier variable has its name.
    fn check_variable(&self, var: &VarDef, fresh: bool) -> Result<(), String> {
        check_identifier("variable", &var.name)?;
        if !fresh {
            return Err(format!("name {} is declared more than once", var.name));
        }
        for s in &var.index {
            if !self.sets.contains_key(s) {
                return Err(format!("variable {} is indexed by {}", var.name, self.unknown_set(s)));
            }
        }
        check_index_names("variable", &var.name, &var.index_names)?;
        if var.var_type == VarType::Binary && (var.lower.is_some() || var.upper.is_some() || var.domain.is_some()) {
            return Err(format!("binary variable {} must not declare bounds or a domain", var.name));
        }
        if let Some(domain) = &var.domain {
            if var.var_type != VarType::Integer {
                return Err(format!("variable {}: only integer variables can have a domain", var.name));
            }
            if var.lower.is_some() || var.upper.is_some() {
                return Err(format!("variable {}: use either a domain or lower/upper, not both", var.name));
            }
            match domain {
                DomainDef::Set(set) => match self.sets.get(set) {
                    None => return Err(format!("variable {}: domain refers to {}", var.name, self.unknown_set(set))),
                    Some(SetDef::List(items)) if items.is_empty() => {
                        return Err(format!("variable {}: domain set {set} is empty", var.name));
                    }
                    Some(SetDef::Range(r)) if r.from > r.to => {
                        return Err(format!("variable {}: domain set {set} is empty", var.name));
                    }
                    Some(_) => {}
                },
                DomainDef::Ranges(domain) => {
                    if domain.is_empty() {
                        return Err(format!("variable {}: domain is empty", var.name));
                    }
                    for (i, [lo, hi]) in domain.iter().enumerate() {
                        if lo > hi {
                            return Err(format!("variable {}: domain range [{lo}, {hi}] is empty", var.name));
                        }
                        if i > 0 && *lo <= domain[i - 1][1] {
                            return Err(format!(
                                "variable {}: domain ranges must be sorted and non-overlapping",
                                var.name
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `fresh`: no parameter, variable, or earlier interval has its name.
    fn check_interval(&self, iv: &IntervalDef, fresh: bool) -> Result<(), String> {
        check_identifier("interval", &iv.name)?;
        if !fresh {
            return Err(format!("name {} is declared more than once", iv.name));
        }
        for s in &iv.index {
            if !self.sets.contains_key(s) {
                return Err(format!("interval {} is indexed by {}", iv.name, self.unknown_set(s)));
            }
        }
        check_index_names("interval", &iv.name, &iv.index_names)
    }

    fn check_solve(&self) -> Result<(), String> {
        match (self.solve.method, &self.solve.objective) {
            (Method::Satisfy, Some(_)) => Err("solve.objective is not allowed when method is satisfy".into()),
            (Method::Minimize | Method::Maximize, None) => {
                Err("solve.objective is required when method is minimize or maximize".into())
            }
            _ => Ok(()),
        }
    }
}

fn check_set(name: &str, set: &SetDef) -> Result<(), String> {
    check_identifier("set", name)?;
    if let SetDef::List(items) = set {
        let unique: HashSet<_> = items.iter().collect();
        if unique.len() != items.len() {
            return Err(format!("set {name} has duplicate elements"));
        }
    }
    Ok(())
}

impl Problem {
    /// Checks that a parameter table's keys are elements of its index sets,
    /// level by level. (A misspelled key would otherwise go unread, and the
    /// element it was meant for take the default.)
    fn check_keys(
        &self,
        name: &str,
        index: &[String],
        keys: &[Keys],
        values: &Map<String, Value>,
    ) -> Result<(), String> {
        if let Some(key) = keys[0].stranger(values.keys()) {
            return Err(format!(
                "parameter {name}: the key {key:?} isn't an element of {} ({})",
                index[0],
                self.sets[&index[0]].describe()
            ));
        }
        for value in values.values() {
            match value {
                Value::Object(inner) if index.len() > 1 => self.check_keys(name, &index[1..], &keys[1..], inner)?,
                // null: no value, which as a variable's bound means none.
                Value::Number(_) | Value::Null if index.len() == 1 => {}
                _ => return Err(format!("parameter {name} values are not nested like its index")),
            }
        }
        Ok(())
    }
}

/// Checks the names an index list gives its indexes: identifiers, each once.
fn check_index_names(what: &str, item: &str, names: &[Option<String>]) -> Result<(), String> {
    for (k, name) in names.iter().enumerate() {
        let Some(name) = name else { continue };
        check_identifier("index", name).map_err(|e| format!("{what} {item}: {e}"))?;
        if names[..k].contains(&Some(name.clone())) {
            return Err(format!("{what} {item}: two indexes are named {name}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading the JSON. Every error says where in the file it is, as a JSON path
// (`constraints[2].table.tuples[0]`, counting from 0), with the name of the
// variable, interval, or constraint it is in. Reading doesn't stop at an
// error: it is noted in the `Sink`, a placeholder takes the value's place
// (None, which a field's reader turns into "absent" or a default), and the
// next field is read, so that one run reports everything.

/// The marks in `Sink::broken` for the sections that have no name.
const VARIABLES: &str = "$variables";
const SOLVE: &str = "$solve";

/// What reading a file found wrong.
#[derive(Default)]
struct Sink {
    issues: RefCell<Vec<Issue>>,
    /// See [`Reading::broken`].
    broken: RefCell<HashSet<String>>,
}

impl Sink {
    fn count(&self) -> usize {
        self.issues.borrow().len()
    }

    fn broken(&self, name: &str) {
        self.broken.borrow_mut().insert(name.to_string());
    }
}

/// A place in the file.
#[derive(Clone)]
struct At<'s> {
    path: String,
    /// The name of the item (variable, interval, constraint) it is in.
    name: Option<String>,
    sink: &'s Sink,
}

impl<'s> At<'s> {
    fn top(sink: &'s Sink) -> At<'s> {
        At { path: String::new(), name: None, sink }
    }

    fn key(&self, key: &str) -> At<'s> {
        let path = if self.path.is_empty() { key.to_string() } else { format!("{}.{key}", self.path) };
        At { path, name: self.name.clone(), sink: self.sink }
    }

    fn index(&self, i: usize) -> At<'s> {
        At { path: format!("{}[{i}]", self.path), name: self.name.clone(), sink: self.sink }
    }

    fn named(&self, name: Option<&str>) -> At<'s> {
        At { path: self.path.clone(), name: name.map(str::to_string), sink: self.sink }
    }

    /// Notes something wrong here (`variables[1].type (carrier): ...`); the
    /// value is None.
    fn report<T>(&self, problem: impl std::fmt::Display) -> Option<T> {
        let text = match (&self.name, self.path.is_empty()) {
            (_, true) => problem.to_string(),
            (Some(name), false) => format!("{} ({name}): {problem}", self.path),
            (None, false) => format!("{}: {problem}", self.path),
        };
        self.sink.issues.borrow_mut().push(Issue {
            place: (!self.path.is_empty()).then(|| self.path.clone()),
            name: self.name.clone(),
            text: format!("invalid problem file: {text}"),
            structural: true,
        });
        None
    }

    /// The value isn't what the format has here.
    fn expected<T>(&self, what: &str, found: &Value) -> Option<T> {
        // The commonest slip in generated files: a number in quotes.
        let hint = match found {
            Value::String(s) if s.trim().parse::<f64>().is_ok() && what.contains("number") => {
                " (write numbers without quotes)"
            }
            _ => "",
        };
        self.report(format!("expected {what}, found {}{hint}", describe(found)))
    }

    /// The item this is in can't be taken as written: errors about its uses
    /// would only repeat the one reported here.
    fn broken(&self) {
        if let Some(name) = &self.name {
            self.sink.broken(name);
        }
    }
}

/// A value, for "found ..." in messages.
fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) if s.chars().count() > 40 => format!("{:?}...", s.chars().take(40).collect::<String>()),
        Value::String(s) => format!("{s:?}"),
        Value::Array(_) => "a list".into(),
        Value::Object(_) => "an object".into(),
    }
}

/// `"a", "b", and "c"`.
fn quoted(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    match quoted.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// A JSON object of the file, with only the fields the format has there.
struct Fields<'a, 's> {
    map: &'a Map<String, Value>,
    at: At<'s>,
}

impl<'a, 's> Fields<'a, 's> {
    /// None (with a note) if `value` isn't an object. A field the format
    /// doesn't have there is noted, and otherwise ignored.
    fn new(value: &'a Value, at: At<'s>, allowed: &[&str]) -> Option<Fields<'a, 's>> {
        let Value::Object(map) = value else { return at.expected("an object", value) };
        for key in map.keys().filter(|k| !allowed.contains(&k.as_str())) {
            let meant = closest(key, allowed.iter().copied().filter(|a| !map.contains_key(*a)));
            at.report::<()>(match meant {
                Some(meant) => format!("unknown field {key:?}; did you mean {meant:?}?"),
                None => format!("unknown field {key:?} (the fields are {})", quoted(allowed)),
            });
        }
        Some(Fields { map, at })
    }

    /// The field with its place, if it is there (null counts as absent).
    fn get(&self, key: &str) -> Option<(&'a Value, At<'s>)> {
        self.map.get(key).filter(|v| !v.is_null()).map(|v| (v, self.at.key(key)))
    }

    /// The field's value if it is there and reads well.
    fn opt<T>(&self, key: &str, read: impl FnOnce(&'a Value, &At<'s>) -> Option<T>) -> Option<T> {
        self.get(key).and_then(|(v, at)| read(v, &at))
    }

    /// As `opt`, for a field that must be there.
    fn req<T>(&self, key: &str, read: impl FnOnce(&'a Value, &At<'s>) -> Option<T>) -> Option<T> {
        match self.get(key) {
            Some((v, at)) => read(v, &at),
            None => self.at.report(format!("missing {key:?}")),
        }
    }

    /// As `opt`, for a field without which the item isn't what the file
    /// meant (a variable's type): if it is there and doesn't read, the item
    /// is marked broken.
    fn shape<T>(&self, key: &str, read: impl FnOnce(&'a Value, &At<'s>) -> Option<T>) -> Option<T> {
        let (v, at) = self.get(key)?;
        let value = read(v, &at);
        if value.is_none() {
            self.at.broken();
        }
        value
    }
}

fn string(value: &Value, at: &At) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        other => at.expected("a string", other),
    }
}

fn number(value: &Value, at: &At) -> Option<f64> {
    match value.as_f64() {
        Some(x) if value.is_number() => Some(x),
        _ => at.expected("a number", value),
    }
}

fn integer(value: &Value, at: &At) -> Option<i64> {
    value.as_i64().or_else(|| at.expected("a whole number", value))
}

fn boolean(value: &Value, at: &At) -> Option<bool> {
    value.as_bool().or_else(|| at.expected("true or false", value))
}

/// A list, each item read with its place. None if any item doesn't read
/// (each is noted).
fn list<T>(value: &Value, at: &At, what: &str, read: impl Fn(&Value, &At) -> Option<T>) -> Option<Vec<T>> {
    let Value::Array(items) = value else { return at.expected(what, value) };
    let read: Vec<Option<T>> = items.iter().enumerate().map(|(i, item)| read(item, &at.index(i))).collect();
    read.into_iter().collect()
}

fn names(value: &Value, at: &At) -> Option<Vec<String>> {
    list(value, at, "a list of set names", string)
}

fn element(value: &Value, at: &At) -> Option<Elem> {
    match value {
        Value::String(s) => Some(Elem::Str(s.clone())),
        Value::Number(_) => integer(value, at).map(Elem::Int),
        other => at.expected("a set element (a string or a whole number)", other),
    }
}

/// One of the listed words.
fn word<T: Copy>(value: &Value, at: &At, words: &[(&str, T)]) -> Option<T> {
    let found = value.as_str().and_then(|s| words.iter().find(|w| w.0 == s));
    let all: Vec<&str> = words.iter().map(|w| w.0).collect();
    let one_of = || quoted(&all).replace(", and ", ", or ").replace(" and ", " or ");
    found.map(|w| w.1).or_else(|| at.expected(&one_of(), value))
}

/// A number, or an expression (a string that isn't just a number, which
/// would be a number in quotes by mistake).
fn bound(value: &Value, at: &At) -> Option<Bound> {
    match value {
        Value::Number(_) => number(value, at).map(Bound::Num),
        Value::String(s) if s.trim().parse::<f64>().is_err() => Some(Bound::Expr(s.clone())),
        other => at.expected("a number or an expression", other),
    }
}

/// An index list: set names, each with or without a name for its index
/// (`"TASKS"`, `"t in TASKS"`). Returns the sets and the names.
fn index_list(value: &Value, at: &At) -> Option<(Vec<String>, Vec<Option<String>>)> {
    let entries = names(value, at)?;
    Some(
        entries
            .into_iter()
            .map(|entry| match entry.split_once(" in ") {
                Some((index, set)) => (set.trim().to_string(), Some(index.trim().to_string())),
                None => (entry, None),
            })
            .unzip(),
    )
}

fn set_def(value: &Value, at: &At) -> Option<SetDef> {
    match value {
        Value::Array(_) => list(value, at, "a list of elements", element).map(SetDef::List),
        Value::Object(_) => {
            let f = Fields::new(value, at.clone(), &["from", "to"])?;
            let (from, to) = (f.req("from", integer), f.req("to", integer));
            Some(SetDef::Range(RangeDef { from: from?, to: to? }))
        }
        other => at.expected(r#"a list of elements or {"from": .., "to": ..}"#, other),
    }
}

fn param_def(value: &Value, at: &At) -> Option<ParamDef> {
    const WHAT: &str = r#"a number or a table {"index": [..], "values": {..}}"#;
    match value {
        Value::Number(_) => number(value, at).map(ParamDef::Scalar),
        Value::Object(_) => {
            let f = Fields::new(value, at.clone(), &["description", "index", "values", "default"])?;
            let values = f.req("values", |v, at| match v {
                Value::Object(map) => Some(map.clone()),
                other => at.expected("an object keyed by set elements", other),
            });
            let (index, default) = (f.req("index", names), f.opt("default", number));
            Some(ParamDef::Table(TableDef {
                description: f.opt("description", string),
                index: index?,
                values: values?,
                default,
            }))
        }
        other => at.expected(WHAT, other),
    }
}

fn var_def(value: &Value, at: &At) -> Option<VarDef> {
    const FIELDS: [&str; 8] = ["name", "description", "type", "index", "lower", "upper", "domain", "start"];
    let at = at.named(value.get("name").and_then(Value::as_str));
    let f = Fields::new(value, at.clone(), &FIELDS)?;
    let types = [("continuous", VarType::Continuous), ("integer", VarType::Integer), ("binary", VarType::Binary)];
    let domain = |v: &Value, at: &At| match v {
        Value::String(set) => Some(DomainDef::Set(set.clone())),
        Value::Array(_) => {
            let range = |r: &Value, at: &At| match list(r, at, "a range [low, high]", integer)?.as_slice() {
                &[low, high] => Some([low, high]),
                _ => at.expected("a range [low, high]", r),
            };
            list(v, at, "a list of ranges", range).map(DomainDef::Ranges)
        }
        other => at.expected("a list of ranges [[low, high], ..] or the name of a set", other),
    };
    // Without a type it is taken as continuous, and broken: what the file
    // says about it elsewhere can't be judged.
    let var_type = f.req("type", |v, at| word(v, at, &types)).unwrap_or_else(|| {
        at.broken();
        VarType::Continuous
    });
    let (index, index_names) = f.shape("index", index_list).unwrap_or_default();
    // A bound that doesn't read is left out: nothing else depends on it.
    let var = VarDef {
        name: f.req("name", string)?,
        description: f.opt("description", string),
        var_type,
        index,
        index_names,
        lower: f.opt("lower", bound),
        upper: f.opt("upper", bound),
        domain: f.shape("domain", domain),
        start: f.opt("start", bound),
    };
    Some(var)
}

fn interval_def(value: &Value, at: &At) -> Option<IntervalDef> {
    let at = at.named(value.get("name").and_then(Value::as_str));
    let f = Fields::new(value, at.clone(), &["name", "description", "index", "start", "duration", "presence"])?;
    let (name, start, duration) = (f.req("name", string), f.req("start", bound), f.req("duration", bound));
    let (index, index_names) = f.opt("index", index_list).unwrap_or_default();
    Some(IntervalDef {
        name: name?,
        description: f.opt("description", string),
        index,
        index_names,
        start: start?,
        duration: duration?,
        presence: f.opt("presence", string),
    })
}

/// What a `min`, `max`, or `abs` constraint kind of formats 1.1 and 1.2 said,
/// as the comparison that says it now: `makespan == max(j in JOBS) end[j]`.
fn retired_kind(kind: &str, spec: &Value) -> Option<String> {
    let (value, of) = (spec.get("value")?.as_str()?, spec.get("of")?.as_str()?.trim());
    let term = if kind == "abs" {
        format!("abs({of})")
    } else {
        // A list: [item | bindings], or [a, b, c].
        let inner = of.strip_prefix('[')?.strip_suffix(']')?;
        match inner.split_once('|') {
            Some((item, bindings)) => format!("{kind}({}) ({})", bindings.trim(), item.trim()),
            None => format!("{kind}({})", inner.trim()),
        }
    };
    Some(format!("{value} == {term}"))
}

fn constraint_def(value: &Value, at: &At, position: usize) -> Option<ConstraintDef> {
    const FIELDS: [&str; 10] = [
        "name",
        "description",
        "forall",
        "expr",
        "enforce_if",
        "all_different",
        "element",
        "table",
        "no_overlap",
        "cumulative",
    ];
    let at = at.named(value.get("name").and_then(Value::as_str));
    // The kinds that expressions replaced (format 1.3): say what to write.
    for kind in ["min", "max", "abs"] {
        if let Some(spec) = value.get(kind) {
            let now = retired_kind(kind, spec).unwrap_or_else(|| format!("value == {kind}(..)"));
            return at.report(format!(
                "the {kind:?} constraint kind is gone: {kind} is a function in expressions now, so write \
                 \"expr\": {now:?}"
            ));
        }
    }
    let f = Fields::new(value, at, &FIELDS)?;
    let element_def = |v: &Value, at: &At| {
        let f = Fields::new(v, at.clone(), &["index", "array", "value"])?;
        let index = f.req("index", |v, at| match v {
            Value::String(one) => Some(OneOrMany::One(one.clone())),
            Value::Array(_) => list(v, at, "a list of expressions", string).map(OneOrMany::Many),
            other => at.expected("an expression or a list of expressions", other),
        });
        let (array, value) = (f.req("array", string), f.req("value", string));
        Some(ElementDef { index: index?, array: array?, value: value? })
    };
    let table = |v: &Value, at: &At| {
        let f = Fields::new(v, at.clone(), &["vars", "tuples", "forbidden"])?;
        let tuple = |t: &Value, at: &At| list(t, at, "a tuple (a list of elements)", element);
        let vars = f.req("vars", string);
        let tuples = f.req("tuples", |v, at| list(v, at, "a list of tuples", tuple));
        Some(TableConstraintDef {
            vars: vars?,
            tuples: tuples?,
            forbidden: f.opt("forbidden", boolean).unwrap_or(false),
        })
    };
    let cumulative = |v: &Value, at: &At| {
        let f = Fields::new(v, at.clone(), &["intervals", "demands", "capacity"])?;
        let (intervals, demands, capacity) =
            (f.req("intervals", string), f.req("demands", string), f.req("capacity", string));
        Some(CumulativeDef { intervals: intervals?, demands: demands?, capacity: capacity? })
    };
    Some(ConstraintDef {
        position,
        name: f.opt("name", string),
        description: f.opt("description", string),
        forall: f.opt("forall", string),
        expr: f.opt("expr", string),
        all_different: f.opt("all_different", string),
        enforce_if: f.opt("enforce_if", string),
        element: f.opt("element", element_def),
        table: f.opt("table", table),
        no_overlap: f.opt("no_overlap", string),
        cumulative: f.opt("cumulative", cumulative),
    })
}

fn solve_def(value: &Value, at: &At) -> Option<Solve> {
    let f = Fields::new(value, at.clone(), &["method", "objective", "solutions"])?;
    let methods = [("satisfy", Method::Satisfy), ("minimize", Method::Minimize), ("maximize", Method::Maximize)];
    let solutions = |v: &Value, at: &At| match (v.as_str(), v.as_u64().map(usize::try_from)) {
        (Some("one"), _) => Some(Solutions::One),
        (Some("all"), _) => Some(Solutions::All),
        (_, Some(Ok(n))) if n >= 1 => Some(Solutions::UpTo(n)),
        _ => at.expected(r#""one", "all", or a number of solutions (1 or more)"#, v),
    };
    let method = f.req("method", |v, at| word(v, at, &methods));
    Some(Solve {
        method: method?,
        objective: f.opt("objective", string),
        solutions: f.opt("solutions", solutions).unwrap_or_default(),
    })
}

/// An object's entries by name, each read with its place. One that doesn't
/// read is left out and marked broken.
fn table<T>(value: &Value, at: &At, read: impl Fn(&Value, &At) -> Option<T>) -> BTreeMap<String, T> {
    let Value::Object(map) = value else {
        at.expected::<()>("an object keyed by name", value);
        return BTreeMap::new();
    };
    let entry = |(name, v): (&String, &Value)| {
        let value = read(v, &at.key(name));
        if value.is_none() {
            at.sink.broken(name);
        }
        Some((name.clone(), value?))
    };
    map.iter().filter_map(entry).collect()
}

/// A list of items (variables, intervals, constraints), each read with its
/// place and its position. `strict`: an item with anything wrong in it is
/// left out (and marked broken, if it has a name).
fn items<T>(
    value: &Value,
    at: &At,
    what: &str,
    strict: bool,
    read: impl Fn(&Value, &At, usize) -> Option<T>,
) -> Vec<T> {
    let Value::Array(items) = value else {
        at.expected::<()>(what, value);
        return Vec::new();
    };
    let item = |(i, v): (usize, &Value)| {
        let at = at.index(i).named(v.get("name").and_then(Value::as_str));
        let before = at.sink.count();
        let item = read(v, &at, i);
        if strict && at.sink.count() > before {
            at.broken();
            return None;
        }
        item
    };
    items.iter().enumerate().filter_map(item).collect()
}

/// The problem, with placeholders where something didn't read. None (with a
/// note) if the file isn't an object.
fn read_problem(value: &Value, sink: &Sink) -> Option<Problem> {
    const FIELDS: [&str; 8] =
        ["$schema", "version", "sets", "parameters", "variables", "intervals", "constraints", "solve"];
    let top = At::top(sink);
    if !value.is_object() {
        return top.report(format!("expected an object with {}, found {}", quoted(&FIELDS[1..]), describe(value)));
    }
    let f = Fields::new(value, top, &FIELDS)?;
    // In the file's order, which is the order the errors come out in.
    let version = f.req("version", string).unwrap_or_else(|| VERSION.into());
    let sets = f.opt("sets", |v, at| Some(table(v, at, set_def))).unwrap_or_default();
    let parameters = f.opt("parameters", |v, at| Some(table(v, at, param_def))).unwrap_or_default();
    let variables =
        f.req("variables", |v, at| Some(items(v, at, "a list of variables", false, |v, at, _| var_def(v, at))));
    if variables.is_none() {
        sink.broken(VARIABLES);
    }
    // A constraint or an interval with anything wrong in it is left out:
    // half of one would mean something else.
    let intervals =
        f.opt("intervals", |v, at| Some(items(v, at, "a list of intervals", true, |v, at, _| interval_def(v, at))));
    let constraints = f.req("constraints", |v, at| Some(items(v, at, "a list of constraints", true, constraint_def)));
    let solve = f.req("solve", solve_def).unwrap_or_else(|| {
        sink.broken(SOLVE);
        Solve { method: Method::Satisfy, objective: None, solutions: Solutions::One }
    });
    Some(Problem {
        schema: f.opt("$schema", string),
        version,
        sets,
        parameters,
        variables: variables.unwrap_or_default(),
        intervals: intervals.unwrap_or_default(),
        constraints: constraints.unwrap_or_default(),
        solve,
    })
}
