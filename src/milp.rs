//! Branch and bound on the dual simplex (docs/solver-design.md, "Algorithms").
//!
//! Everything here is in the LP's minimization costs, without the objective
//! offset; `solve` converts back to the problem's own sense.
//!
//! Node selection is depth-first until the first integer solution is found
//! (to get an incumbent quickly), then best-bound with plunging: from the
//! node with the best bound, a worker follows children down while their
//! bounds stay near the best one, which finds solutions that the best bound
//! alone would reach late (as in SCIP). Each child starts from its parent's
//! final basis.
//!
//! Branching is reliability branching (as in HiGHS and SCIP): candidates are
//! scored by pseudocosts, the average objective gain per unit of bound change
//! seen so far in each direction. A candidate whose pseudocosts rest on too
//! few observations is strong-branched instead: both child LPs are solved,
//! which gives exact gains, and the child solves are kept for the children.
//!
//! With an incumbent, each node's reduced costs tighten its integer columns'
//! bounds (reduced-cost fixing), for the node and everything below it. When
//! that fixes enough columns at the root, the search restarts: new cut
//! rounds with the tighter root bounds, then a new tree, with the incumbent
//! and the pseudocosts kept.
//!
//! With `want_all`, nodes whose bound ties the incumbent are kept, and an
//! integer-feasible node is split on an unfixed variable (x <= v-1, x = v,
//! x >= v+1) until every variable is fixed, so each solution is collected
//! exactly once, at a leaf.

use crate::cuts;
use crate::heuristics::{Context, Dive, Heuristics};
use crate::lp::Lp;
use crate::propagation::{Propagator, Seeds, Trail};
use crate::simplex::{Basis, LpStatus, Options, Simplex};
use std::cmp::Ordering as Order;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const INTEGRALITY_TOLERANCE: f64 = 1e-6;
pub const OPTIMALITY_TOLERANCE: f64 = 1e-6;
/// Observations per direction before a pseudocost is trusted.
const RELIABILITY: u32 = 4;
/// Most candidates strong-branched at one node.
const MAX_STRONG_BRANCH: usize = 20;
/// Stop strong branching after this many candidates fail to beat the best score.
const LOOKAHEAD: usize = 8;
/// Floor on a gain in the product score, so a zero side doesn't erase the other.
const SCORE_EPSILON: f64 = 1e-6;
/// Reduced costs below this fix nothing (they may be dual infeasibilities).
const MIN_REDUCED_COST: f64 = 1e-6;
/// The search starts over, with new cut rounds, when the root has fixed this
/// share of the integer columns that were free; at most `MAX_RESTARTS` times.
const RESTART_SHARE: f64 = 0.2;
const MAX_RESTARTS: usize = 10;
/// A worker plunges (goes on with a child of the node it has just finished)
/// while the child's bound is within this share of the gap between the best
/// bound and the cutoff, for up to half the depth of the tree so far.
const PLUNGE_SHARE: f64 = 0.25;

/// How far an objective may be from `best` and still tie it.
pub fn tie_tolerance(best: f64) -> f64 {
    OPTIMALITY_TOLERANCE * best.abs().max(1.0)
}

pub struct Settings<'a> {
    /// Collect every solution that ties the optimum.
    pub want_all: bool,
    /// With `want_all`: stop once this many optimal solutions are collected.
    pub solution_limit: Option<usize>,
    /// Stop at the first integer-feasible solution (satisfy, or the
    /// zero-objective search for a MILP with an unbounded relaxation).
    pub stop_at_first: bool,
    pub deadline: Option<Instant>,
    /// Only solutions with objective below this are wanted (a sub-MILP
    /// looking for an improvement on an incumbent).
    pub cutoff: f64,
    pub node_limit: Option<usize>,
    /// A sub-MILP inside a heuristic: no cuts, heuristics, or debug output.
    pub sub_milp: bool,
    /// Where the top-level search reports incumbents and bounds (never set
    /// for sub-MILPs, whose columns are their own).
    pub report: Option<&'a dyn crate::config::Report>,
}

impl Default for Settings<'_> {
    fn default() -> Self {
        Settings {
            want_all: false,
            solution_limit: None,
            stop_at_first: false,
            deadline: None,
            cutoff: f64::INFINITY,
            node_limit: None,
            sub_milp: false,
            report: None,
        }
    }
}

/// Heuristics run at the root, then a dive every `DIVE_FREQUENCY` nodes and
/// RINS every `RINS_FREQUENCY` nodes, while they have used at most
/// `HEURISTIC_SHARE` of the search time.
const DIVE_FREQUENCY: usize = 100;
const RINS_FREQUENCY: usize = 200;
const HEURISTIC_SHARE: f64 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The search finished: the tree is exhausted, or `stop_at_first` hit.
    Complete,
    TimeLimit,
    NodeLimit,
    /// As many optimal solutions as `solution_limit` asked for are
    /// collected; nodes that may hold more are still open.
    SolutionLimit,
    /// The root LP is dual infeasible: the relaxation is unbounded or
    /// infeasible. Nothing else was searched.
    RootUnbounded,
}

pub struct Incumbent {
    pub objective: f64,
    pub x: Vec<f64>,
}

pub struct MilpResult {
    pub end: End,
    pub incumbent: Option<Incumbent>,
    /// With `want_all`: every solution found that ties the incumbent.
    pub solutions: Vec<Vec<f64>>,
    /// A proven lower bound on the optimum.
    pub bound: f64,
}

struct Node {
    lower: Vec<f64>,
    upper: Vec<f64>,
    basis: Option<Basis>,
    /// Lower bound on any solution in this node (the parent's LP objective).
    bound: f64,
    depth: usize,
    seq: usize,
    /// How this node was made, so its LP result can update the pseudocosts.
    /// None for the root and for children whose gain strong branching measured.
    branch: Option<Branch>,
    /// The column whose bound changed from the parent (propagation starts
    /// from its rows); None for the root, which propagates every row.
    changed: Option<usize>,
}

#[derive(Clone, Copy)]
struct Branch {
    col: usize,
    dir: usize,
    /// How far the bound change moved the column from its LP value.
    delta: f64,
    parent_objective: f64,
}

pub(crate) const DOWN: usize = 0;
pub(crate) const UP: usize = 1;

