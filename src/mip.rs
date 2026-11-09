//! Branch and bound on the dual simplex (docs/solver-design.md, "Algorithms").
//!
//! Everything here is in the LP's minimization costs, without the objective
//! offset; `solve` converts back to the problem's own sense.
//!
//! Node selection is depth-first until the first integer solution is found
//! (to get an incumbent quickly), then best-bound. Each child starts from its
//! parent's final basis.
//!
//! Branching is reliability branching (as in HiGHS and SCIP): candidates are
//! scored by pseudocosts, the average objective gain per unit of bound change
//! seen so far in each direction. A candidate whose pseudocosts rest on too
//! few observations is strong-branched instead: both child LPs are solved,
//! which gives exact gains, and the child solves are kept for the children. With `want_all`, nodes whose bound ties the incumbent
//! are kept, and an integer-feasible node is split on an unfixed variable
//! (x <= v-1, x = v, x >= v+1) until every variable is fixed, so each
//! solution is collected exactly once, at a leaf.

use crate::cuts;
use crate::heuristics::{Context, Dive, Heuristics};
use crate::lp::Lp;
use crate::propagation::{Propagator, Seeds, Trail};
use crate::simplex::{self, Basis, LpStatus, Options};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::Instant;

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

/// How far an objective may be from `best` and still tie it.
pub fn tie_tolerance(best: f64) -> f64 {
    OPTIMALITY_TOLERANCE * best.abs().max(1.0)
}

pub struct Settings {
    /// Collect every solution that ties the optimum.
    pub want_all: bool,
    /// Stop at the first integer-feasible solution (satisfy, or the
    /// zero-objective search for a MILP with an unbounded relaxation).
    pub stop_at_first: bool,
    pub deadline: Option<Instant>,
    /// Only solutions with objective below this are wanted (a sub-MIP
    /// looking for an improvement on an incumbent).
    pub cutoff: f64,
    pub node_limit: Option<usize>,
    /// A sub-MIP inside a heuristic: no cuts, heuristics, or debug output.
    pub sub_mip: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            want_all: false,
            stop_at_first: false,
            deadline: None,
            cutoff: f64::INFINITY,
            node_limit: None,
            sub_mip: false,
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
    /// The root LP is dual infeasible: the relaxation is unbounded or
    /// infeasible. Nothing else was searched.
    RootUnbounded,
}

pub struct Incumbent {
    pub objective: f64,
    pub x: Vec<f64>,
}

pub struct MipResult {
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
    fn cmp(&self, other: &Self) -> Ordering {
        other.bound.total_cmp(&self.bound).then(self.depth.cmp(&other.depth)).then(self.seq.cmp(&other.seq))
    }
}
impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Node {}

/// Allowed integer ranges of a column with a gapped domain.
pub type Domain = Option<Vec<(i64, i64)>>;

