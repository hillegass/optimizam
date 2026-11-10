//! The problem file format (docs/problem-format.md): read from JSON by hand,
//! so that a structural error says where in the file it is
//! (`variables[1] (carrier): missing "type"`), plus the checks between
//! sections.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};

type R<T> = Result<T, String>;

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

#[derive(Debug, Clone)]
pub enum Bound {
    Num(f64),
    Param(String),
}

#[derive(Debug)]
pub struct VarDef {
    pub name: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    pub var_type: VarType,
    pub index: Vec<String>,
    pub lower: Option<Bound>,
    pub upper: Option<Bound>,
    pub domain: Option<DomainDef>,
    /// Format 1.2: a starting value for local solvers (a number, or a
    /// parameter indexed like the variable).
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
    /// Format 1.1: `value` equals the minimum of the list `of`.
    pub min: Option<AggregateDef>,
    /// Format 1.1: `value` equals the maximum of the list `of`.
    pub max: Option<AggregateDef>,
    /// Format 1.1: `value` equals the absolute value of the expression `of`.
    pub abs: Option<AggregateDef>,
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
/// `start` and `duration` are a number, or the name of a variable or
/// parameter indexed by the same sets as the interval.
#[derive(Debug)]
pub struct IntervalDef {
    pub name: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    pub index: Vec<String>,
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

/// `min`, `max` (`of` is a list), or `abs` (`of` is an expression).
#[derive(Debug)]
pub struct AggregateDef {
    pub value: String,
    pub of: String,
}

pub enum ConstraintKind<'a> {
    /// A relation, and the `enforce_if` literals if any.
    Expr(&'a str, Option<&'a str>),
    AllDifferent(&'a str),
    Element(&'a ElementDef),
    Table(&'a TableConstraintDef),
    Min(&'a AggregateDef),
    Max(&'a AggregateDef),
    Abs(&'a AggregateDef),
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
            self.min.is_some(),
            self.max.is_some(),
            self.abs.is_some(),
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
        } else if let Some(a) = &self.min {
            ConstraintKind::Min(a)
        } else if let Some(a) = &self.max {
            ConstraintKind::Max(a)
        } else if let Some(a) = &self.abs {
            ConstraintKind::Abs(a)
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

impl Problem {
    pub fn from_json(text: &str) -> Result<Problem, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| format!("invalid problem file: {e}"))?;
        let problem = read_problem(&value).map_err(|e| format!("invalid problem file: {e}"))?;
        problem.validate()?;
        Ok(problem)
    }

    /// Checks that JSON Schema would make, plus cross-references between sections.
    fn validate(&self) -> Result<(), String> {
        let v = &self.version;
        let minor_ok = v.strip_prefix("1.").is_some_and(|m| {
            !m.is_empty() && m.chars().all(|c| c.is_ascii_digit()) && (m == "0" || !m.starts_with('0'))
        });
        if !minor_ok {
            return Err(format!("unsupported version {v:?}; this program reads version 1.x"));
        }
        if self.variables.is_empty() {
            return Err("the problem must declare at least one variable".into());
        }

        let mut names = HashSet::new();
        for (name, set) in &self.sets {
            check_identifier("set", name)?;
            if let SetDef::List(items) = set {
                let unique: HashSet<_> = items.iter().collect();
                if unique.len() != items.len() {
                    return Err(format!("set {name} has duplicate elements"));
                }
            }
        }
        for (name, param) in &self.parameters {
            check_identifier("parameter", name)?;
            if let ParamDef::Table(t) = param {
                for s in &t.index {
                    if !self.sets.contains_key(s) {
                        return Err(format!("parameter {name} is indexed by unknown set {s}"));
                    }
                }
                if t.index.is_empty() {
                    return Err(format!("parameter {name} has an empty index list"));
                }
            }
            names.insert(name.as_str());
        }
        for var in &self.variables {
            check_identifier("variable", &var.name)?;
            if !names.insert(var.name.as_str()) {
                return Err(format!("name {} is declared more than once", var.name));
            }
            for s in &var.index {
                if !self.sets.contains_key(s) {
                    return Err(format!("variable {} is indexed by unknown set {s}", var.name));
                }
            }
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
                        None => return Err(format!("variable {}: domain refers to unknown set {set}", var.name)),
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
            for bound in [&var.lower, &var.upper, &var.start].into_iter().flatten() {
                if let Bound::Param(p) = bound {
                    check_bound_param(self, var, p)?;
                }
            }
        }
        for iv in &self.intervals {
            check_identifier("interval", &iv.name)?;
            if !names.insert(iv.name.as_str()) {
                return Err(format!("name {} is declared more than once", iv.name));
            }
            for s in &iv.index {
                if !self.sets.contains_key(s) {
                    return Err(format!("interval {} is indexed by unknown set {s}", iv.name));
                }
            }
            for (field, bound) in [("start", &iv.start), ("duration", &iv.duration)] {
                if let Bound::Param(name) = bound {
                    self.check_indexed_like(&iv.name, field, name, &iv.index, false)?;
                }
            }
            if let Some(p) = &iv.presence {
                self.check_indexed_like(&iv.name, "presence", p, &iv.index, true)?;
            }
        }
        for c in &self.constraints {
            if let Some(name) = &c.name {
                check_identifier("constraint", name)?;
            }
            c.kind()?;
        }
        match (self.solve.method, &self.solve.objective) {
            (Method::Satisfy, Some(_)) => return Err("solve.objective is not allowed when method is satisfy".into()),
            (Method::Minimize | Method::Maximize, None) => {
                return Err("solve.objective is required when method is minimize or maximize".into());
            }
            _ => {}
        }
        Ok(())
    }
}

impl Problem {
    /// Checks that an interval's `field` names a variable (or, unless
    /// `binary_var`, a parameter) indexed by the same sets as the interval; a
    /// presence must be a binary variable.
    fn check_indexed_like(
        &self,
        iv: &str,
        field: &str,
        name: &str,
        index: &[String],
        binary_var: bool,
    ) -> Result<(), String> {
        if let Some(v) = self.variables.iter().find(|v| v.name == name) {
            if v.index != index {
                return Err(format!("interval {iv}: {field} variable {name} must be indexed by the same sets"));
            }
            if binary_var && v.var_type != VarType::Binary {
                return Err(format!("interval {iv}: presence {name} must be a binary variable"));
            }
            return Ok(());
        }
        if binary_var {
            return Err(format!("interval {iv}: presence {name} is not a variable"));
        }
        match self.parameters.get(name) {
            None => Err(format!("interval {iv}: {field} refers to unknown name {name}")),
            Some(ParamDef::Scalar(_)) if index.is_empty() => Ok(()),
            Some(ParamDef::Table(t)) if t.index == index => Ok(()),
            Some(_) => Err(format!("interval {iv}: {field} parameter {name} must be indexed by the same sets")),
        }
    }
}

fn check_bound_param(problem: &Problem, var: &VarDef, p: &str) -> Result<(), String> {
    match problem.parameters.get(p) {
        None => Err(format!("variable {}: a bound or start refers to unknown parameter {p}", var.name)),
        Some(ParamDef::Scalar(_)) if var.index.is_empty() => Ok(()),
        Some(ParamDef::Table(t)) if t.index == var.index => Ok(()),
        Some(_) => Err(format!(
            "variable {}: bound or start parameter {p} must be indexed by the same sets as the variable",
            var.name
        )),
    }
}

// ---------------------------------------------------------------------------
// Reading the JSON. Every error says where in the file it is, as a JSON path
// (`constraints[2].table.tuples[0]`, counting from 0), with the name of the
// variable, interval, or constraint it is in.

/// A place in the file.
#[derive(Clone)]
struct At {
    path: String,
    /// The name of the item (variable, interval, constraint) it is in.
    name: Option<String>,
}

impl At {
    fn top() -> At {
        At { path: String::new(), name: None }
    }

    fn key(&self, key: &str) -> At {
        let path = if self.path.is_empty() { key.to_string() } else { format!("{}.{key}", self.path) };
        At { path, name: self.name.clone() }
    }

    fn index(&self, i: usize) -> At {
        At { path: format!("{}[{i}]", self.path), name: self.name.clone() }
    }

    fn named(&self, name: Option<&str>) -> At {
        At { path: self.path.clone(), name: name.map(str::to_string) }
    }

    /// An error here: `variables[1].type (carrier): ...`.
    fn err<T>(&self, problem: impl std::fmt::Display) -> R<T> {
        Err(match (&self.name, self.path.is_empty()) {
            (_, true) => problem.to_string(),
            (Some(name), false) => format!("{} ({name}): {problem}", self.path),
            (None, false) => format!("{}: {problem}", self.path),
        })
    }

    /// The value isn't what the format has here.
    fn expected<T>(&self, what: &str, found: &Value) -> R<T> {
        // The commonest slip in generated files: a number in quotes.
        let hint = match found {
            Value::String(s) if s.trim().parse::<f64>().is_ok() && what.contains("number") => {
                " (write numbers without quotes)"
            }
            _ => "",
        };
        self.err(format!("expected {what}, found {}{hint}", describe(found)))
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

/// The number of single-character edits between two names (to suggest the
/// field that a misspelled one was meant to be).
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

/// A JSON object of the file, with only the fields the format has there.
struct Fields<'a> {
    map: &'a Map<String, Value>,
    at: At,
}

impl<'a> Fields<'a> {
    fn new(value: &'a Value, at: At, allowed: &[&str]) -> R<Fields<'a>> {
        let Value::Object(map) = value else { return at.expected("an object", value) };
        if let Some(key) = map.keys().find(|k| !allowed.contains(&k.as_str())) {
            // One slip, or two in a long name ("args" isn't "abs").
            let near = |a: &&str| edit_distance(a, key) <= if key.len() >= 6 { 2 } else { 1 };
            let close = allowed.iter().find(|a| near(a) && !map.contains_key(**a));
            return at.err(match close {
                Some(meant) => format!("unknown field {key:?}; did you mean {meant:?}?"),
                None => format!("unknown field {key:?} (the fields are {})", quoted(allowed)),
            });
        }
        Ok(Fields { map, at })
    }

    /// The field with its place, if it is there (null counts as absent).
    fn get(&self, key: &str) -> Option<(&'a Value, At)> {
        self.map.get(key).filter(|v| !v.is_null()).map(|v| (v, self.at.key(key)))
    }

    fn required(&self, key: &str) -> R<(&'a Value, At)> {
        self.get(key).map_or_else(|| self.at.err(format!("missing {key:?}")), Ok)
    }

    fn opt<T>(&self, key: &str, read: impl FnOnce(&'a Value, &At) -> R<T>) -> R<Option<T>> {
        self.get(key).map(|(v, at)| read(v, &at)).transpose()
    }

    fn req<T>(&self, key: &str, read: impl FnOnce(&'a Value, &At) -> R<T>) -> R<T> {
        let (v, at) = self.required(key)?;
        read(v, &at)
    }
}

fn string(value: &Value, at: &At) -> R<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => at.expected("a string", other),
    }
}

fn number(value: &Value, at: &At) -> R<f64> {
    match value.as_f64() {
        Some(x) if value.is_number() => Ok(x),
        _ => at.expected("a number", value),
    }
}

fn integer(value: &Value, at: &At) -> R<i64> {
    value.as_i64().map_or_else(|| at.expected("a whole number", value), Ok)
}

fn boolean(value: &Value, at: &At) -> R<bool> {
    value.as_bool().map_or_else(|| at.expected("true or false", value), Ok)
}

/// A list, each item read with its place.
fn list<T>(value: &Value, at: &At, what: &str, read: impl Fn(&Value, &At) -> R<T>) -> R<Vec<T>> {
    let Value::Array(items) = value else { return at.expected(what, value) };
    items.iter().enumerate().map(|(i, item)| read(item, &at.index(i))).collect()
}

fn names(value: &Value, at: &At) -> R<Vec<String>> {
    list(value, at, "a list of set names", string)
}

fn element(value: &Value, at: &At) -> R<Elem> {
    match value {
        Value::String(s) => Ok(Elem::Str(s.clone())),
        Value::Number(_) => integer(value, at).map(Elem::Int),
        other => at.expected("a set element (a string or a whole number)", other),
    }
}

/// One of the listed words.
fn word<T: Copy>(value: &Value, at: &At, words: &[(&str, T)]) -> R<T> {
    let found = value.as_str().and_then(|s| words.iter().find(|w| w.0 == s));
    let all: Vec<&str> = words.iter().map(|w| w.0).collect();
    found.map_or_else(
        || at.expected(&quoted(&all).replace(", and ", ", or ").replace(" and ", " or "), value),
        |w| Ok(w.1),
    )
}

/// A number, or the name of a parameter (or, for intervals, a variable).
fn bound(value: &Value, at: &At) -> R<Bound> {
    match value {
        Value::Number(_) => number(value, at).map(Bound::Num),
        Value::String(s) if s.trim().parse::<f64>().is_err() => Ok(Bound::Param(s.clone())),
        other => at.expected("a number or a name", other),
    }
}

fn set_def(value: &Value, at: &At) -> R<SetDef> {
    match value {
        Value::Array(_) => list(value, at, "a list of elements", element).map(SetDef::List),
        Value::Object(_) => {
            let f = Fields::new(value, at.clone(), &["from", "to"])?;
            Ok(SetDef::Range(RangeDef { from: f.req("from", integer)?, to: f.req("to", integer)? }))
        }
        other => at.expected(r#"a list of elements or {"from": .., "to": ..}"#, other),
    }
}

fn param_def(value: &Value, at: &At) -> R<ParamDef> {
    const WHAT: &str = r#"a number or a table {"index": [..], "values": {..}}"#;
    match value {
        Value::Number(_) => number(value, at).map(ParamDef::Scalar),
        Value::Object(_) => {
            let f = Fields::new(value, at.clone(), &["description", "index", "values", "default"])?;
            let values = f.req("values", |v, at| match v {
                Value::Object(map) => Ok(map.clone()),
                other => at.expected("an object keyed by set elements", other),
            })?;
            Ok(ParamDef::Table(TableDef {
                description: f.opt("description", string)?,
                index: f.req("index", names)?,
                values,
                default: f.opt("default", number)?,
            }))
        }
        other => at.expected(WHAT, other),
    }
}

fn var_def(value: &Value, at: &At) -> R<VarDef> {
    const FIELDS: [&str; 8] = ["name", "description", "type", "index", "lower", "upper", "domain", "start"];
    let name = value.get("name").and_then(Value::as_str);
    let f = Fields::new(value, at.named(name), &FIELDS)?;
    let types = [("continuous", VarType::Continuous), ("integer", VarType::Integer), ("binary", VarType::Binary)];
    let domain = |v: &Value, at: &At| match v {
        Value::String(set) => Ok(DomainDef::Set(set.clone())),
        Value::Array(_) => {
            let range = |r: &Value, at: &At| match list(r, at, "a range [low, high]", integer)?.as_slice() {
                &[low, high] => Ok([low, high]),
                _ => at.expected("a range [low, high]", r),
            };
            list(v, at, "a list of ranges", range).map(DomainDef::Ranges)
        }
        other => at.expected("a list of ranges [[low, high], ..] or the name of a set", other),
    };
    Ok(VarDef {
        name: f.req("name", string)?,
        description: f.opt("description", string)?,
        var_type: f.req("type", |v, at| word(v, at, &types))?,
        index: f.opt("index", names)?.unwrap_or_default(),
        lower: f.opt("lower", bound)?,
        upper: f.opt("upper", bound)?,
        domain: f.opt("domain", domain)?,
        start: f.opt("start", bound)?,
    })
}

fn interval_def(value: &Value, at: &At) -> R<IntervalDef> {
    let name = value.get("name").and_then(Value::as_str);
    let f = Fields::new(value, at.named(name), &["name", "description", "index", "start", "duration", "presence"])?;
    Ok(IntervalDef {
        name: f.req("name", string)?,
        description: f.opt("description", string)?,
        index: f.opt("index", names)?.unwrap_or_default(),
        start: f.req("start", bound)?,
        duration: f.req("duration", bound)?,
        presence: f.opt("presence", string)?,
    })
}

fn constraint_def(value: &Value, at: &At, position: usize) -> R<ConstraintDef> {
    const FIELDS: [&str; 13] = [
        "name",
        "description",
        "forall",
        "expr",
        "enforce_if",
        "all_different",
        "element",
        "table",
        "min",
        "max",
        "abs",
        "no_overlap",
        "cumulative",
    ];
    let name = value.get("name").and_then(Value::as_str);
    let f = Fields::new(value, at.named(name), &FIELDS)?;
    let aggregate = |v: &Value, at: &At| {
        let f = Fields::new(v, at.clone(), &["value", "of"])?;
        Ok(AggregateDef { value: f.req("value", string)?, of: f.req("of", string)? })
    };
    let element_def = |v: &Value, at: &At| {
        let f = Fields::new(v, at.clone(), &["index", "array", "value"])?;
        let index = f.req("index", |v, at| match v {
            Value::String(one) => Ok(OneOrMany::One(one.clone())),
            Value::Array(_) => list(v, at, "a list of expressions", string).map(OneOrMany::Many),
            other => at.expected("an expression or a list of expressions", other),
        })?;
        Ok(ElementDef { index, array: f.req("array", string)?, value: f.req("value", string)? })
    };
    let table = |v: &Value, at: &At| {
        let f = Fields::new(v, at.clone(), &["vars", "tuples", "forbidden"])?;
        let tuple = |t: &Value, at: &At| list(t, at, "a tuple (a list of elements)", element);
        Ok(TableConstraintDef {
            vars: f.req("vars", string)?,
            tuples: f.req("tuples", |v, at| list(v, at, "a list of tuples", tuple))?,
            forbidden: f.opt("forbidden", boolean)?.unwrap_or(false),
        })
    };
    let cumulative = |v: &Value, at: &At| {
        let f = Fields::new(v, at.clone(), &["intervals", "demands", "capacity"])?;
        Ok(CumulativeDef {
            intervals: f.req("intervals", string)?,
            demands: f.req("demands", string)?,
            capacity: f.req("capacity", string)?,
        })
    };
    Ok(ConstraintDef {
        position,
        name: f.opt("name", string)?,
        description: f.opt("description", string)?,
        forall: f.opt("forall", string)?,
        expr: f.opt("expr", string)?,
        all_different: f.opt("all_different", string)?,
        enforce_if: f.opt("enforce_if", string)?,
        element: f.opt("element", element_def)?,
        table: f.opt("table", table)?,
        min: f.opt("min", aggregate)?,
        max: f.opt("max", aggregate)?,
        abs: f.opt("abs", aggregate)?,
        no_overlap: f.opt("no_overlap", string)?,
        cumulative: f.opt("cumulative", cumulative)?,
    })
}

fn solve_def(value: &Value, at: &At) -> R<Solve> {
    let f = Fields::new(value, at.clone(), &["method", "objective", "solutions"])?;
    let methods = [("satisfy", Method::Satisfy), ("minimize", Method::Minimize), ("maximize", Method::Maximize)];
    let solutions = |v: &Value, at: &At| match (v.as_str(), v.as_u64().map(usize::try_from)) {
        (Some("one"), _) => Ok(Solutions::One),
        (Some("all"), _) => Ok(Solutions::All),
        (_, Some(Ok(n))) if n >= 1 => Ok(Solutions::UpTo(n)),
        _ => at.expected(r#""one", "all", or a number of solutions (1 or more)"#, v),
    };
    Ok(Solve {
        method: f.req("method", |v, at| word(v, at, &methods))?,
        objective: f.opt("objective", string)?,
        solutions: f.opt("solutions", solutions)?.unwrap_or_default(),
    })
}

/// An object's entries, each read with its place, by name.
fn table<T>(value: &Value, at: &At, read: impl Fn(&Value, &At) -> R<T>) -> R<BTreeMap<String, T>> {
    let Value::Object(map) = value else { return at.expected("an object keyed by name", value) };
    map.iter().map(|(name, v)| Ok((name.clone(), read(v, &at.key(name))?))).collect()
}

fn read_problem(value: &Value) -> R<Problem> {
    const FIELDS: [&str; 8] =
        ["$schema", "version", "sets", "parameters", "variables", "intervals", "constraints", "solve"];
    if !value.is_object() {
        return Err(format!("expected an object with {}, found {}", quoted(&FIELDS[1..]), describe(value)));
    }
    let f = Fields::new(value, At::top(), &FIELDS)?;
    let constraints = f.req("constraints", |v, at| {
        let Value::Array(items) = v else { return at.expected("a list of constraints", v) };
        items.iter().enumerate().map(|(i, c)| constraint_def(c, &at.index(i), i)).collect::<R<Vec<_>>>()
    })?;
    Ok(Problem {
        schema: f.opt("$schema", string)?,
        version: f.req("version", string)?,
        sets: f.opt("sets", |v, at| table(v, at, set_def))?.unwrap_or_default(),
        parameters: f.opt("parameters", |v, at| table(v, at, param_def))?.unwrap_or_default(),
        variables: f.req("variables", |v, at| list(v, at, "a list of variables", var_def))?,
        intervals: f.opt("intervals", |v, at| list(v, at, "a list of intervals", interval_def))?.unwrap_or_default(),
        constraints,
        solve: f.req("solve", solve_def)?,
    })
}
