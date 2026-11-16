//! Writes result files (docs/result-format.md). Written by hand rather than
//! through serde_json so that keys keep set order and small tables stay on
//! one line, matching the example files.

use crate::model::VarInfo;
use crate::problem::{Elem, VarType};
use std::fmt::Write;

/// How a solve ended.
///
/// `Infeasible` and `Unknown` are answers, not errors: the solve itself
/// succeeded. The statuses are those of the result format
/// (`docs/result-format.md`, also in [`crate::format::result`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Status {
    /// Everything requested was found and proven: an optimal solution (for
    /// nonlinear problems, a proven global optimum), a feasible one for a
    /// satisfaction problem, or every solution when all were requested.
    Complete,
    /// Every solution was requested and time ran out, or as many as
    /// [`Model::solution_limit`](crate::Model::solution_limit) asked for
    /// were found; the solutions found are valid, but there may be more.
    Incomplete,
    /// Time ran out with a solution that may not be optimal;
    /// [`SolveResult::objective_bound`](crate::SolveResult::objective_bound)
    /// says how far from optimal it can be.
    NotProvenOptimal,
    /// No solution exists.
    Infeasible,
    /// The objective can improve without limit.
    Unbounded,
    /// No solution was found and nothing was proven: the search stopped
    /// first, at the time limit or for another
    /// [`stop_reason`](crate::SolveResult::stop_reason). Whether a solution
    /// exists isn't known.
    Unknown,
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
    /// Whether the result has a solution: `Complete`, `Incomplete`,
    /// `NotProvenOptimal`, and `LocallyOptimal` do
    /// ([`SolveResult::best`](crate::SolveResult::best) is then Some).
    ///
    /// ```
    /// use optimizam::Status;
    ///
    /// assert!(Status::NotProvenOptimal.has_solution() && !Status::NotProvenOptimal.is_proven());
    /// assert!(Status::Infeasible.is_proven() && !Status::Infeasible.has_solution());
    /// assert!(!Status::Unknown.has_solution() && !Status::Unknown.is_proven());
    /// ```
    pub fn has_solution(self) -> bool {
        matches!(self, Status::Complete | Status::Incomplete | Status::NotProvenOptimal | Status::LocallyOptimal)
    }

    /// Whether the answer is final, so that a longer run would say the
    /// same: `Complete` (what was asked for was found and proven),
    /// `Infeasible`, and `Unbounded`. The others say how far a search got:
    /// a solution that may not be the best, some of the solutions, or
    /// nothing yet.
    pub fn is_proven(self) -> bool {
        matches!(self, Status::Complete | Status::Infeasible | Status::Unbounded)
    }

    /// The status as the result format writes it, e.g. `"not_proven_optimal"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Complete => "complete",
            Status::Incomplete => "incomplete",
            Status::NotProvenOptimal => "not_proven_optimal",
            Status::Infeasible => "infeasible",
            Status::Unbounded => "unbounded",
            Status::Unknown => "unknown",
            Status::LocallyOptimal => "locally_optimal",
            Status::LocallyInfeasible => "locally_infeasible",
        }
    }
}

/// The result format's version: every result file says this one.
pub const VERSION: &str = "1.3";

/// A constraint with no more slack than this is binding (`omit_slack`).
const BINDING: f64 = 1e-6;

/// What a result file says besides its status and solutions.
#[derive(Default)]
pub struct Details<'a> {
    /// What the problem is called, if it has a name.
    pub name: Option<&'a str>,
    /// "minimize", "maximize", or "satisfy": which way `objective_bound`
    /// bounds, without the problem file at hand.
    pub sense: &'a str,
    /// Leave the zero entries out of indexed variables and reduced costs.
    pub omit_zeros: bool,
    /// List only the constraints without slack.
    pub omit_slack: bool,
    /// Bound on the optimal objective, in the problem's own sense.
    pub objective_bound: Option<f64>,
    /// `|objective - bound| / max(1, |objective|)`, with a solution and a bound.
    pub gap: Option<f64>,
    /// Why the search stopped before it finished, if it did.
    pub stop_reason: Option<&'a str>,
    /// Why the problem is infeasible.
    pub conflict: Option<Conflict<'a>>,
    /// The variables an unbounded objective can run away along.
    pub unbounded: &'a [String],
    pub statistics: Statistics<'a>,
}

/// What the solve took.
#[derive(Default)]
pub struct Statistics<'a> {
    /// The engine the problem went to; None for an unfinished solve.
    pub engine: Option<&'a str>,
    /// The threads its search could use.
    pub threads: Option<usize>,
    /// Seconds, from the start of the solve.
    pub time: f64,
    /// What the engine branches on (see docs/result-format.md); None when it
    /// has no such count.
    pub nodes: Option<u64>,
}

