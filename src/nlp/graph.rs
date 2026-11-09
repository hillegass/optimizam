//! Expression graphs of nonlinear functions, with exact first and second
//! derivatives (automatic differentiation).
//!
//! A [`Graph`] holds nodes in topological order: every node's children come
//! before it. Building a node that already exists returns the existing one
//! (hash-consing), so a subexpression shared by several constraints, such as
//! `exp(-k * t)`, is evaluated once. Constant subexpressions are folded.
//!
//! An [`Evaluator`] compiles a set of root nodes (the objective and the
//! constraints) and computes, at a point x:
//!
//! - every node's value (a forward pass);
//! - the gradient of one root (a reverse pass over the nodes it reaches);
//! - the Hessian of a weighted sum of roots, `sum_k w_k * Hess f_k`, which is
//!   the Hessian of the Lagrangian: forward-over-reverse. The adjoints of the
//!   weighted sum are computed once; then, for a direction v, a forward
//!   tangent pass and a reverse second-order pass give `H v`.
//!
//! The Hessian's structural pattern comes from the graph: a node that
//! combines its children nonlinearly (a product, a function, a power) couples
//! the variables they depend on. Directions are compressed by coloring:
//! variables that never appear in a common row of the pattern share one
//! direction, so a separable function needs a single pass.
//!
//! Values outside a function's domain (`ln` of a negative number, division by
//! zero) give NaN or infinite values; callers check for them.

use crate::expr::Func;
use std::collections::HashMap;

pub type Node = u32;

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Const(f64),
    Var(usize),
    /// `constant + sum(coef * child)`.
    Sum(f64, Vec<(f64, Node)>),
    Mul(Node, Node),
    Div(Node, Node),
    /// A power with an integer exponent other than 0 and 1.
    PowI(Node, i32),
    /// A power with a non-integer constant exponent.
    PowC(Node, f64),
    /// `a ^ b` with a non-constant exponent.
    Pow(Node, Node),
    Func(Func, Node),
}

/// A hashable form of an op (floats by their bits).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Const(u64),
    Var(usize),
    Sum(u64, Vec<(u64, Node)>),
    Mul(Node, Node),
    Div(Node, Node),
    PowI(Node, i32),
    PowC(Node, u64),
    Pow(Node, Node),
    Func(Func, Node),
}

fn bits(v: f64) -> u64 {
    // +0 and -0 are the same constant.
    if v == 0.0 { 0 } else { v.to_bits() }
}

impl Op {
    fn key(&self) -> Key {
        match self {
            Op::Const(c) => Key::Const(bits(*c)),
            Op::Var(j) => Key::Var(*j),
            Op::Sum(c, terms) => Key::Sum(bits(*c), terms.iter().map(|&(a, n)| (bits(a), n)).collect()),
            Op::Mul(a, b) => Key::Mul(*a.min(b), *a.max(b)),
            Op::Div(a, b) => Key::Div(*a, *b),
            Op::PowI(a, n) => Key::PowI(*a, *n),
            Op::PowC(a, c) => Key::PowC(*a, bits(*c)),
            Op::Pow(a, b) => Key::Pow(*a, *b),
            Op::Func(f, a) => Key::Func(*f, *a),
        }
    }
}

#[derive(Clone, Default)]
pub struct Graph {
    ops: Vec<Op>,
    index: HashMap<Key, Node>,
}

/// The nodes in order (the index for hash-consing is derived from them).
impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.ops.iter().enumerate()).finish()
    }
}