/// A MILP for branch and bound: an LP plus integrality, per column.
#[derive(Clone)]
pub struct Mip {
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
pub(crate) fn candidates(mip: &Mip, x: &[f64], lower: &[f64], upper: &[f64]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (col, &value) in x.iter().enumerate() {
        if !mip.integer[col] {
            continue;
        }
        let dom = &mip.domain[col];
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

/// Solves the child of `node` with column `col` restricted to `[lo, hi]`.
#[allow(clippy::too_many_arguments)]
fn probe(
    lp: &Lp,
    propagator: &Propagator,
    node: &Node,
    basis: Option<&Basis>,
    col: usize,
    lo: f64,
    hi: f64,
    opts: &Options,
) -> Probe {
    if lo > hi {
        return Probe::Pruned;
    }
    let (mut lower, mut upper) = (node.lower.clone(), node.upper.clone());
    lower[col] = lo;
    upper[col] = hi;
    // An infeasible child is often found by propagation alone, without an LP.
    if propagator.propagate(&mut lower, &mut upper, Seeds::Cols(&[col]), &mut Trail::default()).is_err() {
        return Probe::Pruned;
    }
    let sol = simplex::solve(lp, &lower, &upper, basis, opts);
    match sol.status {
        LpStatus::Optimal => Probe::Solved { objective: sol.objective, basis: sol.basis },
        LpStatus::Infeasible | LpStatus::ObjectiveBound | LpStatus::UnboundedOrInfeasible => Probe::Pruned,
        LpStatus::TimeLimit | LpStatus::Error(_) => Probe::Unknown,
    }
}

/// The branching decision at a node.
enum Decision {
    /// Every child is infeasible or cut off.
    Prune,
    /// Branch on this candidate, with strong-branching results if it has them.
    Branch(Candidate, Option<[Probe; 2]>),
}

/// Reliability branching: pseudocost scores, with strong branching for
/// candidates whose pseudocosts aren't reliable yet.
#[allow(clippy::too_many_arguments)]
fn choose(
    lp: &Lp,
    propagator: &Propagator,
    node: &Node,
    basis: Option<&Basis>,
    objective: f64,
    mut cands: Vec<Candidate>,
    pc: &mut Pseudocosts,
    opts: &Options,
) -> Decision {
    let estimate = |pc: &Pseudocosts, c: &Candidate| {
        let gain = |dir| if c.delta(dir).is_finite() { pc.get(c.col, dir) * c.delta(dir) } else { f64::INFINITY };
        score(gain(DOWN), gain(UP))
    };
    cands.sort_by(|a, b| estimate(pc, b).total_cmp(&estimate(pc, a)));

    let mut best: Option<(f64, usize, Option<[Probe; 2]>)> = None;
    let (mut strong, mut no_improve) = (0, 0);
    for (k, c) in cands.iter().enumerate() {
        let timed_out = crate::config::expired(opts.deadline);
        let (s, probes) = if !pc.reliable(c.col) && strong < MAX_STRONG_BRANCH && no_improve < LOOKAHEAD && !timed_out {
            strong += 1;
            let probes = [
                probe(lp, propagator, node, basis, c.col, node.lower[c.col], c.down, opts),
                probe(lp, propagator, node, basis, c.col, c.up, node.upper[c.col], opts),
            ];
            let mut gains = [0.0; 2];
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
            (estimate(pc, c), None)
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

/// Makes `x` the incumbent if it improves on it, and returns whether it did.
fn improve(
    incumbent: &mut Option<Incumbent>,
    solutions: &mut Vec<Vec<f64>>,
    lp: &Lp,
    settings: &Settings,
    x: Vec<f64>,
) -> bool {
    let obj = lp.objective(&x);
    if incumbent.as_ref().is_some_and(|i| obj >= i.objective) {
        return false;
    }
    *incumbent = Some(Incumbent { objective: obj, x });
    if settings.want_all {
        let limit = obj + tie_tolerance(obj);
        solutions.retain(|s| lp.objective(s) <= limit);
    }
    true
}

pub fn branch_and_bound(mip: &Mip, settings: &Settings) -> Result<MipResult, String> {
    let lp = &mip.lp;
    let n = lp.num_col;
    let (mut lower, mut upper) = (lp.col_lower.clone(), lp.col_upper.clone());
    for j in 0..n {
        if mip.integer[j] {
            lower[j] = ceil_allowed(&mip.domain[j], lower[j]);
            upper[j] = floor_allowed(&mip.domain[j], upper[j]);
        }
    }

    // Root cuts. Skipped with a zero objective (any feasible point will do)
    // and with `want_all`, where a numerically unsafe cut could silently drop
    // a tied optimum.
    let mut lp = lp.clone();
    let use_cuts = !settings.sub_mip
        && !settings.want_all
        && !settings.stop_at_first
        && lp.col_cost.iter().any(|&c| c != 0.0)
        && mip.integer.iter().any(|&b| b);
    let mut cut_stats = cuts::Stats::default();
    let root_basis = if use_cuts {
        cuts::separate_root(&mut lp, &mip.integer, &lower, &upper, settings.deadline, &mut cut_stats)
    } else {
        None
    };
    let debug = !settings.sub_mip && crate::config::debug();
    if debug {
        crate::config::debug_log!("cuts: {cut_stats:?}, rows {}", lp.num_row);
    }
    let (mut nodes, mut lp_iterations, mut lp_seconds) = (0usize, 0usize, 0.0f64);
    let lp = &lp;

    let (root_lower, root_upper) = (lower.clone(), upper.clone());
    let mut heuristics = (!settings.sub_mip && mip.integer.iter().any(|&b| b)).then(|| Heuristics::new(lp));
    let mut seq = 0;
    let mut stack = vec![Node {
        lower,
        upper,
        basis: root_basis,
        bound: f64::NEG_INFINITY,
        depth: 0,
        seq,
        branch: None,
        changed: None,
    }];
    let propagator = Propagator::new(lp, &mip.integer, &mip.domain);
    let mut trail = Trail::default();
    let (mut propagation_pruned, mut propagation_changes) = (0usize, 0usize);
    let mut pc = Pseudocosts::new(n);
    let mut heap: BinaryHeap<Node> = BinaryHeap::new();
    let mut incumbent: Option<Incumbent> = None;
    let mut solutions: Vec<Vec<f64>> = Vec::new();

    let cutoff = |inc: &Option<Incumbent>| match inc {
        None => settings.cutoff,
        Some(i) if settings.want_all => i.objective + tie_tolerance(i.objective),
        Some(i) => i.objective - tie_tolerance(i.objective),
    };

    let search_start = Instant::now();
    let end = loop {
        let node = if incumbent.is_none() {
            stack.pop().or_else(|| heap.pop())
        } else {
            heap.extend(stack.drain(..));
            heap.pop()
        };
        let Some(mut node) = node else { break End::Complete };
        let cut = cutoff(&incumbent);
        if node.bound > cut {
            continue;
        }
        if crate::config::expired(settings.deadline) {
            stack.push(node);
            break End::TimeLimit;
        }
        if settings.node_limit.is_some_and(|limit| nodes >= limit) {
            stack.push(node);
            break End::NodeLimit;
        }

        // Propagate the branching change (every row at the root) before the LP.
        trail.changes.clear();
        let changed = node.changed.map(|j| [j]);
        let seeds = changed.as_ref().map_or(Seeds::All, |c| Seeds::Cols(c));
        if propagator.propagate(&mut node.lower, &mut node.upper, seeds, &mut trail).is_err() {
            propagation_pruned += 1;
            continue;
        }
        propagation_changes += trail.changes.len();

        let opts = Options { deadline: settings.deadline, objective_bound: cut };
        let started = Instant::now();
        let mut sol = simplex::solve(lp, &node.lower, &node.upper, node.basis.as_ref(), &opts);
        nodes += 1;
        lp_iterations += sol.iterations;
        lp_seconds += started.elapsed().as_secs_f64();
        if matches!(sol.status, LpStatus::Error(_)) && node.basis.is_some() {
            sol = simplex::solve(lp, &node.lower, &node.upper, None, &opts);
        }
        match &sol.status {
            LpStatus::Optimal => {}
            LpStatus::Infeasible | LpStatus::ObjectiveBound => continue,
            LpStatus::UnboundedOrInfeasible if node.depth == 0 => {
                return Ok(MipResult { end: End::RootUnbounded, incumbent: None, solutions, bound: f64::NEG_INFINITY });
            }
            // Bound changes cannot make a bounded relaxation unbounded, so this
            // is numerical trouble in an infeasible node.
            LpStatus::UnboundedOrInfeasible => continue,
            LpStatus::TimeLimit => {
                stack.push(node);
                break End::TimeLimit;
            }
            LpStatus::Error(e) => return Err(format!("LP solver failed: {e}")),
        }
        if let Some(b) = node.branch {
            pc.record(b.col, b.dir, b.delta, sol.objective - b.parent_objective);
        }
        let bound = node.bound.max(sol.objective);
        if bound > cut {
            continue;
        }
        let x = sol.col_value;

        let child = |lower: Vec<f64>, upper: Vec<f64>, col: usize, seq: &mut usize| {
            *seq += 1;
            Node {
                lower,
                upper,
                basis: sol.basis.clone(),
                bound,
                depth: node.depth + 1,
                seq: *seq,
                branch: None,
                changed: Some(col),
            }
        };

        let cands = candidates(mip, &x, &node.lower, &node.upper);
        if !cands.is_empty() {
            if let Some(h) = heuristics.as_mut() {
                let root = node.depth == 0;
                let share_ok = h.stats.seconds <= HEURISTIC_SHARE * search_start.elapsed().as_secs_f64();
                let dive_now = !root && share_ok && nodes % DIVE_FREQUENCY == 0;
                let rins_now = !root && share_ok && incumbent.is_some() && nodes % RINS_FREQUENCY == 0;
                // Heuristic solutions must strictly improve on the incumbent.
                let context = |inc: &Option<Incumbent>| Context {
                    mip,
                    lp,
                    cutoff: inc.as_ref().map_or(settings.cutoff, |i| i.objective - tie_tolerance(i.objective)),
                    deadline: settings.deadline,
                };
                let mut found: Vec<(Vec<f64>, &str)> = Vec::new();
                if root {
                    for (kind, name) in [(Dive::Fractional, "fractional dive"), (Dive::Coefficient, "coefficient dive")]
                    {
                        let ctx = context(&incumbent);
                        if let Some(y) = h.dive(&ctx, &node.lower, &node.upper, &x, sol.basis.as_ref(), kind, None) {
                            improve(&mut incumbent, &mut solutions, lp, settings, y.clone());
                            found.push((y, name));
                        }
                    }
                    if let Some(y) = h.rens(&context(&incumbent), &x, &node.lower, &node.upper) {
                        improve(&mut incumbent, &mut solutions, lp, settings, y.clone());
                        found.push((y, "RENS"));
                    }
                }
                if dive_now {
                    let guide = incumbent.as_ref().map(|i| i.x.clone());
                    let kind = match (&guide, (nodes / DIVE_FREQUENCY) % 2) {
                        (Some(_), _) => Dive::Guided,
                        (None, 0) => Dive::Fractional,
                        (None, _) => Dive::Coefficient,
                    };
                    let ctx = context(&incumbent);
                    if let Some(y) =
                        h.dive(&ctx, &node.lower, &node.upper, &x, sol.basis.as_ref(), kind, guide.as_deref())
                    {
                        improve(&mut incumbent, &mut solutions, lp, settings, y.clone());
                        found.push((y, "dive"));
                    }
                }
                if rins_now {
                    let guide = incumbent.as_ref().map(|i| i.x.clone()).unwrap();
                    if let Some(y) = h.rins(&context(&incumbent), &x, &guide, &root_lower, &root_upper) {
                        improve(&mut incumbent, &mut solutions, lp, settings, y.clone());
                        found.push((y, "RINS"));
                    }
                }
                if debug {
                    for (y, name) in &found {
                        let t = search_start.elapsed().as_secs_f64();
                        crate::config::debug_log!("incumbent {} ({name}) at node {nodes}, {t:.2}s", lp.objective(y));
                    }
                }
                if settings.stop_at_first && incumbent.is_some() {
                    break End::Complete;
                }
                if bound > cutoff(&incumbent) {
                    continue;
                }
            }
            let (c, probes) =
                match choose(lp, &propagator, &node, sol.basis.as_ref(), sol.objective, cands, &mut pc, &opts) {
                    Decision::Prune => continue,
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
                let mut nd = child(l, u, j, &mut seq);
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
            stack.extend(children);
            continue;
        }

        // Integer-feasible.
        let x: Vec<f64> = x.iter().enumerate().map(|(j, &v)| if mip.integer[j] { v.round() } else { v }).collect();
        let obj = lp.objective(&x);
        if improve(&mut incumbent, &mut solutions, lp, settings, x.clone()) && debug {
            crate::config::debug_log!(
                "incumbent {obj} (tree) at node {nodes}, {:.2}s",
                search_start.elapsed().as_secs_f64()
            );
        }
        if settings.stop_at_first {
            break End::Complete;
        }
        if !settings.want_all {
            continue;
        }
        match (0..n).find(|&j| node.lower[j] < node.upper[j]) {
            None => {
                let best = incumbent.as_ref().map_or(obj, |i| i.objective);
                if obj <= best + tie_tolerance(best) {
                    solutions.push(x);
                }
            }
            Some(j) => {
                let (v, dom) = (x[j], &mip.domain[j]);
                let (down, up) = (floor_allowed(dom, v - 1.0), ceil_allowed(dom, v + 1.0));
                if down >= node.lower[j] {
                    let mut u = node.upper.clone();
                    u[j] = down;
                    stack.push(child(node.lower.clone(), u, j, &mut seq));
                }
                if up <= node.upper[j] {
                    let mut l = node.lower.clone();
                    l[j] = up;
                    stack.push(child(l, node.upper.clone(), j, &mut seq));
                }
                let (mut l, mut u) = (node.lower.clone(), node.upper.clone());
                l[j] = v;
                u[j] = v;
                stack.push(child(l, u, j, &mut seq));
            }
        }
    };

    if debug {
        crate::config::debug_log!("nodes {nodes}, node LP iterations {lp_iterations}, node LP seconds {lp_seconds:.2}");
        crate::config::debug_log!(
            "propagation: {propagation_pruned} nodes pruned, {propagation_changes} bound changes"
        );
        if let Some(h) = &heuristics {
            crate::config::debug_log!("heuristics: {:?}", h.stats);
        }
    }
    let bound = match (end, &incumbent) {
        (End::Complete, Some(i)) => i.objective,
        (End::Complete, None) => f64::INFINITY,
        _ => {
            // The interrupted node was pushed back, so this covers every open node.
            let b = stack.iter().chain(heap.iter()).map(|nd| nd.bound).fold(f64::INFINITY, f64::min);
            incumbent.as_ref().map_or(b, |i| b.min(i.objective))
        }
    };
    Ok(MipResult { end, incumbent, solutions, bound })
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
