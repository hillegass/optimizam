//! What a problem file may say but probably doesn't mean: warnings.
//!
//! Read off the file's expressions as written, without expanding them (the
//! conversion hands each one over when it has parsed it):
//! - an index of a `sum`, a list, or a `forall` that hides an index around
//!   it, a parameter, or a variable of the same name;
//! - two constraints with one name;
//! - parameters that nothing uses, and variables that appear in no
//!   constraint and not in the objective.

use crate::expr::{Bindings, Builtin, Cond, Expr, List};
use crate::problem::{Bound, ConstraintDef, Problem};
use std::collections::{BTreeMap, HashSet};

pub(crate) struct Lint<'a> {
    problem: &'a Problem,
    /// The names that expressions refer to (parameters, variables, intervals).
    used: HashSet<String>,
    warnings: Vec<String>,
    /// Where the expression being read is: `constraint capacity`.
    place: String,
    /// The constraints' names, each with the places that have it.
    named: BTreeMap<&'a str, Vec<String>>,
}

impl<'a> Lint<'a> {
    pub fn new(problem: &'a Problem) -> Lint<'a> {
        let mut used = HashSet::new();
        // Names used outside expressions: bounds, starts, and interval fields.
        for v in &problem.variables {
            for bound in [&v.lower, &v.upper, &v.start].into_iter().flatten() {
                if let Bound::Param(name) = bound {
                    used.insert(name.clone());
                }
            }
        }
        for iv in &problem.intervals {
            for bound in [&iv.start, &iv.duration] {
                if let Bound::Param(name) = bound {
                    used.insert(name.clone());
                }
            }
            used.extend(iv.presence.clone());
        }
        Lint { problem, used, warnings: Vec::new(), place: String::new(), named: BTreeMap::new() }
    }

    /// Reads one constraint: its `forall` and, inside it, its expressions
    /// and lists.
    pub fn constraint(&mut self, c: &'a ConstraintDef, forall: &Bindings, exprs: &[&Expr], lists: &[&List]) {
        if let Some(name) = &c.name {
            self.named.entry(name).or_default().push(format!("constraints[{}]", c.position));
        }
        self.place = format!("constraint {}", c.label());
        self.bound(forall, &mut Vec::new(), "the forall", |lint, scope| {
            exprs.iter().for_each(|e| lint.expr(e, scope));
            lists.iter().for_each(|l| lint.list(l, scope));
        });
    }

    pub fn objective(&mut self, objective: &Expr) {
        self.place = "the objective".into();
        self.expr(objective, &mut Vec::new());
    }

    /// The warnings, in the order of the file, then what is never used.
    pub fn finish(mut self) -> Vec<String> {
        for (name, places) in std::mem::take(&mut self.named).into_iter().filter(|(_, places)| places.len() > 1) {
            self.warn(format!("{} constraints are named {name}: {}", places.len(), places.join(", ")));
        }
        let problem = self.problem;
        for name in problem.parameters.keys().filter(|name| !self.used.contains(*name)) {
            self.warnings.push(format!("parameter {name} is never used"));
        }
        for v in problem.variables.iter().filter(|v| !self.used.contains(&v.name)) {
            self.warnings.push(format!("variable {} appears in no constraint and not in the objective", v.name));
        }
        self.warnings
    }

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
            Expr::Apply(_, args) => args.iter().for_each(|a| self.expr(a, scope)),
            Expr::Over(f, b, term) => {
                let what = if *f == Builtin::Min { "a min" } else { "a max" };
                self.bound(b, scope, what, |lint, scope| lint.expr(term, scope))
            }
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
}