impl Graph {
    pub fn new() -> Self {
        Graph::default()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn op(&self, n: Node) -> &Op {
        &self.ops[n as usize]
    }

    fn add(&mut self, op: Op) -> Node {
        let key = op.key();
        if let Some(&n) = self.index.get(&key) {
            return n;
        }
        let n = self.ops.len() as Node;
        self.ops.push(op);
        self.index.insert(key, n);
        n
    }

    fn constant_of(&self, n: Node) -> Option<f64> {
        match self.ops[n as usize] {
            Op::Const(c) => Some(c),
            _ => None,
        }
    }

    pub fn constant(&mut self, c: f64) -> Node {
        self.add(Op::Const(c))
    }

    pub fn var(&mut self, j: usize) -> Node {
        self.add(Op::Var(j))
    }

    /// `constant + sum(coef * node)`, with constant terms folded, repeated
    /// nodes merged, and zero coefficients dropped.
    pub fn sum(&mut self, constant: f64, terms: &[(f64, Node)]) -> Node {
        let mut constant = constant;
        let mut merged: Vec<(f64, Node)> = Vec::with_capacity(terms.len());
        let mut position: HashMap<Node, usize> = HashMap::new();
        for &(a, n) in terms {
            match self.ops[n as usize] {
                Op::Const(c) => constant += a * c,
                // Inline a child sum: sums built term by term would otherwise
                // nest, a node (and a relaxation column) per partial sum.
                Op::Sum(c, ref inner) => {
                    constant += a * c;
                    for &(b, m) in inner {
                        match position.get(&m) {
                            Some(&p) => merged[p].0 += a * b,
                            None => {
                                position.insert(m, merged.len());
                                merged.push((a * b, m));
                            }
                        }
                    }
                }
                _ => match position.get(&n) {
                    Some(&p) => merged[p].0 += a,
                    None => {
                        position.insert(n, merged.len());
                        merged.push((a, n));
                    }
                },
            }
        }
        merged.retain(|t| t.0 != 0.0);
        match merged.as_slice() {
            [] => self.constant(constant),
            &[(a, n)] if a == 1.0 && constant == 0.0 => n,
            _ => self.add(Op::Sum(constant, merged)),
        }
    }

    pub fn scale(&mut self, a: f64, n: Node) -> Node {
        self.sum(0.0, &[(a, n)])
    }

    /// `c * u` for a node that is a scaled node (a sum with one term and no
    /// constant), else (1, node).
    fn unscaled(&self, n: Node) -> (f64, Node) {
        match &self.ops[n as usize] {
            Op::Sum(c, terms) if *c == 0.0 && terms.len() == 1 => (terms[0].0, terms[0].1),
            _ => (1.0, n),
        }
    }

    /// Scale factors move out of products, powers, and quotients
    /// ((c u) * v = c (u v)), so that `u * v` is one node however many rows
    /// scale it differently.
    pub fn mul(&mut self, a: Node, b: Node) -> Node {
        match (self.constant_of(a), self.constant_of(b)) {
            (Some(x), Some(y)) => self.constant(x * y),
            (Some(c), None) => self.scale(c, b),
            (None, Some(c)) => self.scale(c, a),
            (None, None) => {
                let ((ca, ua), (cb, ub)) = (self.unscaled(a), self.unscaled(b));
                let p = self.add(Op::Mul(ua, ub));
                if ca * cb == 1.0 { p } else { self.scale(ca * cb, p) }
            }
        }
    }

    pub fn div(&mut self, a: Node, b: Node) -> Node {
        match (self.constant_of(a), self.constant_of(b)) {
            (Some(x), Some(y)) => self.constant(x / y),
            (_, Some(c)) => self.scale(1.0 / c, a),
            _ => {
                let ((ca, ua), (cb, ub)) = (self.unscaled(a), self.unscaled(b));
                let q = self.add(Op::Div(ua, ub));
                if ca / cb == 1.0 { q } else { self.scale(ca / cb, q) }
            }
        }
    }

    pub fn powi(&mut self, a: Node, n: i32) -> Node {
        match (self.constant_of(a), n) {
            (_, 0) => self.constant(1.0),
            (_, 1) => a,
            (Some(x), n) => self.constant(x.powi(n)),
            (None, n) => {
                let (c, u) = self.unscaled(a);
                let p = self.add(Op::PowI(u, n));
                let f = c.powi(n);
                if f == 1.0 { p } else { self.scale(f, p) }
            }
        }
    }

    pub fn pow(&mut self, a: Node, b: Node) -> Node {
        match (self.constant_of(a), self.constant_of(b)) {
            (Some(x), Some(y)) => self.constant(x.powf(y)),
            (_, Some(y)) if y.fract() == 0.0 && y.abs() < 1e9 => self.powi(a, y as i32),
            (_, Some(y)) => self.add(Op::PowC(a, y)),
            _ => self.add(Op::Pow(a, b)),
        }
    }

    pub fn func(&mut self, f: Func, a: Node) -> Node {
        match self.constant_of(a) {
            Some(x) => self.constant(f.apply(x)),
            None => self.add(Op::Func(f, a)),
        }
    }

    /// A copy holding only the nodes reachable from `roots`, numbered in
    /// the order a depth-first walk from the roots (in order, children in
    /// order) finishes them, so that equal expressions built in different
    /// ways, or with partial results left behind, give the same graph.
    /// Returns the copy and each root's node in it.
    pub fn compact(&self, roots: &[Node]) -> (Graph, Vec<Node>) {
        let children = |op: &Op| -> Vec<Node> {
            match op {
                Op::Const(_) | Op::Var(_) => vec![],
                Op::Sum(_, terms) => terms.iter().map(|t| t.1).collect(),
                Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => vec![*a, *b],
                Op::PowI(a, _) | Op::PowC(a, _) | Op::Func(_, a) => vec![*a],
            }
        };
        let mut out = Graph::new();
        let mut map: Vec<Option<Node>> = vec![None; self.ops.len()];
        let mut new_roots = Vec::with_capacity(roots.len());
        for &root in roots {
            // An explicit stack: expressions can be deeper than the call stack.
            let mut stack = vec![(root, false)];
            while let Some((n, expanded)) = stack.pop() {
                if map[n as usize].is_some() {
                    continue;
                }
                let op = &self.ops[n as usize];
                if !expanded {
                    stack.push((n, true));
                    stack.extend(
                        children(op).into_iter().rev().filter(|&c| map[c as usize].is_none()).map(|c| (c, false)),
                    );
                    continue;
                }
                let m = |c: &Node| map[*c as usize].expect("children are copied first");
                let copy = match op {
                    Op::Const(c) => Op::Const(*c),
                    Op::Var(j) => Op::Var(*j),
                    Op::Sum(c, terms) => Op::Sum(*c, terms.iter().map(|(a, t)| (*a, m(t))).collect()),
                    Op::Mul(a, b) => Op::Mul(m(a), m(b)),
                    Op::Div(a, b) => Op::Div(m(a), m(b)),
                    Op::Pow(a, b) => Op::Pow(m(a), m(b)),
                    Op::PowI(a, k) => Op::PowI(m(a), *k),
                    Op::PowC(a, c) => Op::PowC(m(a), *c),
                    Op::Func(f, a) => Op::Func(*f, m(a)),
                };
                map[n as usize] = Some(out.add(copy));
            }
            new_roots.push(map[root as usize].unwrap());
        }
        (out, new_roots)
    }

    /// The variables a node depends on, sorted.
    pub fn vars_of(&self, root: Node) -> Vec<usize> {
        let mut seen = vec![false; self.ops.len()];
        let mut stack = vec![root];
        let mut vars = Vec::new();
        while let Some(n) = stack.pop() {
            if std::mem::replace(&mut seen[n as usize], true) {
                continue;
            }
            match &self.ops[n as usize] {
                Op::Var(j) => vars.push(*j),
                op => for_each_child(op, |c| stack.push(c)),
            }
        }
        vars.sort_unstable();
        vars
    }

    /// Every node's value at x (NaN or infinite outside a domain).
    pub fn eval(&self, x: &[f64], vals: &mut Vec<f64>) {
        vals.clear();
        vals.reserve(self.ops.len());
        for op in &self.ops {
            let v = |n: Node| vals[n as usize];
            let value = match op {
                Op::Const(c) => *c,
                Op::Var(j) => x[*j],
                Op::Sum(c, terms) => terms.iter().fold(*c, |s, &(a, n)| s + a * v(n)),
                Op::Mul(a, b) => v(*a) * v(*b),
                Op::Div(a, b) => v(*a) / v(*b),
                Op::PowI(a, n) => v(*a).powi(*n),
                Op::PowC(a, c) => v(*a).powf(*c),
                Op::Pow(a, b) => v(*a).powf(v(*b)),
                Op::Func(f, a) => f.apply(v(*a)),
            };
            vals.push(value);
        }
    }

    /// The gradient of one node with respect to the variables, from every
    /// node's value `vals` (a reverse pass over the nodes below it); None if
    /// a partial derivative isn't finite there.
    pub fn gradient_of(&self, root: Node, vals: &[f64]) -> Option<Vec<(usize, f64)>> {
        let mut adj = vec![0.0; root as usize + 1];
        adj[root as usize] = 1.0;
        let mut grad: Vec<(usize, f64)> = Vec::new();
        for i in (0..=root as usize).rev() {
            let w = adj[i];
            if w == 0.0 {
                continue;
            }
            match &self.ops[i] {
                Op::Const(_) => {}
                Op::Var(j) => grad.push((*j, w)),
                Op::Sum(_, terms) => {
                    for &(a, c) in terms {
                        adj[c as usize] += w * a;
                    }
                }
                op => {
                    let [a, b] = pair(op);
                    let (d, _) = partials(op, vals[a as usize], vals[b as usize], vals[i]);
                    let sides = if is_binary(op) { 2 } else { 1 };
                    for (side, c) in [a, b].into_iter().enumerate().take(sides) {
                        if !matches!(self.ops[c as usize], Op::Const(_)) {
                            adj[c as usize] += w * d[side];
                        }
                    }
                }
            }
        }
        grad.iter().all(|g| g.1.is_finite()).then_some(grad)
    }

    /// The value of one node at x.
    pub fn value(&self, n: Node, x: &[f64]) -> f64 {
        let mut vals = Vec::new();
        self.eval(x, &mut vals);
        vals[n as usize]
    }
}

/// Calls f on each child of an op (with repeats, e.g. both sides of `x * x`).
fn for_each_child(op: &Op, mut f: impl FnMut(Node)) {
    match op {
        Op::Const(_) | Op::Var(_) => {}
        Op::Sum(_, terms) => terms.iter().for_each(|&(_, n)| f(n)),
        Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => {
            f(*a);
            f(*b);
        }
        Op::PowI(a, _) | Op::PowC(a, _) | Op::Func(_, a) => f(*a),
    }
}

/// First and second partial derivatives of a unary or binary op with
/// respect to its children `[a, b]` (b unused for unary ops), at the
/// children's values and the node's value v.
fn partials(op: &Op, a: f64, b: f64, v: f64) -> ([f64; 2], [[f64; 2]; 2]) {
    match op {
        Op::Mul(..) => ([b, a], [[0.0, 1.0], [1.0, 0.0]]),
        Op::Div(..) => {
            let r = 1.0 / b;
            let h_ab = -r * r;
            ([r, -a * r * r], [[0.0, h_ab], [h_ab, 2.0 * a * r * r * r]])
        }
        Op::PowI(_, n) => {
            let n = *n;
            let d = n as f64 * a.powi(n - 1);
            let h = if n == 1 { 0.0 } else { (n as f64) * (n - 1) as f64 * a.powi(n - 2) };
            ([d, 0.0], [[h, 0.0], [0.0, 0.0]])
        }
        Op::PowC(_, c) => {
            let c = *c;
            ([c * a.powf(c - 1.0), 0.0], [[c * (c - 1.0) * a.powf(c - 2.0), 0.0], [0.0, 0.0]])
        }
        Op::Pow(..) => {
            let ln = a.ln();
            let h_ab = a.powf(b - 1.0) * (1.0 + b * ln);
            ([b * a.powf(b - 1.0), v * ln], [[b * (b - 1.0) * a.powf(b - 2.0), h_ab], [h_ab, v * ln * ln]])
        }
        Op::Func(f, _) => {
            let (d, h) = match f {
                Func::Ln => (1.0 / a, -1.0 / (a * a)),
                Func::Exp => (v, v),
                Func::Sqrt => (0.5 / v, -0.25 / (v * a)),
                Func::Sin => (a.cos(), -v),
                Func::Cos => (-a.sin(), -v),
                Func::Tan => {
                    let s = 1.0 + v * v;
                    (s, 2.0 * v * s)
                }
                Func::Atan => {
                    let s = 1.0 / (1.0 + a * a);
                    (s, -2.0 * a * s * s)
                }
                Func::Tanh => {
                    let s = 1.0 - v * v;
                    (s, -2.0 * v * s)
                }
            };
            ([d, 0.0], [[h, 0.0], [0.0, 0.0]])
        }
        Op::Const(_) | Op::Var(_) | Op::Sum(..) => unreachable!("not a unary or binary op"),
    }
}

/// The children of a unary or binary op, as `[a, b]` (b = a for unary).
fn pair(op: &Op) -> [Node; 2] {
    match op {
        Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => [*a, *b],
        Op::PowI(a, _) | Op::PowC(a, _) | Op::Func(_, a) => [*a, *a],
        _ => unreachable!(),
    }
}

fn is_binary(op: &Op) -> bool {
    matches!(op, Op::Mul(..) | Op::Div(..) | Op::Pow(..))
}

/// Derivatives of a fixed set of roots of a graph.
pub struct Evaluator<'g> {
    graph: &'g Graph,
    roots: Vec<Node>,
    /// For each root, the nodes it reaches, in decreasing order.
    reach: Vec<Vec<Node>>,
    /// The node of each variable that appears (None if it doesn't).
    var_node: Vec<Option<Node>>,
    /// Whether each node is constant (no variable below it).
    constant: Vec<bool>,
    /// Lower triangle (i >= j) of the Hessian's structural pattern, sorted.
    pattern: Vec<(usize, usize)>,
    /// Groups of variables that share one Hessian direction.
    colors: Vec<Vec<usize>>,
    vals: Vec<f64>,
    adj: Vec<f64>,
    tan: Vec<f64>,
    adj_tan: Vec<f64>,
}

