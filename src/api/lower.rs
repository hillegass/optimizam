//! Lowers a builder [`Model`] to the solver's model, through the same
//! [`Assembly`] that problem files go through, with the same checks and
//! messages.

use super::builder::{Item, Model, VarKind};
use super::expression::{Expression, LiteralKind, Owner, Tree, Variable};
use crate::encode::{Affine, Global, Interval, Literal};
use crate::model::{Assembly, INF, LinearModel, NOT_AFFINE, NonlinearRow, Row, VarInfo, to_ranges};
use crate::nlp::graph::{Graph, Node, Op};
use crate::problem::{Elem, VarType};
use std::collections::HashSet;

type R<T> = Result<T, String>;

/// Whether `name` is an identifier of the expression grammar.
pub(crate) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !matches!(name, "sum" | "in" | "and" | "or" | "not")
        && !crate::expr::Func::ALL.iter().any(|f| f.name() == name)
}

pub(crate) fn var_type(kind: VarKind) -> VarType {
    match kind {
        VarKind::Continuous => VarType::Continuous,
        VarKind::Integer => VarType::Integer,
        VarKind::Binary => VarType::Binary,
    }
}

/// Checks the variable names: identifiers, and unique.
pub(crate) fn check_names(model: &Model) -> R<()> {
    let mut seen = HashSet::new();
    for f in &model.families {
        let name = f.name();
        if !is_identifier(&name) {
            return Err(format!(
                "variable name {name:?} isn't an identifier (letters, digits, and underscores, not starting with a \
                 digit, and not a keyword or function name)"
            ));
        }
        if !seen.insert(name.clone()) {
            return Err(format!("two variables are named {name}"));
        }
    }
    Ok(())
}

struct Lowering<'m> {
    model: &'m Model,
    graph: Graph,
    kinds: Vec<VarKind>,
}

impl Lowering<'_> {
    fn owned(&self, owner: Owner, place: &str) -> R<()> {
        match owner {
            Owner::None => Ok(()),
            Owner::Model(id) if id == self.model.id => Ok(()),
            _ => Err(format!("{place} uses a variable of another model")),
        }
    }

    fn col(&self, v: Variable, place: &str) -> R<usize> {
        if v.model != self.model.id {
            return Err(format!("{place} uses a variable of another model"));
        }
        Ok(v.col)
    }

    /// The affine expression, or an error if it has a nonlinear term.
    fn affine(&self, e: &Expression, place: &str) -> R<Affine> {
        self.owned(e.owner, place)?;
        if !e.is_affine() {
            return Err(format!("unsupported: nonlinear term ({NOT_AFFINE}) in {place}"));
        }
        let coefs: Vec<(usize, f64)> = e.linear.iter().filter(|c| *c.1 != 0.0).map(|(&j, &a)| (j, a)).collect();
        if !e.constant.is_finite() || coefs.iter().any(|c| !c.1.is_finite()) {
            return Err(not_finite(place));
        }
        Ok(Affine { coefs, constant: e.constant })
    }

    /// The expression's nonlinear terms as one graph node, or None if it has
    /// none (or they fold to a constant, which is added to `constant`).
    fn nonlinear_part(&mut self, e: &Expression, constant: &mut f64, place: &str) -> R<Option<Node>> {
        if e.nonlinear.is_empty() {
            return Ok(None);
        }
        let mut terms = Vec::with_capacity(e.nonlinear.len());
        for (a, t) in &e.nonlinear {
            terms.push((*a, self.tree(t)));
        }
        let node = self.graph.sum(0.0, &terms);
        match *self.graph.op(node) {
            Op::Const(c) if c.is_finite() => {
                *constant += c;
                Ok(None)
            }
            Op::Const(_) => Err(not_finite(place)),
            _ => Ok(Some(node)),
        }
    }

    /// The whole expression as a graph node.
    fn node(&mut self, e: &Expression) -> Node {
        let mut terms: Vec<(f64, Node)> =
            e.linear.iter().filter(|c| *c.1 != 0.0).map(|(&j, &a)| (a, self.graph.var(j))).collect();
        for (a, t) in &e.nonlinear {
            terms.push((*a, self.tree(t)));
        }
        self.graph.sum(e.constant, &terms)
    }

    fn tree(&mut self, t: &Tree) -> Node {
        match t {
            Tree::Mul(a, b) => {
                let (a, b) = (self.node(a), self.node(b));
                self.graph.mul(a, b)
            }
            Tree::Div(a, b) => {
                let (a, b) = (self.node(a), self.node(b));
                self.graph.div(a, b)
            }
            Tree::PowI(a, n) => {
                let a = self.node(a);
                self.graph.powi(a, *n)
            }
            Tree::Pow(a, b) => {
                let (a, b) = (self.node(a), self.node(b));
                self.graph.pow(a, b)
            }
            Tree::Func(f, a) => {
                let a = self.node(a);
                self.graph.func(*f, a)
            }
        }
    }

    fn interval(&self, k: usize) -> R<Interval> {
        let spec = &self.model.intervals[k];
        let label = format!("_iv{k}");
        let place = format!("interval {label}");
        let integral = |a: &Affine| {
            a.constant.fract() == 0.0
                && a.coefs.iter().all(|&(j, c)| c.fract() == 0.0 && self.kinds[j] != VarKind::Continuous)
        };
        let start = self.affine(&spec.start, &place)?;
        let duration = self.affine(&spec.duration, &place)?;
        if !integral(&start) || !integral(&duration) {
            return Err(format!("interval {label}: start and duration must be integer-valued"));
        }
        if duration.coefs.is_empty() && duration.constant < 0.0 {
            return Err(format!("interval {label}: the duration is negative"));
        }
        let presence = match spec.presence {
            None => None,
            Some(v) => {
                let col = self.col(v, &place)?;
                if self.kinds[col] != VarKind::Binary {
                    return Err(format!("interval {label}: the presence variable must be binary"));
                }
                Some(col)
            }
        };
        Ok(Interval { label, start, duration, presence })
    }
}

