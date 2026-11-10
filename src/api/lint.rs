//! What a problem file may say but probably doesn't mean: warnings.
//!
//! Read off the file's expressions as written, without expanding them:
//! - an index of a `sum`, a list, or a `forall` that hides an index around
//!   it, a parameter, or a variable of the same name;
//! - two constraints with one name;
//! - parameters that nothing uses, and variables that appear in no
//!   constraint and not in the objective.
//!
//! Strings that don't parse are skipped: reading the problem reports those.

use crate::expr::{self, Bindings, Cond, Expr, List};
use crate::problem::{Bound, ConstraintDef, ConstraintKind, OneOrMany, Problem};
use std::collections::{BTreeMap, HashSet};

struct Lint<'a> {
    problem: &'a Problem,
    /// The names that expressions refer to (parameters, variables, intervals).
    used: HashSet<String>,
    warnings: Vec<String>,
    /// Where the expression being read is: `constraint capacity`.
    place: String,
}

impl Lint<'_> {
    fn warn(&mut self, warning: String) {
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }

    fn expr(&mut self, e: &Expr, scope: &mut Vec<String>) {
        match e {
            Expr::Num(_) | Expr::Str(_) => {}
            Expr::Ref(name, args) => {
                if args.is_some() || !scope.contains(name) {
                    self.used.insert(name.clone());
                }
                for a in args.iter().flatten() {
                    self.expr(a, scope);
                }
            }
            Expr::Neg(a) | Expr::Pow(a, _) | Expr::Call(_, a) => self.expr(a, scope),
            Expr::Add(terms) => terms.iter().for_each(|(_, t)| self.expr(t, scope)),
            Expr::Bin(_, a, b) | Expr::PowE(a, b) => {
                self.expr(a, scope);
                self.expr(b, scope);
            }
            Expr::Sum(b, term) => self.bound(b, scope, "a sum", |lint, scope| lint.expr(term, scope)),
        }
    }

    fn cond(&mut self, c: &Cond, scope: &mut Vec<String>) {
        match c {
            Cond::Cmp(_, a, b) => {
                self.expr(a, scope);
                self.expr(b, scope);
            }
            Cond::And(a, b) | Cond::Or(a, b) => {
                self.cond(a, scope);
                self.cond(b, scope);
            }
            Cond::Not(a) => self.cond(a, scope),
        }
    }

    fn list(&mut self, list: &List, scope: &mut Vec<String>) {
        match list {
            List::Explicit(items) => items.iter().for_each(|e| self.expr(e, scope)),
            List::Comprehension(body, b) => self.bound(b, scope, "a list", |lint, scope| lint.expr(body, scope)),
        }
    }

    /// Reads `inside` with the indexes of `b` (those of `what`) bound, noting
    /// the ones that hide another name.
    fn bound(
        &mut self,
        b: &Bindings,
        scope: &mut Vec<String>,
        what: &str,
        inside: impl FnOnce(&mut Self, &mut Vec<String>),
    ) {
        let outer = scope.len();
        for (index, _) in &b.vars {
            let hidden = if scope.contains(index) {
                Some("an index of the same name around it")
            } else if self.problem.parameters.contains_key(index) {
                Some("the parameter of that name")
            } else if self.problem.variables.iter().any(|v| v.name == *index) {
                Some("the variable of that name")
            } else {
                None
            };
            if let Some(hidden) = hidden {
                self.warn(format!("{}: the index {index} of {what} hides {hidden}", self.place));
            }
            scope.push(index.clone());
        }
        if let Some(c) = &b.cond {
            self.cond(c, scope);
        }
        inside(self, scope);
        scope.truncate(outer);
    }

    /// The expressions of one constraint, inside its `forall`.
    fn constraint(&mut self, c: &ConstraintDef, scope: &mut Vec<String>) {
        let Ok(kind) = c.kind() else { return };
        let mut expression = |lint: &mut Self, text: &str| {
            if let Ok(e) = expr::parse_expr(text) {
                lint.expr(&e, scope);
            }
        };
        let mut lists: Vec<&str> = Vec::new();
        match kind {
            ConstraintKind::Expr(text, enforce_if) => {
                if let Ok((_, lhs, rhs)) = expr::parse_relation(text) {
                    self.expr(&lhs, scope);
                    self.expr(&rhs, scope);
                }
                for literal in enforce_if.and_then(|text| expr::parse_literals(text).ok()).unwrap_or_default() {
                    self.expr(&literal.var, scope);
                    if let Some((_, e)) = &literal.compare {
                        self.expr(e, scope);
                    }
                }
                return;
            }
            ConstraintKind::AllDifferent(text) | ConstraintKind::NoOverlap(text) => lists.push(text),
            ConstraintKind::Table(t) => lists.push(&t.vars),
            ConstraintKind::Element(e) => {
                match &e.index {
                    OneOrMany::One(text) => expression(self, text),
                    OneOrMany::Many(texts) => texts.iter().for_each(|text| expression(self, text)),
                }
                expression(self, &e.value);
                lists.push(&e.array);
            }
            ConstraintKind::Min(a) | ConstraintKind::Max(a) => {
                expression(self, &a.value);
                lists.push(&a.of);
            }
            ConstraintKind::Abs(a) => {
                expression(self, &a.value);
                expression(self, &a.of);
            }
            ConstraintKind::Cumulative(d) => {
                expression(self, &d.capacity);
                lists.extend([d.intervals.as_str(), d.demands.as_str()]);
            }
        }
        for text in lists {
            if let Ok(list) = expr::parse_list(text) {
                self.list(&list, scope);
            }
        }
    }
}

/// The warnings for `problem`, in the order of the file.
pub(crate) fn lint(problem: &Problem) -> Vec<String> {
    let mut lint = Lint { problem, used: HashSet::new(), warnings: Vec::new(), place: String::new() };

    // Names used outside expressions: bounds, starts, and interval fields.
    for v in &problem.variables {
        for bound in [&v.lower, &v.upper, &v.start].into_iter().flatten() {
            if let Bound::Param(name) = bound {
                lint.used.insert(name.clone());
            }
        }
    }
    for iv in &problem.intervals {
        for bound in [&iv.start, &iv.duration] {
            if let Bound::Param(name) = bound {
                lint.used.insert(name.clone());
            }
        }
        lint.used.extend(iv.presence.clone());
    }

    let mut named: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for c in &problem.constraints {
        if let Some(name) = &c.name {
            named.entry(name).or_default().push(format!("constraints[{}]", c.position));
        }
        lint.place = format!("constraint {}", c.label());
        let mut scope = Vec::new();
        match c.forall.as_deref().map(expr::parse_bindings) {
            Some(Ok(forall)) => lint.bound(&forall, &mut scope, "the forall", |lint, scope| lint.constraint(c, scope)),
            Some(Err(_)) => {}
            None => lint.constraint(c, &mut scope),
        }
    }
    if let Some(Ok(objective)) = problem.solve.objective.as_deref().map(expr::parse_expr) {
        lint.place = "the objective".into();
        lint.expr(&objective, &mut Vec::new());
    }

    for (name, places) in named.into_iter().filter(|(_, places)| places.len() > 1) {
        lint.warn(format!("{} constraints are named {name}: {}", places.len(), places.join(", ")));
    }
    for name in problem.parameters.keys().filter(|name| !lint.used.contains(*name)) {
        lint.warnings.push(format!("parameter {name} is never used"));
    }
    for v in problem.variables.iter().filter(|v| !lint.used.contains(&v.name)) {
        lint.warnings.push(format!("variable {} appears in no constraint and not in the objective", v.name));
    }
    lint.warnings
}