impl<'g> Evaluator<'g> {
    /// Compiles the roots; `num_vars` is the length of x.
    pub fn new(graph: &'g Graph, roots: &[Node], num_vars: usize) -> Self {
        let n = graph.len();
        let mut var_node = vec![None; num_vars];
        let mut constant = vec![true; n];
        // Variables each node depends on (sorted), and the pattern pairs.
        let mut deps: Vec<Vec<usize>> = Vec::with_capacity(n);
        let mut useful = vec![false; n];
        for &r in roots {
            mark(graph, r, &mut useful);
        }
        let mut pattern: std::collections::BTreeSet<(usize, usize)> = Default::default();
        let couple = |a: &[usize], b: &[usize], pattern: &mut std::collections::BTreeSet<(usize, usize)>| {
            for &i in a {
                for &j in b {
                    pattern.insert((i.max(j), i.min(j)));
                }
            }
        };
        for (k, op) in graph.ops.iter().enumerate() {
            let d: Vec<usize> = match op {
                Op::Const(_) => vec![],
                Op::Var(j) => {
                    var_node[*j] = Some(k as Node);
                    vec![*j]
                }
                op => {
                    let mut d: Vec<usize> = Vec::new();
                    for_each_child(op, |c| d.extend_from_slice(&deps[c as usize]));
                    d.sort_unstable();
                    d.dedup();
                    d
                }
            };
            constant[k] = d.is_empty();
            if useful[k] && !constant[k] {
                match op {
                    Op::Mul(a, b) => couple(&deps[*a as usize], &deps[*b as usize], &mut pattern),
                    Op::Div(a, b) => {
                        couple(&deps[*a as usize], &deps[*b as usize], &mut pattern);
                        couple(&deps[*b as usize], &deps[*b as usize], &mut pattern);
                    }
                    Op::PowI(a, _) | Op::PowC(a, _) | Op::Func(_, a) => {
                        couple(&deps[*a as usize], &deps[*a as usize], &mut pattern)
                    }
                    Op::Pow(..) => couple(&d, &d, &mut pattern),
                    _ => {}
                }
            }
            deps.push(d);
        }
        let pattern: Vec<(usize, usize)> = pattern.into_iter().collect();
        let reach = roots
            .iter()
            .map(|&r| {
                let mut seen = vec![false; n];
                mark(graph, r, &mut seen);
                (0..n as Node).rev().filter(|&k| seen[k as usize]).collect()
            })
            .collect();
        let colors = color(&pattern, num_vars);
        Evaluator {
            graph,
            roots: roots.to_vec(),
            reach,
            var_node,
            constant,
            pattern,
            colors,
            vals: Vec::new(),
            adj: vec![0.0; n],
            tan: vec![0.0; n],
            adj_tan: vec![0.0; n],
        }
    }

