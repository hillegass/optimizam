//! Expands a [`Problem`] into a flat linear model: one column per variable
//! element, one row per constraint instance. Rejects anything nonlinear
//! (docs/solver-design.md, "Scope").

use crate::expr::{self, BinOp, Bindings, CmpOp, Cond, Expr, List, Rel};
use crate::problem::{Bound, ConstraintKind, DomainDef, Elem, Method, ParamDef, Problem, SetDef, Solutions, VarType};
use serde_json::Value as Json;
use std::collections::{BTreeMap, HashMap};

pub const INF: f64 = f64::INFINITY;
/// Most values one column may have when `all_different` encodes them as binaries.
const MAX_ENCODED_VALUES: usize = 10_000;
/// Most binaries all the `all_different` encodings together may add.
const MAX_ENCODED_TOTAL: usize = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sense {
    Satisfy,
    Minimize,
    Maximize,
}

/// One declared variable and the columns of its elements, in index order.
pub struct VarInfo {
    pub name: String,
    pub indexed: bool,
    pub var_type: VarType,
    pub elements: Vec<(Vec<Elem>, usize)>,
    /// For a symbolic set-valued variable: its set's elements. A column's
    /// value k (1-based) stands for element k.
    pub symbolic: Option<Vec<Elem>>,
}

pub struct Row {
    pub coefs: Vec<(usize, f64)>,
    pub lower: f64,
    pub upper: f64,
}

pub struct LinearModel {
    pub col_lower: Vec<f64>,
    pub col_upper: Vec<f64>,
    pub col_integer: Vec<bool>,
    /// Allowed integer ranges for columns with a gapped `domain`.
    pub col_domain: Vec<Option<Vec<(i64, i64)>>>,
    pub rows: Vec<Row>,
    /// Objective in the problem's own sense; zero for satisfy.
    pub obj: Vec<f64>,
    pub obj_offset: f64,
    pub sense: Sense,
    pub want_all: bool,
    pub vars: Vec<VarInfo>,
    /// Set when a constraint with no variables is violated, e.g. `0 >= 1`.
    pub violated_constant_row: Option<String>,
    /// The problem uses a format 1.1 constraint kind (such as
    /// `all_different`), which the CP engine is suited to. Its constraints
    /// are encoded here as linear rows on extra (unreported) columns.
    pub uses_cp_kinds: bool,
}

impl LinearModel {
    pub fn num_cols(&self) -> usize {
        self.col_lower.len()
    }

    pub fn objective_value(&self, x: &[f64]) -> f64 {
        self.obj_offset + self.obj.iter().zip(x).map(|(c, v)| c * v).sum::<f64>()
    }
}

/// An affine function of the columns.
#[derive(Clone, Debug, Default)]
struct Lin {
    coefs: HashMap<usize, f64>,
    constant: f64,
}

#[derive(Clone, Debug)]
enum Val {
    Num(f64),
    Str(String),
    Lin(Lin),
}

fn describe(v: &Val) -> &'static str {
    match v {
        Val::Num(_) => "a number",
        Val::Str(_) => "a string set element",
        Val::Lin(_) => "an expression with variables",
    }
}

