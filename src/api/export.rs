//! Writes a builder model in the JSON problem format, flat: one variable per
//! [`Model::add_var`], one indexed variable per [`Model::add_vars`] or
//! [`Model::add_family`] (over its sets), one constraint per constraint
//! added. The version is 1.2, or 1.3 when the file uses string literals
//! (`x['north', 3]`, `machine == 'mill'`) or `abs`, `min`, and `max` terms.
//!
//! What the format can't say directly gets auxiliary variables named with a
//! leading `_`: intervals whose start, duration, or presence isn't a scalar
//! variable or a constant, and `element` items that aren't all constants.

use super::Error;
use super::ErrorKind;
use super::builder::{IntervalVar, Item, Model, VarKind};
use super::expression::{Constraint, Expression, LiteralKind, Mode, Owner, Tree, Variable};
use super::family::{Element, Set};
use super::lower::{check_family, check_names, col_bounds, col_name, is_identifier};
use serde_json::{Map, Value, json};

struct Writer<'m> {
    model: &'m Model,
    /// Each column's name in expressions (`x` or `x[3]`), and whether it is a
    /// scalar variable.
    refs: Vec<(String, bool)>,
    /// Whether a column's key has a string element (its reference needs a
    /// string literal).
    string_keys: Vec<bool>,
    /// Whether the file uses what format 1.3 added: string literals, or
    /// `abs`, `min`, and `max` in expressions.
    latest: std::cell::Cell<bool>,
    /// Each column's set, for symbolic variables.
    symbolic: Vec<Option<Set>>,
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

/// A set element as a JSON value (in tables and sets).
fn element_json(e: &Element) -> Value {
    match e {
        Element::Int(i) => json!(i),
        Element::Str(s) => json!(s.as_ref()),
    }
}

