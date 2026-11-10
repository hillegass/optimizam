//! Variables, expressions, constraints, and literals of the builder API.

use super::family::Element;
use crate::expr::Func;
use std::collections::BTreeMap;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Not, Sub, SubAssign};
use std::sync::Arc;

/// A variable of a [`Model`](crate::Model), created by
/// [`Model::add_var`](crate::Model::add_var).
///
/// A small `Copy` handle: use it in expressions (`2 * x + y`), and read its
/// value from a [`Solution`](crate::Solution) with `solution[x]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Variable {
    pub(crate) model: u64,
    pub(crate) col: usize,
}

/// Which model an expression's variables belong to, so that mixing
/// variables of two models is reported instead of silently misread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Owner {
    #[default]
    None,
    Model(u64),
    Mixed,
}

impl Owner {
    pub(crate) fn merge(self, other: Owner) -> Owner {
        match (self, other) {
            (Owner::None, o) | (o, Owner::None) => o,
            (Owner::Model(a), Owner::Model(b)) if a == b => Owner::Model(a),
            _ => Owner::Mixed,
        }
    }
}

/// An expression in the variables: a constant, plus constants times
/// variables, plus nonlinear terms.
///
/// Built with the arithmetic operators from [`Variable`]s, numbers (`f64` and
/// `i32`), and other expressions, and with the functions
/// [`ln`](Expression::ln), [`exp`](Expression::exp), [`sqrt`](Expression::sqrt),
/// the trigonometric functions, and powers. Comparing it with
/// [`leq`](Expression::leq), [`geq`](Expression::geq), or
/// [`eq`](Expression::eq) (or with the [`constraint!`](crate::constraint)
/// macro) gives a [`Constraint`].
///
/// ```
/// use optimizam::{Expression, Model, var};
///
/// let mut m = Model::new();
/// let x: Vec<_> = (0..3).map(|_| m.add_var(var().min(0.0))).collect();
/// let total: Expression = x.iter().sum();
/// let weighted = 2 * x[0] + 3.5 * x[1] - x[2] / 4 + 1;
/// let curved = (x[0] * x[1]).sqrt() + x[2].exp();
/// ```
///
/// Products of two non-constant expressions, quotients by them, and functions
/// of them are nonlinear. Nonlinear constraints and objectives are solved by
/// the nonlinear engine; the CP constraint kinds and
/// [`Constraint::only_if`] take only affine expressions, and
/// [`abs`](Expression::abs), [`min`], and [`max`] of them, which are
/// piecewise linear and leave a model linear.
#[derive(Clone, Debug, Default)]
#[must_use = "an expression does nothing until it is used in a constraint or objective"]
pub struct Expression {
    pub(crate) owner: Owner,
    /// Column to coefficient.
    pub(crate) linear: BTreeMap<usize, f64>,
    pub(crate) constant: f64,
    /// Coefficient times a nonlinear term.
    pub(crate) nonlinear: Vec<(f64, Arc<Tree>)>,
}

impl Expression {
    /// Every column the expression uses, nonlinear terms included.
    pub(crate) fn columns(&self, out: &mut Vec<usize>) {
        out.extend(self.linear.iter().filter(|c| *c.1 != 0.0).map(|c| *c.0));
        for (_, t) in &self.nonlinear {
            match &**t {
                Tree::Mul(a, b) | Tree::Div(a, b) | Tree::Pow(a, b) => {
                    a.columns(out);
                    b.columns(out);
                }
                Tree::PowI(a, _) | Tree::Func(_, a) | Tree::Abs(a) => a.columns(out),
                Tree::Min(items) | Tree::Max(items) => items.iter().for_each(|e| e.columns(out)),
            }
        }
    }
}

/// A term that isn't affine: a nonlinear one, or a piecewise linear one
/// (`abs`, `min`, `max`), which lowering turns into a column.
#[derive(Debug)]
pub(crate) enum Tree {
    Mul(Expression, Expression),
    Div(Expression, Expression),
    PowI(Expression, i32),
    Pow(Expression, Expression),
    Func(Func, Expression),
    Abs(Expression),
    Min(Vec<Expression>),
    Max(Vec<Expression>),
}