    /// The graph evaluated.
    pub fn graph(&self) -> &'g Graph {
        self.graph
    }

    /// Root k's node.
    pub fn root(&self, k: usize) -> Node {
        self.roots[k]
    }

    /// The lower triangle (row >= column) of the Hessian's pattern.
    pub fn hessian_pattern(&self) -> &[(usize, usize)] {
        &self.pattern
    }

    /// Hessian directions per evaluation (after coloring).
    pub fn hessian_passes(&self) -> usize {
        self.colors.len()
    }

    /// Evaluates every node at x; returns the roots' values.
    pub fn eval(&mut self, x: &[f64]) -> Vec<f64> {
        self.graph.eval(x, &mut self.vals);
        self.roots.iter().map(|&r| self.vals[r as usize]).collect()
    }

    /// The gradient of root k at the point last evaluated, as (variable,
    /// partial derivative) pairs sorted by variable.
    pub fn gradient(&mut self, k: usize) -> Vec<(usize, f64)> {
        let root = self.roots[k];
        self.adj[root as usize] = 1.0;
        let mut grad = Vec::new();
        for &node in &self.reach[k] {
            let i = node as usize;
            let w = std::mem::replace(&mut self.adj[i], 0.0);
            if w == 0.0 {
                continue;
            }
            let op = &self.graph.ops[i];
            match op {
                Op::Const(_) => {}
                Op::Var(j) => grad.push((*j, w)),
                Op::Sum(_, terms) => {
                    for &(a, c) in terms {
                        self.adj[c as usize] += w * a;
                    }
                }
                op => {
                    let [a, b] = pair(op);
                    let (d, _) = partials(op, self.vals[a as usize], self.vals[b as usize], self.vals[i]);
                    for (side, c) in [a, b].into_iter().enumerate().take(if is_binary(op) { 2 } else { 1 }) {
                        if !self.constant[c as usize] {
                            self.adj[c as usize] += w * d[side];
                        }
                    }
                }
            }
        }
        grad.sort_unstable_by_key(|g| g.0);
        grad
    }

    /// The lower triangle of `sum_k weights[k] * Hess root_k` at the point
    /// last evaluated, on [`Self::hessian_pattern`] (same order).
    pub fn hessian(&mut self, weights: &[f64]) -> Vec<f64> {
        let n = self.graph.len();
        // Adjoints of the weighted sum of roots.
        self.adj.iter_mut().for_each(|a| *a = 0.0);
        for (&r, &w) in self.roots.iter().zip(weights) {
            self.adj[r as usize] += w;
        }
        for i in (0..n).rev() {
            let w = self.adj[i];
            if w == 0.0 {
                continue;
            }
            let op = &self.graph.ops[i];
            match op {
                Op::Const(_) | Op::Var(_) => {}
                Op::Sum(_, terms) => {
                    for &(a, c) in terms {
                        self.adj[c as usize] += w * a;
                    }
                }
                op => {
                    let [a, b] = pair(op);
                    let (d, _) = partials(op, self.vals[a as usize], self.vals[b as usize], self.vals[i]);
                    for (side, c) in [a, b].into_iter().enumerate().take(if is_binary(op) { 2 } else { 1 }) {
                        if !self.constant[c as usize] {
                            self.adj[c as usize] += w * d[side];
                        }
                    }
                }
            }
        }
        // H v for each color's direction v; entries read off by row.
        let mut column_color = vec![usize::MAX; self.var_node.len()];
        for (c, group) in self.colors.iter().enumerate() {
            for &j in group {
                column_color[j] = c;
            }
        }
        let mut hv: Vec<Vec<f64>> = Vec::with_capacity(self.colors.len());
        for c in 0..self.colors.len() {
            hv.push(self.hessian_vector(&self.colors[c].clone()));
        }
        self.adj.iter_mut().for_each(|a| *a = 0.0);
        self.pattern.iter().map(|&(i, j)| hv[column_color[j]][i]).collect()
    }

    /// `H v` for v the sum of the unit vectors of `cols` (after the
    /// adjoints are set), indexed by variable.
    fn hessian_vector(&mut self, cols: &[usize]) -> Vec<f64> {
        let n = self.graph.len();
        let mut direction = vec![false; self.var_node.len()];
        cols.iter().for_each(|&j| direction[j] = true);
        // Forward tangents.
        for i in 0..n {
            let op = &self.graph.ops[i];
            self.tan[i] = match op {
                Op::Const(_) => 0.0,
                Op::Var(j) => {
                    if direction[*j] {
                        1.0
                    } else {
                        0.0
                    }
                }
                Op::Sum(_, terms) => terms.iter().map(|&(a, c)| a * self.tan[c as usize]).sum(),
                op => {
                    let [a, b] = pair(op);
                    let (d, _) = partials(op, self.vals[a as usize], self.vals[b as usize], self.vals[i]);
                    let mut t = 0.0;
                    for (side, c) in [a, b].into_iter().enumerate().take(if is_binary(op) { 2 } else { 1 }) {
                        if !self.constant[c as usize] && self.tan[c as usize] != 0.0 {
                            t += d[side] * self.tan[c as usize];
                        }
                    }
                    t
                }
            };
        }
        // Reverse second-order adjoints (the adjoints' tangents).
        self.adj_tan.iter_mut().for_each(|a| *a = 0.0);
        let mut out = vec![0.0; self.var_node.len()];
        for i in (0..n).rev() {
            let (w, wt) = (self.adj[i], self.adj_tan[i]);
            if w == 0.0 && wt == 0.0 {
                continue;
            }
            let op = &self.graph.ops[i];
            match op {
                Op::Const(_) => {}
                Op::Var(j) => out[*j] = wt,
                Op::Sum(_, terms) => {
                    for &(a, c) in terms {
                        self.adj_tan[c as usize] += wt * a;
                    }
                }
                op => {
                    let [a, b] = pair(op);
                    let (d, h) = partials(op, self.vals[a as usize], self.vals[b as usize], self.vals[i]);
                    let sides = if is_binary(op) { 2 } else { 1 };
                    let t = [self.tan[a as usize], self.tan[b as usize]];
                    for (side, c) in [a, b].into_iter().enumerate().take(sides) {
                        if self.constant[c as usize] {
                            continue;
                        }
                        let mut second = 0.0;
                        for other in 0..sides {
                            let oc = [a, b][other];
                            if !self.constant[oc as usize] && t[other] != 0.0 {
                                second += h[side][other] * t[other];
                            }
                        }
                        let mut add = 0.0;
                        if wt != 0.0 {
                            add += wt * d[side];
                        }
                        if w != 0.0 && second != 0.0 {
                            add += w * second;
                        }
                        self.adj_tan[c as usize] += add;
                    }
                }
            }
        }
        out
    }
}