/// Per-column average objective gain per unit change, for each direction.
struct Pseudocosts {
    sum: [Vec<f64>; 2],
    count: [Vec<u32>; 2],
    total_sum: [f64; 2],
    total_count: [u32; 2],
}

impl Pseudocosts {
    fn new(n: usize) -> Self {
        Pseudocosts {
            sum: [vec![0.0; n], vec![0.0; n]],
            count: [vec![0; n], vec![0; n]],
            total_sum: [0.0; 2],
            total_count: [0; 2],
        }
    }

    fn record(&mut self, col: usize, dir: usize, delta: f64, gain: f64) {
        let unit = gain.max(0.0) / delta.max(INTEGRALITY_TOLERANCE);
        self.sum[dir][col] += unit;
        self.count[dir][col] += 1;
        self.total_sum[dir] += unit;
        self.total_count[dir] += 1;
    }

    /// The column's pseudocost, or the average over all columns if it has none.
    fn get(&self, col: usize, dir: usize) -> f64 {
        if self.count[dir][col] > 0 {
            self.sum[dir][col] / self.count[dir][col] as f64
        } else if self.total_count[dir] > 0 {
            self.total_sum[dir] / self.total_count[dir] as f64
        } else {
            1.0
        }
    }

    fn reliable(&self, col: usize) -> bool {
        self.count[DOWN][col] >= RELIABILITY && self.count[UP][col] >= RELIABILITY
    }
}

fn score(down_gain: f64, up_gain: f64) -> f64 {
    down_gain.max(SCORE_EPSILON) * up_gain.max(SCORE_EPSILON)
}

/// A column that violates integrality (or its domain) and the bounds of its
/// two children.
pub(crate) struct Candidate {
    pub(crate) col: usize,
    pub(crate) value: f64,
    pub(crate) down: f64,
    pub(crate) up: f64,
}

impl Candidate {
    fn delta(&self, dir: usize) -> f64 {
        if dir == DOWN { self.value - self.down } else { self.up - self.value }
    }
}

/// Result of solving one child LP during strong branching.
enum Probe {
    /// Infeasible or cut off: the child need not be explored.
    Pruned,
    Solved {
        objective: f64,
        basis: Option<Basis>,
    },
    /// No information (numerical trouble).
    Unknown,
}

// BinaryHeap is a max-heap; the "greatest" node is the one to explore next:
// lowest bound, then deepest, then most recently created.
impl Ord for Node {
    fn cmp(&self, other: &Self) -> Order {
        other.bound.total_cmp(&self.bound).then(self.depth.cmp(&other.depth)).then(self.seq.cmp(&other.seq))
    }
}
impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Order> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Order::Equal
    }
}
impl Eq for Node {}

/// Allowed integer ranges of a column with a gapped domain.
pub type Domain = Option<Vec<(i64, i64)>>;

/// A MILP for branch and bound: an LP plus integrality, per column.
#[derive(Clone)]
pub struct Milp {
    pub lp: Lp,
    pub integer: Vec<bool>,
    pub domain: Vec<Domain>,
}

pub(crate) fn in_domain(domain: &Domain, v: f64) -> bool {
    match domain {
        None => true,
        Some(ranges) => ranges.iter().any(|&(a, b)| a as f64 <= v && v <= b as f64),
    }
}

/// The largest allowed integer <= v, or -inf if there is none.
pub fn floor_allowed(domain: &Domain, v: f64) -> f64 {
    let f = (v + INTEGRALITY_TOLERANCE).floor();
    match domain {
        None => f,
        Some(ranges) => {
            ranges.iter().rev().find(|&&(a, _)| a as f64 <= f).map_or(f64::NEG_INFINITY, |&(_, b)| f.min(b as f64))
        }
    }
}

/// The smallest allowed integer >= v, or +inf if there is none.
pub fn ceil_allowed(domain: &Domain, v: f64) -> f64 {
    let c = (v - INTEGRALITY_TOLERANCE).ceil();
    match domain {
        None => c,
        Some(ranges) => ranges.iter().find(|&&(_, b)| b as f64 >= c).map_or(f64::INFINITY, |&(a, _)| c.max(a as f64)),
    }
}

/// The integer columns that violate integrality or their domain.
pub(crate) fn candidates(milp: &Milp, x: &[f64], lower: &[f64], upper: &[f64]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (col, &value) in x.iter().enumerate() {
        if !milp.integer[col] {
            continue;
        }
        let dom = &milp.domain[col];
        let (down, up) = if (value - value.round()).abs() > INTEGRALITY_TOLERANCE {
            (floor_allowed(dom, value), ceil_allowed(dom, value))
        } else if !in_domain(dom, value.round()) {
            // In a domain gap: step past it.
            (floor_allowed(dom, value.round() - 1.0), ceil_allowed(dom, value.round() + 1.0))
        } else {
            continue;
        };
        // A side outside the node's bounds is empty; clamp so its delta stays sane.
        let down = if down >= lower[col] { down } else { f64::NEG_INFINITY };
        let up = if up <= upper[col] { up } else { f64::INFINITY };
        out.push(Candidate { col, value, down, up });
    }
    out
}

/// What strong branching probes with: a node, the solver that stands at its
/// optimal basis, and, when the worker is alone in the search (as it is at
/// the root), the threads that the other workers would be using.
struct Prober<'a> {
    simplex: &'a Simplex<'a>,
    propagator: &'a Propagator,
    node: &'a Node,
    basis: Option<&'a Basis>,
    opts: &'a Options,
    helpers: usize,
}

