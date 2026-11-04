//! Expands a [`Problem`] into a flat linear model: one column per variable
//! element, one row per constraint instance. Rejects anything nonlinear
//! (docs/solver-design.md, "Scope").

use crate::expr::{self, BinOp, Bindings, CmpOp, Cond, Expr, Rel};
use crate::problem::{Bound, Elem, Method, ParamDef, Problem, SetDef, Solutions, VarType};
use serde_json::Value as Json;
use std::collections::HashMap;

pub const INF: f64 = f64::INFINITY;

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
    let mut vars = Vec::new();
    for v in &problem.variables {
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
            let (lo, hi, dom) = match (v.var_type, &v.domain) {
                (VarType::Binary, _) => (0.0, 1.0, None),
                (_, Some(d)) => {
                    let ranges: Vec<(i64, i64)> = d.iter().map(|[a, b]| (*a, *b)).collect();
                    let (lo, hi) = (ranges[0].0 as f64, ranges[ranges.len() - 1].1 as f64);
                    (lo, hi, if ranges.len() > 1 { Some(ranges) } else { None })
                }
                _ => (bound(&v.lower, -INF)?, bound(&v.upper, INF)?, None),
            };
            lower.push(lo);
            upper.push(hi);
            integer.push(v.var_type != VarType::Continuous);
            domains.push(dom);
            cols.insert(idx.clone(), col);
            elements.push((idx, col));
        }
        ctx.var_cols.insert(v.name.as_str(), cols);
        vars.push(VarInfo { name: v.name.clone(), indexed: !v.index.is_empty(), var_type: v.var_type, elements });
    }
    let n = lower.len();

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
    let no_bindings = Bindings { vars: vec![], cond: None };
    for (i, c) in problem.constraints.iter().enumerate() {
        let name = c.name.clone().unwrap_or_else(|| format!("#{}", i + 1));
        let (rel, lhs, rhs) = expr::parse_relation(&c.expr).map_err(|e| format!("constraint {name}: {e}"))?;
        let bindings = match &c.forall {
            Some(f) => expr::parse_bindings(f).map_err(|e| format!("constraint {name}: forall: {e}"))?,
            None => no_bindings.clone(),
        };
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
        for (j, v) in lin.coefs {
            obj[j] = v;
        }
        obj_offset = lin.constant;
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
    })
}
