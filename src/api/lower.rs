//! Lowers a builder [`Model`] to the solver's model, through the same
//! [`Assembly`] that problem files go through, with the same checks and
//! messages.

use super::builder::{Family, Item, Model, VarKind};
use super::expression::{Expression, LiteralKind, Owner, Tree, Variable};
use super::family::{Element, Set};
use crate::encode::{Affine, Global, Interval, Literal};
use crate::model::{Assembly, INF, LinearModel, NOT_AFFINE, NonlinearRow, Row, VarInfo};
use crate::nlp::graph::{Graph, Node, Op};
use crate::problem::{Elem, VarType};
use std::collections::HashSet;

type R<T> = Result<T, String>;

/// Whether `name` is an identifier of the expression grammar.
pub(crate) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    // As in problem files, function names aren't reserved (`exp[i]` and
    // `exp(x)` are told apart by the bracket).
    first_ok
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !matches!(name, "sum" | "in" | "and" | "or" | "not")
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
                 digit, and not one of sum, in, and, or, not)"
            ));
        }
        if !seen.insert(name.clone()) {
            return Err(format!("two variables are named {name}"));
        }
    }
    Ok(())
}

/// Checks that a family's specs agree on what the format declares once per
/// variable: the kind, the domain, and a symbolic variable's set.
pub(crate) fn check_family(f: &Family) -> R<()> {
    let first = f.spec(0);
    let same_values = |a: &Option<Set>, b: &Option<Set>| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.same(b),
        _ => false,
    };
    if f.specs
        .iter()
        .any(|s| s.kind != first.kind || s.domain != first.domain || !same_values(&s.values, &first.values))
    {
        return Err(format!(
            "variable {}: every element must have the same kind and domain (only bounds and starts may differ)",
            f.name()
        ));
    }
    Ok(())
}

/// A column's bounds, and its domain's ranges when they have gaps.
pub(crate) type ColBounds = (f64, f64, Option<Vec<(i64, i64)>>);

/// The bounds of a family's `k`th variable, and its domain's ranges when
/// they have gaps: from its spec, then `Model::set_bounds`.
pub(crate) fn col_bounds(model: &Model, f: &Family, k: usize) -> R<ColBounds> {
    let s = f.spec(k);
    let name = || if f.indexed() { format!("{}[{}]", f.name(), key_text(&f.key(k))) } else { f.name() };
    let (lo, hi, ranges) = match (s.kind, &s.domain, &s.values) {
        (_, _, Some(set)) => (1.0, set.len() as f64, None),
        (VarKind::Binary, _, _) => (0.0, 1.0, None),
        (VarKind::Continuous, Some(_), _) => {
            return Err(format!("variable {}: a domain needs an integer variable", f.name()));
        }
        (_, Some(ranges), _) => {
            if ranges.is_empty() {
                return Err(format!("variable {}: the domain is empty", f.name()));
            }
            (ranges[0].0 as f64, ranges[ranges.len() - 1].1 as f64, Some(ranges.clone()))
        }
        _ => (s.lower.unwrap_or(-INF), s.upper.unwrap_or(INF), None),
    };
    if lo.is_nan() || hi.is_nan() || s.start.is_some_and(|v| !v.is_finite()) {
        return Err(format!("variable {}: a bound or start isn't a number", name()));
    }
    let Some(&(l, u)) = model.bounds.get(&(f.first + k)) else {
        return Ok((lo, hi, ranges.filter(|r| r.len() > 1)));
    };
    if l.is_nan() || u.is_nan() {
        return Err(format!("variable {}: a bound isn't a number", name()));
    }
    if s.values.is_some() {
        return Err(format!("variable {}: a symbolic variable's bounds can't be set", name()));
    }
    Ok(match (s.kind, ranges) {
        (VarKind::Binary, _) => (l.max(0.0), u.min(1.0), None),
        (_, Some(ranges)) => {
            // The domain's values within the new bounds.
            let (a, b) = (l.ceil().max(-9e18) as i64, u.floor().min(9e18) as i64);
            let clipped: Vec<(i64, i64)> =
                ranges.iter().filter(|r| r.1 >= a && r.0 <= b).map(|r| (r.0.max(a), r.1.min(b))).collect();
            match clipped.as_slice() {
                [] => (l, l - 1.0, None),
                [one] => (one.0 as f64, one.1 as f64, None),
                _ => (clipped[0].0 as f64, clipped[clipped.len() - 1].1 as f64, Some(clipped)),
            }
        }
        _ => (l, u, None),
    })
}

/// A key as written in subscripts and messages: `north,3`.
pub(crate) fn key_text(key: &[Element]) -> String {
    key.iter().map(|e| e.to_string()).collect::<Vec<_>>().join(",")
}

