//! The solver's model: one column per variable element, one row per
//! constraint instance. Rows and the objective are affine, except that
//! `expr` constraints (without `enforce_if`) and the objective may have a
//! nonlinear part, kept as a node of an expression graph
//! ([`LinearModel::nonlinear`]). Problem files and builder models both
//! reach it through an [`Assembly`] (`api/lower.rs`), which encodes the
//! format 1.1 constraint kinds.

use crate::encode::{Columns, Encoder, Global};
use crate::nlp::graph::{Graph, Node};
use crate::problem::{Elem, Problem, SetDef, VarType};
use std::collections::HashMap;

pub const INF: f64 = f64::INFINITY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sense {
    Satisfy,
    Minimize,
    Maximize,
}

/// One declared variable and the columns of its elements, in index order.
#[derive(Clone, Debug)]
pub struct VarInfo {
    pub name: String,
    pub indexed: bool,
    pub var_type: VarType,
    pub elements: Vec<(Vec<Elem>, usize)>,
    /// For a symbolic set-valued variable: its set's elements. A column's
    /// value k (1-based) stands for element k.
    pub symbolic: Option<Vec<Elem>>,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub coefs: Vec<(usize, f64)>,
    pub lower: f64,
    pub upper: f64,
}

#[derive(Clone, Debug)]
pub struct LinearModel {
    pub col_lower: Vec<f64>,
    pub col_upper: Vec<f64>,
    pub col_integer: Vec<bool>,
    /// Allowed integer ranges for columns with a gapped `domain`.
    pub col_domain: Vec<Option<Vec<(i64, i64)>>>,
    /// Starting values (`start`) of the problem's columns, for the nonlinear
    /// engine (shorter than the columns when extra columns were added).
    pub col_start: Vec<Option<f64>>,
    pub rows: Vec<Row>,
    /// The builder constraint (its place among the model's items) that each
    /// of the first rows is; the rows after them are the encodings'.
    pub row_items: Vec<usize>,
    /// Objective in the problem's own sense; zero for satisfy.
    pub obj: Vec<f64>,
    pub obj_offset: f64,
    pub sense: Sense,
    /// Every solution (every optimal one) is wanted, or as many as
    /// `solution_limit` says.
    pub want_all: bool,
    pub solution_limit: Option<usize>,
    pub vars: Vec<VarInfo>,
    /// Set when a constraint with no variables is violated, e.g. `0 >= 1`.
    pub violated_constant_row: Option<String>,
    /// The problem uses a format 1.1 constraint kind (such as
    /// `all_different`), which the CP engine is suited to. Its constraints
    /// are encoded here as linear rows on extra (unreported) columns, except
    /// for `scheduling`.
    pub uses_cp_kinds: bool,
    /// `no_overlap` and `cumulative` instances, and `all_different` instances
    /// with large multi-column items (see `encode.rs`), not yet encoded: the
    /// CP engine propagates them natively, and
    /// [`LinearModel::encode_scheduling`] encodes them for the MILP engine.
    pub scheduling: Vec<Global>,
    /// The columns of `abs`, `min`, and `max` terms that rows hold from one
    /// side only, each after the ones its items use.
    pub loose: Vec<Loose>,
    /// Nonlinear rows and the objective's nonlinear part, if there are any.
    pub nonlinear: Option<Nonlinear>,
}

/// A row `lower <= coefs * x + node(x) <= upper`.
#[derive(Clone, Debug)]
pub struct NonlinearRow {
    pub label: String,
    pub coefs: Vec<(usize, f64)>,
    pub node: Node,
    pub lower: f64,
    pub upper: f64,
}

/// The nonlinear parts of a model. The objective is
/// `obj * x + obj_offset + objective(x)`, in the problem's own sense.
#[derive(Clone, Debug)]
pub struct Nonlinear {
    pub graph: Graph,
    pub rows: Vec<NonlinearRow>,
    pub objective: Option<Node>,
}

impl Nonlinear {
    /// Where the first nonlinear term is, for messages.
    pub fn first_place(&self) -> String {
        match self.rows.first() {
            Some(r) => format!("constraint {}", r.label),
            None => "the objective".into(),
        }
    }
}

impl LinearModel {
    pub fn num_cols(&self) -> usize {
        self.col_lower.len()
    }

    /// Encodes the `scheduling` instances as linear rows (on extra columns).
    pub fn encode_scheduling(&mut self) -> Result<(), String> {
        let scheduling = std::mem::take(&mut self.scheduling);
        let cols = Columns {
            lower: &mut self.col_lower,
            upper: &mut self.col_upper,
            integer: &mut self.col_integer,
            domain: &mut self.col_domain,
        };
        let mut encoder = Encoder::new(cols, &mut self.rows, self.want_all);
        for g in &scheduling {
            if let Some(v) = encoder.encode(g)?
                && self.violated_constant_row.is_none()
            {
                self.violated_constant_row = Some(v);
            }
        }
        self.obj.resize(self.col_lower.len(), 0.0);
        Ok(())
    }

    /// `x` with the loose columns at their terms' values. A solution may
    /// leave such a column off its term (a maximum above every item) where
    /// that costs nothing, or in a solution that isn't optimal; moving it
    /// onto the term keeps every row and can only improve the objective.
    pub fn settled<'a>(&self, x: &'a [f64]) -> std::borrow::Cow<'a, [f64]> {
        if self.loose.is_empty() || self.loose.iter().any(|piece| piece.col >= x.len()) {
            return x.into();
        }
        let mut x = x.to_vec();
        for piece in &self.loose {
            let values = piece.items.iter().map(|a| a.constant + a.coefs.iter().map(|&(j, c)| c * x[j]).sum::<f64>());
            x[piece.col] = match piece.term {
                Term::Abs => values.map(f64::abs).fold(0.0, f64::max),
                Term::Max => values.fold(-INF, f64::max),
                Term::Min => values.fold(INF, f64::min),
            };
        }
        x.into()
    }

    pub fn objective_value(&self, x: &[f64]) -> f64 {
        let nonlinear = match &self.nonlinear {
            Some(Nonlinear { graph, objective: Some(node), .. }) => graph.value(*node, x),
            _ => 0.0,
        };
        self.obj_offset + self.obj.iter().zip(x).map(|(c, v)| c * v).sum::<f64>() + nonlinear
    }
}

/// A piecewise linear term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Term {
    Abs,
    Min,
    Max,
}

impl Term {
    pub fn name(self) -> &'static str {
        match self {
            Term::Abs => "abs",
            Term::Min => "min",
            Term::Max => "max",
        }
    }
}

/// A column that stands for a piecewise linear term of `items` (for `abs`,
/// one item) and that rows hold from one side only: at or above an absolute
/// value or a maximum, at or below a minimum.
#[derive(Debug, Clone)]
pub struct Loose {
    pub col: usize,
    pub items: Vec<crate::encode::Affine>,
    pub term: Term,
}

/// Why a nonlinear expression is rejected where only affine ones may appear.
pub(crate) const NOT_AFFINE: &str = "only expr constraints without enforce_if, and the objective, may be nonlinear";

pub(crate) fn build_sets(problem: &Problem) -> Result<HashMap<&str, Vec<Elem>>, String> {
    let mut sets = HashMap::new();
    for (name, def) in &problem.sets {
        let elems = match def {
            SetDef::List(v) => v.clone(),
            SetDef::Range(r) => {
                if r.from > r.to {
                    return Err(format!("set {name}: range from {} to {} is empty", r.from, r.to));
                }
                (r.from..=r.to).map(Elem::Int).collect()
            }
        };
        sets.insert(name.as_str(), elems);
    }
    Ok(sets)
}

