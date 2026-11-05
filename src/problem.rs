//! The problem file format (docs/problem-format.md), as deserialized from JSON,
//! plus the structural checks that JSON Schema would perform.

use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Problem {
    #[serde(rename = "$schema")]
    #[allow(dead_code)]
    pub schema: Option<String>,
    pub version: String,
    #[serde(default)]
    pub sets: BTreeMap<String, SetDef>,
    #[serde(default)]
    pub parameters: BTreeMap<String, ParamDef>,
    pub variables: Vec<VarDef>,
    /// Format 1.1: time intervals for no_overlap and cumulative.
    #[serde(default)]
    pub intervals: Vec<IntervalDef>,
    pub constraints: Vec<ConstraintDef>,
    pub solve: Solve,
}

/// A set element: a string or an integer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(untagged)]
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

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SetDef {
    List(Vec<Elem>),
    Range(RangeDef),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RangeDef {
    pub from: i64,
    pub to: i64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ParamDef {
    Scalar(f64),
    Table(TableDef),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDef {
    #[allow(dead_code)]
    pub description: Option<String>,
    pub index: Vec<String>,
    pub values: serde_json::Map<String, Value>,
    pub default: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VarType {
    Continuous,
    Integer,
    Binary,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Bound {
    Num(f64),
    Param(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VarDef {
    pub name: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub var_type: VarType,
    #[serde(default)]
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
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum DomainDef {
    Ranges(Vec<[i64; 2]>),
    Set(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConstraintDef {
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
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElementDef {
    pub index: OneOrMany,
    pub array: String,
    pub value: String,
}

/// One expression string, or a list of them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

/// `table`: the listed variables equal one of the tuples (or, forbidden, none).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableConstraintDef {
    pub vars: String,
    pub tuples: Vec<Vec<Elem>>,
    #[serde(default)]
    pub forbidden: bool,
}

/// An interval: [start, start + duration), present when `presence` is 1.
/// `start` and `duration` are a number, or the name of a variable or
/// parameter indexed by the same sets as the interval.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntervalDef {
    pub name: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    #[serde(default)]
    pub index: Vec<String>,
    pub start: Bound,
    pub duration: Bound,
    pub presence: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CumulativeDef {
    pub intervals: String,
    pub demands: String,
    pub capacity: String,
}

/// `min`, `max` (`of` is a list), or `abs` (`of` is an expression).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// The constraint's kind, or an error for a missing, repeated, or
    /// unsupported kind.
    pub fn kind(&self) -> Result<ConstraintKind<'_>, String> {
        let label = self.name.as_deref().unwrap_or("(unnamed)");
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Satisfy,
    Minimize,
    Maximize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Solutions {
    #[default]
    One,
    All,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Solve {
    pub method: Method,
    pub objective: Option<String>,
    #[serde(default)]
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
        let problem: Problem = serde_json::from_str(text).map_err(|e| format!("invalid problem file: {e}"))?;
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