enum EvalError {
    NonLinear(&'static str),
    Other(String),
}

impl From<String> for EvalError {
    fn from(s: String) -> Self {
        EvalError::Other(s)
    }
}

type R<T> = Result<T, EvalError>;
/// Called once per binding combination, with the bound names in `env`.
type BindingFn<'f, 'a> = dyn FnMut(&Ctx<'a>, &mut Vec<(String, Val)>) -> R<()> + 'f;

struct Ctx<'a> {
    problem: &'a Problem,
    sets: HashMap<&'a str, Vec<Elem>>,
    var_cols: HashMap<&'a str, HashMap<Vec<Elem>, usize>>,
}

impl<'a> Ctx<'a> {
    fn param(&self, name: &str, args: &[Elem]) -> Result<f64, String> {
        match &self.problem.parameters[name] {
            ParamDef::Scalar(v) => {
                if args.is_empty() {
                    Ok(*v)
                } else {
                    Err(format!("scalar parameter {name} used with an index"))
                }
            }
            ParamDef::Table(t) => {
                if args.len() != t.index.len() {
                    return Err(format!("parameter {name} needs {} indices, got {}", t.index.len(), args.len()));
                }
                let missing = || {
                    t.default.ok_or_else(|| {
                        let keys: Vec<_> = args.iter().map(Elem::key).collect();
                        format!("parameter {name}[{}] has no value and no default", keys.join(","))
                    })
                };
                let mut node = &t.values;
                for (k, a) in args.iter().enumerate() {
                    match node.get(&a.key()) {
                        None => return missing(),
                        Some(Json::Object(m)) if k + 1 < args.len() => node = m,
                        Some(Json::Number(n)) if k + 1 == args.len() => {
                            return n.as_f64().ok_or_else(|| format!("parameter {name} has a bad number"));
                        }
                        Some(_) => return Err(format!("parameter {name} values are not nested like its index")),
                    }
                }
                missing()
            }
        }
    }

    fn eval(&self, e: &Expr, env: &mut Vec<(String, Val)>) -> R<Val> {
        match e {
            Expr::Num(v) => Ok(Val::Num(*v)),
            Expr::Ref(name, args) => self.eval_ref(name, args.as_deref(), env),
            Expr::Neg(a) => Ok(scale(self.eval(a, env)?, -1.0)?),
            Expr::Add(terms) => {
                let mut total = Val::Num(0.0);
                for (sign, t) in terms {
                    total = add(total, self.eval(t, env)?, *sign)?;
                }
                Ok(total)
            }
            Expr::Bin(op, a, b) => {
                let x = self.eval(a, env)?;
                let y = self.eval(b, env)?;
                match op {
                    BinOp::Mul => match (x, y) {
                        (Val::Lin(_), Val::Lin(_)) => Err(EvalError::NonLinear("a product of variables")),
                        (Val::Num(c), v) | (v, Val::Num(c)) => scale(v, c),
                        (s, t) => Err(format!("cannot multiply {} by {}", describe(&s), describe(&t)).into()),
                    },
                    BinOp::Div => match (x, y) {
                        (_, Val::Lin(_)) => Err(EvalError::NonLinear("division by a variable")),
                        (_, Val::Num(0.0)) => Err("division by zero".to_string().into()),
                        (v, Val::Num(c)) => scale(v, 1.0 / c),
                        (s, t) => Err(format!("cannot divide {} by {}", describe(&s), describe(&t)).into()),
                    },
                }
            }
            Expr::Pow(a, n) => match (self.eval(a, env)?, n) {
                (Val::Num(v), n) => Ok(Val::Num(v.powi(*n))),
                (v @ Val::Lin(_), 1) => Ok(v),
                (Val::Lin(_), 0) => Ok(Val::Num(1.0)),
                (Val::Lin(_), _) => Err(EvalError::NonLinear("a power of a variable")),
                (v, _) => Err(format!("cannot raise {} to a power", describe(&v)).into()),
            },
            Expr::Sum(b, body) => {
                let mut total = Val::Num(0.0);
                self.for_each_binding(b, env, &mut |ctx, env| {
                    let term = ctx.eval(body, env)?;
                    total = add(std::mem::replace(&mut total, Val::Num(0.0)), term, 1.0)?;
                    Ok(())
                })?;
                Ok(total)
            }
        }
    }