/// Constraints that can't hold together, and variables with no value left.
pub struct Conflict<'a> {
    pub constraints: &'a [String],
    pub variables: &'a [String],
    pub minimal: bool,
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

/// Puts `value` at `keys`, one level of tables for each key. `in_order`:
/// the keys come as a product of sets does, so an entry's table is the last
/// one or a new one (a variable's elements); otherwise it is looked for (a
/// constraint's members, which come in the order they were added).
fn insert(table: &mut Vec<(String, Node)>, keys: &[Elem], value: String, in_order: bool) {
    let k = keys[0].key();
    if keys.len() == 1 {
        table.push((k, Node::Leaf(value)));
        return;
    }
    let is = |entry: &(String, Node)| entry.0 == k && matches!(entry.1, Node::Table(_));
    let at = match table.last() {
        Some(last) if is(last) => table.len() - 1,
        _ => match (!in_order).then(|| table.iter().position(is)).flatten() {
            Some(at) => at,
            None => {
                table.push((k, Node::Table(Vec::new())));
                table.len() - 1
            }
        },
    };
    if let (_, Node::Table(sub)) = &mut table[at] {
        insert(sub, &keys[1..], value, in_order);
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

/// A constraint as a solution of a result file reports it.
pub struct ConstraintOut<'a> {
    /// The name its family goes by (its own, for a constraint by itself).
    pub family: &'a str,
    /// Its key in the family: empty for a constraint by itself.
    pub key: &'a [Elem],
    /// The slack the solution leaves; None if the constraint isn't enforced in it.
    pub slack: Option<f64>,
    /// The dual, in a linear program.
    pub dual: Option<f64>,
}

/// One solution of a result file.
pub struct SolutionOut<'a> {
    /// The objective value (None for satisfy).
    pub objective: Option<f64>,
    /// A value for every column.
    pub values: &'a [f64],
    /// The comparison constraints, if they are reported.
    pub constraints: Option<Vec<ConstraintOut<'a>>>,
    /// A linear program's reduced costs, by column.
    pub reduced_costs: Option<&'a [f64]>,
}

/// Writes `"key": { name: value or table, ... }` for every variable, each
/// column's entry written by `entry`. `keep` says which entries of indexed
/// variables are written.
fn write_variables(
    out: &mut String,
    key: &str,
    vars: &[VarInfo],
    entry: &dyn Fn(&VarInfo, usize) -> String,
    keep: &dyn Fn(usize) -> bool,
) {
    writeln!(out, "      {}: {{", quote(key)).unwrap();
    for (v, var) in vars.iter().enumerate() {
        let node = if var.indexed {
            let mut table = Vec::new();
            for (idx, col) in var.elements.iter().filter(|e| keep(e.1)) {
                insert(&mut table, idx, entry(var, *col), true);
            }
            Node::Table(table)
        } else {
            Node::Leaf(entry(var, var.elements[0].1))
        };
        write!(out, "        {}: ", quote(&var.name)).unwrap();
        write_node(out, &node, 8);
        out.push_str(if v + 1 < vars.len() { ",\n" } else { "\n" });
    }
    out.push_str("      }");
}

/// Writes `"constraints": { name: entry or table, ... }`: each family of
/// constraints under its name, as a table with a level for each element of
/// its members' keys, as an indexed variable's values are laid out; a
/// constraint by itself is its entry. `binding_only` leaves out the
/// constraints with slack, and a family with none left.
fn write_constraints(out: &mut String, constraints: &[ConstraintOut], binding_only: bool) {
    let entry = |c: &ConstraintOut| {
        let mut fields = Vec::new();
        fields.push(match c.slack {
            Some(slack) => format!("\"slack\": {}", number(slack)),
            None => "\"enforced\": false".to_string(),
        });
        fields.extend(c.dual.map(|d| format!("\"dual\": {}", number(d))));
        format!("{{ {} }}", fields.join(", "))
    };
    // The families, in the order of their first members.
    let mut families: Vec<(&str, Vec<&ConstraintOut>)> = Vec::new();
    let mut place: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for c in constraints {
        let at = *place.entry(c.family).or_insert(families.len());
        if at == families.len() {
            families.push((c.family, Vec::new()));
        }
        families[at].1.push(c);
    }
    let mut entries: Vec<(String, Node)> = Vec::new();
    for (family, members) in &families {
        let binding = |c: &&&ConstraintOut| !binding_only || c.slack.is_some_and(|s| s <= BINDING);
        let kept: Vec<&&ConstraintOut> = members.iter().filter(binding).collect();
        let depth = members[0].key.len();
        if depth > 0 && members.iter().all(|c| c.key.len() == depth) {
            let mut table = Vec::new();
            for c in &kept {
                insert(&mut table, c.key, entry(c), false);
            }
            if !table.is_empty() {
                entries.push((family.to_string(), Node::Table(table)));
            }
        } else {
            // A constraint by itself; or members whose keys make no table
            // (some with a key and some without), each by its whole name.
            for c in kept {
                let key: Vec<String> = c.key.iter().map(Elem::key).collect();
                let name = if key.is_empty() { family.to_string() } else { format!("{family}[{}]", key.join(",")) };
                entries.push((name, Node::Leaf(entry(c))));
            }
        }
    }
    out.push_str(",\n      \"constraints\": {");
    for (k, (name, node)) in entries.iter().enumerate() {
        write!(out, "\n        {}: ", quote(name)).unwrap();
        write_node(out, node, 8);
        out.push_str(if k + 1 < entries.len() { "," } else { "" });
    }
    out.push_str(if entries.is_empty() { "}" } else { "\n      }" });
}