/// A set element in an expression: a number, or a string literal (format
/// 1.3; a quote inside is doubled).
fn element_literal(e: &Element) -> String {
    match e {
        Element::Int(i) => i.to_string(),
        Element::Str(s) => format!("'{}'", s.replace('\'', "''")),
    }
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
            if a != 0.0 {
                push(a, self.reference(j)?)?;
            }
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
            Tree::Abs(a) => {
                self.latest.set(true);
                format!("abs({})", self.expr(a, place)?)
            }
            Tree::Min(items) | Tree::Max(items) => {
                self.latest.set(true);
                let items = items.iter().map(|e| self.expr(e, place)).collect::<Result<Vec<_>, _>>()?;
                format!("{}({})", if matches!(t, Tree::Min(_)) { "min" } else { "max" }, items.join(", "))
            }
        })
    }

    /// An operand of `*`, `/`, or `^`: in parentheses unless it is a lone
    /// variable, a non-negative constant, or a function call.
    fn operand(&self, e: &Expression, place: &str) -> Result<String, Error> {
        let mut linear = e.linear.values().filter(|&&a| a != 0.0);
        let atom = match (linear.next(), linear.next(), e.nonlinear.as_slice()) {
            (None, _, []) => e.constant >= 0.0,
            (Some(&a), None, []) => a == 1.0 && e.constant == 0.0,
            (None, _, [(a, t)]) => *a == 1.0 && e.constant == 0.0 && (matches!(**t, Tree::Func(..)) || t.piecewise()),
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
            Some(_) if v.model == self.model.id => {
                if self.string_keys[v.col] {
                    self.latest.set(true);
                }
                Ok(&self.refs[v.col].0)
            }
            _ => Err(invalid(format!("{place} uses a variable of another model"))),
        }
    }

    /// A column's name in an expression.
    fn reference(&self, j: usize) -> Result<String, Error> {
        if self.string_keys[j] {
            self.latest.set(true);
        }
        Ok(self.refs[j].0.clone())
    }

    /// An element compared with a column's variable in a literal.
    fn element(&self, col: usize, e: &Element, place: &str) -> Result<String, Error> {
        match e {
            Element::Int(i) => num(*i as f64, place),
            Element::Str(_) if self.symbolic[col].is_some() => {
                self.latest.set(true);
                Ok(element_literal(e))
            }
            Element::Str(s) => {
                Err(invalid(format!("{place}: {s:?} compared with the numeric variable {}", col_name(self.model, col))))
            }
        }
    }

    /// Adds `set` to the sets section (once), and returns its name.
    fn add_set(&mut self, set: &Set) -> Result<String, Error> {
        let name = set.name();
        if !is_identifier(name) {
            return Err(invalid(format!("set name {name:?} isn't an identifier")));
        }
        let elements = set.elements();
        let ints: Option<Vec<i64>> =
            elements.iter().map(|e| if let Element::Int(i) = e { Some(*i) } else { None }).collect();
        let value = match ints {
            Some(v) if !v.is_empty() && v.windows(2).all(|w| w[1] == w[0] + 1) => {
                json!({ "from": v[0], "to": v[v.len() - 1] })
            }
            _ => Value::Array(
                elements
                    .iter()
                    .map(|e| match e {
                        Element::Int(i) => json!(i),
                        Element::Str(s) => json!(s.as_ref()),
                    })
                    .collect(),
            ),
        };
        match self.sets.get(name) {
            Some(v) if *v != value => Err(invalid(format!("two different sets are named {name}"))),
            Some(_) => Ok(name.to_string()),
            None => {
                self.sets.insert(name.to_string(), value);
                Ok(name.to_string())
            }
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

    /// Declares a family's variable: its sets, kind, domain, bounds, and
    /// starts (per element as parameters when they differ), with
    /// `set_bounds` changes as constraints.
    fn family(&mut self, f: &super::builder::Family) -> Result<(), Error> {
        check_family(f).map_err(invalid)?;
        let (name, s) = (f.name(), f.spec(0));
        let mut v = Map::new();
        v.insert("name".into(), json!(name));
        let kind = match s.kind {
            VarKind::Continuous => "continuous",
            VarKind::Integer => "integer",
            VarKind::Binary => "binary",
        };
        v.insert("type".into(), json!(kind));
        let sets = f.sets();
        let mut index = Vec::new();
        for set in &sets {
            index.push(self.add_set(set)?);
        }
        if f.indexed() {
            v.insert("index".into(), json!(index));
        }
        // Each element's bounds and start from its spec (`set_bounds`
        // changes are written as constraints below).
        let spec_bounds = |k: usize| -> (f64, f64) {
            let s = f.spec(k);
            (s.lower.unwrap_or(f64::NEG_INFINITY), s.upper.unwrap_or(f64::INFINITY))
        };
        if let Some(set) = &s.values {
            v.insert("domain".into(), json!(self.add_set(set)?));
        } else if s.kind != VarKind::Binary {
            if let Some(ranges) = &s.domain {
                let ranges: Vec<[i64; 2]> = ranges.iter().map(|&(a, b)| [a, b]).collect();
                v.insert("domain".into(), json!(ranges));
            } else {
                for (side, field) in [(0, "lower"), (1, "upper")] {
                    let values: Vec<f64> =
                        (0..f.len).map(|k| if side == 0 { spec_bounds(k).0 } else { spec_bounds(k).1 }).collect();
                    if let Some(b) = self.per_element(f, &format!("_{name}_{field}"), &values, field)? {
                        v.insert(field.into(), b);
                    }
                }
            }
        }
        let starts: Vec<Option<f64>> = (0..f.len).map(|k| f.spec(k).start).collect();
        if starts.iter().any(Option::is_some) {
            // A start missing on some elements is written as 0 (a start is a hint).
            let values: Vec<f64> = starts.iter().map(|s| s.unwrap_or(0.0)).collect();
            if let Some(b) = self.per_element(f, &format!("_{name}_start"), &values, "start")? {
                v.insert("start".into(), b);
            }
        }
        self.variables.push(Value::Object(v));
        for k in 0..f.len {
            let key = f.key(k);
            let subscript: Vec<String> = key.iter().map(element_literal).collect();
            let r = if f.indexed() { format!("{name}[{}]", subscript.join(", ")) } else { name.clone() };
            self.refs.push((r, !f.indexed()));
            self.string_keys.push(key.iter().any(|e| matches!(e, Element::Str(_))));
            self.symbolic.push(s.values.clone());
            let (lo, hi, _) = col_bounds(self.model, f, k).map_err(invalid)?;
            self.cols.push((s.kind, lo, hi));
        }
        // Bounds changed with `set_bounds`.
        for k in 0..f.len {
            let col = f.first + k;
            let Some(&(lo, hi)) = self.model.bounds.get(&col) else { continue };
            let r = self.reference(col)?;
            let place = format!("variable {}", col_name(self.model, col));
            if lo == hi {
                self.constraints.push(json!({ "expr": format!("{r} == {}", num(lo, &place)?) }));
                continue;
            }
            if lo > f64::NEG_INFINITY {
                self.constraints.push(json!({ "expr": format!("{r} >= {}", num(lo, &place)?) }));
            }
            if hi < f64::INFINITY {
                self.constraints.push(json!({ "expr": format!("{r} <= {}", num(hi, &place)?) }));
            }
        }
        Ok(())
    }

    /// A per-element field: a number when every element has the same value
    /// (None when that is infinite: no bound), or else a parameter indexed
    /// like the variable.
    fn per_element(
        &mut self,
        f: &super::builder::Family,
        param: &str,
        values: &[f64],
        what: &str,
    ) -> Result<Option<Value>, Error> {
        let name = f.name();
        if values.iter().any(|x| x.is_nan()) {
            return Err(invalid(format!("variable {name}: a {what} isn't a number")));
        }
        if values.windows(2).all(|w| w[0] == w[1]) {
            return Ok(values.first().filter(|x| x.is_finite()).map(|&x| json_num(x)));
        }
        if values.iter().any(|x| !x.is_finite()) {
            return Err(invalid(format!(
                "variable {name}: a {what} that is infinite for some elements and not others can't be written"
            )));
        }
        // Nested one object level per set, keyed by element.
        let mut table = Map::new();
        for (k, &x) in values.iter().enumerate() {
            let key = f.key(k);
            let mut level = &mut table;
            for (i, e) in key.iter().enumerate() {
                let slot = e.to_string();
                if i + 1 == key.len() {
                    level.insert(slot, json_num(x));
                } else {
                    level = level.entry(slot).or_insert_with(|| json!({})).as_object_mut().unwrap();
                }
            }
        }
        let index: Vec<String> = f.sets().iter().map(|s| s.name().to_string()).collect();
        self.parameters.insert(param.to_string(), json!({ "index": index, "values": table }));
        Ok(Some(json!(param)))
    }

    /// An `element` whose items are keyed by elements (several indexes, or
    /// symbolic ones): one set per index of the keys it uses, the items as a
    /// parameter (constants) or an auxiliary variable over them, and a 0/1
    /// parameter in the comprehension's condition when the keys don't cover
    /// the whole product.
    fn keyed_element(
        &mut self,
        index: &[Expression],
        items: &[(Vec<Element>, Expression)],
        value: &Expression,
        place: &str,
    ) -> Result<(), Error> {
        let id = self.aux;
        self.aux += 1;
        let dims = index.len();
        let mut keys: Vec<Vec<Element>> = vec![Vec::new(); dims];
        let mut seen: Vec<std::collections::HashSet<&Element>> = vec![Default::default(); dims];
        for (key, _) in items {
            if key.len() != dims {
                return Err(invalid(format!("{place}: a key has {} elements for {dims} indexes", key.len())));
            }
            for (d, e) in key.iter().enumerate() {
                if seen[d].insert(e) {
                    keys[d].push(e.clone());
                }
            }
        }
        // A symbolic index ranges over its variable's own set (the format
        // requires it); an integer one over the keys used.
        let mut sets = Vec::with_capacity(dims);
        for (d, k) in keys.iter_mut().enumerate() {
            let mut it = index[d].linear.iter().filter(|c| *c.1 != 0.0);
            let symbolic = match (it.next(), it.next()) {
                (Some((&j, _)), None) => self.symbolic.get(j).cloned().flatten(),
                _ => None,
            };
            match symbolic {
                Some(set) => {
                    *k = set.elements().to_vec();
                    sets.push(self.add_set(&set)?);
                }
                None => sets.push(self.add_set(&Set::new(format!("_element{id}_{d}"), k.clone()))?),
            }
        }
        let binds: Vec<String> = (0..dims).map(|d| format!("k{d}")).collect();
        let full = keys.iter().map(Vec::len).product::<usize>() == items.len();
        let table = |values: &mut dyn Iterator<Item = (&Vec<Element>, Value)>| -> Map<String, Value> {
            let mut out = Map::new();
            for (key, v) in values {
                let mut level = &mut out;
                for (i, e) in key.iter().enumerate() {
                    if i + 1 == key.len() {
                        level.insert(e.to_string(), v.clone());
                    } else {
                        level = level.entry(e.to_string()).or_insert_with(|| json!({})).as_object_mut().unwrap();
                    }
                }
            }
            out
        };
        let constants: Option<Vec<f64>> = items.iter().map(|(_, e)| e.as_constant()).collect();
        let name = match constants {
            Some(values) => {
                let name = format!("_elementvalues{id}");
                let values = table(&mut items.iter().zip(values).map(|((k, _), v)| (k, json_num(v))));
                self.parameters.insert(name.clone(), json!({ "index": sets, "values": values }));
                name
            }
            None => {
                let name = format!("_elementitems{id}");
                let ranges: Vec<_> = items.iter().map(|(_, e)| self.range(e)).collect();
                let lo = ranges.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
                let hi = ranges.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max);
                let integral = ranges.iter().all(|r| r.2);
                let mut v = Map::new();
                v.insert("name".into(), json!(name));
                v.insert("type".into(), json!(if integral { "integer" } else { "continuous" }));
                v.insert("index".into(), json!(sets));
                if lo.is_finite() {
                    v.insert("lower".into(), json_num(lo));
                }
                if hi.is_finite() {
                    v.insert("upper".into(), json_num(hi));
                }
                self.variables.push(Value::Object(v));
                for (key, e) in items {
                    let subscript: Vec<String> = key.iter().map(|e| self.element_text(e)).collect();
                    let expr = format!("{name}[{}] == {}", subscript.join(", "), self.expr(e, place)?);
                    self.constraints.push(json!({ "expr": expr }));
                }
                name
            }
        };
        let bindings: Vec<String> = binds.iter().zip(&sets).map(|(b, s)| format!("{b} in {s}")).collect();
        let condition = if full {
            String::new()
        } else {
            let present = format!("_elementpresent{id}");
            let values = table(&mut items.iter().map(|(k, _)| (k, json!(1))));
            self.parameters.insert(present.clone(), json!({ "index": sets, "values": values, "default": 0 }));
            format!(": {present}[{}] == 1", binds.join(", "))
        };
        let array = format!("[{name}[{}] | {}{condition}]", binds.join(", "), bindings.join(", "));
        let index: Vec<String> = index.iter().map(|e| self.expr(e, place)).collect::<Result<_, _>>()?;
        let index = if dims == 1 { json!(index[0]) } else { json!(index) };
        let value = self.expr(value, place)?;
        self.constraints.push(json!({ "element": { "index": index, "array": array, "value": value } }));
        Ok(())
    }

    /// An element as written in an expression (a string literal sets the
    /// version to 1.3).
    fn element_text(&self, e: &Element) -> String {
        if matches!(e, Element::Str(_)) {
            self.latest.set(true);
        }
        element_literal(e)
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
        let differs = c.mode == Mode::Differs;
        if differs {
            self.latest.set(true);
        }
        if let Some(element) = &c.element {
            // A literal as a constraint: compared with the element itself.
            let col = *body.linear.keys().next().ok_or_else(|| invalid(format!("{place}: no variable to compare")))?;
            let op = if differs { "!=" } else { "==" };
            relations.push(format!("{e} {op} {}", self.element(col, element, &place)?));
        } else if let (true, 0.0, Some((x, y))) = (differs, upper, body.as_difference()) {
            relations.push(format!("{} != {}", self.reference(x)?, self.reference(y)?));
        } else if differs {
            relations.push(format!("{e} != {}", num(upper, &place)?));
        } else if c.lower == c.upper {
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
            let r = self.var_ref(lit.var, &place)?.to_string();
            literals.push(match &lit.kind {
                LiteralKind::True => r,
                LiteralKind::False => format!("not {r}"),
                LiteralKind::Is(e) => format!("{r} == {}", self.element(lit.var.col, e, &place)?),
                LiteralKind::IsNot(e) => format!("{r} != {}", self.element(lit.var.col, e, &place)?),
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
            Item::Removed => return Ok(()),
            Item::Expr(c) => return self.constraint(c, label),
            Item::AllDifferent(items) if items.len() < 2 => return Ok(()),
            Item::AllDifferent(items) => json!({ "all_different": self.list(items, &place)? }),
            Item::Table { vars, tuples, forbidden } => {
                if vars.is_empty() {
                    return Err(empty("table"));
                }
                let refs = vars.iter().map(|&v| self.var_ref(v, &place)).collect::<Result<Vec<_>, _>>()?;
                let tuples: Vec<Vec<Value>> = tuples.iter().map(|t| t.iter().map(element_json).collect()).collect();
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
                let positional = index.len() == 1
                    && items.iter().enumerate().all(|(k, (key, _))| key.len() == 1 && key[0] == Element::from(k));
                if !positional {
                    return self.keyed_element(index, items, value, &place);
                }
                let (index, items): (&Expression, Vec<Expression>) =
                    (&index[0], items.iter().map(|(_, e)| e.clone()).collect());
                let items = &items;
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
    // A model that doesn't lower is reported as it would be when solved.
    super::lower::lower(model).map_err(Error::from_solver)?;
    let mut w = Writer {
        model,
        refs: Vec::with_capacity(model.num_cols),
        string_keys: Vec::with_capacity(model.num_cols),
        latest: std::cell::Cell::new(false),
        symbolic: Vec::with_capacity(model.num_cols),
        cols: Vec::with_capacity(model.num_cols),
        sets: Map::new(),
        parameters: Map::new(),
        variables: Vec::new(),
        intervals: Vec::new(),
        constraints: Vec::new(),
        aux: 0,
    };
    for f in &model.families {
        w.family(f)?;
    }
    w.intervals()?;
    for (i, item) in model.items.iter().enumerate() {
        w.item(item, &format!("constraints[{i}]"))?;
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
    } else if let Some(n) = model.solution_limit {
        solve.insert("solutions".into(), json!(n));
    }
    let mut out = Map::new();
    out.insert("version".into(), json!(if w.latest.get() { "1.3" } else { "1.2" }));
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
