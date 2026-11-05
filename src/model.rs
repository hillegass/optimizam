//! Expands a [`Problem`] into a flat model: one column per variable element,
//! one row per constraint instance. Rows and the objective are affine, except
//! that `expr` constraints (without `enforce_if`) and the objective may have
//! a nonlinear part, kept as a node of an expression graph
//! ([`LinearModel::nonlinear`]).

use crate::encode::{Affine, Columns, Encoder, Global, Interval, Literal};
use crate::expr::{self, BinOp, Bindings, CmpOp, Cond, Expr, List, Rel};
use crate::nlp::graph::{Graph, Node, Op};
use crate::problem::{
    Bound, ConstraintKind, DomainDef, Elem, Method, OneOrMany, ParamDef, Problem, SetDef, Solutions, VarType,
};
use serde_json::Value as Json;
use std::cell::RefCell;
use std::collections::HashMap;

pub const INF: f64 = f64::INFINITY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sense {
    Satisfy,
    Minimize,
    Maximize,
}

/// One declared variable and the columns of its elements, in index order.
#[derive(Clone)]
pub struct VarInfo {
    pub name: String,
    pub indexed: bool,
    pub var_type: VarType,
    pub elements: Vec<(Vec<Elem>, usize)>,
    /// For a symbolic set-valued variable: its set's elements. A column's
    /// value k (1-based) stands for element k.
    pub symbolic: Option<Vec<Elem>>,
}

#[derive(Clone)]
pub struct Row {
    pub coefs: Vec<(usize, f64)>,
    pub lower: f64,
    pub upper: f64,
}

#[derive(Clone)]
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
    /// are encoded here as linear rows on extra (unreported) columns, except
    /// for `scheduling`.
    pub uses_cp_kinds: bool,
    /// `no_overlap` and `cumulative` instances, not yet encoded: the CP engine
    /// propagates them natively, and [`LinearModel::encode_scheduling`]
    /// encodes them for the MILP engine.
    pub scheduling: Vec<Global>,
    /// Nonlinear rows and the objective's nonlinear part, if there are any.
    pub nonlinear: Option<Nonlinear>,
}

/// A row `lower <= coefs * x + node(x) <= upper`.
#[derive(Clone)]
#[allow(dead_code)] // read by the nonlinear solver (not yet implemented)
pub struct NonlinearRow {
    pub label: String,
    pub coefs: Vec<(usize, f64)>,
    pub node: Node,
    pub lower: f64,
    pub upper: f64,
}

/// The nonlinear parts of a model. The objective is
/// `obj * x + obj_offset + objective(x)`, in the problem's own sense.
#[derive(Clone)]
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

    pub fn objective_value(&self, x: &[f64]) -> f64 {
        let nonlinear = match &self.nonlinear {
            Some(Nonlinear { graph, objective: Some(node), .. }) => graph.value(*node, x),
            _ => 0.0,
        };
        self.obj_offset + self.obj.iter().zip(x).map(|(c, v)| c * v).sum::<f64>() + nonlinear
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
    /// An affine part plus a nonlinear part (a non-constant graph node).
    Nl(Lin, Node),
}

fn describe(v: &Val) -> &'static str {
    match v {
        Val::Num(_) => "a number",
        Val::Str(_) => "a string set element",
        Val::Lin(_) => "an expression with variables",
        Val::Nl(..) => "a nonlinear expression",
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
    /// Nonlinear parts of expressions.
    graph: RefCell<Graph>,
}

