//! Writes a builder model in the JSON problem format (format 1.2), flat: one
//! variable per [`Model::add_var`] and one indexed variable per
//! [`Model::add_vars`], one constraint per constraint added.
//!
//! What the format can't say directly gets auxiliary variables named with a
//! leading `_`: intervals whose start, duration, or presence isn't a scalar
//! variable or a constant, and `element` items that aren't all constants.

use super::Error;
use super::ErrorKind;
use super::builder::{IntervalVar, Item, Model, VarKind};
use super::expression::{Constraint, Expression, LiteralKind, Owner, Tree, Variable};
use super::lower::{check_names, is_identifier};
use crate::model::to_ranges;
use serde_json::{Map, Value, json};

struct Writer<'m> {
    model: &'m Model,
    /// Each column's name in expressions (`x` or `x[3]`), and whether it is a
    /// scalar variable.
    refs: Vec<(String, bool)>,
    /// Each column's kind and bounds.
    cols: Vec<(VarKind, f64, f64)>,
    sets: Map<String, Value>,
    parameters: Map<String, Value>,
    variables: Vec<Value>,
    intervals: Vec<Value>,
    constraints: Vec<Value>,
    aux: usize,
}

fn invalid(message: String) -> Error {
    Error::new(ErrorKind::Invalid, message)
}

/// A number in an expression: negative ones in parentheses.
fn num(x: f64, place: &str) -> Result<String, Error> {
    if !x.is_finite() {
        return Err(invalid(format!(
            "{place}: a constant isn't a finite number (a division by zero, or a function outside its domain)"
        )));
    }
    let x = if x == 0.0 { 0.0 } else { x }; // no "-0"
    Ok(if x < 0.0 { format!("({x})") } else { format!("{x}") })
}

/// The order of keys in the written file (others follow, numbers in
/// numeric order), as in the examples.
const KEY_ORDER: &[&str] = &[
    "version",
    "sets",
    "parameters",
    "variables",
    "intervals",
    "constraints",
    "solve",
    "name",
    "description",
    "type",
    "index",
    "lower",
    "upper",
    "domain",
    "start",
    "duration",
    "presence",
    "expr",
    "enforce_if",
    "all_different",
    "table",
    "element",
    "min",
    "max",
    "abs",
    "no_overlap",
    "cumulative",
    "vars",
    "tuples",
    "forbidden",
    "array",
    "value",
    "of",
    "demands",
    "capacity",
    "method",
    "objective",
    "solutions",
    "values",
    "from",
    "to",
];

fn ordered(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<_> = map.iter().collect();
    let rank = |k: &str| KEY_ORDER.iter().position(|o| *o == k).unwrap_or(KEY_ORDER.len());
    entries.sort_by(|a, b| {
        let (ka, kb) = (a.0.as_str(), b.0.as_str());
        rank(ka).cmp(&rank(kb)).then_with(|| match (ka.parse::<i64>(), kb.parse::<i64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => ka.cmp(kb),
        })
    });
    entries
}

/// A value on one line, keys in order.
fn inline(v: &Value) -> String {
    match v {
        Value::Object(map) if map.is_empty() => "{}".into(),
        Value::Object(map) => {
            let parts: Vec<String> =
                ordered(map).into_iter().map(|(k, v)| format!("{}: {}", Value::from(k.as_str()), inline(v))).collect();
            format!("{{ {} }}", parts.join(", "))
        }
        Value::Array(items) => format!("[{}]", items.iter().map(inline).collect::<Vec<_>>().join(", ")),
        v => v.to_string(),
    }
}

/// The file: one top-level key per line, and one set, parameter, variable,
/// interval, or constraint per line.
fn render(top: &Map<String, Value>) -> String {
    let mut out = String::from("{\n");
    let entries = ordered(top);
    for (i, (k, v)) in entries.iter().enumerate() {
        out.push_str(&format!("  {}: ", Value::from(k.as_str())));
        match v {
            Value::Object(map) if !map.is_empty() && matches!(k.as_str(), "sets" | "parameters") => {
                out.push_str("{\n");
                let inner = ordered(map);
                for (j, (k, v)) in inner.iter().enumerate() {
                    let comma = if j + 1 < inner.len() { "," } else { "" };
                    out.push_str(&format!("    {}: {}{comma}\n", Value::from(k.as_str()), inline(v)));
                }
                out.push_str("  }");
            }
            Value::Array(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (j, item) in items.iter().enumerate() {
                    let comma = if j + 1 < items.len() { "," } else { "" };
                    out.push_str(&format!("    {}{comma}\n", inline(item)));
                }
                out.push_str("  ]");
            }
            v => out.push_str(&inline(v)),
        }
        out.push_str(if i + 1 < entries.len() { ",\n" } else { "\n" });
    }
    out.push_str("}\n");
    out
}

