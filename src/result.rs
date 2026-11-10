//! Writes result files (docs/result-format.md). Written by hand rather than
//! through serde_json so that keys keep set order and small tables stay on
//! one line, matching the example files.

use crate::model::VarInfo;
use crate::problem::{Elem, VarType};
use std::fmt::Write;

/// How a solve ended.
///
/// `Infeasible` and `Timeout` are answers, not errors: the solve itself
/// succeeded. The statuses are those of the result format
/// (`docs/result-format.md`, also in [`crate::format::result`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Status {
    /// Everything requested was found and proven: an optimal solution (for
    /// nonlinear problems, a proven global optimum), a feasible one for a
    /// satisfaction problem, or every solution when all were requested.
    Complete,
    /// Every solution was requested and time ran out; the solutions found
    /// are valid, but there may be more.
    Incomplete,
    /// Time ran out with a solution that may not be optimal;
    /// [`SolveResult::objective_bound`](crate::SolveResult::objective_bound)
    /// says how far from optimal it can be.
    NotProvenOptimal,
    /// No solution exists.
    Infeasible,
    /// The objective can improve without limit.
    Unbounded,
    /// Time ran out before any solution was found.
    Timeout,
    /// Nonlinear problems: a local optimum that wasn't proven globally
    /// optimal in time (a better one may exist; the objective bound bounds
    /// it).
    LocallyOptimal,
    /// Nonlinear problems: no feasible point was found; the search converged
    /// to a local minimum of the infeasibility.
    LocallyInfeasible,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Status {
    /// The status as the result format writes it, e.g. `"not_proven_optimal"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Complete => "complete",
            Status::Incomplete => "incomplete",
            Status::NotProvenOptimal => "not_proven_optimal",
            Status::Infeasible => "infeasible",
            Status::Unbounded => "unbounded",
            Status::Timeout => "timeout",
            Status::LocallyOptimal => "locally_optimal",
            Status::LocallyInfeasible => "locally_infeasible",
        }
    }

    /// The result format version that has this status.
    fn version(self) -> &'static str {
        match self {
            Status::LocallyOptimal | Status::LocallyInfeasible => "1.1",
            _ => "1.0",
        }
    }
}

/// What a result file says besides its status and solutions.
#[derive(Default)]
pub struct Details<'a> {
    /// Bound on the optimal objective, in the problem's own sense.
    pub objective_bound: Option<f64>,
    /// Why the problem is infeasible (result format 1.2).
    pub conflict: Option<Conflict<'a>>,
    /// The variables an unbounded objective can run away along (1.2).
    pub unbounded: &'a [String],
}

/// Constraints that can't hold together, and variables with no value left.
pub struct Conflict<'a> {
    pub constraints: &'a [String],
    pub variables: &'a [String],
    pub minimal: bool,
}

impl Details<'_> {
    /// The result format version that has these fields, if not 1.0.
    fn version(&self) -> Option<&'static str> {
        (self.conflict.is_some() || !self.unbounded.is_empty()).then_some("1.2")
    }
}

fn list(names: &[String]) -> String {
    format!("[{}]", names.iter().map(|n| quote(n)).collect::<Vec<_>>().join(", "))
}