fn not_finite(place: &str) -> String {
    format!("{place}: a constant isn't a finite number (a division by zero, or a function outside its domain)")
}

pub(crate) fn lower(model: &Model) -> R<LinearModel> {
    check_names(model)?;
    let n = model.num_cols;
    let (mut lower, mut upper, mut integer, mut domains, mut starts) =
        (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), vec![]);
    let mut kinds = Vec::with_capacity(n);
    let mut vars = Vec::with_capacity(model.families.len());
    for f in &model.families {
        let s = &f.spec;
        let (lo, hi, domain) = match (s.kind, &s.domain) {
            (VarKind::Binary, _) => (0.0, 1.0, None),
            (VarKind::Continuous, Some(_)) => {
                return Err(format!("variable {}: a domain needs an integer variable", f.name()));
            }
            (_, Some(values)) => {
                if values.is_empty() {
                    return Err(format!("variable {}: the domain is empty", f.name()));
                }
                let ranges = to_ranges(values);
                let (lo, hi) = (ranges[0].0 as f64, ranges[ranges.len() - 1].1 as f64);
                (lo, hi, (ranges.len() > 1).then_some(ranges))
            }
            _ => (s.lower.unwrap_or(-INF), s.upper.unwrap_or(INF), None),
        };
        if lo.is_nan() || hi.is_nan() || s.start.is_some_and(|v| !v.is_finite()) {
            return Err(format!("variable {}: a bound or start isn't a number", f.name()));
        }
        let mut elements = Vec::with_capacity(f.len);
        for k in 0..f.len {
            let col = f.first + k;
            lower.push(lo);
            upper.push(hi);
            integer.push(s.kind != VarKind::Continuous);
            domains.push(domain.clone());
            starts.push(s.start);
            kinds.push(s.kind);
            elements.push((if f.indexed { vec![Elem::Int(k as i64)] } else { vec![] }, col));
        }
        vars.push(VarInfo { name: f.name(), indexed: f.indexed, var_type: var_type(s.kind), elements, symbolic: None });
    }
    if model.all_solutions
        && let Some(v) = vars.iter().find(|v| v.var_type == VarType::Continuous)
    {
        return Err(format!(
            "unsupported: \"solutions\": \"all\" requires every variable to be integer or binary, but {} is continuous",
            v.name
        ));
    }

    let mut low = Lowering { model, graph: Graph::new(), kinds };
    let mut rows = Vec::new();
    let mut nonlinear_rows = Vec::new();
    let mut violated = None;
    let mut globals = Vec::new();

    // Intervals first, as for problem files.
    let mut intervals = Vec::with_capacity(model.intervals.len());
    for k in 0..model.intervals.len() {
        let interval = low.interval(k)?;
        if !interval.duration.coefs.is_empty() {
            globals.push(Global::IntervalDuration { interval: interval.clone() });
        }
        intervals.push(interval);
    }
    let interval_list = |ivs: &[super::builder::IntervalVar], place: &str| -> R<Vec<Interval>> {
        ivs.iter()
            .map(|iv| {
                if iv.model != model.id {
                    Err(format!("{place} uses an interval of another model"))
                } else {
                    Ok(intervals[iv.index].clone())
                }
            })
            .collect()
    };

    for (i, item) in model.items.iter().enumerate() {
        let default_label = format!("#{}", i + 1);
        match item {
            Item::Expr(c) => {
                let label = c.name.clone().unwrap_or(default_label);
                let place = format!("constraint {label}");
                low.owned(c.expr.owner, &place)?;
                if c.lower.is_nan() || c.upper.is_nan() {
                    return Err(format!("{place}: a bound isn't a number"));
                }
                if !c.literals.is_empty() && !c.expr.is_affine() {
                    return Err(format!("unsupported: nonlinear term ({NOT_AFFINE}) in {place}"));
                }
                let mut constant = c.expr.constant;
                let node = low.nonlinear_part(&c.expr, &mut constant, &place)?;
                let coefs: Vec<(usize, f64)> =
                    c.expr.linear.iter().filter(|c| *c.1 != 0.0).map(|(&j, &a)| (j, a)).collect();
                if !constant.is_finite() || coefs.iter().any(|c| !c.1.is_finite()) {
                    return Err(not_finite(&place));
                }
                let (lo, hi) = (c.lower - constant, c.upper - constant);
                if let Some(node) = node {
                    nonlinear_rows.push(NonlinearRow { label, coefs, node, lower: lo, upper: hi });
                    continue;
                }
                if !c.literals.is_empty() {
                    let mut literals = Vec::with_capacity(c.literals.len());
                    for lit in &c.literals {
                        let col = low.col(lit.var, &place)?;
                        literals.push(match lit.kind {
                            LiteralKind::True => Literal::Col { col, negated: false },
                            LiteralKind::False => Literal::Col { col, negated: true },
                            LiteralKind::Is(value) => Literal::Value { col, value, equal: true },
                            LiteralKind::IsNot(value) => Literal::Value { col, value, equal: false },
                        });
                    }
                    globals.push(Global::Enforced { label, coefs, lower: lo, upper: hi, literals });
                } else if coefs.is_empty() {
                    let tol = 1e-9 * (1.0 + constant.abs());
                    if (0.0 < lo - tol || 0.0 > hi + tol) && violated.is_none() {
                        violated = Some(label);
                    }
                } else {
                    rows.push(Row { coefs, lower: lo, upper: hi });
                }
            }
            Item::AllDifferent(items) => {
                let place = format!("constraint {default_label}");
                let items = items.iter().map(|e| low.affine(e, &place)).collect::<R<Vec<_>>>()?;
                globals.push(Global::AllDifferent { label: default_label, items });
            }
            Item::Table { vars, tuples, forbidden } => {
                let place = format!("constraint {default_label}");
                let cols = vars.iter().map(|&v| low.col(v, &place)).collect::<R<Vec<_>>>()?;
                if let Some(t) = tuples.iter().find(|t| t.len() != cols.len()) {
                    return Err(format!("{place}: a tuple has {} entries for {} variables", t.len(), cols.len()));
                }
                let (tuples, forbidden) = (tuples.clone(), *forbidden);
                globals.push(Global::Table { label: default_label, vars: cols, tuples, forbidden });
            }
            Item::Element { index, items, value } => {
                let place = format!("constraint {default_label}");
                let index = vec![low.affine(index, &place)?];
                let mut combos = Vec::with_capacity(items.len());
                for (k, item) in items.iter().enumerate() {
                    combos.push((vec![k as i64], low.affine(item, &place)?));
                }
                let value = low.affine(value, &place)?;
                globals.push(Global::Element { label: default_label, index, combos, value });
            }
            Item::MinMax { value, items, maximize } => {
                let place = format!("constraint {default_label}");
                let value = low.affine(value, &place)?;
                let items = items.iter().map(|e| low.affine(e, &place)).collect::<R<Vec<_>>>()?;
                globals.push(Global::MinMax { label: default_label, value, items, maximize: *maximize });
            }
            Item::Abs { value, of } => {
                let place = format!("constraint {default_label}");
                let (value, of) = (low.affine(value, &place)?, low.affine(of, &place)?);
                globals.push(Global::Abs { label: default_label, value, of });
            }
            Item::NoOverlap(ivs) => {
                let intervals = interval_list(ivs, &format!("constraint {default_label}"))?;
                globals.push(Global::NoOverlap { label: default_label, intervals });
            }
            Item::Cumulative { intervals: ivs, demands, capacity } => {
                let place = format!("constraint {default_label}");
                let intervals = interval_list(ivs, &place)?;
                let demands = demands.iter().map(|e| low.affine(e, &place)).collect::<R<Vec<_>>>()?;
                let capacity = low.affine(capacity, &place)?;
                globals.push(Global::Cumulative { label: default_label, intervals, demands, capacity });
            }
        }
    }

    // Objective
    let objective = &model.objective;
    low.owned(objective.owner, "the objective")?;
    let mut obj_offset = objective.constant;
    let objective_node = low.nonlinear_part(objective, &mut obj_offset, "the objective")?;
    let mut obj = vec![0.0; n];
    for (&j, &a) in &objective.linear {
        obj[j] = a;
    }
    if !obj_offset.is_finite() || obj.iter().any(|a| !a.is_finite()) {
        return Err(not_finite("the objective"));
    }

    Assembly {
        lower,
        upper,
        integer,
        domains,
        starts,
        vars,
        rows,
        nonlinear_rows,
        graph: low.graph,
        globals,
        violated,
        obj,
        obj_offset,
        objective_node,
        sense: model.sense,
        want_all: model.all_solutions,
    }
    .finish()
}
