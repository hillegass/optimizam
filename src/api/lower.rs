//! Lowers a builder [`Model`] to the solver's model, through the same
//! [`Assembly`] that problem files go through, with the same checks and
//! messages.

use super::builder::{Family, Item, Model, VarKind};
use super::expression::{Expression, LiteralKind, Mode, Owner, Tree, Variable};
use super::family::{Element, Set};
use crate::encode::{Affine, Global, Interval, Literal};
use crate::model::{Assembly, INF, LinearModel, Loose, NOT_AFFINE, NonlinearRow, Row, Sense, Term, VarInfo};
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

/// Which way a term is pushed by what it appears in: down by an objective to
/// minimize or by a row's upper limit, up by the opposite, both ways by a
/// row with two limits.
#[derive(Clone, Copy, PartialEq)]
enum Pull {
    Down,
    Up,
    Both,
}

impl Pull {
    /// The pull on a term with coefficient `a`.
    fn on(self, a: f64) -> Pull {
        match self {
            Pull::Down if a < 0.0 => Pull::Up,
            Pull::Up if a < 0.0 => Pull::Down,
            pull => pull,
        }
    }
}

struct Lowering<'m> {
    model: &'m Model,
    graph: Graph,
    kinds: Vec<VarKind>,
    /// Each column's set, for symbolic variables.
    symbolic: Vec<Option<Set>>,
    lower: Vec<f64>,
    upper: Vec<f64>,
    /// Whether every solution is wanted (then no column may be left loose).
    enumerate: bool,
    /// What holds the columns of `abs`, `min`, and `max` terms to their
    /// values: rows, added after the model's own, and instances.
    piece_rows: Vec<Row>,
    globals: Vec<Global>,
    /// The columns that rows hold from one side only.
    loose: Vec<Loose>,
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

    /// The integer a column takes for `e`, which must be one of its values
    /// if the column is symbolic: `is`, `is_not`, and `equals` name an
    /// element in the program's text, where one outside the set is a slip.
    fn value_of(&self, col: usize, e: &Element, place: &str) -> R<i64> {
        let quoted = |e: &Element| match e {
            Element::Str(s) => format!("{s:?}"),
            other => other.to_string(),
        };
        self.column_value(col, e, place)?.ok_or_else(|| {
            let values: Vec<String> = self.symbolic[col].iter().flat_map(|s| s.elements()).map(quoted).collect();
            let name = col_name(self.model, col);
            format!("{place}: {} isn't one of the values of {name} ({})", quoted(e), values.join(", "))
        })
    }

    /// The integer a column takes for `e`: the number itself, or for a
    /// symbolic column the element's 1-based position in its set (None if
    /// it isn't in the set: in a table, such a tuple never matches).
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
    fn affine(&mut self, e: &Expression, place: &str) -> R<Affine> {
        self.affine_pulled(e, Pull::Both, place)
    }

    /// The affine expression, with a column for each `abs`, `min`, and `max`
    /// term; `pull` says which way the expression is pushed.
    fn affine_pulled(&mut self, e: &Expression, pull: Pull, place: &str) -> R<Affine> {
        self.owned(e.owner, place)?;
        self.no_symbolic(e, place)?;
        if e.nonlinear.iter().any(|t| !t.1.piecewise()) {
            return Err(format!("unsupported: nonlinear term ({NOT_AFFINE}) in {place}"));
        }
        let mut coefs: Vec<(usize, f64)> = e.linear.iter().filter(|c| *c.1 != 0.0).map(|(&j, &a)| (j, a)).collect();
        coefs.extend(self.pieces(e, pull, place)?);
        if !e.constant.is_finite() || coefs.iter().any(|c| !c.1.is_finite()) {
            return Err(not_finite(place));
        }
        Ok(Affine { coefs, constant: e.constant })
    }

    /// The columns of the expression's own `abs`, `min`, and `max` terms,
    /// each with the term's coefficient. (New columns, so they come after
    /// every column the expression names.)
    fn pieces(&mut self, e: &Expression, pull: Pull, place: &str) -> R<Vec<(usize, f64)>> {
        let mut coefs = Vec::new();
        for (a, t) in e.nonlinear.iter().filter(|t| t.1.piecewise() && t.0 != 0.0) {
            coefs.push((self.piece(t, pull.on(*a), place)?, *a));
        }
        Ok(coefs)
    }

    /// The items of an `abs`, `min`, or `max` term (for `abs`, the one
    /// argument), pushed the way `pull` says.
    fn items(&mut self, t: &Tree, pull: Pull, place: &str) -> R<Vec<Affine>> {
        let (name, args): (&str, &[Expression]) = match t {
            Tree::Abs(a) => ("abs", std::slice::from_ref(a)),
            Tree::Min(items) => ("min", items),
            Tree::Max(items) => ("max", items),
            _ => unreachable!("not a piecewise linear term"),
        };
        if args.iter().any(|e| e.nonlinear.iter().any(|t| !t.1.piecewise())) {
            return Err(format!(
                "unsupported: {name} of a nonlinear expression in {place} (its arguments must be affine)"
            ));
        }
        args.iter().map(|e| self.affine_pulled(e, pull, place)).collect()
    }

    /// Holds `value` to the term exactly, by the encoding of its kind, which
    /// needs every item's range to be finite.
    fn exact(&mut self, t: &Tree, value: Affine, mut items: Vec<Affine>, place: &str) -> R<()> {
        let unbounded = |a: &Affine| {
            let (lo, hi) = self.range(a);
            !lo.is_finite() || !hi.is_finite()
        };
        if items.iter().any(unbounded) {
            let (name, also) = match t {
                Tree::Abs(_) => ("abs", ""),
                Tree::Max(_) => ("max", ""),
                _ => ("min", ", for a min maximized or bounded below"),
            };
            return Err(format!(
                "{place}: this {name} needs finite bounds on the variables inside it (it is free of that only \
                 where it is minimized or bounded above{also})"
            ));
        }
        let label = place.strip_prefix("constraint ").unwrap_or(place).to_string();
        self.globals.push(match t {
            Tree::Abs(_) => Global::Abs { label, value, of: items.swap_remove(0) },
            _ => Global::MinMax { label, value, items, maximize: matches!(t, Tree::Max(_)) },
        });
        Ok(())
    }

    /// What `value == abs(..)` (or `min`, or `max`) says: `value` held to
    /// the term, with no column for the term itself.
    fn hold(&mut self, t: &Tree, value: Affine, place: &str) -> R<()> {
        let items = self.items(t, Pull::Both, place)?;
        self.exact(t, value, items, place)
    }

    /// A new column for an `abs`, `min`, or `max` term, with bounds from its
    /// arguments' ranges. Where only one side of the term can matter (an
    /// absolute value or a maximum pushed down, a minimum pushed up), rows
    /// hold it from that side, and nothing else is needed; anywhere else the
    /// term's encoding holds it exactly.
    fn piece(&mut self, t: &Tree, pull: Pull, place: &str) -> R<usize> {
        let (term, rows_do) = match t {
            Tree::Abs(_) => (Term::Abs, pull == Pull::Down),
            Tree::Max(_) => (Term::Max, pull == Pull::Down),
            Tree::Min(_) => (Term::Min, pull == Pull::Up),
            _ => unreachable!("not a piecewise linear term"),
        };
        let rows_do = rows_do && !self.enumerate;
        // Inside a maximum or a minimum the pull goes on to the items; an
        // absolute value takes its argument with both signs.
        let inner = if rows_do && term != Term::Abs { pull } else { Pull::Both };
        let items = self.items(t, inner, place)?;
        let ranges: Vec<(f64, f64)> = items.iter().map(|a| self.range(a)).collect();
        let least = |pick: fn(f64, f64) -> f64, start: f64| ranges.iter().map(|r| r.0).fold(start, pick);
        let most = |pick: fn(f64, f64) -> f64, start: f64| ranges.iter().map(|r| r.1).fold(start, pick);
        let (lo, hi) = match t {
            Tree::Abs(_) => (ranges[0].0.max(-ranges[0].1).max(0.0), ranges[0].1.max(-ranges[0].0)),
            Tree::Max(_) => (least(f64::max, -INF), most(f64::max, -INF)),
            _ => (least(f64::min, INF), most(f64::min, INF)),
        };
        let integral = items.iter().all(|a| a.constant.fract() == 0.0 && self.integral(&a.coefs));
        let col = self.lower.len();
        self.lower.push(lo);
        self.upper.push(hi);
        self.kinds.push(if integral { VarKind::Integer } else { VarKind::Continuous });
        self.symbolic.push(None);
        let value = Affine { coefs: vec![(col, 1.0)], constant: 0.0 };
        if rows_do {
            // column - item >= 0 (for a minimum, <= 0), and for abs the same with -item.
            let signs: &[f64] = if term == Term::Abs { &[1.0, -1.0] } else { &[1.0] };
            for item in &items {
                for &sign in signs {
                    let mut coefs: Vec<(usize, f64)> = item.coefs.iter().map(|&(j, a)| (j, -sign * a)).collect();
                    coefs.push((col, 1.0));
                    let (lower, upper) = match t {
                        Tree::Min(_) => (-INF, sign * item.constant),
                        _ => (sign * item.constant, INF),
                    };
                    self.piece_rows.push(Row { coefs, lower, upper });
                }
            }
            self.loose.push(Loose { col, items, term });
        } else {
            self.exact(t, value, items, place)?;
        }
        Ok(col)
    }

    /// A new binary column.
    fn binary(&mut self) -> usize {
        self.lower.push(0.0);
        self.upper.push(1.0);
        self.kinds.push(VarKind::Binary);
        self.symbolic.push(None);
        self.lower.len() - 1
    }

    /// Whether the coefficients are whole numbers on integer columns.
    fn integral(&self, coefs: &[(usize, f64)]) -> bool {
        coefs.iter().all(|&(j, a)| a.fract() == 0.0 && self.kinds[j] != VarKind::Continuous)
    }

    /// The least and the greatest value of `a` within the columns' bounds.
    fn range(&self, a: &Affine) -> (f64, f64) {
        let (mut lo, mut hi) = (a.constant, a.constant);
        for &(j, c) in &a.coefs {
            let (l, u) = (self.lower[j], self.upper[j]);
            lo += if c > 0.0 { c * l } else { c * u };
            hi += if c > 0.0 { c * u } else { c * l };
        }
        (lo, hi)
    }

    /// The expression's nonlinear terms as one graph node, or None if it has
    /// none (or they fold to a constant, which is added to `constant`); its
    /// own `abs`, `min`, and `max` terms aren't among them ([`Self::pieces`]).
    fn nonlinear_part(&mut self, e: &Expression, constant: &mut f64, place: &str) -> R<Option<Node>> {
        if e.nonlinear.iter().all(|t| t.1.piecewise()) {
            return Ok(None);
        }
        let mut terms = Vec::with_capacity(e.nonlinear.len());
        for (a, t) in e.nonlinear.iter().filter(|t| !t.1.piecewise()) {
            terms.push((*a, self.tree(t, place)?));
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
    fn node(&mut self, e: &Expression, place: &str) -> R<Node> {
        let mut terms: Vec<(f64, Node)> =
            e.linear.iter().filter(|c| *c.1 != 0.0).map(|(&j, &a)| (a, self.graph.var(j))).collect();
        for (a, t) in &e.nonlinear {
            terms.push((*a, self.tree(t, place)?));
        }
        Ok(self.graph.sum(e.constant, &terms))
    }

    fn tree(&mut self, t: &Tree, place: &str) -> R<Node> {
        Ok(match t {
            Tree::Mul(a, b) => {
                let (a, b) = (self.node(a, place)?, self.node(b, place)?);
                self.graph.mul(a, b)
            }
            Tree::Div(a, b) => {
                let (a, b) = (self.node(a, place)?, self.node(b, place)?);
                self.graph.div(a, b)
            }
            Tree::PowI(a, n) => {
                let a = self.node(a, place)?;
                self.graph.powi(a, *n)
            }
            Tree::Pow(a, b) => {
                let (a, b) = (self.node(a, place)?, self.node(b, place)?);
                self.graph.pow(a, b)
            }
            Tree::Func(f, a) => {
                let a = self.node(a, place)?;
                self.graph.func(*f, a)
            }
            // Inside a nonlinear term, a column held exactly.
            Tree::Abs(_) | Tree::Min(_) | Tree::Max(_) => {
                let col = self.piece(t, Pull::Both, place)?;
                self.graph.var(col)
            }
        })
    }

    /// `all_different` items: affine expressions without symbolic
    /// variables, or symbolic variables of one set, compared by position.
    fn all_different(&mut self, items: &[Expression], place: &str) -> R<Vec<Affine>> {
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

    fn interval(&mut self, k: usize) -> R<Interval> {
        let spec = &self.model.intervals[k];
        let label = spec.name.clone().unwrap_or_else(|| format!("_iv{k}"));
        let place = format!("interval {label}");
        let start = self.affine(&spec.start, &place)?;
        let duration = self.affine(&spec.duration, &place)?;
        let integral = |a: &Affine| a.constant.fract() == 0.0 && self.integral(&a.coefs);
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
    if model.solution_limit == Some(0) {
        return Err("a solution limit must be 1 or more".into());
    }
    let enumerate = model.all_solutions || model.solution_limit.is_some();
    if enumerate && let Some(v) = vars.iter().find(|v| v.var_type == VarType::Continuous) {
        return Err(format!(
            "unsupported: \"solutions\": \"all\" (or a number) requires every variable to be integer or binary, \
             but {} is continuous",
            v.name
        ));
    }

    let mut low = Lowering {
        model,
        graph: Graph::new(),
        kinds,
        symbolic,
        lower,
        upper,
        enumerate,
        piece_rows: Vec::new(),
        globals: Vec::new(),
        loose: Vec::new(),
    };
    let (mut rows, mut row_items) = (Vec::new(), Vec::new());
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
        let default_label = model.labels.get(&i).cloned().unwrap_or_else(|| format!("constraints[{i}]"));
        match item {
            Item::Expr(c) => {
                let label = c.name.clone().unwrap_or(default_label);
                let place = format!("constraint {label}");
                low.owned(c.expr.owner, &place)?;
                // `!=` and strict comparisons mean something between whole numbers only.
                let whole = |low: &Lowering, coefs: &[(usize, f64)], constant: f64, smooth: bool| -> R<()> {
                    if c.mode == Mode::Within || (smooth && constant.fract() == 0.0 && low.integral(coefs)) {
                        return Ok(());
                    }
                    Err(match c.mode {
                        Mode::Differs => format!(
                            "{place}: != needs integer-valued sides (integer variables with whole coefficients): \
                             nothing keeps two real numbers apart"
                        ),
                        _ => format!(
                            "{place}: a strict comparison (< or >) needs integer-valued sides (integer variables \
                             with whole coefficients); between real numbers, write <= or >="
                        ),
                    })
                };
                let (coefs, lo, hi, constant) = if let Some(e) = &c.element {
                    // A literal as a constraint: v at e's value (for a
                    // symbolic v, its position), or not at it.
                    let col = low.bare(&c.expr).ok_or_else(|| format!("{place}: no variable to compare"))?;
                    let p = low.value_of(col, e, &place)? as f64;
                    (vec![(col, 1.0)], p, p, -p)
                } else {
                    // Two symbolic variables of one set may be equal or
                    // differ (by their positions); nothing else is arithmetic.
                    let same_set = c.expr.as_difference().is_some_and(
                        |(x, y)| matches!((&low.symbolic[x], &low.symbolic[y]), (Some(a), Some(b)) if a.same(b)),
                    );
                    if !(same_set && c.lower == 0.0 && c.upper == 0.0) {
                        low.no_symbolic(&c.expr, &place)?;
                    }
                    if c.lower.is_nan() || c.upper.is_nan() {
                        return Err(format!("{place}: a bound isn't a number"));
                    }
                    if !c.literals.is_empty() && c.expr.nonlinear.iter().any(|t| !t.1.piecewise()) {
                        return Err(format!("unsupported: nonlinear term ({NOT_AFFINE}) in {place}"));
                    }
                    // `value == max(..)`: the equation's other side is held to
                    // the term itself, which then needs no column of its own.
                    if let ([(a, t)], Mode::Within, true) = (c.expr.nonlinear.as_slice(), c.mode, c.literals.is_empty())
                        && t.piecewise()
                        && a.abs() == 1.0
                        && c.lower == c.upper
                    {
                        // a * term + rest == lower, so term == (lower - rest) / a.
                        let rest = Expression { nonlinear: Vec::new(), ..c.expr.clone() };
                        let mut value = low.affine(&rest, &place)?;
                        value.coefs.iter_mut().for_each(|coef| coef.1 /= -a);
                        value.constant = (c.lower - value.constant) / a;
                        if !value.constant.is_finite() {
                            return Err(format!("{place}: a bound isn't a number"));
                        }
                        low.hold(t, value, &place)?;
                        continue;
                    }
                    let mut constant = c.expr.constant;
                    let node = low.nonlinear_part(&c.expr, &mut constant, &place)?;
                    let mut coefs: Vec<(usize, f64)> =
                        c.expr.linear.iter().filter(|c| *c.1 != 0.0).map(|(&j, &a)| (j, a)).collect();
                    let pull = match (c.lower == -INF, c.upper == INF) {
                        (true, false) => Pull::Down,
                        (false, true) => Pull::Up,
                        _ => Pull::Both,
                    };
                    coefs.extend(low.pieces(&c.expr, pull, &place)?);
                    if !constant.is_finite() || coefs.iter().any(|c| !c.1.is_finite()) {
                        return Err(not_finite(&place));
                    }
                    let (lo, hi) = (c.lower - constant, c.upper - constant);
                    whole(&low, &coefs, constant, node.is_none())?;
                    if let Some(node) = node {
                        nonlinear_rows.push(NonlinearRow { label, coefs, node, lower: lo, upper: hi });
                        continue;
                    }
                    (coefs, lo, hi, constant)
                };
                whole(&low, &coefs, constant, true)?;
                let mut literals = Vec::with_capacity(c.literals.len());
                for lit in &c.literals {
                    let col = low.col(lit.var, &place)?;
                    match &lit.kind {
                        LiteralKind::True => literals.push(Literal::Col { col, negated: false }),
                        LiteralKind::False => literals.push(Literal::Col { col, negated: true }),
                        LiteralKind::Is(e) | LiteralKind::IsNot(e) => {
                            let equal = matches!(lit.kind, LiteralKind::Is(_));
                            literals.push(Literal::Value { col, value: low.value_of(col, e, &place)?, equal });
                        }
                    }
                }
                if c.mode == Mode::Differs {
                    if !literals.is_empty() {
                        // Below the value or above it, as a new binary says
                        // (which is 0 while a literal fails, so that every
                        // solution is still found once).
                        let side = low.binary();
                        for (lower, upper, negated) in [(-INF, lo - 1.0, false), (lo + 1.0, INF, true)] {
                            let mut literals = literals.clone();
                            literals.push(Literal::Col { col: side, negated });
                            let (label, coefs) = (label.clone(), coefs.clone());
                            globals.push(Global::Enforced { label, coefs, lower, upper, literals });
                        }
                        for literal in literals {
                            let fails = match literal {
                                Literal::Col { col, negated } => Literal::Col { col, negated: !negated },
                                Literal::Value { col, value, equal } => Literal::Value { col, value, equal: !equal },
                            };
                            let (label, coefs) = (label.clone(), vec![(side, 1.0)]);
                            globals.push(Global::Enforced {
                                label,
                                coefs,
                                lower: 0.0,
                                upper: 0.0,
                                literals: vec![fails],
                            });
                        }
                    } else if coefs.is_empty() {
                        if lo == 0.0 && violated.is_none() {
                            violated = Some(label);
                        }
                    } else {
                        // The terms with positive coefficients differ from the
                        // rest: `x != y` is the two of them all different.
                        let positive = coefs.iter().copied().filter(|c| c.1 > 0.0).collect();
                        let rest = coefs.iter().filter(|c| c.1 < 0.0).map(|&(j, a)| (j, -a)).collect();
                        let items =
                            vec![Affine { coefs: positive, constant: 0.0 }, Affine { coefs: rest, constant: lo }];
                        globals.push(Global::AllDifferent { label, items });
                    }
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
                    row_items.push(i);
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
    let pull = match model.sense {
        Sense::Minimize => Pull::Down,
        Sense::Maximize => Pull::Up,
        Sense::Satisfy => Pull::Both,
    };
    let pieces = low.pieces(objective, pull, "the objective")?;
    let mut obj = vec![0.0; low.lower.len()];
    for (&j, &a) in objective.linear.iter().chain(pieces.iter().map(|(j, a)| (j, a))) {
        obj[j] = a;
    }
    // The columns of abs, min, and max terms, and what holds them.
    let Lowering { graph, kinds, lower, upper, piece_rows, globals: piece_globals, loose, .. } = low;
    integer.extend(kinds[n..].iter().map(|&kind| kind != VarKind::Continuous));
    domains.resize(lower.len(), None);
    rows.extend(piece_rows);
    globals.extend(piece_globals);
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
        row_items,
        nonlinear_rows,
        graph,
        globals,
        loose,
        violated,
        obj,
        obj_offset,
        objective_node,
        sense: model.sense,
        want_all: enumerate,
        solution_limit: model.solution_limit,
    }
    .finish()
}
