//! Writes result files (docs/result-format.md). Written by hand rather than
//! through serde_json so that keys keep set order and small tables stay on
//! one line, matching the example files.

use crate::model::{LinearModel, Sense};
use crate::problem::{Elem, VarType};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Complete,
    Incomplete,
    NotProvenOptimal,
    Infeasible,
    Unbounded,
    Timeout,
    /// Result format 1.1: a local optimum (nonlinear problems).
    #[allow(dead_code)] // produced by the nonlinear engine (not yet implemented)
    LocallyOptimal,
    /// Result format 1.1: no feasible point found; the search converged to a
    /// local minimum of infeasibility (nonlinear problems).
    #[allow(dead_code)]
    LocallyInfeasible,
}

impl Status {
    fn as_str(self) -> &'static str {
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

pub fn to_json(model: &LinearModel, outcome: &Outcome) -> String {
    let mut out = String::new();
    writeln!(out, "{{\n  \"version\": {},", quote(outcome.status.version())).unwrap();
    writeln!(out, "  \"status\": {},", quote(outcome.status.as_str())).unwrap();
    if outcome.solutions.is_empty() {
        out.push_str("  \"solutions\": []");
    } else {
        out.push_str("  \"solutions\": [\n");
        for (s, x) in outcome.solutions.iter().enumerate() {
            out.push_str("    {\n");
            if model.sense != Sense::Satisfy {
                writeln!(out, "      \"objective\": {},", number(model.objective_value(x))).unwrap();
            }
            out.push_str("      \"variables\": {\n");
            for (v, var) in model.vars.iter().enumerate() {
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
                out.push_str(if v + 1 < model.vars.len() { ",\n" } else { "\n" });
            }
            out.push_str("      }\n    }");
            out.push_str(if s + 1 < outcome.solutions.len() { ",\n" } else { "\n" });
        }
        out.push_str("  ]");
    }
    if let Some(b) = outcome.objective_bound {
        write!(out, ",\n  \"objective_bound\": {}", number(b)).unwrap();
    }
    out.push_str("\n}\n");
    out
}