/// Marks the nodes a root reaches.
fn mark(graph: &Graph, root: Node, seen: &mut [bool]) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if std::mem::replace(&mut seen[n as usize], true) {
            continue;
        }
        for_each_child(&graph.ops[n as usize], |c| stack.push(c));
    }
}

/// Greedy coloring of the pattern's columns so that no two columns of a
/// color have an entry in a common row (each entry is then read directly
/// from one Hessian-vector product).
fn color(pattern: &[(usize, usize)], num_vars: usize) -> Vec<Vec<usize>> {
    // Rows of each column (the pattern is symmetric).
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); num_vars];
    for &(i, j) in pattern {
        rows[j].push(i);
        if i != j {
            rows[i].push(j);
        }
    }
    // Columns with an entry in each row.
    let cols_of_row = rows.clone();
    let mut colors: Vec<Vec<usize>> = Vec::new();
    let mut color_of = vec![usize::MAX; num_vars];
    for j in 0..num_vars {
        if rows[j].is_empty() {
            continue;
        }
        let mut forbidden = std::collections::HashSet::new();
        for &r in &rows[j] {
            for &k in &cols_of_row[r] {
                if color_of[k] != usize::MAX {
                    forbidden.insert(color_of[k]);
                }
            }
        }
        let c = (0..).find(|c| !forbidden.contains(c)).unwrap();
        if c == colors.len() {
            colors.push(Vec::new());
        }
        colors[c].push(j);
        color_of[j] = c;
    }
    colors
}