    fn eval_ref(&self, name: &str, args: Option<&[Expr]>, env: &mut Vec<(String, Val)>) -> R<Val> {
        if args.is_none()
            && let Some((_, v)) = env.iter().rev().find(|(n, _)| n == name)
        {
            return Ok(v.clone());
        }
        let keys = match args {
            None => Vec::new(),
            Some(args) => {
                let mut keys = Vec::with_capacity(args.len());
                for a in args {
                    keys.push(to_elem(self.eval(a, env)?)?);
                }
                keys
            }
        };
        if self.problem.parameters.contains_key(name) {
            return Ok(Val::Num(self.param(name, &keys)?));
        }
        if let Some(cols) = self.var_cols.get(name) {
            return match cols.get(&keys) {
                Some(&col) => Ok(Val::Lin(Lin { coefs: HashMap::from([(col, 1.0)]), constant: 0.0 })),
                None => {
                    let k: Vec<_> = keys.iter().map(Elem::key).collect();
                    Err(format!("{name}[{}] is outside the variable's index sets", k.join(",")).into())
                }
            };
        }
        Err(format!("unknown name {name}").into())
    }

    fn test(&self, c: &Cond, env: &mut Vec<(String, Val)>) -> R<bool> {
        Ok(match c {
            Cond::And(a, b) => self.test(a, env)? && self.test(b, env)?,
            Cond::Or(a, b) => self.test(a, env)? || self.test(b, env)?,
            Cond::Not(a) => !self.test(a, env)?,
            Cond::Cmp(op, a, b) => {
                let x = self.eval(a, env)?;
                let y = self.eval(b, env)?;
                let ord = match (&x, &y) {
                    (Val::Num(p), Val::Num(q)) => p.partial_cmp(q),
                    (Val::Str(p), Val::Str(q)) => Some(p.cmp(q)),
                    (Val::Num(_), Val::Str(_)) | (Val::Str(_), Val::Num(_)) => None,
                    _ => return Err("a condition cannot depend on a variable".to_string().into()),
                };
                use std::cmp::Ordering::*;
                match (op, ord) {
                    (CmpOp::Eq, o) => o == Some(Equal),
                    (CmpOp::Ne, o) => o != Some(Equal),
                    (_, None) => return Err("cannot order a number against a string".to_string().into()),
                    (CmpOp::Lt, Some(o)) => o == Less,
                    (CmpOp::Le, Some(o)) => o != Greater,
                    (CmpOp::Gt, Some(o)) => o == Greater,
                    (CmpOp::Ge, Some(o)) => o != Less,
                }
            }
        })
    }

    /// Evaluates a list's items, as affine expressions, in order.
    fn eval_list(&self, list: &List, env: &mut Vec<(String, Val)>) -> R<Vec<Lin>> {
        match list {
            List::Explicit(items) => items.iter().map(|e| self.eval(e, env).and_then(into_lin)).collect(),
            List::Comprehension(body, b) => {
                let mut out = Vec::new();
                self.for_each_binding(b, env, &mut |ctx, env| {
                    out.push(into_lin(ctx.eval(body, env)?)?);
                    Ok(())
                })?;
                Ok(out)
            }
        }
    }

    /// Calls `f` once per combination of the bindings that satisfies the condition.
    fn for_each_binding(&self, b: &Bindings, env: &mut Vec<(String, Val)>, f: &mut BindingFn<'_, 'a>) -> R<()> {
        for (_, set) in &b.vars {
            if !self.sets.contains_key(set.as_str()) {
                return Err(format!("unknown set {set}").into());
            }
        }
        self.bind_from(b, 0, env, f)
    }