/// Every combination of elements of the named sets, in row-major order.
pub(crate) fn product(sets: &HashMap<&str, Vec<Elem>>, names: &[String]) -> Vec<Vec<Elem>> {
    let mut out = vec![Vec::new()];
    for n in names {
        out = out
            .into_iter()
            .flat_map(|t| {
                sets[n.as_str()].iter().map(move |e| {
                    let mut t = t.clone();
                    t.push(e.clone());
                    t
                })
            })
            .collect();
    }
    out
}

/// Builds the solver model of a problem file: read into a builder model
/// (`api/convert.rs`), then lowered like any other (`api/lower.rs`), as
/// solving one does.
#[cfg(test)]
pub fn build(problem: &Problem) -> Result<LinearModel, String> {
    let model = crate::api::convert::to_model(problem)?;
    crate::api::lower::lower(&model)
}

/// A model's columns, rows, and constraint instances before the format 1.1
/// kinds are encoded: what [`build`] (from a problem file) and the builder
/// API (`api/lower.rs`) both produce.
pub(crate) struct Assembly {
    pub lower: Vec<f64>,
    pub upper: Vec<f64>,
    pub integer: Vec<bool>,
    pub domains: Vec<Option<Vec<(i64, i64)>>>,
    pub starts: Vec<Option<f64>>,
    pub vars: Vec<VarInfo>,
    pub rows: Vec<Row>,
    /// The item of the builder model that each row is.
    pub row_items: Vec<usize>,
    pub nonlinear_rows: Vec<NonlinearRow>,
    pub graph: Graph,
    pub globals: Vec<Global>,
    pub loose: Vec<Loose>,
    /// The first violated constant row, if any.
    pub violated: Option<String>,
    pub obj: Vec<f64>,
    pub obj_offset: f64,
    pub objective_node: Option<Node>,
    pub sense: Sense,
    pub want_all: bool,
    pub solution_limit: Option<usize>,
}

impl Assembly {
    /// Finds value symmetry, encodes the format 1.1 instances (deferring
    /// scheduling and large all_different ones to the engines), and returns
    /// the model.
    pub fn finish(self) -> Result<LinearModel, String> {
        let Assembly {
            mut lower,
            mut upper,
            mut integer,
            mut domains,
            starts,
            vars,
            mut rows,
            row_items,
            nonlinear_rows,
            graph,
            globals,
            loose,
            mut violated,
            mut obj,
            obj_offset,
            objective_node,
            sense,
            want_all,
            solution_limit,
        } = self;
        // Only the nodes the rows and the objective use, in a canonical order,
        // whichever way the expressions were built.
        let (graph, nonlinear_rows, objective_node) = {
            let mut roots: Vec<Node> = nonlinear_rows.iter().map(|r| r.node).collect();
            roots.extend(objective_node);
            let (graph, new_roots) = graph.compact(&roots);
            let mut nonlinear_rows = nonlinear_rows;
            for (r, &n) in nonlinear_rows.iter_mut().zip(&new_roots) {
                r.node = n;
            }
            (graph, nonlinear_rows, objective_node.map(|_| *new_roots.last().unwrap()))
        };
        let uses_cp_kinds = !globals.is_empty();
        let mut scheduling = Vec::new();
        // Value symmetry, found before the encodings add their rows; precedence
        // removes solutions, so not when all of them are wanted.
        let symmetric = if uses_cp_kinds && !want_all && crate::config::symmetry() {
            let cols = crate::symmetry::Columns { lower: &lower, upper: &upper, integer: &integer, domain: &domains };
            crate::symmetry::value_classes(&rows, &globals, &obj, &cols)
        } else {
            Vec::new()
        };
        if crate::config::debug() {
            for class in &symmetric {
                crate::config::debug_log!(
                    "symmetry: {} columns with {} interchangeable values",
                    class.cols.len(),
                    class.values.len()
                );
            }
        }
        if uses_cp_kinds {
            let cols = Columns { lower: &mut lower, upper: &mut upper, integer: &mut integer, domain: &mut domains };
            let mut encoder = Encoder::new(cols, &mut rows, want_all);
            for g in globals {
                if matches!(g, Global::NoOverlap { .. } | Global::Cumulative { .. }) {
                    encoder.check_scheduling(&g)?;
                    scheduling.push(g);
                    continue;
                }
                if encoder.defer_all_different(&g)? {
                    scheduling.push(g);
                    continue;
                }
                if let Some(v) = encoder.encode(&g)?
                    && violated.is_none()
                {
                    violated = Some(v);
                }
            }
            for class in &symmetric {
                encoder.value_precedence(&class.cols, &class.values)?;
            }
            obj.resize(lower.len(), 0.0);
        }

        Ok(LinearModel {
            col_lower: lower,
            col_upper: upper,
            col_integer: integer,
            col_domain: domains,
            col_start: starts,
            rows,
            row_items,
            obj,
            obj_offset,
            sense,
            want_all,
            solution_limit,
            vars,
            violated_constant_row: violated,
            uses_cp_kinds,
            scheduling,
            loose,
            nonlinear: (!nonlinear_rows.is_empty() || objective_node.is_some()).then_some(Nonlinear {
                graph,
                rows: nonlinear_rows,
                objective: objective_node,
            }),
        })
    }
}

