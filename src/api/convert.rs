//! Reads a problem file into a [`Model`]: sets become [`Set`]s, variables
//! families, every instance of an indexed constraint one constraint of the
//! model (labeled `name[i,j]`), and the objective an [`Expression`].
//!
//! The evaluation mirrors the format's semantics (`docs/problem-format.md`)
//! exactly, so that the model lowers to the same solver model as the file
//! always has: the same columns, rows in the same order with the same
//! coefficients, the same constraint instances, and the same messages.

use super::builder::{IntervalVar, Model, VarSpec, var};
use super::expression::{Constraint, Expression, Literal, LiteralKind, Owner, Tree, Variable};
use super::family::{Element, Set, VarFamily};
use crate::expr::{self, BinOp, Bindings, CmpOp, Cond, Expr, List, Rel};
use crate::model::{Sense, build_sets, product, to_ranges};
use crate::problem::{
    Bound, ConstraintKind, DomainDef, Elem, Method, OneOrMany, ParamDef, Problem, Solutions, VarType,
};
use serde_json::Value as Json;
use std::collections::HashMap;
use std::sync::Arc;

/// Why evaluation failed: a nonlinear term where only affine ones may be
/// (reported with the constraint's place), or anything else.
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
type Env = Vec<(String, Val)>;
type BindingFn<'f> = dyn FnMut(&Ctx, &mut Env) -> R<()> + 'f;

#[derive(Clone, Debug)]
enum Val {
    Num(f64),
    Str(String),
    Expr(Expression),
}

fn describe(v: &Val) -> &'static str {
    match v {
        Val::Num(_) => "a number",
        Val::Str(_) => "a string set element",
        Val::Expr(e) if e.nonlinear.is_empty() => "an expression with variables",
        Val::Expr(_) => "a nonlinear expression",
    }
}

struct Ctx<'a> {
    problem: &'a Problem,
    sets: HashMap<&'a str, Vec<Elem>>,
    /// Each variable's columns by key.
    vars: HashMap<&'a str, HashMap<Vec<Elem>, Variable>>,
    model_id: u64,
}

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
        Val::Expr(_) => Err("an index cannot depend on a variable".to_string().into()),
    }
}

/// The value as an affine expression (an error for strings and nonlinear
/// expressions).
fn into_affine(v: Val) -> R<Expression> {
    match v {
        Val::Num(c) => Ok(Expression { constant: c, ..Expression::default() }),
        Val::Expr(e) if e.nonlinear.is_empty() => Ok(e),
        Val::Expr(_) => Err(EvalError::NonLinear(crate::model::NOT_AFFINE)),
        Val::Str(s) => Err(format!("string {s:?} used in arithmetic").into()),
    }
}

/// The value as an expression (nonlinear ones included).
fn into_expr(v: Val) -> R<Expression> {
    match v {
        Val::Num(c) => Ok(Expression { constant: c, ..Expression::default() }),
        Val::Expr(e) => Ok(e),
        Val::Str(s) => Err(format!("string {s:?} used in arithmetic").into()),
    }
}

/// An expression with no variable (all coefficients zero, no nonlinear
/// term) as the number it is, as the graph folds it.
fn reduced(v: Val) -> Val {
    match v {
        Val::Expr(e) if e.nonlinear.is_empty() && e.linear.values().all(|&a| a == 0.0) => Val::Num(e.constant),
        v => v,
    }
}

/// A nonlinear term (coefficient 1).
fn term(t: Tree, owner: Owner) -> Val {
    Val::Expr(Expression { owner, linear: Default::default(), constant: 0.0, nonlinear: vec![(1.0, Arc::new(t))] })
}