/// A JSON number.
fn json_num(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9e15 { json!(x as i64) } else { json!(x) }
}

impl Writer<'_> {
    fn owned(&self, e: &Expression, place: &str) -> Result<(), Error> {
        match e.owner {
            Owner::None => Ok(()),
            Owner::Model(id) if id == self.model.id => Ok(()),
            _ => Err(invalid(format!("{place} uses a variable of another model"))),
        }
    }

    fn expr(&self, e: &Expression, place: &str) -> Result<String, Error> {
        self.owned(e, place)?;
        let mut terms: Vec<(bool, String)> = Vec::new();
        let mut push = |coef: f64, body: String| -> Result<(), Error> {
            if coef == 0.0 {
                return Ok(());
            }
            let magnitude = num(coef.abs(), place)?;
            terms.push((coef < 0.0, if coef.abs() == 1.0 { body } else { format!("{magnitude} * {body}") }));
            Ok(())
        };
        for (&j, &a) in &e.linear {
            push(a, self.refs[j].0.clone())?;
        }
        for (a, t) in &e.nonlinear {
            push(*a, self.tree(t, place)?)?;
        }
        let mut out = String::new();
        if e.constant != 0.0 || terms.is_empty() {
            out = num(e.constant, place)?;
        }
        for (negative, body) in terms {
            out = match (out.is_empty(), negative) {
                (true, false) => body,
                (true, true) => format!("-{body}"),
                (false, false) => format!("{out} + {body}"),
                (false, true) => format!("{out} - {body}"),
            };
        }
        Ok(out)
    }

    fn tree(&self, t: &Tree, place: &str) -> Result<String, Error> {
        Ok(match t {
            Tree::Mul(a, b) => format!("{} * {}", self.operand(a, place)?, self.operand(b, place)?),
            Tree::Div(a, b) => format!("{} / {}", self.operand(a, place)?, self.operand(b, place)?),
            Tree::PowI(a, n) if *n >= 0 => format!("{}^{n}", self.operand(a, place)?),
            Tree::PowI(a, n) => format!("{}^({n})", self.operand(a, place)?),
            Tree::Pow(a, b) => format!("{}^{}", self.operand(a, place)?, self.operand(b, place)?),
            Tree::Func(f, a) => format!("{}({})", f.name(), self.expr(a, place)?),
        })
    }

    /// An operand of `*`, `/`, or `^`: in parentheses unless it is a lone
    /// variable, a non-negative constant, or a function call.
    fn operand(&self, e: &Expression, place: &str) -> Result<String, Error> {
        let mut linear = e.linear.values().filter(|&&a| a != 0.0);
        let atom = match (linear.next(), linear.next(), e.nonlinear.as_slice()) {
            (None, _, []) => e.constant >= 0.0,
            (Some(&a), None, []) => a == 1.0 && e.constant == 0.0,
            (None, _, [(a, t)]) => *a == 1.0 && e.constant == 0.0 && matches!(**t, Tree::Func(..)),
            _ => false,
        };
        let s = self.expr(e, place)?;
        Ok(if atom { s } else { format!("({s})") })
    }

    fn list(&self, items: &[Expression], place: &str) -> Result<String, Error> {
        let items = items.iter().map(|e| self.expr(e, place)).collect::<Result<Vec<_>, _>>()?;
        Ok(format!("[{}]", items.join(", ")))
    }

    fn var_ref(&self, v: Variable, place: &str) -> Result<&str, Error> {
        match self.refs.get(v.col) {
            Some((r, _)) if v.model == self.model.id => Ok(r),
            _ => Err(invalid(format!("{place} uses a variable of another model"))),
        }
    }

    /// The range of an affine expression over the variables' bounds, and
    /// whether it is integer-valued.
    fn range(&self, e: &Expression) -> (f64, f64, bool) {
        let (mut lo, mut hi) = (e.constant, e.constant);
        let mut integral = e.constant.fract() == 0.0;
        for (&j, &a) in &e.linear {
            let (kind, l, u) =
                self.cols.get(j).copied().unwrap_or((VarKind::Continuous, f64::NEG_INFINITY, f64::INFINITY));
            let (x, y) = if a >= 0.0 { (a * l, a * u) } else { (a * u, a * l) };
            lo += if x.is_nan() { 0.0 } else { x };
            hi += if y.is_nan() { 0.0 } else { y };
            integral &= kind != VarKind::Continuous && a.fract() == 0.0;
        }
        (lo, hi, integral && e.nonlinear.is_empty())
    }

    /// A new auxiliary variable equal to `e` (an element per item when
    /// `items` has several, indexed by `set`), with bounds from `e`'s range.
    fn aux_var(&mut self, prefix: &str, items: &[Expression], set: Option<&str>, place: &str) -> Result<String, Error> {
        let name = format!("_{prefix}{}", self.aux);
        self.aux += 1;
        let ranges: Vec<_> = items.iter().map(|e| self.range(e)).collect();
        let lo = ranges.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
        let hi = ranges.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max);
        let integral = ranges.iter().all(|r| r.2);
        let mut v = Map::new();
        v.insert("name".into(), json!(name));
        v.insert("type".into(), json!(if integral { "integer" } else { "continuous" }));
        if let Some(set) = set {
            v.insert("index".into(), json!([set]));
        }
        if lo.is_finite() {
            v.insert("lower".into(), json_num(lo));
        }
        if hi.is_finite() {
            v.insert("upper".into(), json_num(hi));
        }
        self.variables.push(Value::Object(v));
        for (k, e) in items.iter().enumerate() {
            let target = if set.is_some() { format!("{name}[{k}]") } else { name.clone() };
            let expr = format!("{target} == {}", self.expr(e, place)?);
            self.constraints.push(json!({ "expr": expr }));
        }
        Ok(name)
    }

    fn new_set(&mut self, prefix: &str, len: usize) -> String {
        let name = format!("_{prefix}{}", self.aux);
        self.aux += 1;
        self.sets.insert(name.clone(), json!({ "from": 0, "to": len as i64 - 1 }));
        name
    }

    fn interval_name(&self, iv: IntervalVar, place: &str) -> Result<String, Error> {
        if iv.model != self.model.id {
            return Err(invalid(format!("{place} uses an interval of another model")));
        }
        Ok(format!("_iv{}", iv.index))
    }

    fn intervals(&mut self) -> Result<(), Error> {
        for k in 0..self.model.intervals.len() {
            let spec = self.model.intervals[k].clone();
            let place = format!("interval _iv{k}");
            let mut field = |e: &Expression, what: &str| -> Result<Value, Error> {
                self.owned(e, &place)?;
                if let Some(c) = e.as_constant() {
                    return Ok(json_num(c));
                }
                if e.constant == 0.0 && e.nonlinear.is_empty() && e.linear.len() == 1 {
                    let (&j, &a) = e.linear.iter().next().unwrap();
                    if a == 1.0 && self.refs.get(j).is_some_and(|r| r.1) {
                        return Ok(json!(self.refs[j].0));
                    }
                }
                Ok(json!(self.aux_var(what, std::slice::from_ref(e), None, &place)?))
            };
            let start = field(&spec.start, "ivstart")?;
            let duration = field(&spec.duration, "ivduration")?;
            let mut iv = Map::new();
            iv.insert("name".into(), json!(format!("_iv{k}")));
            iv.insert("start".into(), start);
            iv.insert("duration".into(), duration);
            if let Some(p) = spec.presence {
                let r = self.var_ref(p, &place)?.to_string();
                let presence = if self.refs[p.col].1 {
                    r
                } else {
                    let name = format!("_ivpresence{}", self.aux);
                    self.aux += 1;
                    self.variables.push(json!({ "name": name, "type": "binary" }));
                    self.constraints.push(json!({ "expr": format!("{name} == {r}") }));
                    name
                };
                iv.insert("presence".into(), json!(presence));
            }
            self.intervals.push(Value::Object(iv));
        }
        Ok(())
    }

    fn constraint(&mut self, c: &Constraint, label: &str) -> Result<(), Error> {
        let place = format!("constraint {label}");
        // The constant moves to the bounds: `x + 3 <= 5` is written `x <= 2`.
        let mut body = c.expr.clone();
        let constant = std::mem::take(&mut body.constant);
        let e = self.expr(&body, &place)?;
        let (lower, upper) = (c.lower - constant, c.upper - constant);
        let mut relations = Vec::new();
        if c.lower == c.upper {
            relations.push(format!("{e} == {}", num(upper, &place)?));
        } else {
            if c.lower > f64::NEG_INFINITY {
                relations.push(format!("{e} >= {}", num(lower, &place)?));
            }
            if c.upper < f64::INFINITY {
                relations.push(format!("{e} <= {}", num(upper, &place)?));
            }
        }
        let mut literals = Vec::new();
        for lit in &c.literals {
            let r = self.var_ref(lit.var, &place)?;
            literals.push(match lit.kind {
                LiteralKind::True => r.to_string(),
                LiteralKind::False => format!("not {r}"),
                LiteralKind::Is(v) => format!("{r} == {}", num(v as f64, &place)?),
                LiteralKind::IsNot(v) => format!("{r} != {}", num(v as f64, &place)?),
            });
        }
        let several = relations.len() > 1;
        for (k, relation) in relations.into_iter().enumerate() {
            let mut out = Map::new();
            match &c.name {
                Some(name) if is_identifier(name) => {
                    let name = if several { format!("{name}_{}", ["lower", "upper"][k]) } else { name.clone() };
                    out.insert("name".into(), json!(name));
                }
                Some(name) => {
                    out.insert("description".into(), json!(name));
                }
                None => {}
            }
            out.insert("expr".into(), json!(relation));
            if !literals.is_empty() {
                out.insert("enforce_if".into(), json!(literals.join(" and ")));
            }
            self.constraints.push(Value::Object(out));
        }
        Ok(())
    }

    fn item(&mut self, item: &Item, label: &str) -> Result<(), Error> {
        let place = format!("constraint {label}");
        let empty = |what: &str| invalid(format!("{place}: {what} needs at least one item"));
        let value = match item {
            Item::Expr(c) => return self.constraint(c, label),
            Item::AllDifferent(items) if items.len() < 2 => return Ok(()),
            Item::AllDifferent(items) => json!({ "all_different": self.list(items, &place)? }),
            Item::Table { vars, tuples, forbidden } => {
                if vars.is_empty() {
                    return Err(empty("table"));
                }
                let refs = vars.iter().map(|&v| self.var_ref(v, &place)).collect::<Result<Vec<_>, _>>()?;
                let mut t = json!({ "vars": format!("[{}]", refs.join(", ")), "tuples": tuples });
                if *forbidden {
                    t["forbidden"] = json!(true);
                }
                json!({ "table": t })
            }
            Item::Element { index, items, value } => {
                if items.is_empty() {
                    return Err(empty("element"));
                }
                let set = self.new_set("element", items.len());
                let constants: Option<Vec<f64>> = items.iter().map(Expression::as_constant).collect();
                let array = match constants {
                    Some(values) => {
                        let name = format!("_elementvalues{}", self.aux);
                        self.aux += 1;
                        let values: Map<String, Value> =
                            values.iter().enumerate().map(|(k, &v)| (k.to_string(), json_num(v))).collect();
                        self.parameters.insert(name.clone(), json!({ "index": [set], "values": values }));
                        format!("[{name}[k] | k in {set}]")
                    }
                    None => {
                        let name = self.aux_var("elementitems", items, Some(&set), &place)?;
                        format!("[{name}[k] | k in {set}]")
                    }
                };
                let (index, value) = (self.expr(index, &place)?, self.expr(value, &place)?);
                json!({ "element": { "index": index, "array": array, "value": value } })
            }
            Item::MinMax { value, items, maximize } => {
                if items.is_empty() {
                    return Err(empty(if *maximize { "max_of" } else { "min_of" }));
                }
                let body = json!({ "value": self.expr(value, &place)?, "of": self.list(items, &place)? });
                let mut c = Map::new();
                c.insert(if *maximize { "max" } else { "min" }.into(), body);
                Value::Object(c)
            }
            Item::Abs { value, of } => {
                json!({ "abs": { "value": self.expr(value, &place)?, "of": self.expr(of, &place)? } })
            }
            Item::NoOverlap(ivs) if ivs.is_empty() => return Ok(()),
            Item::NoOverlap(ivs) => {
                let names = ivs.iter().map(|&iv| self.interval_name(iv, &place)).collect::<Result<Vec<_>, _>>()?;
                json!({ "no_overlap": format!("[{}]", names.join(", ")) })
            }
            Item::Cumulative { intervals, .. } if intervals.is_empty() => return Ok(()),
            Item::Cumulative { intervals, demands, capacity } => {
                let names =
                    intervals.iter().map(|&iv| self.interval_name(iv, &place)).collect::<Result<Vec<_>, _>>()?;
                json!({ "cumulative": {
                    "intervals": format!("[{}]", names.join(", ")),
                    "demands": self.list(demands, &place)?,
                    "capacity": self.expr(capacity, &place)?,
                } })
            }
        };
        self.constraints.push(value);
        Ok(())
    }
}