/// Sorted distinct integers as inclusive ranges of consecutive values.
pub(crate) fn to_ranges(values: &[i64]) -> Vec<(i64, i64)> {
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for &v in values {
        match ranges.last_mut() {
            Some((_, hi)) if v <= *hi + 1 => *hi = (*hi).max(v),
            _ => ranges.push((v, v)),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::Status;
    use crate::solve::{Engine, solve};

    fn load(json: &str) -> Result<LinearModel, String> {
        build(&Problem::from_json(json)?)
    }

    fn load_error(json: &str) -> String {
        match load(json) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    /// Solves with the given engine; returns (status, number of solutions).
    fn count(json: &str, engine: Engine) -> (Status, usize) {
        let model = load(json).unwrap();
        let out = solve(&model, None, engine).unwrap();
        (out.status, out.solutions.len())
    }

    fn queens(n: usize) -> String {
        format!(
            r#"{{"version": "1.1", "sets": {{"N": {{"from": 1, "to": {n}}}}},
            "variables": [{{"name": "col", "type": "integer", "index": ["N"], "domain": "N"}}],
            "constraints": [
              {{"all_different": "[col[r] | r in N]"}},
              {{"all_different": "[col[r] + r | r in N]"}},
              {{"all_different": "[col[r] - r | r in N]"}}],
            "solve": {{"method": "satisfy", "solutions": "all"}}}}"#
        )
    }

    /// String literals (format 1.3) in subscripts, conditions, literals, and
    /// a whole constraint comparing a symbolic variable; the string `'3'`
    /// isn't the integer 3.
    #[test]
    fn string_literals() {
        let json = |constraint: &str| {
            format!(
                r#"{{"version": "1.3", "sets": {{"P": ["a", "b", "3"], "M": ["m1", "m2"]}},
                "variables": [{{"name": "x", "type": "integer", "index": ["P"], "lower": 0, "upper": 5}},
                              {{"name": "m", "type": "integer", "domain": "M"}}],
                "constraints": [{constraint}],
                "solve": {{"method": "maximize", "objective": "sum(p in P) x[p]"}}}}"#
            )
        };
        let solve_json = |c: &str| {
            let model = load(&json(c)).unwrap();
            let out = solve(&model, None, Engine::Auto).unwrap();
            (out.status, out.solutions.first().map(|x| model.objective_value(x)))
        };
        assert_eq!(solve_json(r#"{"expr": "x['a'] + x['3'] <= 4"}"#), (Status::Complete, Some(9.0)));
        assert_eq!(solve_json(r#"{"expr": "sum(p in P: p != 'b') x[p] <= 1"}"#), (Status::Complete, Some(6.0)));
        let fixed = r#"{"expr": "m == 'm2'"}, {"expr": "x['b'] <= 1", "enforce_if": "m == 'm2'"}"#;
        assert_eq!(solve_json(fixed), (Status::Complete, Some(11.0)));
        // Fixing the other machine frees x['b'].
        let other = r#"{"expr": "m == 'm1'"}, {"expr": "x['b'] <= 1", "enforce_if": "m == 'm2'"}"#;
        assert_eq!(solve_json(other), (Status::Complete, Some(15.0)));
        assert!(load_error(&json(r#"{"expr": "x[3] <= 1"}"#)).contains("outside"), "3 isn't '3'");
        assert!(load_error(&json(r#"{"expr": "x['c'] <= 1"}"#)).contains("outside"));
        assert!(load_error(&json(r#"{"expr": "x['a'] + 'b' <= 1"}"#)).contains("arithmetic"));
        // A literal that isn't one of the variable's values is a slip: an error.
        for wrong in [r#"{"expr": "x['a'] <= 1", "enforce_if": "m == 'm3'"}"#, r#"{"expr": "m == 'm3'"}"#] {
            let e = load_error(&json(wrong));
            assert!(e.ends_with("'m3' isn't one of the values of m (m1, m2)"), "{e}");
        }
        // An element from the data that isn't one is no slip: `==` never
        // holds (nothing to enforce), `!=` always does.
        let never = r#"{"forall": "k in P: k == '3'", "expr": "x['a'] <= 1", "enforce_if": "m == k"}"#;
        assert_eq!(solve_json(never), (Status::Complete, Some(15.0)));
        let always = r#"{"forall": "k in P: k == '3'", "expr": "x['a'] <= 1", "enforce_if": "m != k"}"#;
        assert_eq!(solve_json(always), (Status::Complete, Some(11.0)));
        assert_eq!(solve_json(r#"{"forall": "k in P: k == '3'", "expr": "m == k"}"#), (Status::Infeasible, None));
    }

    #[test]
    fn all_different_counts_queens() {
        for (n, expected) in [(1, 1), (2, 0), (3, 0), (4, 2), (5, 10), (6, 4), (7, 40), (8, 92)] {
            let status = if expected == 0 { Status::Infeasible } else { Status::Complete };
            assert_eq!(count(&queens(n), Engine::Cp), (status, expected), "{n}-queens, CP");
        }
        assert_eq!(count(&queens(6), Engine::Milp), (Status::Complete, 4), "6-queens, MILP");
    }

    /// x, y, z in 1..3 with x + y, z, 4 all different: x + y in 2..6 must
    /// avoid z and 4. Brute force: 18 of the 27 points.
    #[test]
    fn all_different_with_a_multi_column_item_and_a_constant() {
        let json = r#"{"version": "1.1",
            "variables": [
              {"name": "x", "type": "integer", "lower": 1, "upper": 3},
              {"name": "y", "type": "integer", "lower": 1, "upper": 3},
              {"name": "z", "type": "integer", "lower": 1, "upper": 3}],
            "constraints": [{"all_different": "[x + y, z, 4]"}],
            "solve": {"method": "satisfy", "solutions": "all"}}"#;
        let mut expected = 0;
        for x in 1..=3 {
            for y in 1..=3 {
                for z in 1..=3 {
                    let s = x + y;
                    if s != z && s != 4 && z != 4 {
                        expected += 1;
                    }
                }
            }
        }
        assert_eq!(count(json, Engine::Cp), (Status::Complete, expected));
        assert_eq!(count(json, Engine::Milp), (Status::Complete, expected));
    }

    #[test]
    fn repeated_items_are_infeasible() {
        let json = |list: &str| {
            format!(
                r#"{{"version": "1.1", "variables": [{{"name": "x", "type": "integer", "lower": 0, "upper": 5}}],
                "constraints": [{{"all_different": "{list}"}}], "solve": {{"method": "satisfy"}}}}"#
            )
        };
        assert_eq!(count(&json("[x, x]"), Engine::Cp).0, Status::Infeasible);
        assert_eq!(count(&json("[2, x, 2]"), Engine::Cp).0, Status::Infeasible);
        assert_eq!(count(&json("[2, x, 3]"), Engine::Cp), (Status::Complete, 1));
    }

    #[test]
    fn functions_of_constants_are_evaluated() {
        let json = r#"{"version": "1.2", "sets": {"I": [1, 2]},
            "parameters": {"rate": {"index": ["I"], "values": {"1": 0.05, "2": 0.1}}, "years": 3},
            "variables": [{"name": "x", "type": "continuous", "index": ["I"], "lower": 0, "upper": 1, "start": 0.5}],
            "constraints": [{"expr": "sum(i in I) (1 + rate[i])^years * x[i] + ln(exp(2)) * sqrt(4) <= 2^0.5"}],
            "solve": {"method": "maximize", "objective": "sum(i in I) exp(rate[i] * years) * x[i] + sin(0) + atan(tan(1))"}}"#;
        let m = load(json).unwrap();
        let row = &m.rows[0];
        assert!((row.coefs[0].1 - 1.05f64.powi(3)).abs() < 1e-12 && (row.coefs[1].1 - 1.1f64.powi(3)).abs() < 1e-12);
        assert!((row.upper - (2f64.sqrt() - 4.0)).abs() < 1e-12);
        assert!((m.obj[1] - 0.3f64.exp()).abs() < 1e-12 && (m.obj_offset - 1.0).abs() < 1e-12);
        let bad = |e: &str| json.replace("ln(exp(2))", e);
        assert!(load_error(&bad("ln(-1)")).contains("ln(-1) is not a finite number"));
        assert!(load_error(&bad("(-8)^(1/3)")).contains("is not a finite number"));
        // Functions of variables make the row nonlinear.
        for e in ["ln(x[1])", "2^x[1]", "x[1]^x[2]", "x[1] * x[2]", "1 / x[2]", "x[1]^2.5"] {
            let m = load(&bad(e)).unwrap();
            let nl = m.nonlinear.as_ref().unwrap_or_else(|| panic!("{e}: not nonlinear"));
            assert_eq!((m.rows.len(), nl.rows.len()), (0, 1), "{e}");
        }
        let start = json.replace(r#""start": 0.5"#, r#""start": "nope""#);
        assert!(load_error(&start).contains("start refers to unknown parameter nope"));
    }

    #[test]
    fn nonlinear_rows_and_objective() {
        let json = r#"{"version": "1.2", "sets": {"I": [1, 2, 3]},
            "parameters": {"w": {"index": ["I"], "values": {"1": 1, "2": 2, "3": 3}}},
            "variables": [{"name": "x", "type": "continuous", "index": ["I"], "lower": 0.1, "upper": 5},
                          {"name": "y", "type": "continuous"}],
            "constraints": [
                {"name": "circle", "expr": "sum(i in I) x[i]^2 <= 4 + y"},
                {"name": "link", "forall": "i in I", "expr": "w[i] * exp(x[i]) - 2 * y >= ln(w[i] * x[i])"},
                {"name": "plain", "expr": "x[1] + x[2] <= 3"}],
            "solve": {"method": "minimize", "objective": "3 * y + sum(i in I) x[i] * x[i] + 1"}}"#;
        let m = load(json).unwrap();
        assert_eq!(m.rows.len(), 1); // plain
        let nl = m.nonlinear.as_ref().unwrap();
        let labels: Vec<&str> = nl.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["circle", "link[1]", "link[2]", "link[3]"]);
        // circle: sum x^2 - y <= 4, with the affine part (-y) kept apart.
        let circle = &nl.rows[0];
        assert_eq!((circle.coefs.clone(), circle.lower, circle.upper), (vec![(3, -1.0)], -INF, 4.0));
        let x = [1.0, 2.0, 0.5, 0.25];
        assert_eq!(nl.graph.value(circle.node, &x), 1.0 + 4.0 + 0.25);
        let link2 = &nl.rows[2];
        assert_eq!((link2.coefs.clone(), link2.lower, link2.upper), (vec![(3, -2.0)], 0.0, INF));
        assert!((nl.graph.value(link2.node, &x) - (2.0 * 2f64.exp() - (2.0f64 * 2.0).ln())).abs() < 1e-12);
        // x[i] * x[i] and x[i]^2 are different nodes, but x[i] is shared.
        assert_eq!(m.obj[3], 3.0);
        assert!((m.objective_value(&x) - (3.0 * 0.25 + 1.0 + 4.0 + 0.25 + 1.0)).abs() < 1e-12);
        // x*y - y*x cancels: the row is affine.
        let cancel = json.replace("sum(i in I) x[i]^2 <= 4 + y", "x[1] * y - y * x[1] + x[2] <= 4");
        assert_eq!(load(&cancel).unwrap().rows.len(), 2);
    }

    #[test]
    fn nonlinear_terms_only_where_allowed() {
        let base = |constraint: &str| {
            format!(
                r#"{{"version": "1.2", "sets": {{"R": ["a", "b"]}},
                "variables": [{{"name": "x", "type": "integer", "lower": 1, "upper": 3}},
                              {{"name": "b", "type": "binary"}},
                              {{"name": "r", "type": "integer", "domain": "R"}}],
                "constraints": [{constraint}], "solve": {{"method": "satisfy"}}}}"#
            )
        };
        let affine_only = "only expr constraints without enforce_if, and the objective, may be nonlinear";
        for c in [
            r#"{"all_different": "[x * x, 2]"}"#,
            r#"{"expr": "x * x <= 4", "enforce_if": "b"}"#,
            r#"{"max": {"value": "x", "of": "[ln(x), 1]"}}"#,
        ] {
            let e = load_error(&base(c));
            assert!(e.contains(affine_only), "{c}: {e}");
        }
        let e = load_error(&base(r#"{"expr": "r * x <= 4"}"#));
        assert!(e.contains("symbolic variable r can't be used in arithmetic"), "{e}");
        let e = load_error(&base(r#"{"expr": "sum(i in R: x * x > 1) b <= 1"}"#));
        assert!(e.contains("depend"), "{e}");
    }

    #[test]
    fn rejects_what_is_not_supported() {
        let symbolic = r#"{"version": "1.1", "sets": {"R": ["a", "b"]},
            "variables": [{"name": "r", "type": "integer", "domain": "R"}],
            "constraints": [{"expr": "r <= 1"}], "solve": {"method": "satisfy"}}"#;
        assert!(load_error(symbolic).contains("symbolic variable r can't be used in arithmetic"));
        let no_overlap = r#"{"version": "1.1", "variables": [{"name": "x", "type": "integer", "lower": 0, "upper": 1}],
            "constraints": [{"no_overlap": "[x]"}], "solve": {"method": "satisfy"}}"#;
        assert!(load_error(no_overlap).contains("x is not an interval"));
        let enforce_alldiff = r#"{"version": "1.1", "variables": [{"name": "x", "type": "binary"}],
            "constraints": [{"all_different": "[x, 1]", "enforce_if": "x"}], "solve": {"method": "satisfy"}}"#;
        assert!(load_error(enforce_alldiff).contains("enforce_if is only allowed with expr"));
        let unbounded = r#"{"version": "1.1", "variables": [{"name": "x", "type": "integer", "lower": 0}],
            "constraints": [{"all_different": "[x, 3]"}], "solve": {"method": "satisfy"}}"#;
        assert!(load_error(unbounded).contains("finite bounds"));
    }

    /// Counts the integer points of the box `lo..=hi` (3 variables x, y, z)
    /// that satisfy `check`.
    fn brute(lo: [i64; 3], hi: [i64; 3], check: &dyn Fn(i64, i64, i64) -> bool) -> usize {
        let mut n = 0;
        for x in lo[0]..=hi[0] {
            for y in lo[1]..=hi[1] {
                for z in lo[2]..=hi[2] {
                    n += check(x, y, z) as usize;
                }
            }
        }
        n
    }

    fn xyz(constraints: &str, extra: &str) -> String {
        format!(
            r#"{{"version": "1.1", "sets": {{"K": {{"from": 1, "to": 4}}, "J": [0, 2, 3]}},
            "parameters": {{"c": {{"index": ["K"], "values": {{"1": 5, "2": -1, "3": 2, "4": 0}}}},
                           "d": {{"index": ["J", "J"], "values": {{"0": {{"0": 1, "2": 4, "3": -2}}, "2": {{"0": 0, "2": 3, "3": 1}}, "3": {{"0": 2, "2": -3, "3": 5}}}}}},
                           "ok": {{"index": ["K"], "values": {{"1": 1, "2": 0, "3": 1, "4": 1}}}}}},
            "variables": [
              {{"name": "x", "type": "integer", "lower": 0, "upper": 4}},
              {{"name": "y", "type": "integer", "lower": -2, "upper": 3}},
              {{"name": "z", "type": "integer", "lower": 0, "upper": 3}}{extra}],
            "constraints": [{constraints}],
            "solve": {{"method": "satisfy", "solutions": "all"}}}}"#
        )
    }

    fn agree(json: &str, expected: usize) {
        for engine in [Engine::Cp, Engine::Milp] {
            let (status, n) = count(json, engine);
            let want = if expected == 0 { Status::Infeasible } else { Status::Complete };
            assert_eq!((status, n), (want, expected), "{engine:?}");
        }
    }

    #[test]
    fn table_against_brute_force() {
        let allowed = [(0, -2, 1), (4, 3, 0), (2, 0, 3), (2, 0, 3), (9, 0, 0)];
        let json = xyz(
            r#"{"table": {"vars": "[x, y, z]", "tuples": [[0, -2, 1], [4, 3, 0], [2, 0, 3], [2, 0, 3], [9, 0, 0]]}}"#,
            "",
        );
        agree(&json, brute([0, -2, 0], [4, 3, 3], &|x, y, z| allowed.contains(&(x, y, z))));
        let json = xyz(r#"{"table": {"vars": "[x, z]", "tuples": [[1, 1], [2, 3]], "forbidden": true}}"#, "");
        agree(&json, brute([0, -2, 0], [4, 3, 3], &|x, _, z| (x, z) != (1, 1) && (x, z) != (2, 3)));
        // A repeated variable: [x, x] can only match tuples with equal entries.
        let json = xyz(r#"{"table": {"vars": "[x, x]", "tuples": [[1, 1], [2, 3]]}}"#, "");
        agree(&json, brute([0, -2, 0], [4, 3, 3], &|x, _, _| x == 1));
    }

    #[test]
    fn element_against_brute_force() {
        let c = |k: i64| [5, -1, 2, 0][(k - 1) as usize];
        // Constant items: y == c[x] with x in K (so x >= 1).
        let json = xyz(r#"{"element": {"index": "x", "array": "[c[k] | k in K]", "value": "y"}}"#, "");
        agree(&json, brute([0, -2, 0], [4, 3, 3], &|x, y, _| (1..=4).contains(&x) && y == c(x)));
        // A filtered comprehension: x must be an "ok" k.
        let json = xyz(r#"{"element": {"index": "x", "array": "[c[k] | k in K: ok[k] == 1]", "value": "y"}}"#, "");
        agree(&json, brute([0, -2, 0], [4, 3, 3], &|x, y, _| [1, 3, 4].contains(&x) && y == c(x)));
        // An affine index: c[z + 1] == y.
        let json = xyz(r#"{"element": {"index": "z + 1", "array": "[c[k] | k in K]", "value": "y"}}"#, "");
        agree(&json, brute([0, -2, 0], [4, 3, 3], &|_, y, z| y == c(z + 1)));
        // Two indexes over a set with a gap: y == d[x, z], x and z in {0, 2, 3}.
        let d = |a: i64, b: i64| {
            let pos = |v: i64| [0, 2, 3].iter().position(|&e| e == v).unwrap();
            [[1, 4, -2], [0, 3, 1], [2, -3, 5]][pos(a)][pos(b)]
        };
        let json =
            xyz(r#"{"element": {"index": ["x", "z"], "array": "[d[i, j] | i in J, j in J]", "value": "y"}}"#, "");
        let in_j = |v: i64| [0, 2, 3].contains(&v);
        agree(&json, brute([0, -2, 0], [4, 3, 3], &|x, y, z| in_j(x) && in_j(z) && y == d(x, z)));
    }

    #[test]
    fn element_with_variable_items_against_brute_force() {
        // value z == item k of [x, y + 3, 2], k a fourth variable in 1..3.
        let json = r#"{"version": "1.1", "sets": {"K": {"from": 1, "to": 3}},
            "parameters": {"a": {"index": ["K"], "values": {"1": 1, "2": 0, "3": 0}},
                           "b": {"index": ["K"], "values": {"1": 0, "2": 1, "3": 0}},
                           "c": {"index": ["K"], "values": {"1": 0, "2": 3, "3": 2}}},
            "variables": [
              {"name": "x", "type": "integer", "lower": 0, "upper": 4},
              {"name": "y", "type": "integer", "lower": -2, "upper": 3},
              {"name": "z", "type": "integer", "lower": 0, "upper": 3},
              {"name": "k", "type": "integer", "lower": 1, "upper": 3}],
            "constraints": [{"element": {"index": "k", "array": "[a[i] * x + b[i] * y + c[i] | i in K]", "value": "z"}}],
            "solve": {"method": "satisfy", "solutions": "all"}}"#;
        let mut expected = 0;
        for x in 0..=4 {
            for y in -2..=3 {
                for z in 0..=3 {
                    for k in 1..=3 {
                        let item = [x, y + 3, 2][k - 1];
                        expected += (z == item) as usize;
                    }
                }
            }
        }
        agree(json, expected);
    }

    #[test]
    fn enforce_if_against_brute_force() {
        // x + y <= 2 when b (binary), x >= 3 when not b, z == 1 when x == 4, y <= -1 when z != 2.
        let json = xyz(
            r#"{"expr": "x + y <= 2", "enforce_if": "b"},
               {"expr": "x >= 3", "enforce_if": "not b"},
               {"expr": "z == 1", "enforce_if": "x == 4"},
               {"expr": "y <= -1", "enforce_if": "z != 2 and b"}"#,
            r#", {"name": "b", "type": "binary"}"#,
        );
        let mut expected = 0;
        for b in 0..=1 {
            expected += brute([0, -2, 0], [4, 3, 3], &|x, y, z| {
                (b == 0 || x + y <= 2) && (b == 1 || x >= 3) && (x != 4 || z == 1) && (!(z != 2 && b == 1) || y <= -1)
            });
        }
        agree(&json, expected);
        // A literal on a value outside the domain: never true, so nothing is enforced.
        let json = xyz(r#"{"expr": "x == 0", "enforce_if": "z == 7"}"#, "");
        agree(&json, 5 * 6 * 4);
    }

    /// Named machines: element (cost by machine), all_different, enforce_if
    /// on a symbolic comparison (the slow machine costs a penalty), and a
    /// forbidden table of machine pairs, one naming a machine that doesn't
    /// exist. All optimal assignments, against brute force over permutations.
    #[test]
    fn symbolic_element_enforce_and_table() {
        let json = r#"{"version": "1.1",
            "sets": {"T": ["t1", "t2", "t3"], "M": ["lathe", "mill", "drill"]},
            "parameters": {
              "cost": {"index": ["T", "M"], "values": {
                "t1": {"lathe": 4, "mill": 2, "drill": 1},
                "t2": {"lathe": 4, "mill": 3, "drill": 1},
                "t3": {"lathe": 3, "mill": 1, "drill": 2}}},
              "slow": {"index": ["M"], "values": {"lathe": 0, "mill": 0, "drill": 1}},
              "pair": {"index": ["T"], "values": {"t1": 1, "t2": 1, "t3": 0}}},
            "variables": [
              {"name": "machine", "type": "integer", "index": ["T"], "domain": "M"},
              {"name": "c", "type": "integer", "index": ["T"], "lower": 0, "upper": 10},
              {"name": "late", "type": "binary", "index": ["T"]}],
            "constraints": [
              {"forall": "t in T", "element": {"index": "machine[t]", "array": "[cost[t, m] | m in M]", "value": "c[t]"}},
              {"all_different": "[machine[t] | t in T]"},
              {"forall": "t in T, m in M: slow[m] == 1", "expr": "late[t] == 1", "enforce_if": "machine[t] == m"},
              {"table": {"vars": "[machine[t] | t in T: pair[t] == 1]",
                         "tuples": [["mill", "lathe"], ["drill", "mill"], ["lathe", "drill"], ["nowhere", "mill"]],
                         "forbidden": true}}],
            "solve": {"method": "minimize", "objective": "sum(t in T) (c[t] + 5 * late[t])", "solutions": "all"}}"#;
        let machines = ["lathe", "mill", "drill"];
        let cost = [[4, 2, 1], [4, 3, 1], [3, 1, 2]];
        let forbidden = [("mill", "lathe"), ("drill", "mill"), ("lathe", "drill")];
        let mut best = i64::MAX;
        let mut optima = Vec::new();
        for perm in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            if forbidden.contains(&(machines[perm[0]], machines[perm[1]])) {
                continue;
            }
            let obj: i64 = (0..3).map(|t| cost[t][perm[t]] + if perm[t] == 2 { 5 } else { 0 }).sum();
            if obj < best {
                (best, optima) = (obj, vec![]);
            }
            if obj == best {
                optima.push(perm);
            }
        }
        let model = load(json).unwrap();
        for engine in [Engine::Cp, Engine::Milp] {
            let out = solve(&model, None, engine).unwrap();
            assert_eq!((out.status, out.solutions.len()), (Status::Complete, optima.len()), "{engine:?}");
            for x in &out.solutions {
                assert_eq!(model.objective_value(x), best as f64, "{engine:?}");
            }
        }
    }

    /// value == max / min / |.| of items, with ties: in "all", each point of
    /// (x, y, z) must appear exactly once, with m determined.
    #[test]
    fn min_max_abs_against_brute_force() {
        let m = r#", {"name": "m", "type": "integer", "lower": -5, "upper": 6}"#;
        let all_points = 5 * 6 * 4;
        agree(&xyz(r#"{"max": {"value": "m", "of": "[x, y + 1, 2]"}}"#, m), all_points);
        agree(&xyz(r#"{"min": {"value": "m", "of": "[x, 2 * z - 1]"}}"#, m), all_points);
        agree(&xyz(r#"{"abs": {"value": "m", "of": "y - z"}}"#, m), all_points);
        // With a value too narrow for some points: only those with max <= 3 remain.
        let narrow = r#", {"name": "m", "type": "integer", "lower": -5, "upper": 3}"#;
        let expected = brute([0, -2, 0], [4, 3, 3], &|x, y, _| x.max(y + 1).max(2) <= 3);
        agree(&xyz(r#"{"max": {"value": "m", "of": "[x, y + 1, 2]"}}"#, narrow), expected);
        // An expression as the value: 2 * m == |y - z| needs an even difference.
        let expected = brute([0, -2, 0], [4, 3, 3], &|_, y, z| (y - z).abs() % 2 == 0);
        agree(&xyz(r#"{"abs": {"value": "2 * m", "of": "y - z"}}"#, m), expected);
    }

    /// Minimize max(x, y + 1, 2) subject to x + y >= 4, with every optimum.
    #[test]
    fn minimize_a_maximum_with_all_optima() {
        let json = r#"{"version": "1.1",
            "variables": [
              {"name": "x", "type": "integer", "lower": 0, "upper": 4},
              {"name": "y", "type": "integer", "lower": -2, "upper": 3},
              {"name": "m", "type": "integer", "lower": -10, "upper": 10}],
            "constraints": [{"expr": "x + y >= 4"}, {"max": {"value": "m", "of": "[x, y + 1, 2]"}}],
            "solve": {"method": "minimize", "objective": "m", "solutions": "all"}}"#;
        let mut points = Vec::new();
        for x in 0..=4i64 {
            for y in -2..=3i64 {
                if x + y >= 4 {
                    points.push((x, y, x.max(y + 1).max(2)));
                }
            }
        }
        let best = points.iter().map(|p| p.2).min().unwrap();
        let optima = points.iter().filter(|p| p.2 == best).count();
        let model = load(json).unwrap();
        for engine in [Engine::Cp, Engine::Milp] {
            let out = solve(&model, None, engine).unwrap();
            assert_eq!((out.status, out.solutions.len()), (Status::Complete, optima), "{engine:?}");
            assert!(out.solutions.iter().all(|x| model.objective_value(x) == best as f64), "{engine:?}");
        }
    }

    #[test]
    fn min_max_abs_errors() {
        let all_fractional = xyz(
            r#"{"max": {"value": "m", "of": "[0.5 * x, y]"}}"#,
            r#", {"name": "m", "type": "continuous", "lower": -5, "upper": 6}"#,
        );
        assert!(load_error(&all_fractional).contains("integer or binary"), "\"all\" rejects the continuous m first");
        let json = r#"{"version": "1.1",
            "variables": [{"name": "x", "type": "integer", "lower": 0, "upper": 4},
                          {"name": "m", "type": "integer", "lower": 0, "upper": 4}],
            "constraints": [{"max": {"value": "m", "of": "[0.5 * x, 1]"}}],
            "solve": {"method": "satisfy", "solutions": "all"}}"#;
        assert!(load_error(json).contains("needs integer-valued items"));
        let unbounded = r#"{"version": "1.1",
            "variables": [{"name": "x", "type": "integer", "lower": 0},
                          {"name": "m", "type": "integer", "lower": 0, "upper": 4}],
            "constraints": [{"abs": {"value": "m", "of": "x - 2"}}], "solve": {"method": "satisfy"}}"#;
        assert!(load_error(unbounded).contains("finite bounds"));
    }

    /// Every point of the box (one range per variable), as vectors.
    fn points(ranges: &[(i64, i64)]) -> Vec<Vec<i64>> {
        let mut out = vec![vec![]];
        for &(lo, hi) in ranges {
            out = out.into_iter().flat_map(|p| (lo..=hi).map(move |v| [p.clone(), vec![v]].concat())).collect();
        }
        out
    }

    /// A model with integer variables v0, v1, ... over the given ranges, the
    /// named binaries, intervals, and constraints, enumerating all solutions.
    fn interval_model(ranges: &[(i64, i64)], binaries: &[&str], intervals: &str, constraints: &str) -> String {
        let mut vars: Vec<String> = ranges
            .iter()
            .enumerate()
            .map(|(i, (lo, hi))| format!(r#"{{"name": "v{i}", "type": "integer", "lower": {lo}, "upper": {hi}}}"#))
            .collect();
        vars.extend(binaries.iter().map(|b| format!(r#"{{"name": "{b}", "type": "binary"}}"#)));
        format!(
            r#"{{"version": "1.1", "variables": [{}], "intervals": [{intervals}], "constraints": [{constraints}],
            "solve": {{"method": "satisfy", "solutions": "all"}}}}"#,
            vars.join(", ")
        )
    }

    /// Whether intervals [s, s + d) a and b are compatible under no_overlap.
    fn apart(a: (i64, i64), b: (i64, i64)) -> bool {
        a.0 + a.1 <= b.0 || b.0 + b.1 <= a.0
    }

    #[test]
    fn no_overlap_against_brute_force() {
        // Starts v0..v2 in 0..4, durations 2, 0, 1 (a zero-length interval).
        let ranges = [(0, 4), (0, 4), (0, 4)];
        let json = interval_model(
            &ranges,
            &[],
            r#"{"name": "a", "start": "v0", "duration": 2}, {"name": "b", "start": "v1", "duration": 0},
               {"name": "c", "start": "v2", "duration": 1}"#,
            r#"{"no_overlap": "[a, b, c]"}"#,
        );
        let d = [2, 0, 1];
        let expected = points(&ranges)
            .iter()
            .filter(|p| (0..3).all(|i| (i + 1..3).all(|j| apart((p[i], d[i]), (p[j], d[j])))))
            .count();
        agree(&json, expected);
    }

    #[test]
    fn optional_intervals_and_variable_durations_against_brute_force() {
        // v0, v1 starts in 0..3; v2 a duration in 0..2; v3, v4 presences.
        let ranges = [(0, 3), (0, 3), (0, 2), (0, 1), (0, 1)];
        let json = interval_model(
            &ranges,
            &["p0", "p1"],
            r#"{"name": "a", "start": "v0", "duration": "v2", "presence": "p0"},
               {"name": "b", "start": "v1", "duration": 2, "presence": "p1"}"#,
            r#"{"no_overlap": "[a, b]"}, {"expr": "p0 == v3"}, {"expr": "p1 == v4"}"#,
        );
        let expected =
            points(&ranges).iter().filter(|p| p[3] == 0 || p[4] == 0 || apart((p[0], p[2]), (p[1], 2))).count();
        agree(&json, expected);
    }

    #[test]
    fn cumulative_against_brute_force() {
        // Starts v0..v2 in 0..3, durations 2, v3 (0..2), 0; demands 2, v4 (0..2), 3;
        // capacity 3; the second interval optional (presence p).
        let ranges = [(0, 3), (0, 3), (0, 3), (0, 2), (0, 2), (0, 1)];
        let json = interval_model(
            &ranges,
            &["p"],
            r#"{"name": "a", "start": "v0", "duration": 2}, {"name": "b", "start": "v1", "duration": "v3", "presence": "p"},
               {"name": "c", "start": "v2", "duration": 0}"#,
            r#"{"cumulative": {"intervals": "[a, b, c]", "demands": "[2, v4, 3]", "capacity": "3"}}, {"expr": "p == v5"}"#,
        );
        let expected = points(&ranges)
            .iter()
            .filter(|p| {
                (0..8).all(|t| {
                    let runs = |s: i64, d: i64| s <= t && t < s + d;
                    let used = 2 * runs(p[0], 2) as i64 + if p[5] == 1 { p[4] * runs(p[1], p[3]) as i64 } else { 0 };
                    used <= 3 // the zero-length c never counts
                })
            })
            .count();
        agree(&json, expected);
    }
    /// Random small no_overlap and cumulative instances (optional tasks,
    /// variable durations, demands, and capacities): the number of solutions
    /// and the optimum of a random objective agree with brute force.
    #[test]
    fn scheduling_against_brute_force() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut rand = |n: i64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as i64
        };
        for case in 0..80 {
            let cumulative = case % 2 == 1;
            let n = 2 + rand(3) as usize;
            // Variables: v0.. as (lo, hi); fields refer to them or are constants.
            let mut ranges: Vec<(i64, i64)> = Vec::new();
            let var = |ranges: &mut Vec<(i64, i64)>, lo, hi| {
                ranges.push((lo, hi));
                ranges.len() - 1
            };
            enum F {
                C(i64),
                V(usize),
            }
            let mut tasks = Vec::new();
            for _ in 0..n {
                let start = var(&mut ranges, 0, 2 + rand(3));
                let duration = if rand(3) == 0 { F::V(var(&mut ranges, 0, 2)) } else { F::C(rand(4)) };
                let demand = if rand(3) == 0 { F::V(var(&mut ranges, 0, 2)) } else { F::C(rand(4)) };
                let presence = (rand(3) == 0).then(|| var(&mut ranges, 0, 1));
                tasks.push((start, duration, demand, presence));
            }
            let capacity = if rand(3) == 0 { F::V(var(&mut ranges, 1, 3)) } else { F::C(1 + rand(4)) };
            let field = |f: &F| match f {
                F::C(c) => c.to_string(),
                F::V(v) => format!("\"v{v}\""),
            };
            let value = |f: &F, p: &[i64]| match f {
                F::C(c) => *c,
                F::V(v) => p[*v],
            };
            let intervals: Vec<String> = tasks
                .iter()
                .enumerate()
                .map(|(i, (s, d, _, pr))| {
                    let presence = pr.map_or(String::new(), |p| format!(r#", "presence": "v{p}""#));
                    format!(r#"{{"name": "t{i}", "start": "v{s}", "duration": {}{presence}}}"#, field(d))
                })
                .collect();
            let list = (0..n).map(|i| format!("t{i}")).collect::<Vec<_>>().join(", ");
            let constraint = if cumulative {
                let demands = tasks.iter().map(|t| field(&t.2).replace('"', "")).collect::<Vec<_>>().join(", ");
                format!(
                    r#"{{"cumulative": {{"intervals": "[{list}]", "demands": "[{demands}]", "capacity": "{}"}}}}"#,
                    field(&capacity).replace('"', "")
                )
            } else {
                format!(r#"{{"no_overlap": "[{list}]"}}"#)
            };
            let vars: Vec<String> = ranges
                .iter()
                .enumerate()
                .map(|(i, (lo, hi))| {
                    if tasks.iter().any(|t| t.3 == Some(i)) {
                        format!(r#"{{"name": "v{i}", "type": "binary"}}"#)
                    } else {
                        format!(r#"{{"name": "v{i}", "type": "integer", "lower": {lo}, "upper": {hi}}}"#)
                    }
                })
                .collect();
            let weights: Vec<i64> = (0..ranges.len()).map(|_| rand(7) - 3).collect();
            let objective =
                weights.iter().enumerate().map(|(i, w)| format!("{w} * v{i}")).collect::<Vec<_>>().join(" + ");
            let json = |solve: &str| {
                format!(
                    r#"{{"version": "1.1", "variables": [{}], "intervals": [{}], "constraints": [{constraint}],
                    "solve": {solve}}}"#,
                    vars.join(", "),
                    intervals.join(", ")
                )
            };

            let feasible = |p: &[i64]| -> bool {
                let present = |t: &(usize, F, F, Option<usize>)| t.3.is_none_or(|v| p[v] == 1);
                let span = |t: &(usize, F, F, Option<usize>)| (p[t.0], p[t.0] + value(&t.1, p));
                if cumulative {
                    let cap = value(&capacity, p);
                    (-5..15).all(|time| {
                        let used: i64 = tasks
                            .iter()
                            .filter(|t| present(t) && span(t).0 <= time && time < span(t).1)
                            .map(|t| value(&t.2, p))
                            .sum();
                        used <= cap
                    })
                } else {
                    tasks.iter().enumerate().all(|(i, a)| {
                        tasks[i + 1..]
                            .iter()
                            .all(|b| !present(a) || !present(b) || span(a).1 <= span(b).0 || span(b).1 <= span(a).0)
                    })
                }
            };
            let all = points(&ranges);
            let solutions: Vec<&Vec<i64>> = all.iter().filter(|p| feasible(p)).collect();
            let label = format!("case {case}: {}", json(""));

            // Enumerating many solutions is slow (each one is blocked by a clause).
            if solutions.len() <= 2000 {
                let (status, count) = count(&json(r#"{"method": "satisfy", "solutions": "all"}"#), Engine::Cp);
                let want = if solutions.is_empty() { Status::Infeasible } else { Status::Complete };
                assert_eq!((status, count), (want, solutions.len()), "{label}");
            }

            let best = solutions.iter().map(|p| p.iter().zip(&weights).map(|(x, w)| x * w).sum::<i64>()).min();
            for engine in [Engine::Cp, Engine::Milp] {
                let model = load(&json(&format!(r#"{{"method": "minimize", "objective": "{objective}"}}"#))).unwrap();
                let out = solve(&model, None, engine).unwrap();
                let got = out.solutions.first().map(|x| model.objective_value(x).round() as i64);
                assert_eq!(got, best, "{engine:?} optimum, {label}");
            }
        }
    }

    /// `all_different` on random small problems, deferred to the native
    /// propagator (a difference item with many values) or encoded, against
    /// brute force: all solutions with the CP engine, and
    /// the optimum with both engines.
    #[test]
    fn all_different_against_brute_force() {
        // An item's text and its value at a point.
        type Item = (String, Box<dyn Fn(&[i64]) -> i64>);
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut rand = |n: i64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as i64
        };
        for case in 0..120 {
            let wide = case % 2 == 0; // past the encoding limit: deferred
            let n = 3usize;
            let ranges: Vec<(i64, i64)> = (0..n)
                .map(|_| {
                    let lo = rand(3) - 1;
                    (lo, lo + if wide { 35 + rand(6) } else { 1 + rand(5) })
                })
                .collect();
            // Items: a variable with a coefficient and an offset, a difference, or a constant.
            let items: Vec<Item> = (0..2 + rand(3))
                .map(|_| -> Item {
                    let (i, j) = (rand(n as i64) as usize, rand(n as i64) as usize);
                    match rand(4) {
                        0 if i != j => (format!("v{i} - v{j}"), Box::new(move |p: &[i64]| p[i] - p[j])),
                        1 => {
                            let c = rand(5) - 1;
                            (c.to_string(), Box::new(move |_: &[i64]| c))
                        }
                        _ => {
                            let (a, c) = ([1, -1, 2][rand(3) as usize], rand(5) - 2);
                            (format!("{a} * v{i} + {c}"), Box::new(move |p: &[i64]| a * p[i] + c))
                        }
                    }
                })
                .collect();
            // A row that keeps the solution count small enough to enumerate.
            let weights: Vec<i64> = (0..n).map(|_| 1 + rand(3)).collect();
            let rhs = rand(12) + 2;
            let row = weights.iter().enumerate().map(|(i, w)| format!("{w} * v{i}")).collect::<Vec<_>>().join(" + ");
            let costs: Vec<i64> = (0..n).map(|_| rand(7) - 3).collect();
            let objective =
                costs.iter().enumerate().map(|(i, c)| format!("{c} * v{i}")).collect::<Vec<_>>().join(" + ");
            let vars: Vec<String> = ranges
                .iter()
                .enumerate()
                .map(|(i, (lo, hi))| format!(r#"{{"name": "v{i}", "type": "integer", "lower": {lo}, "upper": {hi}}}"#))
                .collect();
            let list = items.iter().map(|it| it.0.clone()).collect::<Vec<_>>().join(", ");
            let json = |solve: &str| {
                format!(
                    r#"{{"version": "1.1", "variables": [{}], "constraints": [{{"all_different": "[{list}]"}},
                    {{"expr": "{row} <= {rhs}"}}], "solve": {solve}}}"#,
                    vars.join(", ")
                )
            };
            let feasible = |p: &[i64]| {
                let values: Vec<i64> = items.iter().map(|it| (it.1)(p)).collect();
                let distinct = values.iter().enumerate().all(|(k, v)| !values[k + 1..].contains(v));
                distinct && p.iter().zip(&weights).map(|(x, w)| x * w).sum::<i64>() <= rhs
            };
            let solutions: Vec<Vec<i64>> = points(&ranges).into_iter().filter(|p| feasible(p)).collect();
            let label = format!("case {case}: {}", json(""));
            let model = load(&json(r#"{"method": "satisfy"}"#)).unwrap();
            let difference = items.iter().any(|it| it.0.contains(" - v"));
            assert_eq!(!model.scheduling.is_empty(), wide && difference, "deferred exactly when wide, {label}");

            if solutions.len() <= 2000 {
                let (status, count) = count(&json(r#"{"method": "satisfy", "solutions": "all"}"#), Engine::Cp);
                let want = if solutions.is_empty() { Status::Infeasible } else { Status::Complete };
                assert_eq!((status, count), (want, solutions.len()), "{label}");
            }
            let best = solutions.iter().map(|p| p.iter().zip(&costs).map(|(x, c)| x * c).sum::<i64>()).min();
            for engine in [Engine::Cp, Engine::Milp] {
                let model = load(&json(&format!(r#"{{"method": "minimize", "objective": "{objective}"}}"#))).unwrap();
                let out = solve(&model, None, engine).unwrap();
                let got = out.solutions.first().map(|x| model.objective_value(x).round() as i64);
                assert_eq!(got, best, "{engine:?} optimum, {label}");
            }
        }
    }

    /// Graph colorings (with value precedence on the interchangeable colors)
    /// against brute force: the fewest colors with both engines, and
    /// feasibility with a fixed number of colors.
    #[test]
    fn value_symmetry_against_brute_force() {
        let mut state = 0x5851_f42d_4c95_7f2du64;
        let mut rand = |n: i64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as i64
        };
        for case in 0..60 {
            let (n, k) = (3 + rand(3) as usize, 2 + rand(3));
            let edges: Vec<(usize, usize)> =
                (0..n).flat_map(|a| (a + 1..n).map(move |b| (a, b))).filter(|_| rand(2) == 0).collect();
            let pin = rand(2) == 0;
            // A cost on one color breaks the symmetry (no precedence then).
            let cost = rand(4) == 0;
            let mut constraints: Vec<String> =
                edges.iter().map(|(a, b)| format!(r#"{{"all_different": "[c{a}, c{b}]"}}"#)).collect();
            constraints.extend((0..n).map(|v| format!(r#"{{"expr": "used >= c{v}"}}"#)));
            if pin {
                constraints.push(r#"{"expr": "c0 == 1"}"#.into());
            }
            let mut vars: Vec<String> =
                (0..n).map(|v| format!(r#"{{"name": "c{v}", "type": "integer", "lower": 1, "upper": {k}}}"#)).collect();
            vars.push(format!(r#"{{"name": "used", "type": "integer", "lower": 1, "upper": {k}}}"#));
            let objective = if cost { "10 * used + c1" } else { "used" };
            let json = |solve: &str| {
                format!(
                    r#"{{"version": "1.1", "variables": [{}], "constraints": [{}], "solve": {solve}}}"#,
                    vars.join(", "),
                    constraints.join(", ")
                )
            };
            let ok = |p: &[i64]| {
                edges.iter().all(|&(a, b)| p[a] != p[b]) && (0..n).all(|v| p[n] >= p[v]) && (!pin || p[0] == 1)
            };
            let ranges: Vec<(i64, i64)> = vec![(1, k); n + 1];
            let feasible: Vec<Vec<i64>> = points(&ranges).into_iter().filter(|p| ok(p)).collect();
            let value = |p: &[i64]| if cost { 10 * p[n] + p[1] } else { p[n] };
            let best = feasible.iter().map(|p| value(p)).min();
            let label = format!("case {case}: {}", json(""));
            for engine in [Engine::Cp, Engine::Milp] {
                let model = load(&json(&format!(r#"{{"method": "minimize", "objective": "{objective}"}}"#))).unwrap();
                let out = solve(&model, None, engine).unwrap();
                let got = out.solutions.first().map(|x| model.objective_value(x).round() as i64);
                assert_eq!(got, best, "{engine:?} optimum, {label}");
            }
            let model = load(&json(r#"{"method": "satisfy"}"#)).unwrap();
            let out = solve(&model, None, Engine::Cp).unwrap();
            assert_eq!(out.solutions.is_empty(), feasible.is_empty(), "satisfy, {label}");
            if let Some(x) = out.solutions.first() {
                let p: Vec<i64> = x.iter().map(|v| v.round() as i64).collect();
                assert!(ok(&p), "satisfy returned {p:?}, {label}");
            }
        }
    }
}