impl Ctx<'_> {
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

    fn owner(&self) -> Owner {
        Owner::Model(self.model_id)
    }

    fn eval(&self, e: &Expr, env: &mut Env) -> R<Val> {
        match e {
            Expr::Num(v) => Ok(Val::Num(*v)),
            Expr::Str(s) => Ok(Val::Str(s.clone())),
            Expr::Ref(name, args) => self.eval_ref(name, args.as_deref(), env),
            Expr::Neg(a) => self.scale(self.eval(a, env)?, -1.0),
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
                        (x, y) => self.binary(x, y, Tree::Mul),
                    },
                    BinOp::Div => match (x, y) {
                        (s @ Val::Str(_), t) | (s, t @ Val::Str(_)) => {
                            Err(format!("cannot divide {} by {}", describe(&s), describe(&t)).into())
                        }
                        (_, Val::Num(0.0)) => Err("division by zero".to_string().into()),
                        (v, Val::Num(c)) => self.scale(v, 1.0 / c),
                        (x, y) => self.binary(x, y, Tree::Div),
                    },
                }
            }
            Expr::Pow(a, n) => match (self.eval(a, env)?, n) {
                (Val::Num(v), n) => finite(v.powi(*n), || format!("{v}^{n}")),
                (v @ Val::Str(_), _) => Err(format!("cannot raise {} to a power", describe(&v)).into()),
                (v, 1) => Ok(v),
                (_, 0) => Ok(Val::Num(1.0)),
                (v, n) => self.unary(v, |a| Tree::PowI(a, *n), |c| c.powi(*n)),
            },
            Expr::PowE(a, b) => match (self.eval(a, env)?, self.eval(b, env)?) {
                (Val::Num(x), Val::Num(y)) => finite(x.powf(y), || format!("{x}^{y}")),
                (x @ Val::Str(_), y) | (x, y @ Val::Str(_)) => {
                    Err(format!("cannot raise {} to the power {}", describe(&x), describe(&y)).into())
                }
                (x, y) => self.binary(x, y, Tree::Pow),
            },
            Expr::Call(f, a) => match self.eval(a, env)? {
                Val::Num(x) => finite(f.apply(x), || format!("{}({x})", f.name())),
                v @ Val::Str(_) => Err(format!("cannot apply {} to {}", f.name(), describe(&v)).into()),
                v => self.unary(v, |a| Tree::Func(*f, a), |c| f.apply(c)),
            },
            Expr::Sum(b, body) => {
                let mut total = Val::Num(0.0);
                self.for_each_binding(b, env, &mut |ctx, env| {
                    let t = ctx.eval(body, env)?;
                    total = ctx.add(std::mem::replace(&mut total, Val::Num(0.0)), t, 1.0)?;
                    Ok(())
                })?;
                Ok(total)
            }
        }
    }

    /// A nonlinear term of two values (either may be a variable-free
    /// expression, which the graph folds like a number).
    fn binary(&self, x: Val, y: Val, make: impl Fn(Expression, Expression) -> Tree) -> R<Val> {
        let (x, y) = (into_expr(x)?, into_expr(y)?);
        Ok(term(make(x, y), self.owner()))
    }

    /// A nonlinear function of a value (folded if the value has no variable).
    fn unary(&self, v: Val, make: impl Fn(Expression) -> Tree, apply: impl Fn(f64) -> f64) -> R<Val> {
        match reduced(v) {
            Val::Num(c) => Ok(Val::Num(apply(c))),
            v => Ok(term(make(into_expr(v)?), self.owner())),
        }
    }

    fn add(&self, x: Val, y: Val, sign: f64) -> R<Val> {
        if let (Val::Num(a), Val::Num(b)) = (&x, &y) {
            return Ok(Val::Num(a + sign * b));
        }
        let mut l = into_expr(x)?;
        let r = into_expr(y)?;
        for (k, v) in r.linear {
            *l.linear.entry(k).or_insert(0.0) += sign * v;
        }
        l.constant += sign * r.constant;
        l.nonlinear.extend(r.nonlinear.into_iter().map(|(a, t)| (sign * a, t)).filter(|t| t.0 != 0.0));
        l.owner = l.owner.merge(r.owner);
        Ok(Val::Expr(l))
    }

    fn scale(&self, v: Val, c: f64) -> R<Val> {
        match v {
            Val::Num(x) => Ok(Val::Num(x * c)),
            Val::Expr(mut e) => {
                for a in e.linear.values_mut() {
                    *a *= c;
                }
                e.constant *= c;
                for t in &mut e.nonlinear {
                    t.0 *= c;
                }
                e.nonlinear.retain(|t| t.0 != 0.0);
                Ok(Val::Expr(e))
            }
            Val::Str(s) => Err(format!("string {s:?} used in arithmetic").into()),
        }
    }

    fn eval_ref(&self, name: &str, args: Option<&[Expr]>, env: &mut Env) -> R<Val> {
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
        if let Some(cols) = self.vars.get(name) {
            return match cols.get(&keys) {
                Some(&v) => Ok(Val::Expr(v.into())),
                None => {
                    let k: Vec<_> = keys.iter().map(Elem::key).collect();
                    Err(format!("{name}[{}] is outside the variable's index sets", k.join(",")).into())
                }
            };
        }
        Err(format!("unknown name {name}").into())
    }

    fn test(&self, c: &Cond, env: &mut Env) -> R<bool> {
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

    /// A list's items, as affine expressions, in order.
    fn eval_list(&self, list: &List, env: &mut Env) -> R<Vec<Expression>> {
        match list {
            List::Explicit(items) => items.iter().map(|e| self.eval(e, env).and_then(into_affine)).collect(),
            List::Comprehension(body, b) => {
                let mut out = Vec::new();
                self.for_each_binding(b, env, &mut |ctx, env| {
                    out.push(into_affine(ctx.eval(body, env)?)?);
                    Ok(())
                })?;
                Ok(out)
            }
        }
    }

    fn for_each_binding(&self, b: &Bindings, env: &mut Env, f: &mut BindingFn<'_>) -> R<()> {
        for (_, set) in &b.vars {
            if !self.sets.contains_key(set.as_str()) {
                return Err(format!("unknown set {set}").into());
            }
        }
        self.bind_from(b, 0, env, f)
    }

    fn bind_from(&self, b: &Bindings, k: usize, env: &mut Env, f: &mut BindingFn<'_>) -> R<()> {
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

fn element_of(e: &Elem) -> Element {
    Element::from_elem(e)
}

/// The variable an expression is (coefficient 1, nothing else), if it is one.
fn single(e: &Expression) -> Option<usize> {
    let mut it = e.linear.iter().filter(|c| *c.1 != 0.0);
    match (it.next(), it.next()) {
        (Some((&j, &a)), None) if a == 1.0 && e.constant == 0.0 && e.nonlinear.is_empty() => Some(j),
        _ => None,
    }
}

/// Reads `problem` into a model.
pub(crate) fn to_model(problem: &Problem) -> Result<Model, String> {
    let elems = build_sets(problem)?;
    let mut m = Model::new();
    let sets: HashMap<&str, Set> =
        elems.iter().map(|(&name, e)| (name, Set::from_unique(name, e.iter().map(element_of).collect()))).collect();
    let mut ctx = Ctx { problem, sets: elems, vars: HashMap::new(), model_id: m.id };

    // Variables: one family per variable, keyed like the file.
    // The set of each symbolic column (variables over a set with a string).
    let mut col_symbolic: Vec<Option<&str>> = Vec::new();
    let mut var_name: Vec<&str> = Vec::new();
    for v in &problem.variables {
        let index: Vec<&Set> = v.index.iter().map(|s| &sets[s.as_str()]).collect();
        let symbolic_set = match &v.domain {
            Some(DomainDef::Set(s)) if sets[s.as_str()].has_strings() => Some(s.as_str()),
            _ => None,
        };
        let base = match v.var_type {
            VarType::Continuous => var(),
            VarType::Integer => var().integer(),
            VarType::Binary => var().binary(),
        };
        let base = match &v.domain {
            Some(DomainDef::Set(s)) => {
                let set = &sets[s.as_str()];
                if set.has_strings() {
                    base.values(set)
                } else {
                    let mut values: Vec<i64> = set
                        .elements()
                        .iter()
                        .map(|e| if let Element::Int(i) = e { *i } else { unreachable!() })
                        .collect();
                    values.sort_unstable();
                    VarSpec::domain_ranges(base, to_ranges(&values))
                }
            }
            Some(DomainDef::Ranges(d)) => VarSpec::domain_ranges(base, d.iter().map(|[a, b]| (*a, *b)).collect()),
            None => base,
        };
        let keys = product(&ctx.sets, &v.index);
        let mut specs = Vec::with_capacity(keys.len());
        for idx in &keys {
            let bound = |b: &Option<Bound>| -> Result<Option<f64>, String> {
                match b {
                    None => Ok(None),
                    Some(Bound::Num(x)) => Ok(Some(*x)),
                    Some(Bound::Param(p)) => ctx.param(p, idx).map(Some),
                }
            };
            let mut spec = base.clone();
            if v.var_type != VarType::Binary && v.domain.is_none() {
                if let Some(lo) = bound(&v.lower)? {
                    spec = spec.min(lo);
                }
                if let Some(hi) = bound(&v.upper)? {
                    spec = spec.max(hi);
                }
            }
            if let Some(s) = bound(&v.start)? {
                spec = spec.start(s);
            }
            specs.push(spec);
        }
        let family: VarFamily = m.family_from(v.name.clone(), &index, specs);
        let mut cols = HashMap::with_capacity(keys.len());
        for (idx, &var) in keys.into_iter().zip(family.vars()) {
            cols.insert(idx, var);
            col_symbolic.push(symbolic_set);
            var_name.push(v.name.as_str());
        }
        ctx.vars.insert(v.name.as_str(), cols);
    }
    let no_symbolic = |e: &Expression, place: &str| -> Result<(), String> {
        let mut cols = Vec::new();
        e.columns(&mut cols);
        match cols.into_iter().find(|&j| col_symbolic[j].is_some()) {
            Some(j) => Err(format!(
                "{place}: symbolic variable {} can't be used in arithmetic (its values are set elements)",
                var_name[j]
            )),
            None => Ok(()),
        }
    };

    // Intervals, before the constraints (their order is the solver's).
    let mut interval_ids: HashMap<&str, HashMap<Vec<Elem>, IntervalVar>> = HashMap::new();
    for iv in &problem.intervals {
        let mut ids = HashMap::new();
        for idx in product(&ctx.sets, &iv.index) {
            let keys: Vec<String> = idx.iter().map(Elem::key).collect();
            let label = if idx.is_empty() { iv.name.clone() } else { format!("{}[{}]", iv.name, keys.join(",")) };
            let field = |b: &Bound| -> Result<Expression, String> {
                Ok(match b {
                    Bound::Num(x) => Expression { constant: *x, ..Expression::default() },
                    Bound::Param(name) => match ctx.vars.get(name.as_str()) {
                        Some(cols) => cols[&idx].into(),
                        None => Expression { constant: ctx.param(name, &idx)?, ..Expression::default() },
                    },
                })
            };
            let (start, duration) = (field(&iv.start)?, field(&iv.duration)?);
            let presence = iv.presence.as_ref().map(|p| ctx.vars[p.as_str()][&idx]);
            ids.insert(idx, m.interval_named(label, start, duration, presence));
        }
        interval_ids.insert(iv.name.as_str(), ids);
    }

    let column_value_ok = |col: usize, e: &Elem| -> Result<(), String> {
        match (col_symbolic[col], e) {
            (None, Elem::Str(s)) => Err(format!("{s:?} compared with a numeric variable")),
            _ => Ok(()),
        }
    };
    let no_bindings = Bindings { vars: vec![], cond: None };
    for (i, c) in problem.constraints.iter().enumerate() {
        let name = c.name.clone().unwrap_or_else(|| format!("#{}", i + 1));
        let bindings = match &c.forall {
            Some(f) => expr::parse_bindings(f).map_err(|e| format!("constraint {name}: forall: {e}"))?,
            None => no_bindings.clone(),
        };
        let kind = c.kind()?;
        let parse_err = |e: String| format!("constraint {name}: {e}");
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
        let cumulative = match &kind {
            ConstraintKind::Cumulative(d) => Some((
                expr::parse_list(&d.demands).map_err(parse_err)?,
                expr::parse_expr(&d.capacity).map_err(parse_err)?,
            )),
            _ => None,
        };
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
            let inst = instance_label(&name, &bindings, env);
            let fail = |e: EvalError| -> EvalError {
                match e {
                    EvalError::NonLinear(w) => format!("unsupported: nonlinear term ({w}) in constraint {inst}").into(),
                    EvalError::Other(e) => format!("constraint {inst}: {e}").into(),
                }
            };
            let other = |e: String| -> EvalError { format!("constraint {inst}: {e}").into() };
            let place = format!("constraint {inst}");
            match &kind {
                ConstraintKind::Expr(..) => {
                    let (rel, lhs, rhs) = relation.as_ref().unwrap();
                    let (lv, rv) = (ctx.eval(lhs, env).map_err(fail)?, ctx.eval(rhs, env).map_err(fail)?);
                    // A symbolic variable compared with an element: fix it.
                    let symbolic_eq = match (&lv, &rv) {
                        (Val::Expr(l), v @ (Val::Str(_) | Val::Num(_)))
                        | (v @ (Val::Str(_) | Val::Num(_)), Val::Expr(l))
                            if *rel == Rel::Eq && single(l).is_some_and(|j| col_symbolic[j].is_some()) =>
                        {
                            let col = single(l).unwrap();
                            let e = to_elem(v.clone()).map_err(fail)?;
                            Some(Variable { model: ctx.model_id, col }.equals(element_of(&e)))
                        }
                        _ => None,
                    };
                    let mut constraint = match symbolic_eq {
                        Some(c) => c,
                        None => {
                            let diff = ctx.add(lv, rv, -1.0).map_err(fail)?;
                            let e = if literals.is_empty() {
                                into_expr(diff).map_err(fail)?
                            } else {
                                into_affine(diff).map_err(fail)?
                            };
                            no_symbolic(&e, &place).map_err(EvalError::Other)?;
                            let (lower, upper) = match rel {
                                Rel::Le => (f64::NEG_INFINITY, 0.0),
                                Rel::Ge => (0.0, f64::INFINITY),
                                Rel::Eq => (0.0, 0.0),
                            };
                            Constraint { expr: e, lower, upper, literals: Vec::new(), name: None, element: None }
                        }
                    };
                    for lit in &literals {
                        let col = into_affine(ctx.eval(&lit.var, env).map_err(fail)?)
                            .ok()
                            .and_then(|l| single(&l))
                            .ok_or_else(|| other("an enforce_if literal must name one variable".into()))?;
                        let v = Variable { model: ctx.model_id, col };
                        let literal = match &lit.compare {
                            None if lit.negated => Literal { var: v, kind: LiteralKind::False },
                            None => Literal { var: v, kind: LiteralKind::True },
                            Some((equal, e)) => {
                                let e = to_elem(ctx.eval(e, env).map_err(fail)?).map_err(fail)?;
                                column_value_ok(col, &e).map_err(other)?;
                                let e = element_of(&e);
                                Literal {
                                    var: v,
                                    kind: if *equal { LiteralKind::Is(e) } else { LiteralKind::IsNot(e) },
                                }
                            }
                        };
                        constraint.literals.push(literal);
                    }
                    constraint.name = Some(inst.clone());
                    m.add(constraint);
                }
                ConstraintKind::AllDifferent(_) => {
                    let items = ctx.eval_list(list.as_ref().unwrap(), env).map_err(fail)?;
                    // Symbolic items must be bare variables over one set.
                    let mut sets = std::collections::HashSet::new();
                    for item in &items {
                        let symbolic = item.linear.iter().any(|c| *c.1 != 0.0 && col_symbolic[*c.0].is_some());
                        if symbolic && single(item).is_none() {
                            return Err(other("a symbolic variable in all_different must appear alone".into()));
                        }
                        sets.insert(single(item).and_then(|j| col_symbolic[j]));
                    }
                    if sets.len() > 1 {
                        return Err(other(
                            "all_different items must be all integer-valued or all symbolic over one set".into(),
                        ));
                    }
                    let id = m.all_different(items);
                    m.label(id, inst.clone());
                }
                ConstraintKind::Table(t) => {
                    let items = ctx.eval_list(list.as_ref().unwrap(), env).map_err(fail)?;
                    let vars: Vec<usize> = items
                        .iter()
                        .map(single)
                        .collect::<Option<_>>()
                        .ok_or_else(|| other("table vars must be variables".into()))?;
                    let mut tuples = Vec::with_capacity(t.tuples.len());
                    for tuple in &t.tuples {
                        if tuple.len() != vars.len() {
                            return Err(other(format!(
                                "a tuple has {} entries for {} variables",
                                tuple.len(),
                                vars.len()
                            )));
                        }
                        for (&col, e) in vars.iter().zip(tuple) {
                            column_value_ok(col, e).map_err(other)?;
                        }
                        tuples.push(tuple.iter().map(element_of).collect());
                    }
                    let vars = vars.into_iter().map(|col| Variable { model: ctx.model_id, col }).collect();
                    let id = m.table_of(vars, tuples, t.forbidden);
                    m.label(id, inst.clone());
                }
                ConstraintKind::Element(_) => {
                    let (index_exprs, value_expr, comp) = element.as_ref().unwrap();
                    let body = match list.as_ref().unwrap() {
                        List::Comprehension(body, _) => body,
                        List::Explicit(_) => unreachable!(),
                    };
                    let mut index = Vec::new();
                    for (d, e) in index_exprs.iter().enumerate() {
                        let lin = into_affine(ctx.eval(e, env).map_err(fail)?).map_err(fail)?;
                        let set = comp.vars[d].1.as_str();
                        if ctx.sets[set].iter().any(|e| matches!(e, Elem::Str(_))) {
                            if single(&lin).and_then(|j| col_symbolic[j]) != Some(set) {
                                return Err(other(format!(
                                    "an index over set {set} must be a symbolic variable over {set}"
                                )));
                            }
                        } else if lin.linear.iter().any(|c| *c.1 != 0.0 && col_symbolic[*c.0].is_some()) {
                            return Err(other(format!("a symbolic variable can't index the integer set {set}")));
                        }
                        index.push(lin);
                    }
                    let value = into_affine(ctx.eval(value_expr, env).map_err(fail)?).map_err(fail)?;
                    no_symbolic(&value, &place).map_err(EvalError::Other)?;
                    let mut items = Vec::new();
                    ctx.for_each_binding(comp, env, &mut |ctx, env| {
                        let mut key = Vec::new();
                        for (idx_name, _) in &comp.vars {
                            let v = env.iter().rev().find(|(n, _)| n == idx_name).map(|(_, v)| v.clone()).unwrap();
                            key.push(element_of(&to_elem(v)?));
                        }
                        items.push((key, into_affine(ctx.eval(body, env)?)?));
                        Ok(())
                    })
                    .map_err(fail)?;
                    for (_, item) in &items {
                        no_symbolic(item, &place).map_err(EvalError::Other)?;
                    }
                    let id = m.element_keyed(index, items, value);
                    m.label(id, inst.clone());
                }
                ConstraintKind::NoOverlap(_) | ConstraintKind::Cumulative(_) => {
                    let ivs = interval_list(ctx, list.as_ref().unwrap(), env, &interval_ids).map_err(fail)?;
                    match &cumulative {
                        None => {
                            let id = m.no_overlap(&ivs);
                            m.label(id, inst.clone());
                        }
                        Some((demands, capacity)) => {
                            let demands = ctx.eval_list(demands, env).map_err(fail)?;
                            for d in &demands {
                                no_symbolic(d, &place).map_err(EvalError::Other)?;
                            }
                            let capacity = into_affine(ctx.eval(capacity, env).map_err(fail)?).map_err(fail)?;
                            no_symbolic(&capacity, &place).map_err(EvalError::Other)?;
                            let id = m.cumulative(&ivs, demands, capacity);
                            m.label(id, inst.clone());
                        }
                    }
                }
                ConstraintKind::Min(_) | ConstraintKind::Max(_) | ConstraintKind::Abs(_) => {
                    let (value_expr, of_expr) = aggregate.as_ref().unwrap();
                    let value = into_affine(ctx.eval(value_expr, env).map_err(fail)?).map_err(fail)?;
                    no_symbolic(&value, &place).map_err(EvalError::Other)?;
                    if let Some(of) = of_expr {
                        let of = into_affine(ctx.eval(of, env).map_err(fail)?).map_err(fail)?;
                        no_symbolic(&of, &place).map_err(EvalError::Other)?;
                        let id = m.abs_of(value, of);
                        m.label(id, inst.clone());
                    } else {
                        let items = ctx.eval_list(list.as_ref().unwrap(), env).map_err(fail)?;
                        for item in &items {
                            no_symbolic(item, &place).map_err(EvalError::Other)?;
                        }
                        let maximize = matches!(kind, ConstraintKind::Max(_));
                        let id = if maximize { m.max_of(value, items) } else { m.min_of(value, items) };
                        m.label(id, inst.clone());
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
    if let Some(text) = &problem.solve.objective {
        let e = expr::parse_expr(text).map_err(|e| format!("objective: {e}"))?;
        let objective = ctx.eval(&e, &mut Vec::new()).and_then(into_expr).map_err(|e| match e {
            EvalError::NonLinear(what) => format!("unsupported: nonlinear term ({what}) in the objective"),
            EvalError::Other(e) => format!("objective: {e}"),
        })?;
        no_symbolic(&objective, "the objective")?;
        match sense {
            Sense::Minimize => m.minimize(objective),
            Sense::Maximize => m.maximize(objective),
            // A satisfaction problem's objective (the file shouldn't have one).
            Sense::Satisfy => {}
        }
    }
    match problem.solve.solutions {
        Solutions::One => {}
        Solutions::All => m.all_solutions(true),
        Solutions::UpTo(n) => m.solution_limit(n),
    }
    Ok(m)
}

/// A constraint instance's label: `name`, or `name[i,j]` with the bound
/// indexes' values.
fn instance_label(name: &str, b: &Bindings, env: &Env) -> String {
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

/// The intervals a list names (`[task[j] | j in J]`, `[a, b]`).
fn interval_list(
    ctx: &Ctx,
    list: &List,
    env: &mut Env,
    ids: &HashMap<&str, HashMap<Vec<Elem>, IntervalVar>>,
) -> R<Vec<IntervalVar>> {
    let one = |ctx: &Ctx, e: &Expr, env: &mut Env| -> R<IntervalVar> {
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