impl Prober<'_> {
    /// Solves the child of the node in direction `dir` of candidate `c`, on
    /// a copy of the node's solver.
    fn child(&self, c: &Candidate, dir: usize) -> Probe {
        let node = self.node;
        let (lo, hi) = if dir == DOWN { (node.lower[c.col], c.down) } else { (c.up, node.upper[c.col]) };
        if lo > hi {
            return Probe::Pruned;
        }
        let (mut lower, mut upper) = (node.lower.clone(), node.upper.clone());
        lower[c.col] = lo;
        upper[c.col] = hi;
        // An infeasible child is often found by propagation alone, without an LP.
        if self.propagator.propagate(&mut lower, &mut upper, Seeds::Cols(&[c.col]), &mut Trail::default()).is_err() {
            return Probe::Pruned;
        }
        let sol = self.simplex.clone().solve(&lower, &upper, self.basis, self.opts);
        match sol.status {
            LpStatus::Optimal => Probe::Solved { objective: sol.objective, basis: sol.basis },
            LpStatus::Infeasible | LpStatus::ObjectiveBound | LpStatus::UnboundedOrInfeasible => Probe::Pruned,
            LpStatus::TimeLimit | LpStatus::Error(_) => Probe::Unknown,
        }
    }

    /// Both children of each candidate, on this thread and the helpers.
    fn children(&self, cands: &[&Candidate]) -> Vec<[Probe; 2]> {
        let threads = (self.helpers + 1).min(2 * cands.len());
        if threads <= 1 {
            return cands.iter().map(|c| [self.child(c, DOWN), self.child(c, UP)]).collect();
        }
        // Every thread takes the next child LP until none is left.
        let next = AtomicUsize::new(0);
        let done: Mutex<Vec<Option<Probe>>> = Mutex::new((0..2 * cands.len()).map(|_| None).collect());
        let work = || loop {
            let task = next.fetch_add(1, Ordering::Relaxed);
            if task >= 2 * cands.len() {
                break;
            }
            let probe = self.child(cands[task / 2], task % 2);
            done.lock().unwrap()[task] = Some(probe);
        };
        crate::config::team(threads, |_| work(), work);
        let mut done = done.into_inner().unwrap().into_iter().map(|p| p.expect("every child LP was taken"));
        (0..cands.len()).map(|_| [done.next().unwrap(), done.next().unwrap()]).collect()
    }
}

/// The branching decision at a node.
// One is made per node and taken apart at once, so its size doesn't matter.
#[allow(clippy::large_enum_variant)]
enum Decision {
    /// Every child is infeasible or cut off.
    Prune,
    /// Branch on this candidate, with strong-branching results if it has them.
    Branch(Candidate, Option<[Probe; 2]>),
}

/// Reliability branching: pseudocost scores, with strong branching for
/// candidates whose pseudocosts aren't reliable yet. The pseudocosts are
/// locked only to read and record them, not while probing.
fn choose(prober: &Prober, objective: f64, mut cands: Vec<Candidate>, pseudocosts: &Mutex<Pseudocosts>) -> Decision {
    let deadline = prober.opts.deadline;
    let estimate = |pc: &Pseudocosts, c: &Candidate| {
        let gain = |dir| if c.delta(dir).is_finite() { pc.get(c.col, dir) * c.delta(dir) } else { f64::INFINITY };
        score(gain(DOWN), gain(UP))
    };
    {
        let pc = pseudocosts.lock().unwrap();
        cands.sort_by(|a, b| estimate(&pc, b).total_cmp(&estimate(&pc, a)));
    }

    let mut best: Option<(f64, usize, Option<[Probe; 2]>)> = None;
    let (mut strong, mut no_improve) = (0, 0);
    // With helpers, the candidates after this one that are likely to be
    // probed next are probed with it, and wait here for their turn.
    let mut probed: Vec<Option<[Probe; 2]>> = cands.iter().map(|_| None).collect();
    for (k, c) in cands.iter().enumerate() {
        let timed_out = crate::config::expired(deadline);
        let reliable = pseudocosts.lock().unwrap().reliable(c.col);
        let (s, probes) = if !reliable && strong < MAX_STRONG_BRANCH && no_improve < LOOKAHEAD && !timed_out {
            if probed[k].is_none() {
                let room = (prober.helpers / 2 + 1).min(MAX_STRONG_BRANCH - strong);
                let batch: Vec<usize> = {
                    let pc = pseudocosts.lock().unwrap();
                    (k..cands.len()).filter(|&i| i == k || !pc.reliable(cands[i].col)).take(room).collect()
                };
                let targets: Vec<&Candidate> = batch.iter().map(|&i| &cands[i]).collect();
                for (i, probes) in batch.iter().zip(prober.children(&targets)) {
                    probed[*i] = Some(probes);
                }
            }
            strong += 1;
            let probes = probed[k].take().expect("just probed");
            let mut gains = [0.0; 2];
            let mut pc = pseudocosts.lock().unwrap();
            for dir in [DOWN, UP] {
                gains[dir] = match &probes[dir] {
                    Probe::Solved { objective: o, .. } => {
                        pc.record(c.col, dir, c.delta(dir), o - objective);
                        o - objective
                    }
                    Probe::Pruned => f64::INFINITY,
                    Probe::Unknown => pc.get(c.col, dir) * c.delta(dir),
                };
            }
            drop(pc);
            match (&probes[DOWN], &probes[UP]) {
                (Probe::Pruned, Probe::Pruned) => return Decision::Prune,
                // One side is empty, so the node reduces to the other side:
                // branch here at once.
                (Probe::Pruned, _) | (_, Probe::Pruned) => {
                    let c = cands.swap_remove(k);
                    return Decision::Branch(c, Some(probes));
                }
                _ => {}
            }
            (score(gains[DOWN], gains[UP]), Some(probes))
        } else {
            (estimate(&pseudocosts.lock().unwrap(), c), None)
        };
        if best.as_ref().is_none_or(|(b, _, _)| s > *b) {
            best = Some((s, k, probes));
            no_improve = 0;
        } else {
            no_improve += 1;
        }
    }
    let (_, k, probes) = best.expect("choose needs at least one candidate");
    Decision::Branch(cands.swap_remove(k), probes)
}

/// One branch-and-bound search: the problem, what is wanted, and the state
/// that its workers share. There is one worker, on the calling thread, or,
/// with `Options::threads` above 1, one per thread.
struct Search<'a> {
    milp: &'a Milp,
    /// The LP with the root cuts.
    lp: &'a Lp,
    propagator: Propagator,
    root_lower: Vec<f64>,
    root_upper: Vec<f64>,
    want_all: bool,
    solution_limit: Option<usize>,
    stop_at_first: bool,
    deadline: Option<Instant>,
    /// The cutoff before there is an incumbent.
    first_cutoff: f64,
    node_limit: Option<usize>,
    /// Whether to run heuristics (not in a sub-MILP, nor without integers).
    heuristics: bool,
    /// Whether the root may ask for a restart.
    restartable: bool,
    /// The root bounds to start over from, once the root has asked.
    restart: Mutex<Option<(Vec<f64>, Vec<f64>)>>,
    /// How many workers there are.
    threads: usize,
    debug: bool,
    started: Instant,
    /// The open nodes, and what the workers are on.
    tree: Mutex<Tree>,
    /// Signals nodes to take, or the end.
    changed: Condvar,
    best: Mutex<Best>,
    /// The cutoff that the incumbent implies (an f64's bits) and whether
    /// there is an incumbent, for looks without the lock.
    cutoff: AtomicU64,
    found: AtomicBool,
    pseudocosts: Mutex<Pseudocosts>,
    /// Nodes whose LP was solved, nodes created, and the deepest node so far.
    nodes: AtomicUsize,
    seq: AtomicUsize,
    depth: AtomicUsize,
    /// Stops every worker's LP solves when the search has ended.
    halt: Arc<AtomicBool>,
}