/// Writes a result file. `vars` maps the problem's variables to columns.
pub fn to_json(vars: &[VarInfo], status: Status, solutions: &[SolutionOut], details: &Details) -> String {
    let mut out = String::new();
    writeln!(out, "{{\n  \"version\": {},", quote(VERSION)).unwrap();
    if let Some(name) = details.name {
        writeln!(out, "  \"name\": {},", quote(name)).unwrap();
    }
    writeln!(out, "  \"status\": {},", quote(status.as_str())).unwrap();
    writeln!(out, "  \"sense\": {},", quote(details.sense)).unwrap();
    if solutions.is_empty() {
        out.push_str("  \"solutions\": []");
    } else {
        out.push_str("  \"solutions\": [\n");
        for (s, solution) in solutions.iter().enumerate() {
            let x = solution.values;
            out.push_str("    {\n");
            if let Some(objective) = solution.objective {
                writeln!(out, "      \"objective\": {},", number(objective)).unwrap();
            }
            let value = |var: &VarInfo, col: usize| match &var.symbolic {
                // Value k stands for the set's k-th element (1-based).
                Some(elems) => match &elems[(x[col].round() as usize).clamp(1, elems.len()) - 1] {
                    Elem::Int(i) => i.to_string(),
                    Elem::Str(s) => quote(s),
                },
                None if var.var_type == VarType::Continuous => number(x[col]),
                None => number(x[col].round()),
            };
            // A symbolic variable's value is an element, never a zero.
            let symbolic: Vec<usize> =
                vars.iter().filter(|v| v.symbolic.is_some()).flat_map(|v| v.elements.iter().map(|e| e.1)).collect();
            let keep = |col: usize| !details.omit_zeros || x[col] != 0.0 || symbolic.contains(&col);
            write_variables(&mut out, "variables", vars, &value, &keep);
            if let Some(constraints) = &solution.constraints {
                write_constraints(&mut out, constraints, details.omit_slack);
            }
            if let Some(reduced_costs) = solution.reduced_costs {
                out.push_str(",\n");
                let keep = |col: usize| !details.omit_zeros || reduced_costs[col] != 0.0;
                write_variables(&mut out, "reduced_costs", vars, &|_, col| number(reduced_costs[col]), &keep);
            }
            out.push_str("\n    }");
            out.push_str(if s + 1 < solutions.len() { ",\n" } else { "\n" });
        }
        out.push_str("  ]");
    }
    let omitted: Vec<&str> = [(details.omit_zeros, "\"zeros\""), (details.omit_slack, "\"slack\"")]
        .iter()
        .filter(|o| o.0)
        .map(|o| o.1)
        .collect();
    if !omitted.is_empty() {
        write!(out, ",\n  \"omitted\": [{}]", omitted.join(", ")).unwrap();
    }
    if let Some(b) = details.objective_bound {
        write!(out, ",\n  \"objective_bound\": {}", number(b)).unwrap();
    }
    if let Some(gap) = details.gap {
        write!(out, ",\n  \"gap\": {}", number(gap)).unwrap();
    }
    if let Some(reason) = details.stop_reason {
        write!(out, ",\n  \"stop_reason\": {}", quote(reason)).unwrap();
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
    let stats = &details.statistics;
    let mut fields = Vec::new();
    fields.extend(stats.engine.map(|e| format!("\"engine\": {}", quote(e))));
    fields.extend(stats.threads.map(|t| format!("\"threads\": {t}")));
    // To the microsecond.
    fields.push(format!("\"time\": {}", number((stats.time * 1e6).round() / 1e6)));
    fields.extend(stats.nodes.map(|n| format!("\"nodes\": {n}")));
    write!(out, ",\n  \"statistics\": {{ {} }}", fields.join(", ")).unwrap();
    out.push_str("\n}\n");
    out
}