pub(crate) fn to_json(model: &Model) -> Result<String, Error> {
    check_names(model).map_err(invalid)?;
    let mut w = Writer {
        model,
        refs: Vec::with_capacity(model.num_cols),
        cols: Vec::with_capacity(model.num_cols),
        sets: Map::new(),
        parameters: Map::new(),
        variables: Vec::new(),
        intervals: Vec::new(),
        constraints: Vec::new(),
        aux: 0,
    };
    for f in &model.families {
        let (name, s) = (f.name(), &f.spec);
        let mut v = Map::new();
        v.insert("name".into(), json!(name));
        let kind = match s.kind {
            VarKind::Continuous => "continuous",
            VarKind::Integer => "integer",
            VarKind::Binary => "binary",
        };
        v.insert("type".into(), json!(kind));
        if f.indexed {
            let set = format!("_{name}_index");
            w.sets.insert(set.clone(), json!({ "from": 0, "to": f.len as i64 - 1 }));
            v.insert("index".into(), json!([set]));
        }
        let (lo, hi) = match (s.kind, &s.domain) {
            (VarKind::Binary, _) => (0.0, 1.0),
            (VarKind::Continuous, Some(_)) => {
                return Err(invalid(format!("variable {name}: a domain needs an integer variable")));
            }
            (_, Some(values)) if values.is_empty() => {
                return Err(invalid(format!("variable {name}: the domain is empty")));
            }
            (_, Some(values)) => {
                let ranges: Vec<[i64; 2]> = to_ranges(values).into_iter().map(|(a, b)| [a, b]).collect();
                v.insert("domain".into(), json!(ranges));
                (values[0] as f64, values[values.len() - 1] as f64)
            }
            _ => {
                let (lo, hi) = (s.lower.unwrap_or(f64::NEG_INFINITY), s.upper.unwrap_or(f64::INFINITY));
                if lo.is_nan() || hi.is_nan() {
                    return Err(invalid(format!("variable {name}: a bound isn't a number")));
                }
                if lo > f64::NEG_INFINITY {
                    v.insert("lower".into(), json_num(lo));
                }
                if hi < f64::INFINITY {
                    v.insert("upper".into(), json_num(hi));
                }
                (lo, hi)
            }
        };
        if let Some(start) = s.start {
            if !start.is_finite() {
                return Err(invalid(format!("variable {name}: the start isn't a finite number")));
            }
            v.insert("start".into(), json_num(start));
        }
        w.variables.push(Value::Object(v));
        for k in 0..f.len {
            w.refs.push((if f.indexed { format!("{name}[{k}]") } else { name.clone() }, !f.indexed));
            w.cols.push((s.kind, lo, hi));
        }
    }
    w.intervals()?;
    for (i, item) in model.items.iter().enumerate() {
        w.item(item, &format!("#{}", i + 1))?;
    }
    let mut solve = Map::new();
    let method = match model.sense {
        crate::model::Sense::Satisfy => "satisfy",
        crate::model::Sense::Minimize => "minimize",
        crate::model::Sense::Maximize => "maximize",
    };
    solve.insert("method".into(), json!(method));
    if model.sense != crate::model::Sense::Satisfy {
        solve.insert("objective".into(), json!(w.expr(&model.objective, "the objective")?));
    }
    if model.all_solutions {
        solve.insert("solutions".into(), json!("all"));
    }
    let mut out = Map::new();
    out.insert("version".into(), json!("1.2"));
    if !w.sets.is_empty() {
        out.insert("sets".into(), Value::Object(w.sets));
    }
    if !w.parameters.is_empty() {
        out.insert("parameters".into(), Value::Object(w.parameters));
    }
    out.insert("variables".into(), Value::Array(w.variables));
    if !w.intervals.is_empty() {
        out.insert("intervals".into(), Value::Array(w.intervals));
    }
    out.insert("constraints".into(), Value::Array(w.constraints));
    out.insert("solve".into(), Value::Object(solve));
    Ok(render(&out))
}