    fn bind_from(&self, b: &Bindings, k: usize, env: &mut Vec<(String, Val)>, f: &mut BindingFn<'_, 'a>) -> R<()> {
        if k == b.vars.len() {
            if let Some(c) = &b.cond
                && !self.test(c, env)?
            {
                return Ok(());
            }
            return f(self, env);
        }
        let (idx, set) = &b.vars[k];
        for e in &self.sets[set.as_str()] {
            env.push((idx.clone(), elem_val(e)));
            let r = self.bind_from(b, k + 1, env, f);
            env.pop();
            r?;
        }
        Ok(())
    }
}

fn elem_val(e: &Elem) -> Val {
    match e {
        Elem::Int(i) => Val::Num(*i as f64),
        Elem::Str(s) => Val::Str(s.clone()),
    }
}

fn to_elem(v: Val) -> R<Elem> {
    match v {
        Val::Num(x) if x.fract() == 0.0 && x.abs() < 9e15 => Ok(Elem::Int(x as i64)),
        Val::Num(x) => Err(format!("index value {x} is not an integer").into()),
        Val::Str(s) => Ok(Elem::Str(s)),
        Val::Lin(_) => Err("an index cannot depend on a variable".to_string().into()),
    }
}

fn into_lin(v: Val) -> R<Lin> {
    match v {
        Val::Num(c) => Ok(Lin { coefs: HashMap::new(), constant: c }),
        Val::Lin(l) => Ok(l),
        Val::Str(s) => Err(format!("string {s:?} used in arithmetic").into()),
    }
}

fn add(x: Val, y: Val, sign: f64) -> R<Val> {
    if let (Val::Num(a), Val::Num(b)) = (&x, &y) {
        return Ok(Val::Num(a + sign * b));
    }
    let mut l = into_lin(x)?;
    let r = into_lin(y)?;
    for (k, v) in r.coefs {
        *l.coefs.entry(k).or_insert(0.0) += sign * v;
    }
    l.constant += sign * r.constant;
    Ok(Val::Lin(l))
}

fn scale(v: Val, c: f64) -> R<Val> {
    match v {
        Val::Num(x) => Ok(Val::Num(x * c)),
        Val::Lin(mut l) => {
            for v in l.coefs.values_mut() {
                *v *= c;
            }
            l.constant *= c;
            Ok(Val::Lin(l))
        }
        Val::Str(s) => Err(format!("string {s:?} used in arithmetic").into()),
    }
}

fn build_sets(problem: &Problem) -> Result<HashMap<&str, Vec<Elem>>, String> {
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

fn product(sets: &HashMap<&str, Vec<Elem>>, names: &[String]) -> Vec<Vec<Elem>> {
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

fn label(name: &str, b: &Bindings, env: &[(String, Val)]) -> String {
    if b.vars.is_empty() {
        return name.to_string();
    }
    let parts: Vec<String> = b
        .vars
        .iter()
        .map(|(idx, _)| match env.iter().rev().find(|(n, _)| n == idx).map(|(_, v)| v) {
            Some(Val::Num(x)) => x.to_string(),
            Some(Val::Str(s)) => s.clone(),
            _ => "?".into(),
        })
        .collect();
    format!("{name}[{}]", parts.join(","))
}

pub fn build(problem: &Problem) -> Result<LinearModel, String> {
    let sets = build_sets(problem)?;
    let mut ctx = Ctx { problem, sets, var_cols: HashMap::new() };

    // Columns
    let (mut lower, mut upper, mut integer, mut domains) = (vec![], vec![], vec![], vec![]);
    // For each column: its variable, and the set of a symbolic variable.
    let (mut col_var, mut col_symbolic): (Vec<usize>, Vec<Option<&str>>) = (vec![], vec![]);
    let mut vars = Vec::new();
    for (var_index, v) in problem.variables.iter().enumerate() {
        // A domain naming a set: its integers as ranges, or 1..k for a symbolic set.
        let mut symbolic = None;
        let set_domain: Option<Vec<(i64, i64)>> = match &v.domain {
            Some(DomainDef::Set(set)) => {
                let elems = &ctx.sets[set.as_str()];
                if elems.iter().all(|e| matches!(e, Elem::Int(_))) {
                    let mut values: Vec<i64> =
                        elems.iter().map(|e| if let Elem::Int(i) = e { *i } else { unreachable!() }).collect();
                    values.sort_unstable();
                    Some(to_ranges(&values))
                } else {
                    symbolic = Some((set.as_str(), elems.clone()));
                    Some(vec![(1, elems.len() as i64)])
                }
            }
            Some(DomainDef::Ranges(d)) => Some(d.iter().map(|[a, b]| (*a, *b)).collect()),
            None => None,
        };
        let mut elements = Vec::new();
        let mut cols = HashMap::new();
        for idx in product(&ctx.sets, &v.index) {
            let col = lower.len();
            let bound = |b: &Option<Bound>, default: f64| -> Result<f64, String> {
                match b {
                    None => Ok(default),
                    Some(Bound::Num(x)) => Ok(*x),
                    Some(Bound::Param(p)) => ctx.param(p, &idx),
                }
            };
            let (lo, hi, dom) = match (v.var_type, &set_domain) {
                (VarType::Binary, _) => (0.0, 1.0, None),
                (_, Some(ranges)) => {
                    let (lo, hi) = (ranges[0].0 as f64, ranges[ranges.len() - 1].1 as f64);
                    (lo, hi, if ranges.len() > 1 { Some(ranges.clone()) } else { None })
                }
                _ => (bound(&v.lower, -INF)?, bound(&v.upper, INF)?, None),
            };
            lower.push(lo);
            upper.push(hi);
            integer.push(v.var_type != VarType::Continuous);
            domains.push(dom);
            col_var.push(var_index);
            col_symbolic.push(symbolic.as_ref().map(|s| s.0));
            cols.insert(idx.clone(), col);
            elements.push((idx, col));
        }
        ctx.var_cols.insert(v.name.as_str(), cols);
        vars.push(VarInfo {
            name: v.name.clone(),
            indexed: !v.index.is_empty(),
            var_type: v.var_type,
            elements,
            symbolic: symbolic.map(|s| s.1),
        });
    }
    let n = lower.len();
    // Symbolic variables have no arithmetic; only all_different may use them so far.
    let no_symbolic = |cols: &[usize], place: &str| -> Result<(), String> {
        match cols.iter().copied().find(|&j| col_symbolic[j].is_some()) {
            Some(j) => Err(format!(
                "{place}: symbolic variable {} can't be used in arithmetic (its values are set elements)",
                problem.variables[col_var[j]].name
            )),
            None => Ok(()),
        }
    };

    let want_all = problem.solve.solutions == Solutions::All;
    if want_all && let Some(v) = vars.iter().find(|v| v.var_type == VarType::Continuous) {
        return Err(format!(
            "unsupported: \"solutions\": \"all\" requires every variable to be integer or binary, \
                 but {} is continuous",
            v.name
        ));
    }

    // Rows
    let mut rows = Vec::new();
    let mut violated = None;
    // all_different instances: (label, items), encoded after the objective.
    let mut all_different: Vec<(String, Vec<Lin>)> = Vec::new();
    let no_bindings = Bindings { vars: vec![], cond: None };
    for (i, c) in problem.constraints.iter().enumerate() {
        let name = c.name.clone().unwrap_or_else(|| format!("#{}", i + 1));
        let bindings = match &c.forall {
            Some(f) => expr::parse_bindings(f).map_err(|e| format!("constraint {name}: forall: {e}"))?,
            None => no_bindings.clone(),
        };
        let text = match c.kind()? {
            ConstraintKind::Expr(text) => text,
            ConstraintKind::AllDifferent(text) => {
                let list = expr::parse_list(text).map_err(|e| format!("constraint {name}: {e}"))?;
                let mut env = Vec::new();
                let result = ctx.for_each_binding(&bindings, &mut env, &mut |ctx, env| {
                    let inst = label(&name, &bindings, env);
                    match ctx.eval_list(&list, env) {
                        Ok(items) => {
                            all_different.push((inst, items));
                            Ok(())
                        }
                        Err(EvalError::NonLinear(w)) => {
                            Err(format!("unsupported: nonlinear term ({w}) in constraint {inst}").into())
                        }
                        Err(EvalError::Other(e)) => Err(format!("constraint {inst}: {e}").into()),
                    }
                });
                match result {
                    Ok(()) | Err(EvalError::NonLinear(_)) => {}
                    Err(EvalError::Other(e)) => return Err(e),
                }
                continue;
            }
        };
        let (rel, lhs, rhs) = expr::parse_relation(text).map_err(|e| format!("constraint {name}: {e}"))?;
        let mut env = Vec::new();
        let result = ctx.for_each_binding(&bindings, &mut env, &mut |ctx, env| {
            let inst = label(&name, &bindings, env);
            let diff = (|| add(ctx.eval(&lhs, env)?, ctx.eval(&rhs, env)?, -1.0))();
            let lin = match diff.and_then(into_lin) {
                Ok(l) => l,
                Err(EvalError::NonLinear(what)) => {
                    return Err(format!("unsupported: nonlinear term ({what}) in constraint {inst}").into());
                }
                Err(EvalError::Other(e)) => return Err(format!("constraint {inst}: {e}").into()),
            };
            let mut coefs: Vec<(usize, f64)> = lin.coefs.into_iter().filter(|(_, v)| *v != 0.0).collect();
            coefs.sort_unstable_by_key(|(j, _)| *j);
            no_symbolic(&coefs.iter().map(|c| c.0).collect::<Vec<_>>(), &format!("constraint {inst}"))?;
            let k = lin.constant;
            let (lo, hi) = match rel {
                Rel::Le => (-INF, -k),
                Rel::Ge => (-k, INF),
                Rel::Eq => (-k, -k),
            };
            if coefs.is_empty() {
                let tol = 1e-9 * (1.0 + k.abs());
                if (0.0 < lo - tol || 0.0 > hi + tol) && violated.is_none() {
                    violated = Some(inst);
                }
            } else {
                rows.push(Row { coefs, lower: lo, upper: hi });
            }
            Ok(())
        });
        match result {
            Ok(()) => {}
            Err(EvalError::Other(e)) => return Err(e),
            Err(EvalError::NonLinear(w)) => {
                return Err(format!("unsupported: nonlinear term ({w}) in constraint {name}"));
            }
        }
    }

    // Objective
    let sense = match problem.solve.method {
        Method::Satisfy => Sense::Satisfy,
        Method::Minimize => Sense::Minimize,
        Method::Maximize => Sense::Maximize,
    };
    let mut obj = vec![0.0; n];
    let mut obj_offset = 0.0;
    if let Some(text) = &problem.solve.objective {
        let e = expr::parse_expr(text).map_err(|e| format!("objective: {e}"))?;
        let lin = match ctx.eval(&e, &mut Vec::new()).and_then(into_lin) {
            Ok(l) => l,
            Err(EvalError::NonLinear(what)) => {
                return Err(format!("unsupported: nonlinear term ({what}) in the objective"));
            }
            Err(EvalError::Other(e)) => return Err(format!("objective: {e}")),
        };
        no_symbolic(&lin.coefs.iter().filter(|c| *c.1 != 0.0).map(|c| *c.0).collect::<Vec<_>>(), "the objective")?;
        for (j, v) in lin.coefs {
            obj[j] = v;
        }
        obj_offset = lin.constant;
    }

    let uses_cp_kinds = !all_different.is_empty();
    if uses_cp_kinds {
        let mut cols = Columns { lower: &mut lower, upper: &mut upper, integer: &mut integer, domain: &mut domains };
        for (inst, items) in &all_different {
            if let Some(v) = encode_all_different(inst, items, &mut cols, &col_symbolic, &mut rows)?
                && violated.is_none()
            {
                violated = Some(v);
            }
        }
        obj.resize(lower.len(), 0.0);
    }

    Ok(LinearModel {
        col_lower: lower,
        col_upper: upper,
        col_integer: integer,
        col_domain: domains,
        rows,
        obj,
        obj_offset,
        sense,
        want_all,
        vars,
        violated_constant_row: violated,
        uses_cp_kinds,
    })
}

/// Sorted distinct integers as inclusive ranges of consecutive values.
fn to_ranges(values: &[i64]) -> Vec<(i64, i64)> {
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for &v in values {
        match ranges.last_mut() {
            Some((_, hi)) if v <= *hi + 1 => *hi = (*hi).max(v),
            _ => ranges.push((v, v)),
        }
    }
    ranges
}

/// The model's column arrays, which encodings append to.
struct Columns<'a> {
    lower: &'a mut Vec<f64>,
    upper: &'a mut Vec<f64>,
    integer: &'a mut Vec<bool>,
    domain: &'a mut Vec<Option<Vec<(i64, i64)>>>,
}

impl Columns<'_> {
    fn push(&mut self, lo: f64, hi: f64) -> usize {
        self.lower.push(lo);
        self.upper.push(hi);
        self.integer.push(true);
        self.domain.push(None);
        self.lower.len() - 1
    }

    /// The values an integer column can take, or an error if there are too many.
    fn values(&self, col: usize, inst: &str) -> Result<Vec<i64>, String> {
        let (lo, hi) = (self.lower[col], self.upper[col]);
        if !(lo.is_finite() && hi.is_finite()) || hi - lo >= MAX_ENCODED_VALUES as f64 {
            return Err(format!(
                "constraint {inst}: all_different needs every item to have finite bounds with at most \
                 {MAX_ENCODED_VALUES} values"
            ));
        }
        let (lo, hi) = (lo.ceil() as i64, hi.floor() as i64);
        Ok((lo..=hi)
            .filter(|&v| self.domain[col].as_ref().is_none_or(|r| r.iter().any(|&(a, b)| a <= v && v <= b)))
            .collect())
    }
}

/// Encodes `all_different` over affine integer items as linear rows, so both
/// engines can solve it: each item's column x gets a binary `[x = v]` per
/// value (`sum_v [x = v] = 1`, `sum_v v [x = v] = x`), and for every value V,
/// at most one item takes it. An item over several columns first gets a
/// column equal to it. Returns the instance label if constant items alone
/// already repeat a value.
fn encode_all_different(
    inst: &str,
    items: &[Lin],
    cols: &mut Columns,
    col_symbolic: &[Option<&str>],
    rows: &mut Vec<Row>,
) -> Result<Option<String>, String> {
    let integral = |v: f64| v.fract() == 0.0 && v.abs() < 1e15;
    // Each item as (column, coefficient, constant), or a constant.
    let mut terms: Vec<Option<(usize, i64, i64)>> = Vec::new();
    let mut symbolic_set: Option<Option<&str>> = None;
    for item in items {
        let coefs: Vec<(usize, f64)> = item.coefs.iter().filter(|c| *c.1 != 0.0).map(|(&j, &v)| (j, v)).collect();
        let set = coefs.first().and_then(|c| col_symbolic[c.0]);
        if coefs.iter().any(|c| col_symbolic[c.0].is_some())
            && (coefs.len() != 1 || coefs[0].1 != 1.0 || item.constant != 0.0)
        {
            return Err(format!("constraint {inst}: a symbolic variable in all_different must appear alone"));
        }
        match symbolic_set {
            None => symbolic_set = Some(set),
            Some(s) if s != set => {
                return Err(format!(
                    "constraint {inst}: all_different items must be all integer-valued or all symbolic over one set"
                ));
            }
            _ => {}
        }
        if !integral(item.constant) || coefs.iter().any(|&(j, v)| !integral(v) || !cols.integer[j]) {
            return Err(format!(
                "constraint {inst}: all_different items must be integer-valued (integer variables, integer coefficients)"
            ));
        }
        terms.push(match coefs.len() {
            0 => None,
            1 => Some((coefs[0].0, coefs[0].1 as i64, item.constant as i64)),
            _ => {
                // z = item, then encode z.
                let (mut lo, mut hi) = (item.constant, item.constant);
                for &(j, v) in &coefs {
                    let (a, b) = (v * cols.lower[j], v * cols.upper[j]);
                    lo += a.min(b);
                    hi += a.max(b);
                }
                let z = cols.push(lo, hi);
                let mut row: Vec<(usize, f64)> = coefs.iter().map(|&(j, v)| (j, -v)).collect();
                row.push((z, 1.0));
                row.sort_unstable_by_key(|r| r.0);
                rows.push(Row { coefs: row, lower: item.constant, upper: item.constant });
                Some((z, 1, 0))
            }
        });
    }

    // Value encodings, one per column (shared by repeated columns).
    let mut encoded: HashMap<usize, Vec<(i64, usize)>> = HashMap::new();
    for &(col, _, _) in terms.iter().flatten() {
        if encoded.contains_key(&col) {
            continue;
        }
        let values = cols.values(col, inst)?;
        if cols.lower.len() + values.len() > MAX_ENCODED_TOTAL {
            return Err(format!("constraint {inst}: all_different would need too many value variables"));
        }
        let bools: Vec<(i64, usize)> = values.iter().map(|&v| (v, cols.push(0.0, 1.0))).collect();
        rows.push(Row { coefs: bools.iter().map(|&(_, b)| (b, 1.0)).collect(), lower: 1.0, upper: 1.0 });
        let mut link: Vec<(usize, f64)> = bools.iter().filter(|b| b.0 != 0).map(|&(v, b)| (b, v as f64)).collect();
        link.push((col, -1.0));
        link.sort_unstable_by_key(|r| r.0);
        rows.push(Row { coefs: link, lower: 0.0, upper: 0.0 });
        encoded.insert(col, bools);
    }

    // For each value: at most one item takes it.
    let mut takers: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    let mut constants: BTreeMap<i64, i64> = BTreeMap::new();
    for t in &terms {
        match t {
            None => {}
            Some((col, a, c)) => {
                for &(w, b) in &encoded[col] {
                    takers.entry(a * w + c).or_default().push(b);
                }
            }
        }
    }
    for (item, t) in items.iter().zip(&terms) {
        if t.is_none() {
            *constants.entry(item.constant as i64).or_default() += 1;
        }
    }
    if constants.values().any(|&k| k > 1) {
        return Ok(Some(inst.to_string()));
    }
    for (value, bools) in takers {
        let fixed = constants.get(&value).copied().unwrap_or(0);
        if bools.len() as i64 + fixed < 2 {
            continue;
        }
        let mut coefs: BTreeMap<usize, f64> = BTreeMap::new();
        for b in bools {
            *coefs.entry(b).or_default() += 1.0;
        }
        rows.push(Row { coefs: coefs.into_iter().collect(), lower: -INF, upper: (1 - fixed) as f64 });
    }
    Ok(None)
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

    #[test]
    fn all_different_counts_queens() {
        for (n, expected) in [(1, 1), (2, 0), (3, 0), (4, 2), (5, 10), (6, 4), (7, 40), (8, 92)] {
            let status = if expected == 0 { Status::Infeasible } else { Status::Complete };
            assert_eq!(count(&queens(n), Engine::Cp), (status, expected), "{n}-queens, CP");
        }
        assert_eq!(count(&queens(6), Engine::Mip), (Status::Complete, 4), "6-queens, MILP");
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
        assert_eq!(count(json, Engine::Mip), (Status::Complete, expected));
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
    fn rejects_what_is_not_supported() {
        let symbolic = r#"{"version": "1.1", "sets": {"R": ["a", "b"]},
            "variables": [{"name": "r", "type": "integer", "domain": "R"}],
            "constraints": [{"expr": "r <= 1"}], "solve": {"method": "satisfy"}}"#;
        assert!(load_error(symbolic).contains("symbolic variable r can't be used in arithmetic"));
        let table = r#"{"version": "1.1", "variables": [{"name": "x", "type": "integer", "lower": 0, "upper": 1}],
            "constraints": [{"table": {"vars": "[x]", "tuples": [[1]]}}], "solve": {"method": "satisfy"}}"#;
        assert!(load_error(table).contains("table (format 1.1) is not supported yet"));
        let unbounded = r#"{"version": "1.1", "variables": [{"name": "x", "type": "integer", "lower": 0}],
            "constraints": [{"all_different": "[x, 3]"}], "solve": {"method": "satisfy"}}"#;
        assert!(load_error(unbounded).contains("finite bounds"));
    }
}