#[cfg(test)]
pub mod testing {
    //! Random expressions for tests.
    use super::*;

    pub struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        pub fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        pub fn real(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (hi - lo) * (self.next() % 1_000_000) as f64 / 1e6
        }
    }

    /// A random expression over `vars` variables, of the given depth, that
    /// stays in its functions' domains for x in [0.5, 1.5]^n (arguments of
    /// ln, sqrt, divisions, and power bases are kept positive).
    pub fn random(g: &mut Graph, rng: &mut Rng, vars: usize, depth: usize) -> Node {
        if depth == 0 || rng.below(4) == 0 {
            return if rng.below(4) == 0 { g.constant(rng.real(0.5, 2.0)) } else { g.var(rng.below(vars)) };
        }
        let a = random(g, rng, vars, depth - 1);
        // Something positive built from a: exp(a) or 1 + a^2.
        let positive = |g: &mut Graph, rng: &mut Rng, a: Node| {
            if rng.below(2) == 0 {
                let s = g.func(Func::Tanh, a);
                let e = g.scale(0.3, s);
                g.func(Func::Exp, e)
            } else {
                let sq = g.powi(a, 2);
                g.sum(1.0, &[(1.0, sq)])
            }
        };
        match rng.below(13) {
            0 => {
                let b = random(g, rng, vars, depth - 1);
                g.sum(rng.real(-1.0, 1.0), &[(rng.real(-2.0, 2.0), a), (rng.real(-2.0, 2.0), b)])
            }
            1 | 2 => {
                let b = random(g, rng, vars, depth - 1);
                g.mul(a, b)
            }
            3 => {
                let b = random(g, rng, vars, depth - 1);
                let p = positive(g, rng, b);
                g.div(a, p)
            }
            4 => {
                let n = [2, 3, -1, -2][rng.below(4)];
                if n < 0 {
                    let p = positive(g, rng, a);
                    g.powi(p, n)
                } else {
                    g.powi(a, n)
                }
            }
            5 => {
                let p = positive(g, rng, a);
                let c = g.constant(rng.real(-1.5, 2.5));
                g.pow(p, c)
            }
            6 => {
                let p = positive(g, rng, a);
                let b = random(g, rng, vars, depth - 1);
                let t = g.func(Func::Tanh, b);
                g.pow(p, t)
            }
            7 => {
                let p = positive(g, rng, a);
                g.func([Func::Ln, Func::Sqrt][rng.below(2)], p)
            }
            8 => {
                let t = g.func(Func::Tanh, a);
                g.func(Func::Exp, t)
            }
            9 => {
                let t = g.func(Func::Atan, a);
                g.func(Func::Tan, t) // tan(atan(a)) stays away from the poles
            }
            _ => g.func([Func::Sin, Func::Cos, Func::Atan, Func::Tanh][rng.below(4)], a),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Rng, random};
    use super::*;

    /// Checks gradients and the Hessian of a weighted sum of roots against
    /// central differences at x.
    #[allow(clippy::needless_range_loop)] // indexes by variable read more clearly
    fn check(g: &Graph, roots: &[Node], weights: &[f64], x: &[f64], what: &str) {
        let n = x.len();
        let mut ev = Evaluator::new(g, roots, n);
        let f = |x: &[f64]| -> Vec<f64> {
            let mut vals = Vec::new();
            g.eval(x, &mut vals);
            roots.iter().map(|&r| vals[r as usize]).collect()
        };
        let values = ev.eval(x);
        assert!(values.iter().all(|v| v.is_finite()), "{what}: values {values:?}");
        let h = 1e-5;
        let at = |x: &[f64], j: usize, d: f64| -> Vec<f64> {
            let mut y = x.to_vec();
            y[j] += d;
            y
        };
        let close = |a: f64, b: f64, tol: f64| (a - b).abs() <= tol * (1.0 + a.abs().max(b.abs()));
        // Gradients.
        let grads: Vec<Vec<f64>> = (0..roots.len())
            .map(|k| {
                let mut dense = vec![0.0; n];
                for (j, v) in ev.gradient(k) {
                    dense[j] = v;
                }
                dense
            })
            .collect();
        for j in 0..n {
            let (p, m) = (f(&at(x, j, h)), f(&at(x, j, -h)));
            for k in 0..roots.len() {
                let fd = (p[k] - m[k]) / (2.0 * h);
                assert!(close(grads[k][j], fd, 1e-6), "{what}: d root {k} / d x{j}: {} vs {fd}", grads[k][j]);
            }
        }
        // Hessian of the weighted sum, from differences of gradients.
        let lagrangian_grad = |x: &[f64]| -> Vec<f64> {
            let mut e = Evaluator::new(g, roots, n);
            e.eval(x);
            let mut out = vec![0.0; n];
            for (k, w) in weights.iter().enumerate() {
                for (j, v) in e.gradient(k) {
                    out[j] += w * v;
                }
            }
            out
        };
        let mut dense = vec![vec![0.0; n]; n];
        let values = ev.hessian(weights);
        for (&(i, j), &v) in ev.hessian_pattern().iter().zip(&values) {
            assert!(i >= j);
            dense[i][j] = v;
            dense[j][i] = v;
        }
        for j in 0..n {
            let (p, m) = (lagrangian_grad(&at(x, j, h)), lagrangian_grad(&at(x, j, -h)));
            for i in 0..n {
                let fd = (p[i] - m[i]) / (2.0 * h);
                assert!(close(dense[i][j], fd, 1e-5), "{what}: H[{i}][{j}] = {} vs {fd}", dense[i][j]);
            }
        }
    }

    #[test]
    fn derivatives_match_finite_differences() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for case in 0..400 {
            let mut g = Graph::new();
            let n = 1 + rng.below(4);
            let roots: Vec<Node> = (0..1 + rng.below(3)).map(|_| random(&mut g, &mut rng, n, 4)).collect();
            let weights: Vec<f64> = roots.iter().map(|_| rng.real(-2.0, 2.0)).collect();
            let x: Vec<f64> = (0..n).map(|_| rng.real(0.5, 1.5)).collect();
            check(&g, &roots, &weights, &x, &format!("case {case}"));
        }
    }

    #[test]
    fn shared_subexpressions_are_one_node() {
        let mut g = Graph::new();
        let (x, y) = (g.var(0), g.var(1));
        let a = g.mul(x, y);
        let b = g.mul(y, x);
        assert_eq!(a, b);
        let e1 = g.func(Func::Exp, a);
        let e2 = g.func(Func::Exp, b);
        assert_eq!(e1, e2);
        let len = g.len();
        let s = g.sum(1.0, &[(2.0, e1), (3.0, e2)]); // merged: 1 + 5 exp(xy)
        assert_eq!(g.len(), len + 1);
        assert_eq!(g.op(s), &Op::Sum(1.0, vec![(5.0, e1)]));
        // Constants fold.
        let two = g.constant(2.0);
        let c = g.func(Func::Ln, two);
        assert_eq!(g.op(c), &Op::Const(2f64.ln()));
        assert_eq!(g.powi(x, 1), x);
        let zero = g.sum(0.0, &[(1.0, x), (-1.0, x)]);
        assert_eq!(g.op(zero), &Op::Const(0.0));
    }

    #[test]
    fn hessian_pattern_and_coloring() {
        // Separable: sum of exp(x_i): diagonal pattern, one direction.
        let mut g = Graph::new();
        let terms: Vec<(f64, Node)> = (0..6)
            .map(|j| {
                let v = g.var(j);
                (1.0, g.func(Func::Exp, v))
            })
            .collect();
        let f = g.sum(0.0, &terms);
        let ev = Evaluator::new(&g, &[f], 6);
        assert_eq!(ev.hessian_pattern(), &[(0, 0), (1, 1), (2, 2), (3, 3), (4, 4), (5, 5)]);
        assert_eq!(ev.hessian_passes(), 1);
        // x0 * x1 + x2^2 + 3 x3 (linear in x3): no row or column for x3.
        let mut g = Graph::new();
        let (x0, x1, x2, x3) = (g.var(0), g.var(1), g.var(2), g.var(3));
        let p = g.mul(x0, x1);
        let q = g.powi(x2, 2);
        let f = g.sum(0.0, &[(1.0, p), (1.0, q), (3.0, x3)]);
        let mut ev = Evaluator::new(&g, &[f], 4);
        assert_eq!(ev.hessian_pattern(), &[(1, 0), (2, 2)]);
        ev.eval(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ev.hessian(&[2.0]), vec![2.0, 4.0]);
        // A chain x_i * x_{i+1}: tridiagonal coupling needs few colors.
        let mut g = Graph::new();
        let vars: Vec<Node> = (0..20).map(|j| g.var(j)).collect();
        let terms: Vec<(f64, Node)> = vars.windows(2).map(|w| (1.0, g.mul(w[0], w[1]))).collect();
        let f = g.sum(0.0, &terms);
        let ev = Evaluator::new(&g, &[f], 20);
        assert!(ev.hessian_passes() <= 3, "{} passes", ev.hessian_passes());
    }

    #[test]
    fn domain_errors_give_non_finite_values() {
        let mut g = Graph::new();
        let x = g.var(0);
        let l = g.func(Func::Ln, x);
        let s = g.func(Func::Sqrt, x);
        let one = g.constant(1.0);
        let r = g.div(one, x);
        let mut ev = Evaluator::new(&g, &[l, s, r], 1);
        let v = ev.eval(&[-1.0]);
        assert!(v[0].is_nan() && v[1].is_nan() && v[2].is_finite());
        let v = ev.eval(&[0.0]);
        assert!(v[0] == f64::NEG_INFINITY && v[2].is_infinite());
    }
}