struct Tree {
    /// Children of the latest nodes, explored depth first until there is an
    /// incumbent; then every open node is in the heap, best bound first.
    stack: Vec<Node>,
    heap: BinaryHeap<Node>,
    /// The bound of the node each worker is on, if it is on one.
    busy: Vec<Option<f64>>,
    /// How the search ended, once it has.
    end: Option<End>,
}

struct Best {
    incumbent: Option<Incumbent>,
    /// With `want_all`: every solution found that ties the incumbent.
    solutions: Vec<Vec<f64>>,
}

/// What a worker gets when it asks for a node.
#[allow(clippy::large_enum_variant)] // one per node, taken apart at once
enum Take {
    /// A node, and a lower bound on every open node, this one included.
    Node(Node, f64),
    /// Nothing now: other workers are on nodes that may have children.
    Wait,
    /// The search has ended.
    Over,
}

impl<'a> Search<'a> {
    /// Runs the search and returns what each worker's nodes added up to.
    /// The calling thread's worker processes the root alone (every other
    /// node depends on its result); then, with several threads, the others
    /// join it. Each takes the best open node, as one worker would; a worker
    /// that takes the child of the node it has just finished starts its LP
    /// from where it stands.
    fn run(&'a self, report: Option<&'a dyn crate::config::Report>) -> Result<Vec<Totals>, String> {
        let work = || {
            let mut first = Worker::new(self, 0, report);
            let threads = if first.step()? { self.threads } else { 1 };
            let (totals, others) = crate::config::team(threads, |id| Worker::new(self, id, None).run(), || first.run());
            // Another worker may have improved the incumbent after the first
            // one's last look: what is reported last is what is returned.
            first.tell();
            std::iter::once(totals).chain(others).collect()
        };
        if self.threads == 1 { work() } else { crate::config::halting(&self.halt, work) }
    }

    fn cutoff(&self) -> f64 {
        f64::from_bits(self.cutoff.load(Ordering::Acquire))
    }

    /// The next node for worker `id`: the latest one until there is an
    /// incumbent, then the one with the best bound.
    fn take(&self, id: usize) -> Take {
        let mut tree = self.tree.lock().unwrap();
        if tree.end.is_some() {
            return Take::Over;
        }
        let node = if self.found.load(Ordering::Acquire) {
            let Tree { stack, heap, .. } = &mut *tree;
            heap.extend(stack.drain(..));
            heap.pop()
        } else {
            tree.stack.pop().or_else(|| tree.heap.pop())
        };
        match node {
            Some(node) => {
                tree.busy[id] = Some(node.bound);
                let open = tree.stack.iter().chain(tree.heap.peek()).map(|nd| nd.bound);
                let bound = open.chain(tree.busy.iter().flatten().copied()).fold(f64::INFINITY, f64::min);
                Take::Node(node, bound)
            }
            None if tree.busy.iter().all(|b| b.is_none()) => {
                tree.end = Some(End::Complete);
                self.changed.notify_all();
                Take::Over
            }
            None => {
                // Until a busy worker gives children back, or for a moment.
                drop(self.changed.wait_timeout(tree, Duration::from_millis(10)).unwrap());
                Take::Wait
            }
        }
    }

    /// Worker `id` is done with its node, which left these children, besides
    /// one that the worker goes on with, if `kept` has its bound.
    fn give(&self, id: usize, children: Vec<Node>, kept: Option<f64>) {
        let mut tree = self.tree.lock().unwrap();
        tree.stack.extend(children);
        tree.busy[id] = kept;
        self.changed.notify_all();
    }