/// The name of a column's variable, for messages: `x` or `x[north,3]`.
pub(crate) fn col_name(model: &Model, col: usize) -> String {
    let (f, k) = model.family_of(col);
    if f.indexed() { format!("{}[{}]", f.name(), key_text(&f.key(k))) } else { f.name() }
}

struct Lowering<'m> {
    model: &'m Model,
    graph: Graph,
    kinds: Vec<VarKind>,
    /// Each column's set, for symbolic variables.
    symbolic: Vec<Option<Set>>,
}

impl Lowering<'_> {
    /// The column of an expression that is one variable (coefficient 1, no
    /// constant or nonlinear terms).
    fn bare(&self, e: &Expression) -> Option<usize> {
        let mut it = e.linear.iter().filter(|c| *c.1 != 0.0);
        match (it.next(), it.next()) {
            (Some((&j, &a)), None) if a == 1.0 && e.constant == 0.0 && e.nonlinear.is_empty() => Some(j),
            _ => None,
        }
    }

    /// An error if `e` uses a symbolic variable (which has no arithmetic).
    fn no_symbolic(&self, e: &Expression, place: &str) -> R<()> {
        let mut cols = Vec::new();
        e.columns(&mut cols);
        match cols.into_iter().find(|&j| self.symbolic.get(j).is_some_and(Option::is_some)) {
            Some(j) => Err(format!(
                "{place}: symbolic variable {} can't be used in arithmetic (its values are set elements)",
                self.model.family_of(j).0.name()
            )),
            None => Ok(()),
        }
    }

    /// The integer a column takes for `e`: the number itself, or for a
    /// symbolic column the element's 1-based position in its set (None if
    /// it isn't in the set, as for problem files: `==` never holds).
    fn column_value(&self, col: usize, e: &Element, place: &str) -> R<Option<i64>> {
        match (&self.symbolic[col], e) {
            (Some(set), e) => Ok(set.position(e).map(|p| p as i64 + 1)),
            (None, Element::Int(i)) => Ok(Some(*i)),
            (None, Element::Str(s)) => {
                Err(format!("{place}: {s:?} compared with a numeric variable ({})", col_name(self.model, col)))
            }
        }
    }

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
        self.no_symbolic(e, place)?;
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

    /// `all_different` items: affine expressions without symbolic
    /// variables, or symbolic variables of one set, compared by position.
    fn all_different(&self, items: &[Expression], place: &str) -> R<Vec<Affine>> {
        let bare = |e: &Expression| -> Option<usize> {
            let mut it = e.linear.iter().filter(|c| *c.1 != 0.0);
            match (it.next(), it.next()) {
                (Some((&j, &a)), None) if a == 1.0 && e.constant == 0.0 && e.nonlinear.is_empty() => Some(j),
                _ => None,
            }
        };
        let symbolic_set = |e: &Expression| bare(e).and_then(|j| self.symbolic.get(j)?.clone());
        let Some(set) = items.iter().find_map(symbolic_set) else {
            return items.iter().map(|e| self.affine(e, place)).collect();
        };
        items
            .iter()
            .map(|e| {
                self.owned(e.owner, place)?;
                match (bare(e), symbolic_set(e)) {
                    (Some(j), Some(s)) if s.same(&set) => Ok(Affine { coefs: vec![(j, 1.0)], constant: 0.0 }),
                    _ => Err(format!(
                        "{place}: symbolic variables can only be all_different with variables of the same set"
                    )),
                }
            })
            .collect()
    }

    fn interval(&self, k: usize) -> R<Interval> {
        let spec = &self.model.intervals[k];
        let label = spec.name.clone().unwrap_or_else(|| format!("_iv{k}"));
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
    let mut symbolic = Vec::with_capacity(n);
    let mut vars = Vec::with_capacity(model.families.len());
    for f in &model.families {
        check_family(f)?;
        let (kind, values) = (f.spec(0).kind, f.spec(0).values.clone());
        let mut elements = Vec::with_capacity(f.len);
        for k in 0..f.len {
            let (lo, hi, domain) = col_bounds(model, f, k)?;
            lower.push(lo);
            upper.push(hi);
            integer.push(kind != VarKind::Continuous);
            domains.push(domain);
            starts.push(f.spec(k).start);
            kinds.push(kind);
            symbolic.push(values.clone());
            elements.push((f.key(k).iter().map(Element::to_elem).collect(), f.first + k));
        }
        let symbolic = values.map(|set| set.elements().iter().map(Element::to_elem).collect::<Vec<Elem>>());
        vars.push(VarInfo { name: f.name(), indexed: f.indexed(), var_type: var_type(kind), elements, symbolic });
    }
    if model.all_solutions
        && let Some(v) = vars.iter().find(|v| v.var_type == VarType::Continuous)
    {
        return Err(format!(
            "unsupported: \"solutions\": \"all\" requires every variable to be integer or binary, but {} is continuous",
            v.name
        ));
    }

    let mut low = Lowering { model, graph: Graph::new(), kinds, symbolic };
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
        let default_label = model.labels.get(&i).cloned().unwrap_or_else(|| format!("#{}", i + 1));
        match item {
            Item::Expr(c) => {
                let label = c.name.clone().unwrap_or(default_label);
                let place = format!("constraint {label}");
                low.owned(c.expr.owner, &place)?;
                let (coefs, lo, hi, constant) = if let Some(e) = &c.element {
                    // `v.equals(e)`: v at e's value (for a symbolic v, its
                    // position); an element outside its set is infeasible
                    // (0 == 1), as for problem files.
                    let col = low.bare(&c.expr).ok_or_else(|| format!("{place}: equals needs one variable"))?;
                    match low.column_value(col, e, &place)? {
                        Some(p) => (vec![(col, 1.0)], p as f64, p as f64, -p as f64),
                        None => (vec![], 1.0, 1.0, -1.0),
                    }
                } else {
                    low.no_symbolic(&c.expr, &place)?;
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
                    (coefs, lo, hi, constant)
                };
                // An element outside a symbolic variable's set: `is` never
                // holds (nothing to enforce), `is_not` always does (dropped).
                let mut literals = Vec::with_capacity(c.literals.len());
                let mut never = false;
                for lit in &c.literals {
                    let col = low.col(lit.var, &place)?;
                    match &lit.kind {
                        LiteralKind::True => literals.push(Literal::Col { col, negated: false }),
                        LiteralKind::False => literals.push(Literal::Col { col, negated: true }),
                        LiteralKind::Is(e) | LiteralKind::IsNot(e) => {
                            let equal = matches!(lit.kind, LiteralKind::Is(_));
                            match low.column_value(col, e, &place)? {
                                Some(value) => literals.push(Literal::Value { col, value, equal }),
                                None => never |= equal,
                            }
                        }
                    }
                }
                if never {
                    continue;
                }
                // With literals that all hold, still an enforced constraint
                // (which the encoder writes as a plain row), as for problem files.
                if !c.literals.is_empty() {
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
                let items = low.all_different(items, &place)?;
                globals.push(Global::AllDifferent { label: default_label, items });
            }
            Item::Table { vars, tuples, forbidden } => {
                let place = format!("constraint {default_label}");
                let cols = vars.iter().map(|&v| low.col(v, &place)).collect::<R<Vec<_>>>()?;
                let mut values = Vec::with_capacity(tuples.len());
                'tuples: for t in tuples {
                    if t.len() != cols.len() {
                        return Err(format!("{place}: a tuple has {} entries for {} variables", t.len(), cols.len()));
                    }
                    let mut row = Vec::with_capacity(t.len());
                    for (&col, e) in cols.iter().zip(t) {
                        match low.column_value(col, e, &place)? {
                            Some(v) => row.push(v),
                            None => continue 'tuples, // an element outside the set never matches
                        }
                    }
                    values.push(row);
                }
                globals.push(Global::Table { label: default_label, vars: cols, tuples: values, forbidden: *forbidden });
            }
            Item::Element { index, items, value } => {
                let place = format!("constraint {default_label}");
                // A symbolic index is a bare variable, keyed by its set's elements.
                let mut index_sets = Vec::with_capacity(index.len());
                let mut affine_index = Vec::with_capacity(index.len());
                for e in index {
                    match low.bare(e).filter(|&j| low.symbolic[j].is_some()) {
                        Some(j) => {
                            low.owned(e.owner, &place)?;
                            index_sets.push(Some(j));
                            affine_index.push(Affine { coefs: vec![(j, 1.0)], constant: 0.0 });
                        }
                        None => {
                            index_sets.push(None);
                            affine_index.push(low.affine(e, &place)?);
                        }
                    }
                }
                let index = affine_index;
                let mut combos = Vec::with_capacity(items.len());
                for (key, item) in items {
                    if key.len() != index.len() {
                        return Err(format!("{place}: a key has {} elements for {} indexes", key.len(), index.len()));
                    }
                    let mut values = Vec::with_capacity(key.len());
                    for (e, &col) in key.iter().zip(&index_sets) {
                        values.push(match (col, e) {
                            (Some(j), e) => low.column_value(j, e, &place)?.ok_or_else(|| {
                                format!("{place}: {e} isn't one of the values of {}", col_name(model, j))
                            })?,
                            (None, Element::Int(i)) => *i,
                            (None, Element::Str(s)) => {
                                return Err(format!("{place}: the key {s:?} needs a symbolic index"));
                            }
                        });
                    }
                    combos.push((values, low.affine(item, &place)?));
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
            Item::Removed => {}
        }
    }

    // Objective
    let objective = &model.objective;
    low.owned(objective.owner, "the objective")?;
    low.no_symbolic(objective, "the objective")?;
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