/// Why a nonlinear expression is rejected where only affine ones may appear.
const NOT_AFFINE: &str = "only expr constraints without enforce_if, and the objective, may be nonlinear";

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
            Expr::Neg(a) => Ok(self.scale(self.eval(a, env)?, -1.0)?),
            Expr::Add(terms) => {
                let mut total = Val::Num(0.0);
                for (sign, t) in terms {
                    total = self.add(total, self.eval(t, env)?, *sign)?;
                }
                Ok(total)
            }
            Expr::Bin(op, a, b) => {
                let x = self.eval(a, env)?;
                let y = self.eval(b, env)?;
                match op {
                    BinOp::Mul => match (x, y) {
                        (s @ Val::Str(_), t) | (t, s @ Val::Str(_)) => {
                            Err(format!("cannot multiply {} by {}", describe(&s), describe(&t)).into())
                        }
                        (Val::Num(c), v) | (v, Val::Num(c)) => self.scale(v, c),
                        (x, y) => {
                            let (a, b) = (self.node(&x)?, self.node(&y)?);
                            let n = self.graph.borrow_mut().mul(a, b);
                            Ok(self.nonlinear(Lin::default(), n))
                        }
                    },
                    BinOp::Div => match (x, y) {
                        (s @ Val::Str(_), t) | (s, t @ Val::Str(_)) => {
                            Err(format!("cannot divide {} by {}", describe(&s), describe(&t)).into())
                        }
                        (_, Val::Num(0.0)) => Err("division by zero".to_string().into()),
                        (v, Val::Num(c)) => self.scale(v, 1.0 / c),
                        (x, y) => {
                            let (a, b) = (self.node(&x)?, self.node(&y)?);
                            let n = self.graph.borrow_mut().div(a, b);
                            Ok(self.nonlinear(Lin::default(), n))
                        }
                    },
                }
            }
            Expr::Pow(a, n) => match (self.eval(a, env)?, n) {
                (Val::Num(v), n) => finite(v.powi(*n), || format!("{v}^{n}")),
                (v @ Val::Str(_), _) => Err(format!("cannot raise {} to a power", describe(&v)).into()),
                (v, 1) => Ok(v),
                (_, 0) => Ok(Val::Num(1.0)),
                (v, n) => {
                    let a = self.node(&v)?;
                    let p = self.graph.borrow_mut().powi(a, *n);
                    Ok(self.nonlinear(Lin::default(), p))
                }
            },
            Expr::PowE(a, b) => match (self.eval(a, env)?, self.eval(b, env)?) {
                (Val::Num(x), Val::Num(y)) => finite(x.powf(y), || format!("{x}^{y}")),
                (x @ Val::Str(_), y) | (x, y @ Val::Str(_)) => {
                    Err(format!("cannot raise {} to the power {}", describe(&x), describe(&y)).into())
                }
                (x, y) => {
                    let (a, b) = (self.node(&x)?, self.node(&y)?);
                    let p = self.graph.borrow_mut().pow(a, b);
                    Ok(self.nonlinear(Lin::default(), p))
                }
            },
            Expr::Call(f, a) => match self.eval(a, env)? {
                Val::Num(x) => finite(f.apply(x), || format!("{}({x})", f.name())),
                v @ Val::Str(_) => Err(format!("cannot apply {} to {}", f.name(), describe(&v)).into()),
                v => {
                    let a = self.node(&v)?;
                    let n = self.graph.borrow_mut().func(*f, a);
                    Ok(self.nonlinear(Lin::default(), n))
                }
            },
            Expr::Sum(b, body) => {
                let mut total = Val::Num(0.0);
                self.for_each_binding(b, env, &mut |ctx, env| {
                    let term = ctx.eval(body, env)?;
                    total = ctx.add(std::mem::replace(&mut total, Val::Num(0.0)), term, 1.0)?;
                    Ok(())
                })?;
                Ok(total)
            }
        }
    }

    /// A value as a graph node (a constant, a sum of variables, or an affine
    /// part plus a nonlinear node).
    fn node(&self, v: &Val) -> R<Node> {
        let mut g = self.graph.borrow_mut();
        let lin_node = |g: &mut Graph, l: &Lin| -> Node {
            let mut coefs: Vec<(usize, f64)> = l.coefs.iter().filter(|c| *c.1 != 0.0).map(|(&j, &a)| (j, a)).collect();
            coefs.sort_unstable_by_key(|c| c.0);
            let terms: Vec<(f64, Node)> = coefs.into_iter().map(|(j, a)| (a, g.var(j))).collect();
            g.sum(l.constant, &terms)
        };
        match v {
            Val::Num(c) => Ok(g.constant(*c)),
            Val::Lin(l) => Ok(lin_node(&mut g, l)),
            Val::Nl(l, n) => {
                let a = lin_node(&mut g, l);
                Ok(g.sum(0.0, &[(1.0, a), (1.0, *n)]))
            }
            Val::Str(s) => Err(format!("string {s:?} used in arithmetic").into()),
        }
    }

    /// `lin + node`, as an affine value if the node is a constant.
    fn nonlinear(&self, mut lin: Lin, node: Node) -> Val {
        match self.graph.borrow().op(node) {
            Op::Const(c) => {
                lin.constant += c;
                if lin.coefs.values().all(|&a| a == 0.0) { Val::Num(lin.constant) } else { Val::Lin(lin) }
            }
            _ => Val::Nl(lin, node),
        }
    }

    fn add(&self, x: Val, y: Val, sign: f64) -> R<Val> {
        if let (Val::Num(a), Val::Num(b)) = (&x, &y) {
            return Ok(Val::Num(a + sign * b));
        }
        let split = |v: Val| -> R<(Lin, Option<Node>)> {
            match v {
                Val::Nl(l, n) => Ok((l, Some(n))),
                v => Ok((into_lin(v)?, None)),
            }
        };
        let (mut l, nx) = split(x)?;
        let (r, ny) = split(y)?;
        for (k, v) in r.coefs {
            *l.coefs.entry(k).or_insert(0.0) += sign * v;
        }
        l.constant += sign * r.constant;
        let node = match (nx, ny) {
            (None, None) => return Ok(Val::Lin(l)),
            (Some(a), None) => a,
            (None, Some(b)) => self.graph.borrow_mut().scale(sign, b),
            (Some(a), Some(b)) => self.graph.borrow_mut().sum(0.0, &[(1.0, a), (sign, b)]),
        };
        Ok(self.nonlinear(l, node))
    }

    fn scale(&self, v: Val, c: f64) -> R<Val> {
        match v {
            Val::Num(x) => Ok(Val::Num(x * c)),
            Val::Lin(mut l) => {
                for v in l.coefs.values_mut() {
                    *v *= c;
                }
                l.constant *= c;
                Ok(Val::Lin(l))
            }
            Val::Nl(mut l, n) => {
                for v in l.coefs.values_mut() {
                    *v *= c;
                }
                l.constant *= c;
                let n = self.graph.borrow_mut().scale(c, n);
                Ok(self.nonlinear(l, n))
            }
            Val::Str(s) => Err(format!("string {s:?} used in arithmetic").into()),
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

/// The interval instances a list refers to (`[task[j] | j in J]`, `[a, b]`).
fn interval_list(
    ctx: &Ctx,
    list: &List,
    env: &mut Vec<(String, Val)>,
    ids: &HashMap<&str, HashMap<Vec<Elem>, usize>>,
) -> R<Vec<usize>> {
    let one = |ctx: &Ctx, e: &Expr, env: &mut Vec<(String, Val)>| -> R<usize> {
        let Expr::Ref(name, args) = e else {
            return Err("a list of intervals must name intervals".to_string().into());
        };
        let instances = ids.get(name.as_str()).ok_or_else(|| format!("{name} is not an interval"))?;
        let mut keys = Vec::new();
        for a in args.iter().flatten() {
            keys.push(to_elem(ctx.eval(a, env)?)?);
        }
        instances.get(&keys).copied().ok_or_else(|| format!("{name} has no element {keys:?}").into())
    };
    match list {
        List::Explicit(items) => items.iter().map(|e| one(ctx, e, env)).collect(),
        List::Comprehension(body, b) => {
            let mut out = Vec::new();
            ctx.for_each_binding(b, env, &mut |ctx, env| {
                out.push(one(ctx, body, env)?);
                Ok(())
            })?;
            Ok(out)
        }
    }
}

/// A constant result, or an error naming the expression if it isn't a
/// finite number (`ln(-1)`, `(-8)^(1/3)`, `exp(1000)`).
fn finite(v: f64, what: impl Fn() -> String) -> R<Val> {
    if v.is_finite() { Ok(Val::Num(v)) } else { Err(format!("{} is not a finite number", what()).into()) }
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
        Val::Lin(_) | Val::Nl(..) => Err("an index cannot depend on a variable".to_string().into()),
    }
}

fn into_lin(v: Val) -> R<Lin> {
    match v {
        Val::Num(c) => Ok(Lin { coefs: HashMap::new(), constant: c }),
        Val::Lin(l) => Ok(l),
        Val::Nl(..) => Err(EvalError::NonLinear(NOT_AFFINE)),
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
    let mut ctx = Ctx { problem, sets, var_cols: HashMap::new(), graph: RefCell::new(Graph::new()) };

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
    // Symbolic variables have no arithmetic (only lists and comparisons with elements).
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
    let mut nonlinear_rows: Vec<NonlinearRow> = Vec::new();
    let mut violated = None;
    // Format 1.1 constraint instances, encoded after the objective.
    let mut globals: Vec<Global> = Vec::new();

    // Intervals: one instance per index combination, from columns and parameters.
    let mut intervals: Vec<Interval> = Vec::new();
    let mut interval_ids: HashMap<&str, HashMap<Vec<Elem>, usize>> = HashMap::new();
    for iv in &problem.intervals {
        let mut ids = HashMap::new();
        for idx in product(&ctx.sets, &iv.index) {
            let keys: Vec<String> = idx.iter().map(Elem::key).collect();
            let label = if idx.is_empty() { iv.name.clone() } else { format!("{}[{}]", iv.name, keys.join(",")) };
            let field = |b: &Bound| -> Result<Affine, String> {
                let a = match b {
                    Bound::Num(x) => Affine { coefs: vec![], constant: *x },
                    Bound::Param(name) => match ctx.var_cols.get(name.as_str()) {
                        Some(cols) => Affine { coefs: vec![(cols[&idx], 1.0)], constant: 0.0 },
                        None => Affine { coefs: vec![], constant: ctx.param(name, &idx)? },
                    },
                };
                let integral =
                    a.constant.fract() == 0.0 && a.coefs.iter().all(|&(j, _)| integer[j] && col_symbolic[j].is_none());
                if integral {
                    Ok(a)
                } else {
                    Err(format!("interval {label}: start and duration must be integer-valued"))
                }
            };
            let (start, duration) = (field(&iv.start)?, field(&iv.duration)?);
            if duration.coefs.is_empty() && duration.constant < 0.0 {
                return Err(format!("interval {label}: the duration is negative"));
            }
            let presence = iv.presence.as_ref().map(|p| ctx.var_cols[p.as_str()][&idx]);
            let interval = Interval { label, start, duration, presence };
            if !interval.duration.coefs.is_empty() {
                globals.push(Global::IntervalDuration { interval: interval.clone() });
            }
            ids.insert(idx, intervals.len());
            intervals.push(interval);
        }
        interval_ids.insert(iv.name.as_str(), ids);
    }
    let no_bindings = Bindings { vars: vec![], cond: None };
    // Sets with a string element: variables over them are symbolic.
    let symbolic_sets: std::collections::HashSet<&str> = ctx
        .sets
        .iter()
        .filter(|(_, elems)| elems.iter().any(|e| matches!(e, Elem::Str(_))))
        .map(|(name, _)| *name)
        .collect();
    // The integer a column takes for a set element: the element itself, or
    // for a symbolic column its 1-based position (None if not in its set).
    let column_value = |col: usize, e: &Elem| -> Result<Option<i64>, String> {
        match (col_symbolic[col], e) {
            (Some(set), e) => Ok(ctx.sets[set].iter().position(|x| x == e).map(|p| p as i64 + 1)),
            (None, Elem::Int(i)) => Ok(Some(*i)),
            (None, Elem::Str(s)) => Err(format!("{s:?} compared with a numeric variable")),
        }
    };
    for (i, c) in problem.constraints.iter().enumerate() {
        let name = c.name.clone().unwrap_or_else(|| format!("#{}", i + 1));
        let bindings = match &c.forall {
            Some(f) => expr::parse_bindings(f).map_err(|e| format!("constraint {name}: forall: {e}"))?,
            None => no_bindings.clone(),
        };
        let kind = c.kind()?;
        let parse_err = |e: String| format!("constraint {name}: {e}");
        // The relation and literals of an expr; the parsed lists of the others.
        let relation = match &kind {
            ConstraintKind::Expr(text, _) => Some(expr::parse_relation(text).map_err(parse_err)?),
            _ => None,
        };
        let literals = match &kind {
            ConstraintKind::Expr(_, Some(text)) => {
                expr::parse_literals(text).map_err(|e| format!("constraint {name}: enforce_if: {e}"))?
            }
            _ => Vec::new(),
        };
        let list = match &kind {
            ConstraintKind::AllDifferent(text) => Some(expr::parse_list(text).map_err(parse_err)?),
            ConstraintKind::Table(t) => Some(expr::parse_list(&t.vars).map_err(parse_err)?),
            ConstraintKind::Element(e) => Some(expr::parse_list(&e.array).map_err(parse_err)?),
            ConstraintKind::Min(a) | ConstraintKind::Max(a) => Some(expr::parse_list(&a.of).map_err(parse_err)?),
            ConstraintKind::NoOverlap(text) => Some(expr::parse_list(text).map_err(parse_err)?),
            ConstraintKind::Cumulative(d) => Some(expr::parse_list(&d.intervals).map_err(parse_err)?),
            ConstraintKind::Expr(..) | ConstraintKind::Abs(_) => None,
        };
        // cumulative: the demands list and the capacity.
        let cumulative = match &kind {
            ConstraintKind::Cumulative(d) => Some((
                expr::parse_list(&d.demands).map_err(parse_err)?,
                expr::parse_expr(&d.capacity).map_err(parse_err)?,
            )),
            _ => None,
        };
        // min, max, abs: the value, and for abs the expression.
        let aggregate = match &kind {
            ConstraintKind::Min(a) | ConstraintKind::Max(a) => {
                Some((expr::parse_expr(&a.value).map_err(parse_err)?, None))
            }
            ConstraintKind::Abs(a) => Some((
                expr::parse_expr(&a.value).map_err(parse_err)?,
                Some(expr::parse_expr(&a.of).map_err(parse_err)?),
            )),
            _ => None,
        };
        let element = match &kind {
            ConstraintKind::Element(e) => {
                let texts: Vec<&str> = match &e.index {
                    OneOrMany::One(t) => vec![t.as_str()],
                    OneOrMany::Many(ts) => ts.iter().map(String::as_str).collect(),
                };
                let index =
                    texts.iter().map(|t| expr::parse_expr(t)).collect::<Result<Vec<_>, _>>().map_err(parse_err)?;
                let value = expr::parse_expr(&e.value).map_err(parse_err)?;
                let comp = match list.as_ref() {
                    Some(List::Comprehension(_, b)) if b.vars.len() == index.len() => b.clone(),
                    _ => {
                        return Err(format!(
                            "constraint {name}: element's array must be a comprehension with one binding per index"
                        ));
                    }
                };
                Some((index, value, comp))
            }
            _ => None,
        };

        let mut env = Vec::new();
        let result = ctx.for_each_binding(&bindings, &mut env, &mut |ctx, env| {
            let inst = label(&name, &bindings, env);
            let fail = |e: EvalError| -> EvalError {
                match e {
                    EvalError::NonLinear(w) => format!("unsupported: nonlinear term ({w}) in constraint {inst}").into(),
                    EvalError::Other(e) => format!("constraint {inst}: {e}").into(),
                }
            };
            let other = |e: String| -> EvalError { format!("constraint {inst}: {e}").into() };
            // The one column a variable reference denotes.
            let single = |l: &Lin| -> Option<usize> {
                let coefs: Vec<(&usize, &f64)> = l.coefs.iter().filter(|c| *c.1 != 0.0).collect();
                (coefs.len() == 1 && *coefs[0].1 == 1.0 && l.constant == 0.0).then(|| *coefs[0].0)
            };
            let affine = |l: Lin| -> Affine {
                let mut coefs: Vec<(usize, f64)> = l.coefs.into_iter().filter(|c| c.1 != 0.0).collect();
                coefs.sort_unstable_by_key(|c| c.0);
                Affine { coefs, constant: l.constant }
            };
            match &kind {
                ConstraintKind::Expr(..) => {
                    let (rel, lhs, rhs) = relation.as_ref().unwrap();
                    let (lv, rv) = (ctx.eval(lhs, env).map_err(fail)?, ctx.eval(rhs, env).map_err(fail)?);
                    // A symbolic variable compared with an element: fix its position.
                    let symbolic_eq = match (&lv, &rv) {
                        (Val::Lin(l), v @ (Val::Str(_) | Val::Num(_)))
                        | (v @ (Val::Str(_) | Val::Num(_)), Val::Lin(l))
                            if *rel == Rel::Eq && single(l).is_some_and(|j| col_symbolic[j].is_some()) =>
                        {
                            let col = single(l).unwrap();
                            let e = to_elem(v.clone()).map_err(fail)?;
                            Some(match column_value(col, &e).map_err(other)? {
                                Some(p) => (vec![(col, 1.0)], p as f64),
                                None => (vec![], 1.0), // not in the set: 0 == 1
                            })
                        }
                        _ => None,
                    };
                    // The nonlinear part of lhs - rhs, if any.
                    let mut nonlinear_node = None;
                    let (coefs, k) = match symbolic_eq {
                        Some((coefs, p)) => (coefs, -p),
                        None => {
                            let lin = match ctx.add(lv, rv, -1.0).map_err(fail)? {
                                Val::Nl(lin, node) if literals.is_empty() => {
                                    let vars = ctx.graph.borrow().vars_of(node);
                                    no_symbolic(&vars, &format!("constraint {inst}"))?;
                                    nonlinear_node = Some(node);
                                    lin
                                }
                                v => into_lin(v).map_err(fail)?,
                            };
                            let mut coefs: Vec<(usize, f64)> =
                                lin.coefs.into_iter().filter(|(_, v)| *v != 0.0).collect();
                            coefs.sort_unstable_by_key(|(j, _)| *j);
                            no_symbolic(&coefs.iter().map(|c| c.0).collect::<Vec<_>>(), &format!("constraint {inst}"))?;
                            (coefs, lin.constant)
                        }
                    };
                    let (lo, hi) = match rel {
                        Rel::Le => (-INF, -k),
                        Rel::Ge => (-k, INF),
                        Rel::Eq => (-k, -k),
                    };
                    if let Some(node) = nonlinear_node {
                        nonlinear_rows.push(NonlinearRow { label: inst, coefs, node, lower: lo, upper: hi });
                        return Ok(());
                    }
                    // enforce_if literals.
                    let mut lits = Vec::new();
                    for lit in &literals {
                        let col = into_lin(ctx.eval(&lit.var, env).map_err(fail)?)
                            .ok()
                            .and_then(|l| single(&l))
                            .ok_or_else(|| other("an enforce_if literal must name one variable".into()))?;
                        match &lit.compare {
                            None => lits.push(Literal::Col { col, negated: lit.negated }),
                            Some((equal, e)) => {
                                let e = to_elem(ctx.eval(e, env).map_err(fail)?).map_err(fail)?;
                                match column_value(col, &e).map_err(other)? {
                                    Some(value) => lits.push(Literal::Value { col, value, equal: *equal }),
                                    None if *equal => return Ok(()), // never true: nothing to enforce
                                    None => {}                       // always true
                                }
                            }
                        }
                    }
                    if !literals.is_empty() {
                        globals.push(Global::Enforced { label: inst, coefs, lower: lo, upper: hi, literals: lits });
                    } else if coefs.is_empty() {
                        let tol = 1e-9 * (1.0 + k.abs());
                        if (0.0 < lo - tol || 0.0 > hi + tol) && violated.is_none() {
                            violated = Some(inst);
                        }
                    } else {
                        rows.push(Row { coefs, lower: lo, upper: hi });
                    }
                }
                ConstraintKind::AllDifferent(_) => {
                    let items = ctx.eval_list(list.as_ref().unwrap(), env).map_err(fail)?;
                    // Symbolic items must be bare variables over one set.
                    let mut sets = std::collections::HashSet::new();
                    for item in &items {
                        let symbolic_cols: Vec<usize> = item
                            .coefs
                            .iter()
                            .filter(|c| *c.1 != 0.0 && col_symbolic[*c.0].is_some())
                            .map(|c| *c.0)
                            .collect();
                        if !symbolic_cols.is_empty() && single(item).is_none() {
                            return Err(other("a symbolic variable in all_different must appear alone".into()));
                        }
                        sets.insert(single(item).and_then(|j| col_symbolic[j]));
                    }
                    if sets.len() > 1 {
                        return Err(other(
                            "all_different items must be all integer-valued or all symbolic over one set".into(),
                        ));
                    }
                    globals.push(Global::AllDifferent { label: inst, items: items.into_iter().map(affine).collect() });
                }
                ConstraintKind::Table(t) => {
                    let items = ctx.eval_list(list.as_ref().unwrap(), env).map_err(fail)?;
                    let vars: Vec<usize> = items
                        .iter()
                        .map(single)
                        .collect::<Option<_>>()
                        .ok_or_else(|| other("table vars must be variables".into()))?;
                    let mut tuples = Vec::new();
                    'tuples: for tuple in &t.tuples {
                        if tuple.len() != vars.len() {
                            return Err(other(format!(
                                "a tuple has {} entries for {} variables",
                                tuple.len(),
                                vars.len()
                            )));
                        }
                        let mut values = Vec::new();
                        for (&col, e) in vars.iter().zip(tuple) {
                            match column_value(col, e).map_err(other)? {
                                Some(v) => values.push(v),
                                None => continue 'tuples, // an element outside the set never matches
                            }
                        }
                        tuples.push(values);
                    }
                    globals.push(Global::Table { label: inst, vars, tuples, forbidden: t.forbidden });
                }
                ConstraintKind::Element(_) => {
                    let (index_exprs, value_expr, comp) = element.as_ref().unwrap();
                    let body = match list.as_ref().unwrap() {
                        List::Comprehension(body, _) => body,
                        List::Explicit(_) => unreachable!(),
                    };
                    let mut index = Vec::new();
                    for (d, e) in index_exprs.iter().enumerate() {
                        let lin = into_lin(ctx.eval(e, env).map_err(fail)?).map_err(fail)?;
                        let set = comp.vars[d].1.as_str();
                        if symbolic_sets.contains(set) {
                            if single(&lin).and_then(|j| col_symbolic[j]) != Some(set) {
                                return Err(other(format!(
                                    "an index over set {set} must be a symbolic variable over {set}"
                                )));
                            }
                        } else if lin.coefs.iter().any(|c| *c.1 != 0.0 && col_symbolic[*c.0].is_some()) {
                            return Err(other(format!("a symbolic variable can't index the integer set {set}")));
                        }
                        index.push(affine(lin));
                    }
                    let value = into_lin(ctx.eval(value_expr, env).map_err(fail)?).map_err(fail)?;
                    no_symbolic(&value.coefs.keys().copied().collect::<Vec<_>>(), &format!("constraint {inst}"))?;
                    let mut combos = Vec::new();
                    ctx.for_each_binding(comp, env, &mut |ctx, env| {
                        let mut key = Vec::new();
                        for (idx_name, set) in &comp.vars {
                            let v = env.iter().rev().find(|(n, _)| n == idx_name).map(|(_, v)| v.clone()).unwrap();
                            let e = to_elem(v)?;
                            key.push(if symbolic_sets.contains(set.as_str()) {
                                ctx.sets[set.as_str()].iter().position(|x| *x == e).unwrap() as i64 + 1
                            } else {
                                match e {
                                    Elem::Int(i) => i,
                                    Elem::Str(_) => unreachable!(),
                                }
                            });
                        }
                        let item = into_lin(ctx.eval(body, env)?)?;
                        combos.push((key, item));
                        Ok(())
                    })
                    .map_err(fail)?;
                    let mut items = Vec::with_capacity(combos.len());
                    for (key, item) in combos {
                        no_symbolic(&item.coefs.keys().copied().collect::<Vec<_>>(), &format!("constraint {inst}"))?;
                        items.push((key, affine(item)));
                    }
                    globals.push(Global::Element { label: inst, index, combos: items, value: affine(value) });
                }
                ConstraintKind::NoOverlap(_) | ConstraintKind::Cumulative(_) => {
                    let ids = interval_list(ctx, list.as_ref().unwrap(), env, &interval_ids).map_err(fail)?;
                    let ivs: Vec<Interval> = ids.iter().map(|&i| intervals[i].clone()).collect();
                    match &cumulative {
                        None => globals.push(Global::NoOverlap { label: inst, intervals: ivs }),
                        Some((demands, capacity)) => {
                            let checked = |l: Lin| -> R<Affine> {
                                no_symbolic(
                                    &l.coefs.keys().copied().collect::<Vec<_>>(),
                                    &format!("constraint {inst}"),
                                )?;
                                Ok(affine(l))
                            };
                            let demands = ctx.eval_list(demands, env).map_err(fail)?;
                            let demands = demands.into_iter().map(checked).collect::<R<Vec<_>>>()?;
                            let capacity = checked(into_lin(ctx.eval(capacity, env).map_err(fail)?).map_err(fail)?)?;
                            globals.push(Global::Cumulative { label: inst, intervals: ivs, demands, capacity });
                        }
                    }
                }
                ConstraintKind::Min(_) | ConstraintKind::Max(_) | ConstraintKind::Abs(_) => {
                    let (value_expr, of_expr) = aggregate.as_ref().unwrap();
                    // Affine and free of symbolic variables.
                    let checked = |l: Lin| -> R<Affine> {
                        no_symbolic(&l.coefs.keys().copied().collect::<Vec<_>>(), &format!("constraint {inst}"))?;
                        Ok(affine(l))
                    };
                    let value = checked(into_lin(ctx.eval(value_expr, env).map_err(fail)?).map_err(fail)?)?;
                    if let Some(of) = of_expr {
                        let of = checked(into_lin(ctx.eval(of, env).map_err(fail)?).map_err(fail)?)?;
                        globals.push(Global::Abs { label: inst, value, of });
                    } else {
                        let items = ctx.eval_list(list.as_ref().unwrap(), env).map_err(fail)?;
                        let items = items.into_iter().map(checked).collect::<R<Vec<_>>>()?;
                        let maximize = matches!(kind, ConstraintKind::Max(_));
                        globals.push(Global::MinMax { label: inst, value, items, maximize });
                    }
                }
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
    let mut objective_node = None;
    if let Some(text) = &problem.solve.objective {
        let e = expr::parse_expr(text).map_err(|e| format!("objective: {e}"))?;
        let value = ctx.eval(&e, &mut Vec::new()).map_err(|e| match e {
            EvalError::NonLinear(what) => format!("unsupported: nonlinear term ({what}) in the objective"),
            EvalError::Other(e) => format!("objective: {e}"),
        })?;
        let lin = match value {
            Val::Nl(lin, node) => {
                no_symbolic(&ctx.graph.borrow().vars_of(node), "the objective")?;
                objective_node = Some(node);
                lin
            }
            v => into_lin(v).map_err(|e| match e {
                EvalError::NonLinear(what) => format!("unsupported: nonlinear term ({what}) in the objective"),
                EvalError::Other(e) => format!("objective: {e}"),
            })?,
        };
        no_symbolic(&lin.coefs.iter().filter(|c| *c.1 != 0.0).map(|c| *c.0).collect::<Vec<_>>(), "the objective")?;
        for (j, v) in lin.coefs {
            obj[j] = v;
        }
        obj_offset = lin.constant;
    }

    let uses_cp_kinds = !globals.is_empty();
    let mut scheduling = Vec::new();
    if uses_cp_kinds {
        let cols = Columns { lower: &mut lower, upper: &mut upper, integer: &mut integer, domain: &mut domains };
        let mut encoder = Encoder::new(cols, &mut rows, want_all);
        for g in globals {
            if matches!(g, Global::NoOverlap { .. } | Global::Cumulative { .. }) {
                encoder.check_scheduling(&g)?;
                scheduling.push(g);
                continue;
            }
            if let Some(v) = encoder.encode(&g)?
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
        scheduling,
        nonlinear: (!nonlinear_rows.is_empty() || objective_node.is_some()).then(|| Nonlinear {
            graph: ctx.graph.into_inner(),
            rows: nonlinear_rows,
            objective: objective_node,
        }),
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
        for engine in [Engine::Cp, Engine::Mip] {
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
        for engine in [Engine::Cp, Engine::Mip] {
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
        for engine in [Engine::Cp, Engine::Mip] {
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
            for engine in [Engine::Cp, Engine::Mip] {
                let model = load(&json(&format!(r#"{{"method": "minimize", "objective": "{objective}"}}"#))).unwrap();
                let out = solve(&model, None, engine).unwrap();
                let got = out.solutions.first().map(|x| model.objective_value(x).round() as i64);
                assert_eq!(got, best, "{engine:?} optimum, {label}");
            }
        }
    }
}