impl Tree {
    /// Whether the term is piecewise linear in its arguments.
    pub(crate) fn piecewise(&self) -> bool {
        matches!(self, Tree::Abs(_) | Tree::Min(_) | Tree::Max(_))
    }
}

impl From<Variable> for Expression {
    fn from(v: Variable) -> Expression {
        Expression { owner: Owner::Model(v.model), linear: BTreeMap::from([(v.col, 1.0)]), ..Default::default() }
    }
}

impl From<&Variable> for Expression {
    fn from(v: &Variable) -> Expression {
        Expression::from(*v)
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A number, as coefficients, right-hand sides, and bounds take them: any
/// of Rust's integer and floating-point types. A whole number written
/// without a type (`x + 1`, `bounds(0, 10)`) is an `i32`, as elsewhere in
/// Rust.
///
/// Solvers compute in `f64`, so an integer beyond 2^53 is rounded to the
/// nearest `f64`.
///
/// ```
/// use optimizam::{Model, constraint, var};
///
/// let mut m = Model::new();
/// let (count, weight): (usize, u64) = (12, 250);
/// let x = m.add_var(var().integer().bounds(0, count));
/// m.add(constraint!(x * weight <= 3_000_i64));
/// ```
///
/// A number on the *left* of an operator must be an `i32` or an `f64`
/// (`2 * x`, `0.5 * x`): with a second integer type there, Rust couldn't
/// tell what `2 * x + 1` is. Other types go on the right, or through
/// [`Expression::from`]:
///
/// ```
/// use optimizam::{Expression, Model, var};
///
/// let mut m = Model::new();
/// let x = m.add_var(var());
/// let n: usize = 7;
/// let _ = 2 * x + 1;                  // i32 on the left
/// let _ = 0.5 * x - 3;                // f64 on the left
/// let _ = x * n + n;                  // any number on the right
/// let _ = Expression::from(n) * x;    // or converted first
/// ```
pub trait Number: sealed::Sealed + Copy {
    #[doc(hidden)]
    fn to_f64(self) -> f64;
}

macro_rules! numbers {
    ($($t:ty),*) => {$(
        impl sealed::Sealed for $t {}
        impl Number for $t {
            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    )*};
}
numbers!(f64, f32, i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl<T: Number> From<T> for Expression {
    fn from(c: T) -> Expression {
        Expression { constant: c.to_f64(), ..Default::default() }
    }
}

impl From<&Expression> for Expression {
    fn from(e: &Expression) -> Expression {
        e.clone()
    }
}

impl Expression {
    /// The constant expression `c`.
    pub fn constant(c: f64) -> Expression {
        Expression::from(c)
    }

    /// The constant, if the expression has no variables.
    pub(crate) fn as_constant(&self) -> Option<f64> {
        (self.linear.values().all(|&a| a == 0.0) && self.nonlinear.is_empty()).then_some(self.constant)
    }

    /// Whether the expression is affine (no nonlinear terms).
    pub fn is_affine(&self) -> bool {
        self.nonlinear.is_empty()
    }

    fn add_scaled(&mut self, other: Expression, sign: f64) {
        self.owner = self.owner.merge(other.owner);
        for (j, a) in other.linear {
            *self.linear.entry(j).or_insert(0.0) += sign * a;
        }
        self.constant += sign * other.constant;
        self.nonlinear.extend(other.nonlinear.into_iter().map(|(a, t)| (sign * a, t)));
    }

    fn scale(mut self, c: f64) -> Expression {
        for a in self.linear.values_mut() {
            *a *= c;
        }
        self.constant *= c;
        for t in &mut self.nonlinear {
            t.0 *= c;
        }
        self
    }

    fn term(owner: Owner, tree: Tree) -> Expression {
        Expression { owner, nonlinear: vec![(1.0, Arc::new(tree))], ..Default::default() }
    }

    fn product(self, other: Expression) -> Expression {
        match (self.as_constant(), other.as_constant()) {
            (Some(c), _) => other.scale(c),
            (_, Some(c)) => self.scale(c),
            _ => Expression::term(self.owner.merge(other.owner), Tree::Mul(self, other)),
        }
    }

    fn quotient(self, other: Expression) -> Expression {
        match other.as_constant() {
            // Division by zero leaves a non-finite constant or coefficient,
            // which the model rejects when it is solved or exported.
            Some(c) => self.scale(1.0 / c),
            None => Expression::term(self.owner.merge(other.owner), Tree::Div(self, other)),
        }
    }

    fn func(self, f: Func) -> Expression {
        match self.as_constant() {
            Some(c) => Expression::from(f.apply(c)),
            None => Expression::term(self.owner, Tree::Func(f, self)),
        }
    }

    /// The absolute value. Of an affine expression it is piecewise linear,
    /// and the model stays a linear one: the solver adds a variable for the
    /// term (not shown in results), with rows alone where that is exact,
    /// as in `minimize(sum(.. abs ..))` or `abs(..) <= c`, and otherwise
    /// with a binary variable, which needs the expression's variables to
    /// have finite bounds.
    ///
    /// ```
    /// use optimizam::{Model, Options, constraint, var};
    /// # fn main() -> Result<(), optimizam::Error> {
    /// let mut m = Model::new();
    /// let x = m.add_var(var().name("x"));
    /// m.add(constraint!(2 * x >= 3));
    /// m.minimize((x - 5).abs() + (x + 1).abs());
    /// assert_eq!(m.solve(&Options::default())?.objective(), Some(6.0));
    /// # Ok(())
    /// # }
    /// ```
    pub fn abs(self) -> Expression {
        match self.as_constant() {
            Some(c) => Expression::from(c.abs()),
            None => Expression::term(self.owner, Tree::Abs(self)),
        }
    }

    /// The natural logarithm.
    pub fn ln(self) -> Expression {
        self.func(Func::Ln)
    }

    /// The exponential function.
    pub fn exp(self) -> Expression {
        self.func(Func::Exp)
    }

    /// The square root.
    pub fn sqrt(self) -> Expression {
        self.func(Func::Sqrt)
    }

    /// The sine (radians).
    pub fn sin(self) -> Expression {
        self.func(Func::Sin)
    }

    /// The cosine (radians).
    pub fn cos(self) -> Expression {
        self.func(Func::Cos)
    }

    /// The tangent (radians).
    pub fn tan(self) -> Expression {
        self.func(Func::Tan)
    }

    /// The arctangent.
    pub fn atan(self) -> Expression {
        self.func(Func::Atan)
    }

    /// The hyperbolic tangent.
    pub fn tanh(self) -> Expression {
        self.func(Func::Tanh)
    }

    /// The expression to an integer power (`x.powi(2)` is `x * x`).
    pub fn powi(self, n: i32) -> Expression {
        match (self.as_constant(), n) {
            (_, 0) => Expression::from(1.0),
            (_, 1) => self,
            (Some(c), n) => Expression::from(c.powi(n)),
            (None, n) => Expression::term(self.owner, Tree::PowI(self, n)),
        }
    }

    /// The expression to a constant power (`x.powf(0.5)`); for integer `p`,
    /// the same as [`powi`](Expression::powi).
    pub fn powf(self, p: f64) -> Expression {
        self.pow(p)
    }

    /// The expression to the power of another expression (`x^y`, `2^x`).
    pub fn pow(self, exponent: impl Into<Expression>) -> Expression {
        let exponent = exponent.into();
        match (self.as_constant(), exponent.as_constant()) {
            (Some(x), Some(y)) => Expression::from(x.powf(y)),
            (_, Some(y)) if y.fract() == 0.0 && y.abs() < 1e9 => self.powi(y as i32),
            _ => Expression::term(self.owner.merge(exponent.owner), Tree::Pow(self, exponent)),
        }
    }

    fn compare(self, rhs: impl Into<Expression>, lower: f64, upper: f64) -> Constraint {
        let mut expr = self;
        expr.add_scaled(rhs.into(), -1.0);
        Constraint { expr, lower, upper, literals: Vec::new(), name: None, element: None }
    }

    /// The constraint `self <= rhs`.
    pub fn leq(self, rhs: impl Into<Expression>) -> Constraint {
        self.compare(rhs, f64::NEG_INFINITY, 0.0)
    }

    /// The constraint `self >= rhs`.
    pub fn geq(self, rhs: impl Into<Expression>) -> Constraint {
        self.compare(rhs, 0.0, f64::INFINITY)
    }

    /// The constraint `self == rhs`.
    pub fn eq(self, rhs: impl Into<Expression>) -> Constraint {
        self.compare(rhs, 0.0, 0.0)
    }

    /// The constraint `lower <= self <= upper` (either bound may be
    /// infinite).
    pub fn between(self, lower: impl Number, upper: impl Number) -> Constraint {
        let (lower, upper) = (lower.to_f64(), upper.to_f64());
        Constraint { expr: self, lower, upper, literals: Vec::new(), name: None, element: None }
    }

    /// The value of the expression where each column `j` has the value
    /// `x(j)`.
    pub(crate) fn evaluate(&self, x: &dyn Fn(usize) -> f64) -> f64 {
        let mut v = self.constant + self.linear.iter().map(|(&j, &a)| a * x(j)).sum::<f64>();
        for (a, t) in &self.nonlinear {
            v += a * t.evaluate(x);
        }
        v
    }
}

impl Tree {
    fn evaluate(&self, x: &dyn Fn(usize) -> f64) -> f64 {
        match self {
            Tree::Mul(a, b) => a.evaluate(x) * b.evaluate(x),
            Tree::Div(a, b) => a.evaluate(x) / b.evaluate(x),
            Tree::PowI(a, n) => a.evaluate(x).powi(*n),
            Tree::Pow(a, b) => a.evaluate(x).powf(b.evaluate(x)),
            Tree::Func(f, a) => f.apply(a.evaluate(x)),
            Tree::Abs(a) => a.evaluate(x).abs(),
            Tree::Min(items) => items.iter().map(|e| e.evaluate(x)).fold(f64::INFINITY, f64::min),
            Tree::Max(items) => items.iter().map(|e| e.evaluate(x)).fold(f64::NEG_INFINITY, f64::max),
        }
    }
}

/// Methods of [`Expression`] that also work on a [`Variable`] directly
/// (`x.ln()`, `x.leq(3)`).
macro_rules! variable_methods {
    ($($(#[$doc:meta])* $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        impl Variable {
            $(
                $(#[$doc])*
                pub fn $name(self, $($arg: $ty),*) -> $ret {
                    Expression::from(self).$name($($arg),*)
                }
            )*
        }
    };
}

variable_methods! {
    /// The absolute value of the variable (see [`Expression::abs`]).
    abs() -> Expression;
    /// The natural logarithm of the variable.
    ln() -> Expression;
    /// The exponential of the variable.
    exp() -> Expression;
    /// The square root of the variable.
    sqrt() -> Expression;
    /// The sine of the variable (radians).
    sin() -> Expression;
    /// The cosine of the variable (radians).
    cos() -> Expression;
    /// The tangent of the variable (radians).
    tan() -> Expression;
    /// The arctangent of the variable.
    atan() -> Expression;
    /// The hyperbolic tangent of the variable.
    tanh() -> Expression;
    /// The variable to an integer power.
    powi(n: i32) -> Expression;
    /// The variable to a constant power.
    powf(p: f64) -> Expression;
    /// The variable to the power of an expression.
    pow(exponent: impl Into<Expression>) -> Expression;
    /// The constraint `self <= rhs`.
    leq(rhs: impl Into<Expression>) -> Constraint;
    /// The constraint `self >= rhs`.
    geq(rhs: impl Into<Expression>) -> Constraint;
    /// The constraint `self == rhs`.
    eq(rhs: impl Into<Expression>) -> Constraint;
}

impl Variable {
    /// The constraint `lower <= self <= upper`.
    pub fn between(self, lower: impl Number, upper: impl Number) -> Constraint {
        Expression::from(self).between(lower, upper)
    }

    /// The literal `self == value`, for [`Constraint::only_if`]: an integer
    /// variable and a number (`x.is(3)`), or a symbolic variable and one of
    /// its elements (`machine.is("mill")`). An element that isn't one of a
    /// symbolic variable's values is an error when the model is solved: it
    /// is most likely misspelled.
    pub fn is(self, value: impl Into<Element>) -> Literal {
        Literal { var: self, kind: LiteralKind::Is(value.into()) }
    }

    /// The constraint `self == value`: for an integer variable a number,
    /// for a symbolic variable one of its elements (`machine.equals("mill")`).
    /// An element that isn't one of a symbolic variable's values is an
    /// error when the model is solved.
    pub fn equals(self, value: impl Into<Element>) -> Constraint {
        Constraint {
            expr: self.into(),
            lower: 0.0,
            upper: 0.0,
            literals: Vec::new(),
            name: None,
            element: Some(value.into()),
        }
    }

    /// The literal `self != value` (see [`is`](Variable::is)).
    pub fn is_not(self, value: impl Into<Element>) -> Literal {
        Literal { var: self, kind: LiteralKind::IsNot(value.into()) }
    }
}

// Arithmetic. Expressions and variables combine with anything that converts
// into an expression; numbers on the left need impls of their own.

macro_rules! arithmetic {
    ($Trait:ident, $method:ident, |$a:ident, $b:ident| $body:expr) => {
        impl<T: Into<Expression>> $Trait<T> for Expression {
            type Output = Expression;
            fn $method(self, rhs: T) -> Expression {
                let ($a, $b) = (self, rhs.into());
                $body
            }
        }
        impl<T: Into<Expression>> $Trait<T> for &Expression {
            type Output = Expression;
            fn $method(self, rhs: T) -> Expression {
                let ($a, $b) = (self.clone(), rhs.into());
                $body
            }
        }
        impl<T: Into<Expression>> $Trait<T> for Variable {
            type Output = Expression;
            fn $method(self, rhs: T) -> Expression {
                let ($a, $b) = (Expression::from(self), rhs.into());
                $body
            }
        }
        arithmetic!(@scalar $Trait, $method, |$a, $b| $body, f64, i32);
    };
    (@scalar $Trait:ident, $method:ident, |$a:ident, $b:ident| $body:expr, $($scalar:ty),*) => {
        $(
            impl $Trait<Expression> for $scalar {
                type Output = Expression;
                fn $method(self, rhs: Expression) -> Expression {
                    let ($a, $b) = (Expression::from(self), rhs);
                    $body
                }
            }
            impl $Trait<&Expression> for $scalar {
                type Output = Expression;
                fn $method(self, rhs: &Expression) -> Expression {
                    let ($a, $b) = (Expression::from(self), rhs.clone());
                    $body
                }
            }
            impl $Trait<Variable> for $scalar {
                type Output = Expression;
                fn $method(self, rhs: Variable) -> Expression {
                    let ($a, $b) = (Expression::from(self), Expression::from(rhs));
                    $body
                }
            }
        )*
    };
}

arithmetic!(Add, add, |a, b| {
    let mut a = a;
    a.add_scaled(b, 1.0);
    a
});
arithmetic!(Sub, sub, |a, b| {
    let mut a = a;
    a.add_scaled(b, -1.0);
    a
});
arithmetic!(Mul, mul, |a, b| a.product(b));
arithmetic!(Div, div, |a, b| a.quotient(b));

impl Neg for Expression {
    type Output = Expression;
    fn neg(self) -> Expression {
        self.scale(-1.0)
    }
}

impl Neg for &Expression {
    type Output = Expression;
    fn neg(self) -> Expression {
        self.clone().scale(-1.0)
    }
}

impl Neg for Variable {
    type Output = Expression;
    fn neg(self) -> Expression {
        Expression::from(self).scale(-1.0)
    }
}

impl<T: Into<Expression>> AddAssign<T> for Expression {
    fn add_assign(&mut self, rhs: T) {
        self.add_scaled(rhs.into(), 1.0);
    }
}

impl<T: Into<Expression>> SubAssign<T> for Expression {
    fn sub_assign(&mut self, rhs: T) {
        self.add_scaled(rhs.into(), -1.0);
    }
}

impl<T: Into<Expression>> MulAssign<T> for Expression {
    fn mul_assign(&mut self, rhs: T) {
        *self = std::mem::take(self).product(rhs.into());
    }
}

/// The sum of the terms: variables, numbers, expressions.
///
/// The same as `terms.into_iter().sum::<Expression>()`, without having to
/// say what the sum is:
///
/// ```
/// use optimizam::{Model, constraint, sum, var};
///
/// let weight = [3, 4, 2, 5];
/// let mut m = Model::new();
/// let take = m.add_vars(4, var().binary());
/// m.add(constraint!(sum((0..4).map(|i| weight[i] * take[i])) <= 9));
/// m.maximize(sum(&take));
/// ```
pub fn sum<T: Into<Expression>>(terms: impl IntoIterator<Item = T>) -> Expression {
    terms.into_iter().sum()
}

/// The largest of the items: piecewise linear, like [`Expression::abs`], so
/// the model stays a linear one. Rows alone define it where that is exact
/// (a maximum that is minimized or bounded above, as a makespan is); anywhere
/// else it takes binary variables, and the items' variables need finite
/// bounds.
///
/// ```
/// use optimizam::{Model, Options, constraint, max, var};
/// # fn main() -> Result<(), optimizam::Error> {
/// let mut m = Model::new();
/// let x = m.add_var(var().bounds(0, 10).name("x"));
/// m.minimize(max([x - 2, 6 - x, 3 - x / 2]));
/// assert_eq!(m.solve(&Options::default())?.objective(), Some(2.0));
/// # Ok(())
/// # }
/// ```
///
/// # Panics
///
/// If there are no items.
pub fn max<T: Into<Expression>>(items: impl IntoIterator<Item = T>) -> Expression {
    extreme(items, true)
}

/// The smallest of the items; see [`max`].
///
/// # Panics
///
/// If there are no items.
pub fn min<T: Into<Expression>>(items: impl IntoIterator<Item = T>) -> Expression {
    extreme(items, false)
}

fn extreme<T: Into<Expression>>(items: impl IntoIterator<Item = T>, largest: bool) -> Expression {
    // The constants among the items count as one.
    let (mut constant, mut others, mut owner) = (None::<f64>, Vec::new(), Owner::None);
    for item in items {
        let item: Expression = item.into();
        match item.as_constant() {
            Some(c) => constant = Some(constant.map_or(c, |k| if largest { k.max(c) } else { k.min(c) })),
            None => {
                owner = owner.merge(item.owner);
                others.push(item);
            }
        }
    }
    others.extend(constant.map(Expression::from));
    match others.len() {
        0 => panic!("{} of no items", if largest { "max" } else { "min" }),
        1 => others.swap_remove(0),
        _ => Expression::term(owner, if largest { Tree::Max(others) } else { Tree::Min(others) }),
    }
}

impl<T: Into<Expression>> Sum<T> for Expression {
    fn sum<I: Iterator<Item = T>>(iter: I) -> Expression {
        let mut total = Expression::default();
        for t in iter {
            total.add_scaled(t.into(), 1.0);
        }
        total
    }
}

/// A constraint on an expression: `lower <= expression <= upper`, optionally
/// enforced only when [literals](Literal) hold.
///
/// Made with [`Expression::leq`], [`geq`](Expression::geq),
/// [`eq`](Expression::eq), [`between`](Expression::between) (also on
/// [`Variable`]), or the [`constraint!`](crate::constraint) macro, and added
/// with [`Model::add`](crate::Model::add).
///
/// A constraint that isn't added does nothing, so the compiler warns about
/// one that is dropped:
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// use optimizam::{Model, constraint, var};
///
/// let mut m = Model::new();
/// let x = m.add_var(var());
/// constraint!(x <= 3); // error: unused `Constraint` that must be used
/// ```
#[derive(Clone, Debug)]
#[must_use = "a constraint does nothing until it is added with `Model::add`"]
pub struct Constraint {
    pub(crate) expr: Expression,
    pub(crate) lower: f64,
    pub(crate) upper: f64,
    pub(crate) literals: Vec<Literal>,
    pub(crate) name: Option<String>,
    /// Made by [`Variable::equals`]: the variable (`expr`) equals this
    /// element, which for a symbolic variable stands for its position.
    pub(crate) element: Option<Element>,
}

impl Constraint {
    /// Enforce the constraint only when `literal` holds. With several calls,
    /// only when all of them hold. The constraint must be affine.
    ///
    /// ```
    /// use optimizam::{Model, constraint, var};
    ///
    /// let mut m = Model::new();
    /// let open = m.add_var(var().binary());
    /// let shift = m.add_var(var().integer().bounds(0, 3));
    /// let load = m.add_var(var().continuous().bounds(0.0, 100.0));
    /// m.add(constraint!(load <= 0).only_if(!open));
    /// m.add(constraint!(load >= 40).only_if(shift.is(2)));
    /// ```
    pub fn only_if(mut self, literal: impl Into<Literal>) -> Self {
        self.literals.push(literal.into());
        self
    }

    /// Name the constraint, for error messages and the exported problem.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// A condition for [`Constraint::only_if`]: a binary variable is 1 (`b`), or
/// 0 (`!b`), or an integer variable equals a value (`x.is(3)`) or doesn't
/// (`x.is_not(3)`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[must_use = "a literal does nothing until it is passed to `Constraint::only_if`"]
pub struct Literal {
    pub(crate) var: Variable,
    pub(crate) kind: LiteralKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LiteralKind {
    True,
    False,
    Is(Element),
    IsNot(Element),
}

impl From<Variable> for Literal {
    fn from(var: Variable) -> Literal {
        Literal { var, kind: LiteralKind::True }
    }
}

impl Not for Literal {
    type Output = Literal;
    fn not(self) -> Literal {
        let kind = match self.kind {
            LiteralKind::True => LiteralKind::False,
            LiteralKind::False => LiteralKind::True,
            LiteralKind::Is(v) => LiteralKind::IsNot(v),
            LiteralKind::IsNot(v) => LiteralKind::Is(v),
        };
        Literal { var: self.var, kind }
    }
}

impl Not for Variable {
    type Output = Literal;
    fn not(self) -> Literal {
        !Literal::from(self)
    }
}

/// A constraint from a comparison: `constraint!(lhs <= rhs)`, with `<=`,
/// `>=`, or `==`.
///
/// The sides are anything that converts into an [`Expression`]: variables,
/// numbers, and expressions. Rust's comparison operators must return `bool`,
/// hence the macro; it is the same as `Expression::from(lhs).leq(rhs)`.
///
/// ```
/// use optimizam::{Model, constraint, var};
///
/// let mut m = Model::new();
/// let (x, y) = (m.add_var(var().min(0.0)), m.add_var(var().min(0.0)));
/// m.add(constraint!(3 * x + 2 * y <= 12));
/// m.add(constraint!(x - y == 1).name("difference"));
/// ```
///
/// Other comparisons are compile errors that say what to write instead:
/// `!=` (use [`Model::all_different`](crate::Model::all_different) to make
/// integer expressions differ), and strict `<` and `>` (for integers, write
/// `x <= y - 1`).
///
/// ```compile_fail
/// # use optimizam::{Model, constraint, var};
/// # let mut m = Model::new();
/// # let (x, y) = (m.add_var(var().integer()), m.add_var(var().integer()));
/// m.add(constraint!(x != y));
/// ```
///
/// ```compile_fail
/// # use optimizam::{Model, constraint, var};
/// # let mut m = Model::new();
/// # let (x, y) = (m.add_var(var().integer()), m.add_var(var().integer()));
/// m.add(constraint!(x < y));
/// ```
#[macro_export]
macro_rules! constraint {
    (@lhs [$($lhs:tt)+] <= $($rhs:tt)+) => {
        $crate::Expression::from($($lhs)+).leq($($rhs)+)
    };
    (@lhs [$($lhs:tt)+] >= $($rhs:tt)+) => {
        $crate::Expression::from($($lhs)+).geq($($rhs)+)
    };
    (@lhs [$($lhs:tt)+] == $($rhs:tt)+) => {
        $crate::Expression::from($($lhs)+).eq($($rhs)+)
    };
    (@lhs [$($lhs:tt)+] != $($rhs:tt)+) => {
        compile_error!(
            "`!=` isn't a constraint: use `<=`, `>=`, or `==`, or `Model::all_different` to make integer expressions differ"
        )
    };
    (@lhs [$($lhs:tt)*] $next:tt $($rest:tt)*) => {
        $crate::constraint!(@lhs [$($lhs)* $next] $($rest)*)
    };
    // Nothing left, and no comparison seen. (A strict `<` or `>` can't be
    // told from the brackets of `sum::<Expression>()`, so it ends up here.)
    (@lhs [$($lhs:tt)*]) => {
        compile_error!(
            "a constraint needs `<=`, `>=`, or `==` (strict `<` and `>` aren't constraints: for integers, write `x <= y - 1`)"
        )
    };
    ($($t:tt)+) => {
        $crate::constraint!(@lhs [] $($t)+)
    };
}