    /// Ends the search (unless it has ended already), putting back the node
    /// that worker `id` was on, if it didn't finish it.
    fn stop(&self, id: usize, end: End, node: Option<Node>) {
        let mut tree = self.tree.lock().unwrap();
        tree.end.get_or_insert(end);
        tree.stack.extend(node);
        tree.busy[id] = None;
        self.halt.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    /// Makes `x` the incumbent if it improves on it, and returns whether it did.
    fn improve(&self, x: Vec<f64>) -> bool {
        let obj = self.lp.objective(&x);
        let mut best = self.best.lock().unwrap();
        if best.incumbent.as_ref().is_some_and(|i| obj >= i.objective) {
            return false;
        }
        best.incumbent = Some(Incumbent { objective: obj, x });
        if self.want_all {
            let limit = obj + tie_tolerance(obj);
            best.solutions.retain(|s| self.lp.objective(s) <= limit);
        }
        let cutoff = if self.want_all { obj + tie_tolerance(obj) } else { obj - tie_tolerance(obj) };
        self.cutoff.store(cutoff.to_bits(), Ordering::Release);
        self.found.store(true, Ordering::Release);
        true
    }

    /// Whether the solutions collected are as many as were asked for and
    /// optimal: no open node, the best of which has this `bound`, can beat
    /// them.
    fn enough(&self, bound: f64) -> bool {
        let Some(limit) = self.solution_limit else { return false };
        let best = self.best.lock().unwrap();
        let optimal = |i: &Incumbent| bound >= i.objective - tie_tolerance(i.objective);
        best.solutions.len() >= limit && best.incumbent.as_ref().is_some_and(optimal)
    }

    /// Whether one worker is on a node and no other node is open: the other
    /// workers have nothing to do.
    fn alone(&self) -> bool {
        if self.threads == 1 {
            return false;
        }
        let tree = self.tree.lock().unwrap();
        tree.stack.is_empty() && tree.heap.is_empty() && tree.busy.iter().flatten().count() == 1
    }

    /// A new node number (later nodes win ties in the heap).
    fn next_seq(&self) -> usize {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// What one worker's nodes added up to.
#[derive(Default)]
struct Totals {
    lp_iterations: usize,
    lp_seconds: f64,
    propagation_pruned: usize,
    propagation_changes: usize,
    /// Bounds tightened by reduced costs.
    reduced_cost_changes: usize,
    heuristics: Option<crate::heuristics::Stats>,
}

/// What processing a node led to.
enum Step {
    /// The node is done; these are its children (none if it was pruned or
    /// gave a solution).
    Children(Vec<Node>),
    /// The search ends; the node goes back if it wasn't finished.
    Stop(End, Option<Node>),
    /// The root fixed enough columns for new cut rounds to pay: the search
    /// starts over from these root bounds.
    Restart(Vec<f64>, Vec<f64>),
}

/// One thread's part of a search: its LP solver, heuristics, and counts.
struct Worker<'a> {
    search: &'a Search<'a>,
    id: usize,
    /// Where the calling thread's worker reports incumbents and bounds
    /// (the observer lives on that thread), and the last objective it did.
    report: Option<&'a dyn crate::config::Report>,
    reported: f64,
    /// One solver for every node: a child solved right after its parent
    /// starts from the parent's factorization.
    simplex: Simplex<'a>,
    heuristics: Option<Heuristics>,
    trail: Trail,
    /// Plunging: the child that the worker goes on with, how many it has
    /// gone on with since it last took a node from the tree, and the best
    /// bound of the tree at that time.
    next: Option<Node>,
    plunged: usize,
    floor: f64,
    /// The columns whose bounds the reduced costs have just tightened.
    tightened: Vec<usize>,
    totals: Totals,
}

impl<'a> Worker<'a> {
    fn new(search: &'a Search<'a>, id: usize, report: Option<&'a dyn crate::config::Report>) -> Self {
        // An incumbent that the search starts with (after a restart) was
        // reported by the search that found it.
        let reported = search.best.lock().unwrap().incumbent.as_ref().map_or(f64::INFINITY, |i| i.objective);
        Worker {
            search,
            id,
            report,
            reported,
            simplex: Simplex::new(search.lp),
            heuristics: search.heuristics.then(|| Heuristics::new(search.lp)),
            trail: Trail::default(),
            next: None,
            plunged: 0,
            floor: f64::NEG_INFINITY,
            tightened: Vec::new(),
            totals: Totals::default(),
        }
    }

    /// Processes nodes until the search ends.
    fn run(&mut self) -> Result<Totals, String> {
        while self.step()? {}
        self.tell();
        self.totals.heuristics = self.heuristics.take().map(|h| h.stats);
        Ok(std::mem::take(&mut self.totals))
    }

    /// Takes one node, if there is one, and processes it. False once the
    /// search has ended.
    fn step(&mut self) -> Result<bool, String> {
        let s = self.search;
        self.tell();
        let node = match self.next.take() {
            Some(node) => node,
            None => match s.take(self.id) {
                Take::Node(node, bound) => {
                    if s.enough(bound) {
                        s.stop(self.id, End::SolutionLimit, Some(node));
                        return Ok(false);
                    }
                    (self.floor, self.plunged) = (bound, 0);
                    // With an incumbent, a node from the tree is the one with
                    // the best bound, so `bound` is the global one.
                    if let Some(r) = self.report.filter(|_| s.found.load(Ordering::Acquire)) {
                        r.bound(bound);
                    }
                    node
                }
                Take::Over => return Ok(false),
                Take::Wait => {
                    if crate::config::expired(s.deadline) {
                        s.stop(self.id, End::TimeLimit, None);
                    }
                    return Ok(true);
                }
            },
        };
        let cut = s.cutoff();
        if node.bound > cut {
            s.give(self.id, Vec::new(), None);
            return Ok(true);
        }
        if crate::config::expired(s.deadline) {
            s.stop(self.id, End::TimeLimit, Some(node));
            return Ok(false);
        }
        if s.node_limit.is_some_and(|limit| s.nodes.load(Ordering::Relaxed) >= limit) {
            s.stop(self.id, End::NodeLimit, Some(node));
            return Ok(false);
        }
        match self.process(node, cut) {
            Ok(Step::Children(mut children)) => {
                // Plunge into the preferred child (the last), if it is promising.
                let near = |nd: &Node| nd.bound <= self.floor + PLUNGE_SHARE * (s.cutoff() - self.floor);
                if s.found.load(Ordering::Acquire)
                    && self.plunged < s.depth.load(Ordering::Relaxed) / 2
                    && children.last().is_some_and(near)
                {
                    self.plunged += 1;
                    self.next = children.pop();
                }
                s.give(self.id, children, self.next.as_ref().map(|nd| nd.bound));
            }
            Ok(Step::Stop(end, node)) => {
                s.stop(self.id, end, node);
                return Ok(false);
            }
            Ok(Step::Restart(lower, upper)) => {
                // This search is over; another follows.
                *s.restart.lock().unwrap() = Some((lower, upper));
                s.stop(self.id, End::Complete, None);
                return Ok(false);
            }
            Err(message) => {
                s.stop(self.id, End::Complete, None);
                return Err(message);
            }
        }
        Ok(true)
    }

    /// Reports the incumbent, if it is better than the last one reported.
    fn tell(&mut self) {
        let Some(r) = self.report else { return };
        let x = {
            let best = self.search.best.lock().unwrap();
            match &best.incumbent {
                Some(i) if i.objective < self.reported => {
                    self.reported = i.objective;
                    i.x.clone()
                }
                _ => return,
            }
        };
        r.solution(&x);
    }

    /// Makes `x` the incumbent if it improves on it (and reports it).
    fn improve(&mut self, x: Vec<f64>) -> bool {
        let improved = self.search.improve(x);
        if improved {
            self.tell();
        }
        improved
    }

    /// Reduced-cost fixing. At an optimal basis with objective z, a point x
    /// of the node has objective at least z + d_j (x_j - l_j) for a column
    /// at its lower bound with reduced cost d_j > 0 (and likewise at its
    /// upper bound), so with the cutoff, x_j <= l_j + (cutoff - z) / d_j.
    /// Tightens the node's bounds on integer columns that way, listing the
    /// columns in `self.tightened`, and propagates the changes. Err if that
    /// leaves no point of the node below the cutoff.
    fn fix_by_reduced_costs(
        &mut self,
        node: &mut Node,
        x: &[f64],
        reduced_costs: &[f64],
        objective: f64,
        cut: f64,
    ) -> Result<(), crate::propagation::Conflict> {
        let milp = self.search.milp;
        let gap = cut + tie_tolerance(cut) - objective;
        self.tightened.clear();
        for (j, &d) in reduced_costs.iter().enumerate() {
            let (l, u) = (node.lower[j], node.upper[j]);
            if !milp.integer[j] || l == u {
                continue;
            }
            if d > MIN_REDUCED_COST && x[j] - l <= 1e-9 {
                let bound = floor_allowed(&milp.domain[j], l + gap / d);
                if bound < u {
                    node.upper[j] = bound;
                    self.tightened.push(j);
                }
            } else if d < -MIN_REDUCED_COST && u - x[j] <= 1e-9 {
                let bound = ceil_allowed(&milp.domain[j], u + gap / d);
                if bound > l {
                    node.lower[j] = bound;
                    self.tightened.push(j);
                }
            }
        }
        self.totals.reduced_cost_changes += self.tightened.len();
        let before = self.trail.changes.len();
        let seeds = Seeds::Cols(&self.tightened);
        let result = self.search.propagator.propagate(&mut node.lower, &mut node.upper, seeds, &mut self.trail);
        self.totals.propagation_changes += self.trail.changes.len() - before;
        result
    }

    /// Propagation, the LP, heuristics, and branching for one node.
    fn process(&mut self, mut node: Node, cut: f64) -> Result<Step, String> {
        let s = self.search;
        let (milp, lp) = (s.milp, s.lp);
        let n = lp.num_col;
        let pruned = Ok(Step::Children(Vec::new()));
        s.depth.fetch_max(node.depth, Ordering::Relaxed);

        // Propagate the branching change (every row at the root) before the LP.
        self.trail.changes.clear();
        let changed = node.changed.map(|j| [j]);
        let seeds = changed.as_ref().map_or(Seeds::All, |c| Seeds::Cols(c));
        if s.propagator.propagate(&mut node.lower, &mut node.upper, seeds, &mut self.trail).is_err() {
            self.totals.propagation_pruned += 1;
            return pruned;
        }
        self.totals.propagation_changes += self.trail.changes.len();

        let opts = Options { deadline: s.deadline, objective_bound: cut, ..Options::default() };
        // Reduced costs are for fixing, which takes an incumbent: there is
        // one, or (at the root) the heuristics are about to look for one.
        let node_opts = Options { reduced_costs: cut.is_finite() || node.depth == 0, ..opts.clone() };
        let started = Instant::now();
        let mut sol = self.simplex.solve(&node.lower, &node.upper, node.basis.as_ref(), &node_opts);
        let nodes = s.nodes.fetch_add(1, Ordering::Relaxed) + 1;
        self.totals.lp_iterations += sol.iterations;
        self.totals.lp_seconds += started.elapsed().as_secs_f64();
        if matches!(sol.status, LpStatus::Error(_)) && node.basis.is_some() {
            sol = self.simplex.solve(&node.lower, &node.upper, None, &node_opts);
        }
        match &sol.status {
            LpStatus::Optimal => {}
            LpStatus::Infeasible | LpStatus::ObjectiveBound => return pruned,
            LpStatus::UnboundedOrInfeasible if node.depth == 0 => return Ok(Step::Stop(End::RootUnbounded, None)),
            // Bound changes cannot make a bounded relaxation unbounded, so this
            // is numerical trouble in an infeasible node.
            LpStatus::UnboundedOrInfeasible => return pruned,
            LpStatus::TimeLimit => return Ok(Step::Stop(End::TimeLimit, Some(node))),
            LpStatus::Error(e) => return Err(format!("LP solver failed: {e}")),
        }
        if let Some(b) = node.branch {
            s.pseudocosts.lock().unwrap().record(b.col, b.dir, b.delta, sol.objective - b.parent_objective);
        }
        let bound = node.bound.max(sol.objective);
        if let Some(r) = self.report.filter(|_| node.depth == 0) {
            r.bound(bound);
        }
        if bound > cut {
            return pruned;
        }
        let x = sol.col_value;

        let depth = node.depth + 1;
        let child = |lower: Vec<f64>, upper: Vec<f64>, col: usize| Node {
            lower,
            upper,
            basis: sol.basis.clone(),
            bound,
            depth,
            seq: s.next_seq(),
            branch: None,
            changed: Some(col),
        };

        let mut cands = candidates(milp, &x, &node.lower, &node.upper);
        if !cands.is_empty() {
            if let Some(mut h) = self.heuristics.take() {
                let root = node.depth == 0;
                let share_ok = h.stats.seconds <= HEURISTIC_SHARE * s.started.elapsed().as_secs_f64();
                let dive_now = !root && share_ok && nodes.is_multiple_of(DIVE_FREQUENCY);
                let rins_now =
                    !root && share_ok && s.found.load(Ordering::Acquire) && nodes.is_multiple_of(RINS_FREQUENCY);
                // Heuristic solutions must strictly improve on the incumbent.
                let context = || {
                    let best = s.best.lock().unwrap();
                    let cutoff =
                        best.incumbent.as_ref().map_or(s.first_cutoff, |i| i.objective - tie_tolerance(i.objective));
                    Context { milp, lp, cutoff, deadline: s.deadline }
                };
                let guide = || s.best.lock().unwrap().incumbent.as_ref().map(|i| i.x.clone());
                let mut found: Vec<(Vec<f64>, &str)> = Vec::new();
                if root {
                    for (kind, name) in [(Dive::Fractional, "fractional dive"), (Dive::Coefficient, "coefficient dive")]
                    {
                        if let Some(y) =
                            h.dive(&context(), &node.lower, &node.upper, &x, sol.basis.as_ref(), kind, None)
                        {
                            self.improve(y.clone());
                            found.push((y, name));
                        }
                    }
                    if let Some(y) = h.rens(&context(), &x, &node.lower, &node.upper) {
                        self.improve(y.clone());
                        found.push((y, "RENS"));
                    }
                }
                if dive_now {
                    let guide = guide();
                    let kind = match (&guide, (nodes / DIVE_FREQUENCY) % 2) {
                        (Some(_), _) => Dive::Guided,
                        (None, 0) => Dive::Fractional,
                        (None, _) => Dive::Coefficient,
                    };
                    let basis = sol.basis.as_ref();
                    if let Some(y) = h.dive(&context(), &node.lower, &node.upper, &x, basis, kind, guide.as_deref()) {
                        self.improve(y.clone());
                        found.push((y, "dive"));
                    }
                }
                if rins_now
                    && let Some(guide) = guide()
                    && let Some(y) = h.rins(&context(), &x, &guide, &s.root_lower, &s.root_upper)
                {
                    self.improve(y.clone());
                    found.push((y, "RINS"));
                }
                self.heuristics = Some(h);
                if s.debug {
                    for (y, name) in &found {
                        let t = s.started.elapsed().as_secs_f64();
                        crate::config::debug_log!("incumbent {} ({name}) at node {nodes}, {t:.2}s", lp.objective(y));
                    }
                }
                if s.stop_at_first && s.found.load(Ordering::Acquire) {
                    return Ok(Step::Stop(End::Complete, None));
                }
                if bound > s.cutoff() {
                    return pruned;
                }
            }
            let cut = s.cutoff();
            if cut.is_finite() && !sol.col_dual.is_empty() {
                if self.fix_by_reduced_costs(&mut node, &x, &sol.col_dual, sol.objective, cut).is_err() {
                    self.totals.propagation_pruned += 1;
                    return pruned;
                }
                if !self.tightened.is_empty() {
                    if node.depth == 0 && s.restartable {
                        let free = |l: &[f64], u: &[f64]| (0..n).filter(|&j| milp.integer[j] && l[j] < u[j]).count();
                        let (before, after) = (free(&s.root_lower, &s.root_upper), free(&node.lower, &node.upper));
                        if (before - after) as f64 >= RESTART_SHARE * before as f64 {
                            if s.debug {
                                crate::config::debug_log!(
                                    "restart: {} of {before} integer columns fixed",
                                    before - after
                                );
                            }
                            return Ok(Step::Restart(node.lower, node.upper));
                        }
                    }
                    cands = candidates(milp, &x, &node.lower, &node.upper);
                }
            }
            let prober = Prober {
                simplex: &self.simplex,
                propagator: &s.propagator,
                node: &node,
                basis: sol.basis.as_ref(),
                opts: &opts,
                // A worker alone in the search has the others' threads.
                helpers: if s.alone() { s.threads - 1 } else { 0 },
            };
            let (c, probes) = match choose(&prober, sol.objective, cands, &s.pseudocosts) {
                Decision::Prune => return pruned,
                Decision::Branch(c, probes) => (c, probes),
            };
            let j = c.col;
            let mut children: Vec<Node> = Vec::with_capacity(2);
            let probes = probes.map(|p| p.map(Some)).unwrap_or([None, None]);
            for (dir, probe) in probes.into_iter().enumerate() {
                let (mut l, mut u) = (node.lower.clone(), node.upper.clone());
                if dir == DOWN {
                    u[j] = c.down;
                } else {
                    l[j] = c.up;
                }
                if l[j] > u[j] {
                    continue;
                }
                let mut nd = child(l, u, j);
                match probe {
                    Some(Probe::Pruned) => continue,
                    Some(Probe::Solved { objective, basis }) => {
                        nd.bound = bound.max(objective);
                        nd.basis = basis;
                    }
                    Some(Probe::Unknown) | None => {
                        nd.branch = Some(Branch { col: j, dir, delta: c.delta(dir), parent_objective: sol.objective });
                    }
                }
                if nd.bound <= cut {
                    children.push(nd);
                }
            }
            // Depth-first explores the last child pushed: the lower bound first,
            // or the nearer rounding when the bounds tie.
            if children.len() == 2 {
                let (d, u) = (&children[0], &children[1]);
                let prefer_down = if d.bound != u.bound { d.bound < u.bound } else { c.value - c.down < 0.5 };
                if prefer_down {
                    children.swap(0, 1);
                }
            }
            return Ok(Step::Children(children));
        }

        // Integer-feasible.
        let x: Vec<f64> = x.iter().enumerate().map(|(j, &v)| if milp.integer[j] { v.round() } else { v }).collect();
        let obj = lp.objective(&x);
        if self.improve(x.clone()) && s.debug {
            crate::config::debug_log!(
                "incumbent {obj} (tree) at node {nodes}, {:.2}s",
                s.started.elapsed().as_secs_f64()
            );
        }
        if s.stop_at_first {
            return Ok(Step::Stop(End::Complete, None));
        }
        if !s.want_all {
            return pruned;
        }
        let mut children = Vec::new();
        match (0..n).find(|&j| node.lower[j] < node.upper[j]) {
            None => {
                let mut best = s.best.lock().unwrap();
                let value = best.incumbent.as_ref().map_or(obj, |i| i.objective);
                if obj <= value + tie_tolerance(value) {
                    best.solutions.push(x);
                }
            }
            Some(j) => {
                let (v, dom) = (x[j], &milp.domain[j]);
                let (down, up) = (floor_allowed(dom, v - 1.0), ceil_allowed(dom, v + 1.0));
                if down >= node.lower[j] {
                    let mut u = node.upper.clone();
                    u[j] = down;
                    children.push(child(node.lower.clone(), u, j));
                }
                if up <= node.upper[j] {
                    let mut l = node.lower.clone();
                    l[j] = up;
                    children.push(child(l, node.upper.clone(), j));
                }
                let (mut l, mut u) = (node.lower.clone(), node.upper.clone());
                l[j] = v;
                u[j] = v;
                children.push(child(l, u, j));
            }
        }
        Ok(Step::Children(children))
    }
}

pub fn branch_and_bound(milp: &Milp, settings: &Settings) -> Result<MilpResult, String> {
    let n = milp.lp.num_col;
    let (mut lower, mut upper) = (milp.lp.col_lower.clone(), milp.lp.col_upper.clone());
    for j in 0..n {
        if milp.integer[j] {
            lower[j] = ceil_allowed(&milp.domain[j], lower[j]);
            upper[j] = floor_allowed(&milp.domain[j], upper[j]);
        }
    }

    // Root cuts. Skipped with a zero objective (any feasible point will do)
    // and with `want_all`, where a numerically unsafe cut could silently drop
    // a tied optimum.
    let mut lp = milp.lp.clone();
    let num_model_rows = lp.num_row;
    let use_cuts = !settings.sub_milp
        && !settings.want_all
        && !settings.stop_at_first
        && lp.col_cost.iter().any(|&c| c != 0.0)
        && milp.integer.iter().any(|&b| b);
    let mut cut_stats = cuts::Stats::default();
    let debug = !settings.sub_milp && crate::config::debug();
    // Sub-MILPs are part of one worker's node; enumeration is one search's job.
    let threads = if settings.sub_milp || settings.want_all { 1 } else { crate::config::threads() };
    let started = Instant::now();

    // A search, or several: each restart cuts again from the root bounds that
    // the search before it left, and keeps its incumbent and pseudocosts.
    let mut best = Best { incumbent: None, solutions: Vec::new() };
    let mut pseudocosts = Pseudocosts::new(n);
    let (mut nodes, mut restarts) = (0, 0);
    let mut totals: Vec<Totals> = Vec::new();
    let tree = loop {
        let root_basis = if use_cuts {
            let (integer, deadline) = (&milp.integer, settings.deadline);
            cuts::separate_root(&mut lp, num_model_rows, integer, &lower, &upper, deadline, &mut cut_stats)
        } else {
            None
        };
        if debug {
            crate::config::debug_log!("cuts: {cut_stats:?}, rows {}", lp.num_row);
        }
        let root = Node {
            lower: lower.clone(),
            upper: upper.clone(),
            basis: root_basis,
            bound: f64::NEG_INFINITY,
            depth: 0,
            seq: 0,
            branch: None,
            changed: None,
        };
        let cutoff = best.incumbent.as_ref().map_or(settings.cutoff, |i| i.objective - tie_tolerance(i.objective));
        let search = Search {
            milp,
            lp: &lp,
            propagator: Propagator::new(&lp, &milp.integer, &milp.domain),
            root_lower: lower,
            root_upper: upper,
            want_all: settings.want_all,
            solution_limit: settings.solution_limit,
            stop_at_first: settings.stop_at_first,
            deadline: settings.deadline,
            first_cutoff: settings.cutoff,
            node_limit: settings.node_limit,
            heuristics: !settings.sub_milp && milp.integer.iter().any(|&b| b),
            restartable: use_cuts && restarts < MAX_RESTARTS,
            restart: Mutex::new(None),
            threads,
            debug,
            started,
            tree: Mutex::new(Tree { stack: vec![root], heap: BinaryHeap::new(), busy: vec![None; threads], end: None }),
            changed: Condvar::new(),
            cutoff: AtomicU64::new(cutoff.to_bits()),
            found: AtomicBool::new(best.incumbent.is_some()),
            best: Mutex::new(best),
            pseudocosts: Mutex::new(pseudocosts),
            nodes: AtomicUsize::new(nodes),
            seq: AtomicUsize::new(0),
            depth: AtomicUsize::new(0),
            halt: Arc::new(AtomicBool::new(false)),
        };
        totals.extend(search.run(settings.report)?);
        best = search.best.into_inner().unwrap();
        pseudocosts = search.pseudocosts.into_inner().unwrap();
        nodes = search.nodes.into_inner();
        match search.restart.into_inner().unwrap() {
            Some(bounds) => (lower, upper) = bounds,
            None => break search.tree.into_inner().unwrap(),
        }
        restarts += 1;
    };

    let end = tree.end.unwrap_or(End::Complete);
    if end == End::RootUnbounded {
        return Ok(MilpResult { end, incumbent: None, solutions: best.solutions, bound: f64::NEG_INFINITY });
    }
    if debug {
        let sum = |f: fn(&Totals) -> usize| totals.iter().map(f).sum::<usize>();
        let lp_seconds: f64 = totals.iter().map(|t| t.lp_seconds).sum();
        crate::config::debug_log!(
            "nodes {nodes}, node LP iterations {}, node LP seconds {lp_seconds:.2}, restarts {restarts}",
            sum(|t| t.lp_iterations)
        );
        crate::config::debug_log!(
            "propagation: {} nodes pruned, {} bound changes; reduced costs: {} bound changes",
            sum(|t| t.propagation_pruned),
            sum(|t| t.propagation_changes),
            sum(|t| t.reduced_cost_changes)
        );
        let mut heuristics: Option<crate::heuristics::Stats> = None;
        for h in totals.iter().filter_map(|t| t.heuristics.as_ref()) {
            let all = heuristics.get_or_insert_with(Default::default);
            all.dives += h.dives;
            all.dive_successes += h.dive_successes;
            all.sub_milps += h.sub_milps;
            all.sub_milp_successes += h.sub_milp_successes;
            all.seconds += h.seconds;
        }
        if let Some(h) = &heuristics {
            crate::config::debug_log!("heuristics: {h:?}");
        }
    }
    let bound = match (end, &best.incumbent) {
        (End::Complete, Some(i)) => i.objective,
        (End::Complete, None) => f64::INFINITY,
        _ => {
            // Interrupted nodes were put back, so this covers every open node.
            let b = tree.stack.iter().chain(tree.heap.iter()).map(|nd| nd.bound).fold(f64::INFINITY, f64::min);
            best.incumbent.as_ref().map_or(b, |i| b.min(i.objective))
        }
    };
    if !settings.sub_milp {
        crate::config::count_nodes(nodes as u64);
    }
    Ok(MilpResult { end, incumbent: best.incumbent, solutions: best.solutions, bound })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_rounding() {
        let d: Domain = Some(vec![(0, 2), (5, 7)]);
        assert_eq!(floor_allowed(&d, 3.5), 2.0);
        assert_eq!(ceil_allowed(&d, 3.5), 5.0);
        assert_eq!(floor_allowed(&d, 6.2), 6.0);
        assert_eq!(ceil_allowed(&d, 7.5), f64::INFINITY);
        assert_eq!(floor_allowed(&d, -0.5), f64::NEG_INFINITY);
        assert_eq!(floor_allowed(&None, 2.9999999), 3.0);
        assert_eq!(ceil_allowed(&None, 3.0000001), 3.0);
        assert!(!in_domain(&d, 4.0));
        assert!(in_domain(&d, 5.0));
    }
}
