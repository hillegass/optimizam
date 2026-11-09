//! Tokenizer and recursive-descent parser for expression strings
//! (grammar in docs/problem-format.md).

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    /// A string literal (format 1.3): `'north'`.
    Str(String),
    Id(String),
    Kw(&'static str),
    Op(&'static str),
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rel {
    Le,
    Ge,
    Eq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Mul,
    Div,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A function of one argument (format 1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Func {
    Ln,
    Exp,
    Sqrt,
    Sin,
    Cos,
    Tan,
    Atan,
    Tanh,
}

impl Func {
    pub const ALL: [Func; 8] =
        [Func::Ln, Func::Exp, Func::Sqrt, Func::Sin, Func::Cos, Func::Tan, Func::Atan, Func::Tanh];

    pub fn name(self) -> &'static str {
        match self {
            Func::Ln => "ln",
            Func::Exp => "exp",
            Func::Sqrt => "sqrt",
            Func::Sin => "sin",
            Func::Cos => "cos",
            Func::Tan => "tan",
            Func::Atan => "atan",
            Func::Tanh => "tanh",
        }
    }

    fn from_name(name: &str) -> Option<Func> {
        Func::ALL.into_iter().find(|f| f.name() == name)
    }

    /// The function's value (NaN outside its domain: ln and sqrt of
    /// negative numbers, ln(0) is -inf).
    pub fn apply(self, x: f64) -> f64 {
        match self {
            Func::Ln => x.ln(),
            Func::Exp => x.exp(),
            Func::Sqrt => x.sqrt(),
            Func::Sin => x.sin(),
            Func::Cos => x.cos(),
            Func::Tan => x.tan(),
            Func::Atan => x.atan(),
            Func::Tanh => x.tanh(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    /// A string literal (format 1.3), a set element: `'north'`.
    Str(String),
    Ref(String, Option<Vec<Expr>>),
    Neg(Box<Expr>),
    /// `t1 + t2 - t3 ...` as signed terms (+1.0 or -1.0). A flat list rather
    /// than nested binary nodes, so that sums with many thousands of terms
    /// don't recurse once per term when evaluated, cloned, or dropped.
    Add(Vec<(f64, Expr)>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// A power with an integer constant exponent.
    Pow(Box<Expr>, i32),
    /// Any other power (format 1.2): `x^2.5`, `2^x`, `x^y`, `x^(1/3)`.
    PowE(Box<Expr>, Box<Expr>),
    /// A function call (format 1.2): `ln(x)`.
    Call(Func, Box<Expr>),
    Sum(Bindings, Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum Cond {
    Cmp(CmpOp, Expr, Expr),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
}

/// `i in I, j in J: condition`
#[derive(Debug, Clone)]
pub struct Bindings {
    pub vars: Vec<(String, String)>,
    pub cond: Option<Box<Cond>>,
}

/// One `enforce_if` literal: a variable reference, negated (`not b`), or
/// compared with a constant element (`v == e`, `v != e`).
#[derive(Debug, Clone)]
pub struct Literal {
    pub negated: bool,
    pub var: Expr,
    /// `Some((true, e))` for `== e`, `Some((false, e))` for `!= e`.
    pub compare: Option<(bool, Expr)>,
}

/// A list argument of a 1.1 constraint kind: `[a, b, c]` or `[x[i] | i in I]`.
#[derive(Debug, Clone)]
pub enum List {
    Explicit(Vec<Expr>),
    Comprehension(Box<Expr>, Bindings),
}

const KEYWORDS: [&str; 5] = ["sum", "in", "and", "or", "not"];
const OPS: [&str; 18] = ["<=", ">=", "==", "!=", "+", "-", "*", "/", "^", "(", ")", "[", "]", ",", ":", "<", ">", "|"];

fn tokenize(text: &str) -> Result<Vec<Tok>, String> {
    let bytes = text.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || (c == '.' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit())) {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                let mut j = i + 1;
                if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    i = j;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let s = &text[start..i];
            let v: f64 = s.parse().map_err(|_| format!("bad number {s:?} in {text:?}"))?;
            toks.push(Tok::Num(v));
        } else if c == '\'' {
            // A string literal; '' inside it stands for one quote.
            let mut s = String::new();
            let mut j = i + 1;
            loop {
                match text[j..].find('\'') {
                    None => return Err(format!("unterminated string literal in {text:?}")),
                    Some(k) => {
                        s.push_str(&text[j..j + k]);
                        j += k + 1;
                        if text[j..].starts_with('\'') {
                            s.push('\'');
                            j += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            toks.push(Tok::Str(s));
            i = j;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let s = &text[start..i];
            match KEYWORDS.iter().find(|k| **k == s) {
                Some(k) => toks.push(Tok::Kw(k)),
                None => toks.push(Tok::Id(s.to_string())),
            }
        } else {
            let op = OPS
                .iter()
                .find(|op| text[i..].starts_with(**op))
                .ok_or_else(|| format!("unexpected character {c:?} in {text:?}"))?;
            toks.push(Tok::Op(op));
            i += op.len();
        }
    }
    toks.push(Tok::End);
    Ok(toks)
}

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Tok>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Result<Self, String> {
        Ok(Parser { text, toks: tokenize(text)?, pos: 0 })
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if t != Tok::End {
            self.pos += 1;
        }
        t
    }

    /// Steps back over `t`, the token `next` just returned, so that an
    /// error points at it. (`next` doesn't move past the end.)
    fn back(&mut self, t: &Tok) {
        if *t != Tok::End {
            self.pos -= 1;
        }
    }

    fn accept_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Tok::Op(o) if *o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn accept_kw(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Tok::Kw(k) if *k == kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        let at = match self.peek() {
            Tok::Num(v) => v.to_string(),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Id(s) => s.clone(),
            Tok::Kw(s) | Tok::Op(s) => s.to_string(),
            Tok::End => "end of expression".to_string(),
        };
        Err(format!("expected {what} at {at:?} in {:?}", self.text))
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        if self.accept_op(op) { Ok(()) } else { self.err(&format!("'{op}'")) }
    }

    fn expect_id(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Id(s) => Ok(s),
            t => {
                self.back(&t);
                self.err("a name")
            }
        }
    }

    fn done(&self) -> Result<(), String> {
        if *self.peek() == Tok::End { Ok(()) } else { self.err("end of expression") }
    }

    fn expr(&mut self) -> Result<Expr, String> {
        let first = self.term()?;
        let mut terms = Vec::new();
        loop {
            let sign = if self.accept_op("+") {
                1.0
            } else if self.accept_op("-") {
                -1.0
            } else {
                break;
            };
            terms.push((sign, self.term()?));
        }
        if terms.is_empty() {
            return Ok(first);
        }
        terms.insert(0, (1.0, first));
        Ok(Expr::Add(terms))
    }

    fn term(&mut self) -> Result<Expr, String> {
        let mut e = self.factor()?;
        loop {
            let op = if self.accept_op("*") {
                BinOp::Mul
            } else if self.accept_op("/") {
                BinOp::Div
            } else {
                return Ok(e);
            };
            e = Expr::Bin(op, Box::new(e), Box::new(self.factor()?));
        }
    }

    fn factor(&mut self) -> Result<Expr, String> {
        if self.accept_op("-") {
            return Ok(Expr::Neg(Box::new(self.factor()?)));
        }
        let base = self.atom()?;
        if self.accept_op("^") {
            // The exponent is a factor, so `^` is right-associative and
            // `x^-1` works; an integer constant exponent stays a `Pow`.
            let exponent = self.factor()?;
            let integer = match &exponent {
                Expr::Num(v) => Some(*v),
                Expr::Neg(e) => match **e {
                    Expr::Num(v) => Some(-v),
                    _ => None,
                },
                _ => None,
            };
            return Ok(match integer {
                Some(v) if v.fract() == 0.0 && v.abs() < 1e9 => Expr::Pow(Box::new(base), v as i32),
                _ => Expr::PowE(Box::new(base), Box::new(exponent)),
            });
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Num(v) => Ok(Expr::Num(v)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Kw("sum") => {
                self.expect_op("(")?;
                let b = self.bindings()?;
                self.expect_op(")")?;
                Ok(Expr::Sum(b, Box::new(self.term()?)))
            }
            Tok::Op("(") => {
                let e = self.expr()?;
                self.expect_op(")")?;
                Ok(e)
            }
            Tok::Id(name) if matches!(self.peek(), Tok::Op("(")) => {
                let Some(f) = Func::from_name(&name) else {
                    let hint = if name == "log" { "; use ln for the natural logarithm" } else { "" };
                    let names: Vec<&str> = Func::ALL.iter().map(|f| f.name()).collect();
                    return Err(format!(
                        "unknown function {name} in {:?}{hint} (the functions are {})",
                        self.text,
                        names.join(", ")
                    ));
                };
                self.expect_op("(")?;
                let arg = self.expr()?;
                self.expect_op(")")?;
                Ok(Expr::Call(f, Box::new(arg)))
            }
            Tok::Id(name) => {
                if self.accept_op("[") {
                    let mut args = vec![self.expr()?];
                    while self.accept_op(",") {
                        args.push(self.expr()?);
                    }
                    self.expect_op("]")?;
                    Ok(Expr::Ref(name, Some(args)))
                } else {
                    Ok(Expr::Ref(name, None))
                }
            }
            t => {
                self.back(&t);
                self.err("a number, string, name, sum, or '('")
            }
        }
    }

    fn bindings(&mut self) -> Result<Bindings, String> {
        let mut vars = Vec::new();
        loop {
            let idx = self.expect_id()?;
            if !self.accept_kw("in") {
                return self.err("'in'");
            }
            let set = self.expect_id()?;
            vars.push((idx, set));
            if !self.accept_op(",") {
                break;
            }
        }
        let cond = if self.accept_op(":") { Some(Box::new(self.condition()?)) } else { None };
        Ok(Bindings { vars, cond })
    }

    fn condition(&mut self) -> Result<Cond, String> {
        let mut c = self.conj()?;
        while self.accept_kw("or") {
            c = Cond::Or(Box::new(c), Box::new(self.conj()?));
        }
        Ok(c)
    }

    fn conj(&mut self) -> Result<Cond, String> {
        let mut c = self.neg()?;
        while self.accept_kw("and") {
            c = Cond::And(Box::new(c), Box::new(self.neg()?));
        }
        Ok(c)
    }

    fn neg(&mut self) -> Result<Cond, String> {
        if self.accept_kw("not") {
            return Ok(Cond::Not(Box::new(self.neg()?)));
        }
        // "(" may open a parenthesized condition or a parenthesized expression.
        let save = self.pos;
        if self.accept_op("(") {
            if let Ok(c) = self.condition()
                && self.accept_op(")")
            {
                return Ok(c);
            }
            self.pos = save;
        }
        let a = self.expr()?;
        let op = match self.next() {
            Tok::Op("==") => CmpOp::Eq,
            Tok::Op("!=") => CmpOp::Ne,
            Tok::Op("<") => CmpOp::Lt,
            Tok::Op("<=") => CmpOp::Le,
            Tok::Op(">") => CmpOp::Gt,
            Tok::Op(">=") => CmpOp::Ge,
            t => {
                self.back(&t);
                return self.err("a comparison (==, !=, <, <=, >, >=)");
            }
        };
        Ok(Cond::Cmp(op, a, self.expr()?))
    }
}

/// Parses `lhs <= rhs`, `lhs >= rhs`, or `lhs == rhs`.
pub fn parse_relation(text: &str) -> Result<(Rel, Expr, Expr), String> {
    let mut p = Parser::new(text)?;
    let lhs = p.expr()?;
    let rel = match p.next() {
        Tok::Op("<=") => Rel::Le,
        Tok::Op(">=") => Rel::Ge,
        Tok::Op("==") => Rel::Eq,
        t => {
            p.back(&t);
            return p.err("<=, >=, or ==");
        }
    };
    let rhs = p.expr()?;
    p.done()?;
    Ok((rel, lhs, rhs))
}

/// Parses an expression with no relation (an objective).
pub fn parse_expr(text: &str) -> Result<Expr, String> {
    let mut p = Parser::new(text)?;
    let e = p.expr()?;
    p.done()?;
    Ok(e)
}

/// Parses a `forall` binding list.
pub fn parse_bindings(text: &str) -> Result<Bindings, String> {
    let mut p = Parser::new(text)?;
    let b = p.bindings()?;
    p.done()?;
    Ok(b)
}

/// Parses an `enforce_if` conjunction: `literal { and literal }`, where a
/// literal is `not ref`, `ref`, `ref == expr`, or `ref != expr`.
pub fn parse_literals(text: &str) -> Result<Vec<Literal>, String> {
    let mut p = Parser::new(text)?;
    let mut lits = Vec::new();
    loop {
        let negated = p.accept_kw("not");
        let name = p.expect_id()?;
        let args = if p.accept_op("[") {
            let mut args = vec![p.expr()?];
            while p.accept_op(",") {
                args.push(p.expr()?);
            }
            p.expect_op("]")?;
            Some(args)
        } else {
            None
        };
        let var = Expr::Ref(name, args);
        let compare = if negated {
            None
        } else if p.accept_op("==") {
            Some((true, p.expr()?))
        } else if p.accept_op("!=") {
            Some((false, p.expr()?))
        } else {
            None
        };
        lits.push(Literal { negated, var, compare });
        if !p.accept_kw("and") {
            break;
        }
    }
    p.done()?;
    Ok(lits)
}

/// Parses a list: `[expr | bindings]` or `[expr, expr, ...]`.
pub fn parse_list(text: &str) -> Result<List, String> {
    let mut p = Parser::new(text)?;
    p.expect_op("[")?;
    let first = p.expr()?;
    let list = if p.accept_op("|") {
        let b = p.bindings()?;
        List::Comprehension(Box::new(first), b)
    } else {
        let mut items = vec![first];
        while p.accept_op(",") {
            items.push(p.expr()?);
        }
        List::Explicit(items)
    };
    p.expect_op("]")?;
    p.done()?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_literals() {
        let (_, lhs, rhs) = parse_relation("x['north', 3] <= 'o''hare'").unwrap();
        match lhs {
            Expr::Ref(name, Some(args)) => {
                assert_eq!(name, "x");
                assert!(matches!(&args[..], [Expr::Str(s), Expr::Num(n)] if s == "north" && *n == 3.0));
            }
            e => panic!("{e:?}"),
        }
        assert!(matches!(rhs, Expr::Str(s) if s == "o'hare"));
        assert!(matches!(parse_expr("''").unwrap(), Expr::Str(s) if s.is_empty()));
        assert!(matches!(parse_expr("'two words, [and] symbols'").unwrap(), Expr::Str(s) if s.contains("[and]")));
        assert!(parse_expr("'north").unwrap_err().contains("unterminated string literal"));
        assert!(parse_expr("x['a'").is_err());
        assert!(matches!(parse_list("['a', 'b']").unwrap(), List::Explicit(v) if v.len() == 2));
    }

    #[test]
    fn parses_examples() {
        parse_relation("sum(i in ITEMS) weight[i] * take[i] <= capacity").unwrap();
        parse_relation("sum(r in N, c in N: r - c == d) q[r,c] <= 1").unwrap();
        parse_relation("stock[f,p,t] == stock[f,p,t-1] + make[f,p,t] - sum(m in M) ship[f,m,p,t]").unwrap();
        parse_bindings("f in F, p in P, t in T: t > 1").unwrap();
        parse_bindings("i in I, j in J: (i < j) and not (i == 2)").unwrap();
        parse_bindings("i in I: (i + 1) * 2 > 3").unwrap();
        parse_expr("-x^2 + 3.5e-1 * (y - .5)").unwrap();
        assert!(matches!(parse_list("[q[r] + r | r in N]").unwrap(), List::Comprehension(..)));
        assert!(matches!(parse_list("[op[j,s] | j in J, s in S: m[j,s] == k]").unwrap(), List::Comprehension(..)));
        match parse_list("[a, b[2], c + 1]").unwrap() {
            List::Explicit(items) => assert_eq!(items.len(), 3),
            l => panic!("unexpected {l:?}"),
        }
        assert!(parse_list("[x | i in I").is_err());
        assert!(parse_list("x, y").is_err());
        let lits = parse_literals("setup[p,t] and not idle[t] and machine[t] == m and shift[w] != 3").unwrap();
        assert_eq!(lits.len(), 4);
        assert!(!lits[0].negated && lits[0].compare.is_none());
        assert!(lits[1].negated);
        assert!(matches!(lits[2].compare, Some((true, _))));
        assert!(matches!(lits[3].compare, Some((false, _))));
        assert!(parse_literals("x + y").is_err());
        assert!(parse_literals("a or b").is_err());
    }

    #[test]
    fn parses_functions_and_powers() {
        assert!(matches!(parse_expr("ln(x)").unwrap(), Expr::Call(Func::Ln, _)));
        assert!(matches!(parse_expr("tanh(2*x + 1)").unwrap(), Expr::Call(Func::Tanh, _)));
        for f in Func::ALL {
            assert!(matches!(parse_expr(&format!("{}(x)", f.name())).unwrap(), Expr::Call(g, _) if g == f));
        }
        assert!(matches!(parse_expr("x^2").unwrap(), Expr::Pow(_, 2)));
        assert!(matches!(parse_expr("x^-1").unwrap(), Expr::Pow(_, -1)));
        assert!(matches!(parse_expr("x^2.5").unwrap(), Expr::PowE(..)));
        assert!(matches!(parse_expr("2^x").unwrap(), Expr::PowE(..)));
        assert!(matches!(parse_expr("x^(1/3)").unwrap(), Expr::PowE(..)));
        // Right-associative: x^y^z is x^(y^z).
        match parse_expr("x^y^z").unwrap() {
            Expr::PowE(_, e) => assert!(matches!(*e, Expr::PowE(..))),
            e => panic!("unexpected {e:?}"),
        }
        // Unary minus applies to the power: -x^2 is -(x^2).
        assert!(matches!(parse_expr("-x^2").unwrap(), Expr::Neg(_)));
        // A parameter (or variable) may still be named like a function.
        assert!(matches!(parse_expr("exp[i] * x").unwrap(), Expr::Bin(..)));
        assert!(matches!(parse_expr("sin").unwrap(), Expr::Ref(..)));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_relation("x + y").is_err());
        assert!(parse_relation("x <= y <= z").is_err());
        assert!(parse_relation("x # 3 <= 1").is_err());
        assert!(parse_expr("x ^").is_err());
        assert!(parse_expr("ln(x, y)").is_err());
        assert!(parse_expr("ln x").is_err());
        let e = parse_expr("log(x)").unwrap_err();
        assert!(e.contains("use ln"), "{e}");
        assert!(parse_expr("foo(x)").unwrap_err().contains("unknown function foo"));
        assert!(parse_bindings("i of I").is_err());
    }

    /// A sum with many terms parses and drops without deep recursion.
    #[test]
    fn long_sums_are_flat() {
        let text = (1..=200_000).map(|i| format!("{i} * x")).collect::<Vec<_>>().join(" + ");
        match parse_expr(&text).unwrap() {
            Expr::Add(terms) => assert_eq!(terms.len(), 200_000),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn sum_body_is_one_term() {
        // sum(i in I) a[i]*x[i] + 1  parses as  (sum ...) + 1
        match parse_expr("sum(i in I) a[i]*x[i] + 1").unwrap() {
            Expr::Add(terms) => {
                assert_eq!(terms.len(), 2);
                assert!(matches!(terms[0], (1.0, Expr::Sum(..))));
            }
            e => panic!("unexpected {e:?}"),
        }
    }
}
