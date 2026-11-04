//! Tokenizer and recursive-descent parser for expression strings
//! (grammar in docs/problem-format.md).

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
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

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Ref(String, Option<Vec<Expr>>),
    Neg(Box<Expr>),
    /// `t1 + t2 - t3 ...` as signed terms (+1.0 or -1.0). A flat list rather
    /// than nested binary nodes, so that sums with many thousands of terms
    /// don't recurse once per term when evaluated, cloned, or dropped.
    Add(Vec<(f64, Expr)>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Pow(Box<Expr>, i32),
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
            _ => {
                self.pos -= 1;
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
            return match self.next() {
                Tok::Num(v) if v.fract() == 0.0 && v.abs() < 1e9 => Ok(Expr::Pow(Box::new(base), v as i32)),
                _ => {
                    self.pos -= 1;
                    self.err("an integer exponent")
                }
            };
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Num(v) => Ok(Expr::Num(v)),
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
            _ => {
                self.pos -= 1;
                self.err("a number, name, sum, or '('")
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
            _ => {
                self.pos -= 1;
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
        _ => {
            p.pos -= 1;
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
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_relation("x + y").is_err());
        assert!(parse_relation("x <= y <= z").is_err());
        assert!(parse_relation("x # 3 <= 1").is_err());
        assert!(parse_expr("x ^ 1.5").is_err());
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