/// What the solver found, in terms of model columns.
pub struct Outcome {
    pub status: Status,
    pub solutions: Vec<Vec<f64>>,
    /// Bound on the optimal objective, in the problem's own sense.
    pub objective_bound: Option<f64>,
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn number(x: f64) -> String {
    if !x.is_finite() {
        return "null".into();
    }
    let x = if x.abs() < 1e-12 { 0.0 } else { x };
    if x.fract() == 0.0 && x.abs() < 1e15 { format!("{}", x as i64) } else { format!("{x}") }
}

/// A nested table of values keyed by index elements.
enum Node {
    Leaf(String),
    Table(Vec<(String, Node)>),
}

fn insert(table: &mut Vec<(String, Node)>, keys: &[Elem], value: String) {
    let k = keys[0].key();
    if keys.len() == 1 {
        table.push((k, Node::Leaf(value)));
        return;
    }
    if !matches!(table.last(), Some((last, Node::Table(_))) if *last == k) {
        table.push((k, Node::Table(Vec::new())));
    }
    if let Some((_, Node::Table(sub))) = table.last_mut() {
        insert(sub, &keys[1..], value);
    }
}

fn write_node(out: &mut String, node: &Node, indent: usize) {
    match node {
        Node::Leaf(v) => out.push_str(v),
        Node::Table(entries) if entries.iter().all(|(_, n)| matches!(n, Node::Leaf(_))) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, n)| match n {
                    Node::Leaf(v) => format!("{}: {v}", quote(k)),
                    Node::Table(_) => unreachable!(),
                })
                .collect();
            write!(out, "{{ {} }}", parts.join(", ")).unwrap();
        }
        Node::Table(entries) => {
            out.push_str("{\n");
            for (i, (k, n)) in entries.iter().enumerate() {
                write!(out, "{:w$}{}: ", "", quote(k), w = indent + 2).unwrap();
                write_node(out, n, indent + 2);
                out.push_str(if i + 1 < entries.len() { ",\n" } else { "\n" });
            }
            write!(out, "{:w$}}}", "", w = indent).unwrap();
        }
    }
}

/// Writes a result file. `vars` maps the problem's variables to columns;
/// each solution comes with its objective value (None for satisfy).
pub fn to_json(vars: &[VarInfo], status: Status, solutions: &[(Option<f64>, &[f64])], details: &Details) -> String {
    let mut out = String::new();
    writeln!(out, "{{\n  \"version\": {},", quote(details.version().unwrap_or(status.version()))).unwrap();
    writeln!(out, "  \"status\": {},", quote(status.as_str())).unwrap();
    if solutions.is_empty() {
        out.push_str("  \"solutions\": []");
    } else {
        out.push_str("  \"solutions\": [\n");
        for (s, (objective, x)) in solutions.iter().enumerate() {
            out.push_str("    {\n");
            if let Some(objective) = objective {
                writeln!(out, "      \"objective\": {},", number(*objective)).unwrap();
            }
            out.push_str("      \"variables\": {\n");
            for (v, var) in vars.iter().enumerate() {
                let fmt = |col: usize| match &var.symbolic {
                    // Value k stands for the set's k-th element (1-based).
                    Some(elems) => match &elems[(x[col].round() as usize).clamp(1, elems.len()) - 1] {
                        Elem::Int(i) => i.to_string(),
                        Elem::Str(s) => quote(s),
                    },
                    None if var.var_type == VarType::Continuous => number(x[col]),
                    None => number(x[col].round()),
                };
                let node = if var.indexed {
                    let mut table = Vec::new();
                    for (idx, col) in &var.elements {
                        insert(&mut table, idx, fmt(*col));
                    }
                    Node::Table(table)
                } else {
                    Node::Leaf(fmt(var.elements[0].1))
                };
                write!(out, "        {}: ", quote(&var.name)).unwrap();
                write_node(&mut out, &node, 8);
                out.push_str(if v + 1 < vars.len() { ",\n" } else { "\n" });
            }
            out.push_str("      }\n    }");
            out.push_str(if s + 1 < solutions.len() { ",\n" } else { "\n" });
        }
        out.push_str("  ]");
    }
    if let Some(b) = details.objective_bound {
        write!(out, ",\n  \"objective_bound\": {}", number(b)).unwrap();
    }
    if let Some(c) = &details.conflict {
        out.push_str(",\n  \"conflict\": { ");
        if !c.variables.is_empty() {
            write!(out, "\"variables\": {}, ", list(c.variables)).unwrap();
        }
        write!(out, "\"constraints\": {}, \"minimal\": {} }}", list(c.constraints), c.minimal).unwrap();
    }
    if !details.unbounded.is_empty() {
        write!(out, ",\n  \"unbounded\": {{ \"variables\": {} }}", list(details.unbounded)).unwrap();
    }
    out.push_str("\n}\n");
    out
}
