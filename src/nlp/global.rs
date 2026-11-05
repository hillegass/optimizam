//! Spatial branch and bound: global optima of nonlinear problems, with
//! continuous and integer variables.
//!
//! A node is a box of variable bounds. Each node's box is tightened by bound
//! propagation (with the objective required to beat the incumbent), and the
//! linear relaxation over it gives a lower bound; a node that can't improve
//! on the incumbent is pruned. Otherwise it is split in two:
//!
//! - on an integer variable with a fractional value in the relaxation's
//!   solution (floor / ceiling), as in MILP branch and bound;
//! - else on a variable of the nonlinear term that the relaxation's solution
//!   violates most (its column's value vs the term's true value at the
//!   solution), at that variable's value in the solution (kept away from the
//!   box's ends), so that the term's relaxation tightens in both halves.
//!
//! Incumbents come from the relaxation's solution when it is feasible, and
//! from local solves (the interior point method, from the relaxation's
//! solution, inside the node's box, with integer variables rounded and
//! fixed): at the root (also from the problem's start) and then at every
//! node near the top of the tree and periodically below; with integer
//! variables, also from integer local search (`intsearch.rs`) from those
//! points and from each new incumbent. The relaxations
//! also get tangents at the incumbent. Nodes are explored best bound first,
//! so the smallest open bound is a valid bound on the optimum at any time.
//! With no open node left, the incumbent is optimal (within the gap GAP_REL
//! relative, GAP_ABS absolute), or there is no feasible point.

use super::graph::{Node, Op};
use super::interval::Interval;
use super::intsearch::IntSearch;
use super::{Bounding, BoxBound, Local, LocalRun, Warm, model_box, starting_point};
use crate::model::LinearModel;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::Instant;

/// The optimality gap: a node is pruned unless it could improve the
/// incumbent by more than `max(GAP_REL * |f|, GAP_ABS)`, so the answer is
/// proven within that (the usual default of global solvers; proving 1e-6
/// takes boxes so small around the optimum that the tree explodes).
pub const GAP_REL: f64 = 1e-4;
/// The absolute gap must exceed the rows' feasibility loosening (1e-6
/// relative to 1 + |bound|): an objective defined by an equality row can't
/// be bounded closer than that, so a gap of 1e-6 at an optimum of 0 never
/// closes.
pub const GAP_ABS: f64 = 1e-5;
/// A relaxation's solution becomes an incumbent only if it satisfies the
/// rows this closely (relative), much closer than VIOLATION_TOL: otherwise
/// the search would chase objective gains that exist only within the
/// feasibility tolerance. (Local solves converge to about 1e-9 anyway.)
const RELAXATION_POINT_TOL: f64 = 1e-9;
/// Local solves at every node this deep or shallower, and every
/// LOCAL_EVERY nodes below.
const LOCAL_DEPTH: usize = 3;
const LOCAL_EVERY: usize = 25;
/// A continuous variable narrower than this (relative to 1 + |value|) isn't
/// branched on.
const MIN_WIDTH: f64 = 1e-9;
/// The search stops (as if out of time) when, over the last STALL_NODES
/// nodes, the gap between the incumbent and the global bound shrank by less
/// than STALL_PROGRESS: a relaxation too weak to ever close it shouldn't use
/// all the time there is.
const STALL_NODES: usize = 2000;
const STALL_PROGRESS: f64 = 0.05;
/// The node budget (without a time limit) when a nonlinear term has an
/// unbounded variable.
const UNBOUNDED_NODE_LIMIT: usize = 500;
/// Reduced costs below this don't tighten bounds; tightened bounds are
/// loosened by RC_LOOSEN (relative), for the LP's tolerances.
const RC_MIN: f64 = 1e-7;
const RC_LOOSEN: f64 = 1e-7;
/// Rounds of bound tightening (LP-based) at the root.
const OBBT_ROUNDS: usize = 2;
/// The iteration budget (of local solves) of the integer search before
/// outer approximation.
const EARLY_SEARCH_ITERATIONS: usize = 300;
/// Bound tightening (with the cutoff) at nodes this deep or shallower.
const OBBT_DEPTH: usize = 6;
const OBBT_FUTILE: usize = 10;
const OBBT_MIN_SHRINK: f64 = 0.05;
/// Cutting-plane rounds per extreme in bound tightening, at the root and
/// at nodes.
const OBBT_CUT_ROUNDS: usize = 3;
const NODE_OBBT_CUT_ROUNDS: usize = 5;
/// The cuts bound tightening may add, in multiples of the relaxation's rows,
/// at the root and at nodes.
const OBBT_CUT_CAP: usize = 3;
const NODE_OBBT_CUT_CAP: usize = 1;
/// How much a point box's own point (feasible within the tolerance) may
/// beat the local solve's point there (relative to 1 + |f|).
const POINT_BOX_GAIN: f64 = 1e-4;
/// Branch points stay this fraction of the width away from the box's ends.
const SPLIT_MARGIN: f64 = 0.1;

/// Whether bound tightening at nodes pays: it stops after OBBT_FUTILE calls
/// in a row that narrow the box (on average over its variables) by less
/// than OBBT_MIN_SHRINK.
#[derive(Default)]
struct Usefulness {
    futile: usize,
}

impl Usefulness {
    fn is_useful(&self) -> bool {
        self.futile < OBBT_FUTILE
    }

    fn record(&mut self, shrink: f64) {
        self.futile = if shrink < OBBT_MIN_SHRINK { self.futile + 1 } else { 0 };
    }
}

pub enum Outcome {
    /// Proven optimal within GAP_TOL (for satisfy: a feasible point).
    Optimal(Vec<f64>),
    /// No feasible point exists.
    Infeasible,
    /// No branching was possible (an unbounded variable in a nonlinear
    /// term), and the root's local solve converged to a local minimizer of
    /// the infeasibility.
    LocallyInfeasible,
    /// Stopped (time or node limit): the best point found, if any, whether
    /// it is a local optimum (KKT point) of a continuous problem, and a lower
    /// bound on the optimum in the problem's own sense.
    Stopped { x: Option<Vec<f64>>, local_optimum: bool, bound: Option<f64> },
}

struct BoxNode {
    /// The node's lower bound (from its parent until it is processed).
    lower: f64,
    depth: usize,
    seq: usize,
    lo: Vec<f64>,
    hi: Vec<f64>,
    /// The parent's relaxation state.
    warm: Warm,
}

impl PartialEq for BoxNode {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for BoxNode {}
impl PartialOrd for BoxNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for BoxNode {
    /// The heap pops the smallest bound first, then the deepest, then the
    /// earliest created (for a deterministic search).
    fn cmp(&self, other: &Self) -> Ordering {
        other.lower.total_cmp(&self.lower).then(self.depth.cmp(&other.depth)).then(other.seq.cmp(&self.seq))
    }
}

/// The best point found: its objective (minimization), the point, and
/// whether a local solve found it (so it is a KKT point).
struct Incumbent {
    f: f64,
    x: Vec<f64>,
    local: bool,
}

pub fn solve(model: &LinearModel, deadline: Option<Instant>, node_limit: Option<usize>) -> Result<Outcome, String> {
    let debug = std::env::var_os("OPTIMIZAM_DEBUG").is_some();
    let n = model.num_cols();
    if (0..n).any(|j| model.col_lower[j] > model.col_upper[j]) {
        return Ok(Outcome::Infeasible);
    }
    let empty = super::graph::Graph::new();
    let graph = model.nonlinear.as_ref().map_or(&empty, |nl| &nl.graph);
    let local = Local::new(model, graph);
    let bounding = Bounding::new(&local);
    let integer = &model.col_integer;
    let continuous = !integer.iter().any(|&i| i);
    let mut incumbent: Option<Incumbent> = None;
    let gap = |f: f64| (GAP_REL * f.abs()).max(GAP_ABS);
    // `kkt`: a local optimum of a continuous problem (for the status if the
    // search stops early); `solved`: from a local solve rather than the
    // relaxation (which decides the feasibility tolerance).
    let offer = |inc: &mut Option<Incumbent>, x: Vec<f64>, kkt: bool, solved: bool, what: &str| {
        let mut x = x;
        for j in 0..n {
            if integer[j] {
                x[j] = x[j].round();
            }
        }
        let tol = if solved { super::VIOLATION_TOL } else { RELAXATION_POINT_TOL };
        if !local.feasible_within(&x, &model.col_lower, &model.col_upper, tol) {
            return;
        }
        let f = local.objective(&x);
        if inc.as_ref().is_none_or(|i| f < i.f - gap(i.f)) {
            if debug {
                eprintln!("global: incumbent {} from {what}", local.sign * f);
            }
            *inc = Some(Incumbent { f, x, local: kkt });
        }
    };
    // A local solve inside [lo, hi] from x (integers rounded and fixed).
    let local_solve = |lo: &[f64], hi: &[f64], x: &[f64]| -> Option<(Vec<f64>, bool)> {
        let (mut l, mut u, mut s) = (lo.to_vec(), hi.to_vec(), x.to_vec());
        for j in 0..n {
            if integer[j] {
                let v = x[j].round().max(lo[j]).min(hi[j]);
                (l[j], u[j], s[j]) = (v, v, v);
            }
        }
        match local.run(&l, &u, &s, deadline) {
            LocalRun::Done { x: Some(x), .. } => Some((x, true)),
            LocalRun::Done { best: Some(x), .. } => Some((x, false)),
            _ => None,
        }
    };
    let mut root_box: Vec<Interval> = model_box(model)
        .into_iter()
        .enumerate()
        .map(|(j, i)| if integer[j] { Interval::new((i.lo - 1e-9).ceil(), (i.hi + 1e-9).floor()) } else { i })
        .collect();
    if root_box.iter().any(|i| i.is_empty()) {
        return Ok(Outcome::Infeasible);
    }
    // Bound tightening at the root: the variables of nonlinear terms,
    // minimized and maximized over the relaxation (which also bounds
    // variables that only several linear rows together bound).
    let nonlinear_vars: Vec<usize> = {
        let g = &bounding.graph;
        let mut v: Vec<usize> = (0..g.len())
            .filter(|&k| !matches!(g.op(k as Node), Op::Const(_) | Op::Var(_) | Op::Sum(..)))
            .flat_map(|k| g.vars_of(k as Node))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    for _ in 0..OBBT_ROUNDS {
        match bounding.tighten(&root_box, &nonlinear_vars, None, OBBT_CUT_ROUNDS, OBBT_CUT_CAP, deadline) {
            None => break, // propagation proves it infeasible: the search below will too
            Some(b) => {
                for j in 0..n {
                    root_box[j] = if integer[j] {
                        Interval::new((b[j].lo - 1e-9).ceil(), (b[j].hi + 1e-9).floor())
                    } else {
                        b[j]
                    };
                }
            }
        }
    }
    if root_box.iter().any(|i| i.is_empty()) {
        root_box = model_box(model);
    }
    if debug {
        eprintln!("global: root box {:?}", root_box.iter().map(|i| (i.lo, i.hi)).collect::<Vec<_>>());
    }
    // The root's local solve from the problem's start.
    let mut root_bound = None;
    let mut root_lp: Option<Vec<f64>> = None;
    let mut unbounded = false;
    let mut root_locally_infeasible = false;
    if let BoxBound::Bounded { implied, lower, .. } = bounding.bound(&root_box, None, None, &Warm::default(), deadline)
    {
        let start = starting_point(model, &implied);
        let lo: Vec<f64> = root_box.iter().map(|i| i.lo).collect();
        let hi: Vec<f64> = root_box.iter().map(|i| i.hi).collect();
        if continuous {
            match local.run(&lo, &hi, &start, deadline) {
                LocalRun::Done { x: Some(x), .. } => offer(&mut incumbent, x, true, true, "the root's local solve"),
                LocalRun::Done { status, best, .. } => {
                    root_locally_infeasible = status == super::ipm::Status::LocallyInfeasible;
                    if let Some(x) = best {
                        offer(&mut incumbent, x, false, true, "the root's local solve");
                    }
                }
                LocalRun::Infeasible => return Ok(Outcome::Infeasible),
                LocalRun::BadStart => {}
            }
        } else if let Some((x, kkt)) = local_solve(&lo, &hi, &start) {
            offer(&mut incumbent, x, kkt && continuous, true, "the root's local solve");
        }
        // Other starts, if that found nothing: the relaxation's solution, and
        // a point inside the implied box.
        if incumbent.is_none() {
            let mut starts: Vec<Vec<f64>> = lower.iter().map(|l| l.x.clone()).collect();
            starts.push((0..n).map(|j| mid(implied[j].lo, implied[j].hi).max(lo[j]).min(hi[j])).collect());
            // With 1 for variables without bounds (0 is often where ln or a
            // division isn't defined).
            starts.push(
                (0..n)
                    .map(|j| {
                        if implied[j].width().is_infinite() && implied[j].lo < 1.0 && implied[j].hi > 1.0 {
                            1.0
                        } else {
                            mid(implied[j].lo, implied[j].hi)
                        }
                    })
                    .collect(),
            );
            for start in starts {
                if let Some((x, kkt)) = local_solve(&lo, &hi, &start) {
                    offer(&mut incumbent, x, kkt && continuous, true, "a local solve from another start");
                    break;
                }
            }
        }
        if debug && let Some(l) = &lower {
            eprintln!("global: root bound {}", local.sign * l.value);
        }
        root_lp = lower.as_ref().map(|l| l.x.clone());
        root_bound = lower.map(|l| l.value);
        // With an incumbent, only points that beat it matter: propagation
        // with that cutoff can bound variables the rows alone don't (such
        // as one only the objective bounds).
        let mut implied = implied;
        if let Some(inc) = incumbent.as_ref().filter(|_| !local.satisfy)
            && let BoxBound::Bounded { implied: tighter, .. } =
                bounding.bound(&root_box, Some(inc.f + gap(inc.f)), None, &Warm::default(), deadline)
        {
            let narrowed: Vec<Interval> = (0..n).map(|j| root_box[j].intersect(tighter[j])).collect();
            if !narrowed.iter().any(|i| i.is_empty()) {
                root_box = narrowed;
                implied = tighter;
            }
        }
        // Splitting boxes can't converge on a nonlinear term of a variable
        // with an infinite bound: report the local answer and the root's
        // bound instead.
        let g = &bounding.graph;
        unbounded = (0..g.len())
            .filter(|&k| !matches!(g.op(k as Node), Op::Const(_) | Op::Var(_) | Op::Sum(..)))
            .flat_map(|k| g.vars_of(k as Node))
            .any(|j| implied[j].width().is_infinite());
    }
    // A nonlinear term of a variable without finite bounds: branching can
    // still bound the pieces (with propagation and the cutoff), but may
    // never close the gap, so without a time limit the budget is smaller.
    let node_limit = if unbounded { node_limit.map(|l| l.min(UNBOUNDED_NODE_LIMIT)) } else { node_limit };
    if unbounded && debug {
        eprintln!("global: a nonlinear term has an unbounded variable");
    }
    let _ = root_bound;
    // Integer local search: from the relaxations' solutions (the nonlinear
    // one with the integers relaxed, and the linear one), the start, and the
    // incumbent, each rounding once. Outer approximation converges faster
    // from a good incumbent, so searches run before it: from every start
    // when evaluating points is cheap (no local solves), else one short one;
    // the rest only if outer approximation doesn't prove the optimum.
    let search = IntSearch::new(&local);
    let root_lo: Vec<f64> = root_box.iter().map(|i| i.lo).collect();
    let root_hi: Vec<f64> = root_box.iter().map(|i| i.hi).collect();
    let mut searched = f64::INFINITY;
    let mut starts = Vec::new();
    let mut seen: Vec<Vec<f64>> = Vec::new();
    let search_from = |x: &[f64], seen: &mut Vec<Vec<f64>>, budget: usize| -> Option<Vec<f64>> {
        let key: Vec<f64> = (0..n).filter(|&j| integer[j]).map(|j| x[j].round()).collect();
        if seen.contains(&key) {
            return None;
        }
        seen.push(key);
        search.as_ref()?.run_with(&root_lo, &root_hi, x, deadline, budget)
    };
    if search.is_some() {
        let start = starting_point(model, &root_box);
        if let LocalRun::Done { x, best, .. } = local.run(&root_lo, &root_hi, &start, deadline)
            && let Some(x) = x.or(best)
        {
            starts.push(x);
        }
        starts.extend(root_lp.take());
        starts.push(start);
        if let Some(inc) = &incumbent {
            searched = inc.f;
            starts.push(inc.x.clone());
        }
        // Cheap searches from every start; with local solves, a short one
        // from the first.
        let cheap = search.as_ref().is_some_and(|s| !s.solves);
        let first: Vec<Vec<f64>> = if cheap { std::mem::take(&mut starts) } else { starts[..1].to_vec() };
        for x in first {
            if let Some(y) = search_from(&x, &mut seen, EARLY_SEARCH_ITERATIONS) {
                offer(&mut incumbent, y, false, true, "integer search");
            }
        }
        if !cheap {
            // The first start gets the full budget later.
            seen.clear();
        }
    }
    // Convex problems with integers: outer approximation on the MILP
    // engine first.
    if !continuous {
        let start = incumbent.as_ref().map(|i| (i.f, i.x.clone()));
        match super::ecp::solve(model, &local, &bounding, &root_box, start, &gap, deadline) {
            super::ecp::Outcome::Optimal(x) => {
                if debug {
                    eprintln!("global: outer approximation proves the optimum");
                }
                return Ok(Outcome::Optimal(x));
            }
            super::ecp::Outcome::Infeasible => return Ok(Outcome::Infeasible),
            super::ecp::Outcome::Stopped(Some((_, x))) => offer(&mut incumbent, x, false, true, "outer approximation"),
            super::ecp::Outcome::Stopped(None) | super::ecp::Outcome::NotConvex => {}
        }
    }
    // The searches from the other starts.
    for x in starts {
        if let Some(y) = search_from(&x, &mut seen, super::intsearch::BUDGET_ITERATIONS) {
            offer(&mut incumbent, y, false, true, "integer search");
        }
    }
    // Bound tightening again, with the incumbent as a cutoff: only points
    // that beat it matter (with a convex objective, the cutoff is one more
    // convex row). A verdict that no point beats it isn't trusted here (the
    // search proves that itself). The objective's own columns keep their
    // bounds: with the cutoff in a box's bounds, a relaxation that is
    // infeasible by too thin a margin to confirm couldn't be bounded without
    // the cutoff instead.
    let in_objective: Vec<bool> = {
        let mut v: Vec<bool> = model.obj.iter().map(|&c| c != 0.0).collect();
        v.resize(n, false);
        if let Some(o) = local.obj_node {
            for j in local.graph.vars_of(o) {
                v[j] = true;
            }
        }
        v
    };
    if let Some(inc) = incumbent.as_ref().filter(|_| !local.satisfy) {
        for _ in 0..OBBT_ROUNDS {
            let Some(b) = bounding.tighten(
                &root_box,
                &nonlinear_vars,
                Some(inc.f - gap(inc.f)),
                OBBT_CUT_ROUNDS,
                OBBT_CUT_CAP,
                deadline,
            ) else {
                break;
            };
            let narrowed: Vec<Interval> = (0..n)
                .map(|j| {
                    if in_objective[j] {
                        return root_box[j];
                    }
                    let i = root_box[j].intersect(b[j]);
                    if integer[j] { Interval::new((i.lo - 1e-9).ceil(), (i.hi + 1e-9).floor()) } else { i }
                })
                .collect();
            if narrowed.iter().any(|i| i.is_empty()) {
                break;
            }
            root_box = narrowed;
        }
        if debug {
            eprintln!(
                "global: root box with the cutoff {:?}",
                root_box.iter().map(|i| (i.lo, i.hi)).collect::<Vec<_>>()
            );
        }
    }
    let mut heap = BinaryHeap::new();
    let mut seq = 0;
    heap.push(BoxNode {
        lower: f64::NEG_INFINITY,
        depth: 0,
        seq,
        lo: root_box.iter().map(|i| i.lo).collect(),
        hi: root_box.iter().map(|i| i.hi).collect(),
        warm: Warm::default(),
    });
    let mut nodes = 0;
    let mut stopped = false;
    let mut obbt_useful = Usefulness::default();
    // The gap every STALL_NODES nodes.
    let mut history: Vec<f64> = Vec::new();
    // The bound of the nodes still open when stopping early.
    let mut open_bound = f64::INFINITY;
    while let Some(node) = heap.pop() {
        if let Some(inc) = &incumbent
            && node.lower >= inc.f - gap(inc.f)
        {
            // Best bound first: every remaining node is pruned too.
            heap.clear();
            break;
        }
        // Best bound first: the popped node's bound is the global bound.
        if nodes % STALL_NODES == 0 {
            // The gap to the incumbent (infinite without one).
            history.push(incumbent.as_ref().map_or(f64::INFINITY, |i| i.f - node.lower));
            if debug {
                eprintln!("global: {nodes} nodes, bound {}, {} open", local.sign * node.lower, heap.len());
            }
        }
        let stalled = match history.len() {
            h if h >= 2 && nodes % STALL_NODES == 0 => {
                let (old, now) = (history[h - 2], history[h - 1]);
                // With an incumbent but no finite bound for three windows
                // (unbounded variables), the bound isn't coming either.
                let unbounded = incumbent.is_some() && h >= 3 && history[h - 3..].iter().all(|g| g.is_infinite());
                (old.is_finite() && now > (1.0 - STALL_PROGRESS) * old) || unbounded
            }
            _ => false,
        };
        if stalled && debug {
            eprintln!("global: the bound has stalled at {}", local.sign * node.lower);
        }
        if stalled || deadline.is_some_and(|d| Instant::now() >= d) || node_limit.is_some_and(|l| nodes >= l) {
            open_bound = node.lower;
            stopped = true;
            break;
        }
        nodes += 1;
        if local.satisfy && incumbent.is_some() {
            break;
        }
        let boxed: Vec<Interval> = (0..n).map(|j| Interval::new(node.lo[j], node.hi[j])).collect();
        let cutoff = incumbent.as_ref().map(|i| i.f - gap(i.f));
        let hint = incumbent.as_ref().map(|i| i.x.as_slice());
        let (implied, lower, warm) = match bounding.bound(&boxed, cutoff, hint, &node.warm, deadline) {
            BoxBound::Infeasible => continue,
            // On a box tightened by the cutoff near the optimum, the
            // relaxation can be infeasible by a margin too thin to confirm:
            // bound the box without the cutoff instead.
            BoxBound::Bounded { lower: None, .. } if cutoff.is_some() => {
                match bounding.bound(&boxed, None, hint, &node.warm, deadline) {
                    BoxBound::Infeasible => continue,
                    BoxBound::Bounded { implied, lower, warm } => (implied, lower, warm),
                }
            }
            BoxBound::Bounded { implied, lower, warm } => (implied, lower, warm),
        };
        let mut lo: Vec<f64> = implied.iter().map(|i| i.lo).collect();
        let mut hi: Vec<f64> = implied.iter().map(|i| i.hi).collect();
        let (bound, x_lp, node_vals, reduced) = match lower {
            Some(l) => (l.value.max(node.lower), Some(l.x), Some(l.nodes), Some((l.lp_value, l.reduced))),
            None => (node.lower, None, None, None),
        };
        if let Some(inc) = &incumbent
            && bound >= inc.f - gap(inc.f)
        {
            continue;
        }
        // Incumbents: the relaxation's solution, and local solves from it.
        let center: Vec<f64> = (0..n).map(|j| mid(lo[j], hi[j])).collect();
        let point = x_lp.clone().unwrap_or_else(|| center.clone());
        offer(&mut incumbent, point.clone(), false, false, "the relaxation");
        if (node.depth <= LOCAL_DEPTH || nodes % LOCAL_EVERY == 0)
            && let Some((x, kkt)) = local_solve(&lo, &hi, &point)
        {
            offer(&mut incumbent, x, kkt && continuous, true, "a local solve");
        }
        if let Some(s) = &search {
            if (node.depth <= LOCAL_DEPTH || nodes % LOCAL_EVERY == 0)
                && let Some(x) = s.run(&lo, &hi, &point, deadline)
            {
                offer(&mut incumbent, x, false, true, "integer search");
            }
            // From each new incumbent, over the root's box.
            if let Some(inc) = incumbent.as_ref().filter(|i| i.f < searched) {
                searched = inc.f;
                let x = inc.x.clone();
                if let Some(y) = s.run(&root_lo, &root_hi, &x, deadline) {
                    offer(&mut incumbent, y, false, true, "integer search");
                }
            }
        }
        if let Some(inc) = &incumbent
            && bound >= inc.f - gap(inc.f)
        {
            continue;
        }
        if local.satisfy && incumbent.is_some() {
            break;
        }
        // Reduced-cost bound tightening: a column at a bound with reduced
        // cost d can only move (cutoff - z) / d before the relaxation's
        // objective passes the cutoff, so the children's boxes end there.
        if let (Some(inc), Some((z, d)), Some(x)) = (&incumbent, &reduced, &x_lp) {
            let slack = inc.f - gap(inc.f) - z;
            if slack >= 0.0 {
                for j in 0..n {
                    let at = |b: f64| b.is_finite() && (x[j] - b).abs() <= 1e-9 * (1.0 + b.abs());
                    let loosen = |v: f64| RC_LOOSEN * (1.0 + v.abs());
                    if d[j] > RC_MIN && at(lo[j]) {
                        let mut h = lo[j] + slack / d[j];
                        h += loosen(h);
                        if integer[j] {
                            h = (h + 1e-9).floor();
                        }
                        hi[j] = hi[j].min(h.max(lo[j]));
                    } else if d[j] < -RC_MIN && at(hi[j]) {
                        let mut l = hi[j] - slack / -d[j];
                        l -= loosen(l);
                        if integer[j] {
                            l = (l - 1e-9).ceil();
                        }
                        lo[j] = lo[j].max(l.min(hi[j]));
                    }
                }
            }
        }
        // Near the top of the tree, bound tightening with the cutoff (the
        // children inherit the narrower box).
        if node.depth >= 1
            && node.depth <= OBBT_DEPTH
            && obbt_useful.is_useful()
            && let Some(inc) = incumbent.as_ref().filter(|_| !local.satisfy)
        {
            let boxed: Vec<Interval> = (0..n).map(|j| Interval::new(lo[j], hi[j])).collect();
            // None: propagation with the cutoff (as in bounding) shows that
            // no point of the box beats the incumbent.
            let Some(b) = bounding.tighten(
                &boxed,
                &nonlinear_vars,
                Some(inc.f - gap(inc.f)),
                NODE_OBBT_CUT_ROUNDS,
                NODE_OBBT_CUT_CAP,
                deadline,
            ) else {
                continue;
            };
            let (mut shrink, mut counted) = (0.0, 0);
            for j in 0..n {
                if in_objective[j] {
                    continue;
                }
                let mut i = boxed[j].intersect(b[j]);
                if integer[j] {
                    i = Interval::new((i.lo - 1e-9).ceil(), (i.hi + 1e-9).floor());
                }
                if !i.is_empty() {
                    (lo[j], hi[j]) = (i.lo, i.hi);
                }
                let w = boxed[j].width();
                if w.is_finite() && w > 0.0 {
                    shrink += 1.0 - (hi[j] - lo[j]) / w;
                    counted += 1;
                }
            }
            obbt_useful.record(if counted > 0 { shrink / counted as f64 } else { 0.0 });
        }
        // Branch.
        let Some((j, split)) = branching(&bounding, model, &lo, &hi, &point, node_vals.as_deref()) else {
            // Nothing left to split: the box is (nearly) a point. A local
            // solve with its integers fixed (and the continuous variables
            // within the model's bounds, not the box's, which the cutoff
            // narrows) gives its best point. The box's own points count at
            // the feasibility tolerance (so that a feasible box is never
            // lost), but not if they beat the local solve's point by more
            // than POINT_BOX_GAIN: within the tolerance, a point can beat
            // every exactly feasible one by much more than the gap (in
            // MINLPLib's gear4, whose objective is the slack of a row with a
            // constant of 144279, by the 0.144 that 1e-6 relative allows).
            let solved = local_solve(&model.col_lower, &model.col_upper, &point);
            let floor = solved.as_ref().map(|(x, _)| {
                let f = local.objective(x);
                f - POINT_BOX_GAIN * (1.0 + f.abs())
            });
            if let Some((x, kkt)) = solved {
                offer(&mut incumbent, x, kkt && continuous, true, "a point box");
            }
            for x in [&point, &center] {
                if floor.is_none_or(|fl| local.objective(x) >= fl) {
                    offer(&mut incumbent, x.clone(), false, true, "a point box");
                }
            }
            continue;
        };
        for (a, b) in children(integer[j], lo[j], hi[j], split) {
            let (mut clo, mut chi) = (lo.clone(), hi.clone());
            (clo[j], chi[j]) = (a, b);
            seq += 1;
            heap.push(BoxNode { lower: bound, depth: node.depth + 1, seq, lo: clo, hi: chi, warm: warm.clone() });
        }
    }
    if debug {
        eprintln!("global: {nodes} nodes, {} open", heap.len());
    }
    if stopped {
        let bound = heap.iter().map(|b| b.lower).fold(open_bound, f64::min);
        let bound = bound.is_finite().then_some(local.sign * bound + 0.0).filter(|_| !local.satisfy);
        return Ok(match incumbent {
            Some(inc) => {
                // A point not from a local solve: polish it into a KKT point.
                let mut inc = inc;
                if continuous && !inc.local {
                    let (lo, hi) = (&model.col_lower, &model.col_upper);
                    if let LocalRun::Done { x: Some(x), .. } = local.run(lo, hi, &inc.x, deadline) {
                        let f = local.objective(&x);
                        if f <= inc.f + gap(inc.f) {
                            inc = Incumbent { f, x, local: true };
                        }
                    }
                }
                Outcome::Stopped { x: Some(inc.x), local_optimum: inc.local, bound }
            }
            None if root_locally_infeasible => Outcome::LocallyInfeasible,
            None => Outcome::Stopped { x: None, local_optimum: false, bound },
        });
    }
    Ok(match incumbent {
        Some(inc) => {
            // Polish: a local solve from the optimum (integers fixed) sharpens
            // a point found within the tolerances.
            let (lo, hi) = (&model.col_lower, &model.col_upper);
            let polished = local_solve(lo, hi, &inc.x)
                .filter(|(x, _)| local.feasible(x, lo, hi) && local.objective(x) <= inc.f + gap(inc.f));
            Outcome::Optimal(polished.map_or(inc.x, |p| p.0))
        }
        None => Outcome::Infeasible,
    })
}

fn mid(lo: f64, hi: f64) -> f64 {
    match (lo.is_finite(), hi.is_finite()) {
        (true, true) => 0.5 * (lo + hi),
        // Half-infinite: a step away from the finite end that grows with it.
        (true, false) => lo + 1.0 + lo.abs(),
        (false, true) => hi - 1.0 - hi.abs(),
        (false, false) => 0.0,
    }
}

/// The variable to branch on and where: a fractional integer variable, or a
/// variable of the most violated nonlinear term.
fn branching(
    bounding: &Bounding,
    model: &LinearModel,
    lo: &[f64],
    hi: &[f64],
    x: &[f64],
    node_vals: Option<&[Option<f64>]>,
) -> Option<(usize, f64)> {
    let n = model.num_cols();
    let integer = &model.col_integer;
    // The most fractional integer variable.
    let frac = (0..n)
        .filter(|&j| integer[j] && lo[j] < hi[j])
        .map(|j| (j, (x[j] - x[j].floor() - 0.5).abs()))
        .filter(|&(_, d)| d < 0.5 - 1e-6)
        .min_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((j, _)) = frac {
        return Some((j, x[j]));
    }
    let width = |j: usize| -> f64 {
        let w = hi[j] - lo[j];
        if w.is_finite() { w / (1.0 + x[j].abs()) } else { 1e30 }
    };
    let can_split =
        |j: usize| -> bool { lo[j] < hi[j] && (integer[j] || hi[j] - lo[j] > MIN_WIDTH * (1.0 + x[j].abs())) };
    // The most violated nonlinear term in the relaxation's solution.
    let g = &bounding.graph;
    let mut worst: Option<(f64, Node)> = None;
    if let Some(vals) = node_vals {
        let mut point = vec![0.0; g.len()];
        for k in 0..g.len() {
            point[k] = vals[k].unwrap_or(f64::NAN);
        }
        for k in 0..g.len() {
            let op = g.op(k as Node);
            if matches!(op, Op::Const(_) | Op::Var(_) | Op::Sum(..)) || vals[k].is_none() {
                continue;
            }
            let at = |c: Node| point[c as usize];
            let truth = match op {
                Op::Mul(a, b) => at(*a) * at(*b),
                Op::Div(a, b) => at(*a) / at(*b),
                Op::PowI(a, p) => at(*a).powi(*p),
                Op::PowC(a, c) => at(*a).powf(*c),
                Op::Pow(a, b) => at(*a).powf(at(*b)),
                Op::Func(f, a) => f.apply(at(*a)),
                _ => unreachable!(),
            };
            let w = point[k];
            let violation = if truth.is_finite() { (w - truth).abs() / (1.0 + truth.abs()) } else { f64::INFINITY };
            if violation > 1e-9 && worst.is_none_or(|(v, _)| violation > v) {
                worst = Some((violation, k as Node));
            }
        }
    }
    let candidates: Vec<usize> = match worst {
        Some((_, k)) => g.vars_of(k).into_iter().filter(|&j| can_split(j)).collect(),
        None => Vec::new(),
    };
    // The widest candidate (or, without one, the widest variable that a
    // nonlinear term uses, then any variable).
    let pick = |js: &mut dyn Iterator<Item = usize>| js.max_by(|&a, &b| width(a).total_cmp(&width(b)));
    let j = pick(&mut candidates.into_iter())
        .or_else(|| {
            let nonlinear: Vec<usize> = (0..g.len())
                .filter(|&k| !matches!(g.op(k as Node), Op::Const(_) | Op::Var(_) | Op::Sum(..)))
                .flat_map(|k| g.vars_of(k as Node))
                .filter(|&j| can_split(j))
                .collect();
            pick(&mut nonlinear.into_iter())
        })
        .or_else(|| pick(&mut (0..n).filter(|&j| can_split(j))))?;
    // Split at the solution's value, kept away from the ends.
    let (l, h) = (lo[j], hi[j]);
    let v = x[j];
    let split = match (l.is_finite(), h.is_finite()) {
        (true, true) => {
            let m = SPLIT_MARGIN * (h - l);
            if v.is_finite() { v.clamp(l + m, h - m) } else { 0.5 * (l + h) }
        }
        _ => {
            if v.is_finite() && v > l && v < h {
                v
            } else {
                mid(l, h)
            }
        }
    };
    Some((j, split))
}

/// The two halves of [lo, hi] at `split`.
fn children(integer: bool, lo: f64, hi: f64, split: f64) -> Vec<(f64, f64)> {
    if integer {
        let mut f = split.floor();
        if f >= hi {
            f = hi - 1.0;
        }
        if f < lo {
            f = lo;
        }
        vec![(lo, f), (f + 1.0, hi)]
    } else {
        vec![(lo, split), (split, hi)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::Problem;

    fn run(json: &str) -> Outcome {
        let m = crate::model::build(&Problem::from_json(json).unwrap()).unwrap();
        solve(&m, None, Some(20000)).unwrap()
    }

    fn objective(json: &str, x: &[f64]) -> f64 {
        let m = crate::model::build(&Problem::from_json(json).unwrap()).unwrap();
        m.objective_value(x)
    }

    /// Nonconvex problems with several local optima: the global one.
    #[test]
    fn finds_global_optima() {
        // min x^4 - 3x^2 + x on [-2, 2]: local minima near 1.13 (-1.07) and
        // -1.30 (-3.51, global).
        let p = r#"{"version": "1.2", "variables": [{"name": "x", "type": "continuous", "lower": -2, "upper": 2, "start": 1.5}],
            "constraints": [], "solve": {"method": "minimize", "objective": "x^4 - 3 * x^2 + x"}}"#;
        let Outcome::Optimal(x) = run(p) else { panic!("not optimal") };
        assert!((x[0] + 1.3008).abs() < 1e-3, "{x:?}");
        // max sin(x) + sin(3x)/3 on [0, 10]: global maximum near 1.25? The
        // brute force decides.
        let p = r#"{"version": "1.2", "variables": [{"name": "x", "type": "continuous", "lower": 0, "upper": 10, "start": 9}],
            "constraints": [], "solve": {"method": "maximize", "objective": "sin(x) + sin(3 * x) / 3"}}"#;
        let Outcome::Optimal(x) = run(p) else { panic!("not optimal") };
        let best = (0..=1_000_000)
            .map(|i| 10.0 * i as f64 / 1e6)
            .map(|t| t.sin() + (3.0 * t).sin() / 3.0)
            .fold(f64::MIN, f64::max);
        let f = objective(p, &x);
        assert!((f - best).abs() <= 1e-5 * (1.0 + best.abs()), "{f} vs {best}");
        // A bilinear (nonconvex) constraint: min x + y s.t. x y >= 2,
        // 0.5 <= x, y <= 4: x = y = sqrt(2).
        let p = r#"{"version": "1.2", "variables": [{"name": "x", "type": "continuous", "lower": 0.5, "upper": 4},
                {"name": "y", "type": "continuous", "lower": 0.5, "upper": 4}],
            "constraints": [{"expr": "x * y >= 2"}], "solve": {"method": "minimize", "objective": "x + y"}}"#;
        let Outcome::Optimal(x) = run(p) else { panic!("not optimal") };
        assert!((x[0] + x[1] - 2.0 * 2f64.sqrt()).abs() < 1e-5, "{x:?}");
    }

    /// A model over [0.5, 1.5]^2 from graph roots: minimize `obj` subject to
    /// `con <= bound`.
    fn graph_model(graph: crate::nlp::graph::Graph, obj: Node, con: Node, bound: f64, integer: bool) -> LinearModel {
        use crate::model::{Nonlinear, NonlinearRow, Sense};
        LinearModel {
            col_lower: vec![0.5, if integer { 0.0 } else { 0.5 }],
            col_upper: vec![1.5, if integer { 2.0 } else { 1.5 }],
            col_integer: vec![false, integer],
            col_domain: vec![None, None],
            col_start: vec![None, None],
            rows: vec![],
            obj: vec![0.0, 0.0],
            obj_offset: 0.0,
            sense: Sense::Minimize,
            want_all: false,
            vars: vec![],
            violated_constant_row: None,
            uses_cp_kinds: false,
            scheduling: vec![],
            nonlinear: Some(Nonlinear {
                graph,
                rows: vec![NonlinearRow {
                    label: "c".into(),
                    coefs: vec![],
                    node: con,
                    lower: f64::NEG_INFINITY,
                    upper: bound,
                }],
                objective: Some(obj),
            }),
        }
    }

    /// Random nonconvex problems in two variables (one of them integer in
    /// some cases): the global answer is no worse than a fine grid's best,
    /// and not implausibly better.
    #[test]
    fn agrees_with_grid_search() {
        use crate::nlp::graph::testing::{Rng, random};
        let mut rng = Rng(0x5851f42d4c957f2d);
        let mut solved = 0;
        for case in 0..60 {
            let integer = case % 3 == 2;
            let mut g = crate::nlp::graph::Graph::new();
            let obj = random(&mut g, &mut rng, 2, 4);
            let con = random(&mut g, &mut rng, 2, 3);
            let mut vals = Vec::new();
            let xr = [rng.real(0.5, 1.5), if integer { 1.0 } else { rng.real(0.5, 1.5) }];
            g.eval(&xr, &mut vals);
            if !vals[con as usize].is_finite() || g.vars_of(obj).is_empty() {
                continue;
            }
            let bound = vals[con as usize] + 0.05;
            let m = graph_model(g.clone(), obj, con, bound, integer);
            let f_of = |x: &[f64]| {
                let mut v = Vec::new();
                g.eval(x, &mut v);
                (v[obj as usize], v[con as usize])
            };
            let mut grid = f64::INFINITY;
            let ys: Vec<f64> =
                if integer { vec![0.0, 1.0, 2.0] } else { (0..=300).map(|k| 0.5 + k as f64 / 300.0).collect() };
            for i in 0..=300 {
                for &y in &ys {
                    let (f, c) = f_of(&[0.5 + i as f64 / 300.0, y]);
                    if c <= bound && f.is_finite() {
                        grid = grid.min(f);
                    }
                }
            }
            match solve(&m, None, Some(20000)).unwrap() {
                Outcome::Optimal(x) => {
                    let (f, c) = f_of(&x);
                    assert!(c <= bound + 1e-5 * (1.0 + bound.abs()), "case {case}: infeasible {x:?}");
                    assert!(f <= grid + 1e-5 * (1.0 + grid.abs()), "case {case}: {f} worse than the grid's {grid}");
                    assert!(
                        f >= grid - 0.05 * (1.0 + grid.abs()),
                        "case {case}: {f} implausibly below the grid's {grid}"
                    );
                    solved += 1;
                }
                Outcome::Infeasible | Outcome::LocallyInfeasible => {
                    panic!("case {case}: infeasible, but the reference point is feasible")
                }
                Outcome::Stopped { .. } => {}
            }
        }
        assert!(solved >= 30, "only {solved} solved");
    }

    #[test]
    fn mixed_integer_nonlinear() {
        // min (x - 2.6)^2 + (y - 1.4)^2 + n^2 / 4 with integer n in [0, 5] and
        // x + y >= n: n = 2 or so; compare with brute force over n.
        let p = r#"{"version": "1.2", "variables": [{"name": "x", "type": "continuous", "lower": 0, "upper": 5},
                {"name": "y", "type": "continuous", "lower": 0, "upper": 5},
                {"name": "n", "type": "integer", "lower": 0, "upper": 5}],
            "constraints": [{"expr": "x + y >= n + 1.5"}, {"expr": "x * y <= n"}],
            "solve": {"method": "minimize", "objective": "(x - 2.6)^2 + (y - 1.4)^2 + n^2 / 4"}}"#;
        let Outcome::Optimal(x) = run(p) else { panic!("not optimal") };
        assert_eq!(x[2], x[2].round());
        let f = objective(p, &x);
        // Brute force on a grid for each n.
        let mut best = f64::INFINITY;
        for n in 0..=5 {
            let nf = n as f64;
            for i in 0..=500 {
                for k in 0..=500 {
                    let (a, b) = (5.0 * i as f64 / 500.0, 5.0 * k as f64 / 500.0);
                    if a + b >= nf + 1.5 && a * b <= nf {
                        best = best.min((a - 2.6).powi(2) + (b - 1.4).powi(2) + nf * nf / 4.0);
                    }
                }
            }
        }
        assert!(f <= best + 1e-6, "{f} worse than the grid's {best}");
        assert!(f >= best - 0.05, "{f} far below the grid's {best}: infeasible?");
    }

    #[test]
    fn infeasibility_and_limits() {
        // x^2 + y^2 == 4 and x y == 3: no solution (x y <= 2 on the circle).
        let p = r#"{"version": "1.2", "variables": [{"name": "x", "type": "continuous", "lower": -3, "upper": 3},
                {"name": "y", "type": "continuous", "lower": -3, "upper": 3}],
            "constraints": [{"expr": "x^2 + y^2 == 4"}, {"expr": "x * y == 3"}], "solve": {"method": "satisfy"}}"#;
        assert!(matches!(run(p), Outcome::Infeasible));
        // Satisfiable: x y == 1.5 on the circle.
        let q = p.replace("x * y == 3", "x * y == 1.5");
        let Outcome::Optimal(x) = run(&q) else { panic!("not feasible") };
        assert!((x[0] * x[1] - 1.5).abs() < 1e-5 && (x[0] * x[0] + x[1] * x[1] - 4.0).abs() < 1e-5, "{x:?}");
        // A node limit stops with a bound.
        let m = crate::model::build(
            &Problem::from_json(
                r#"{"version": "1.2", "sets": {"I": {"from": 1, "to": 6}},
                "variables": [{"name": "x", "type": "continuous", "index": ["I"], "lower": -3, "upper": 3}],
                "constraints": [], "solve": {"method": "minimize", "objective": "sum(i in I) sin(3 * x[i]) * x[i]^2 * cos(x[i])"}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        match solve(&m, None, Some(5)).unwrap() {
            Outcome::Stopped { x: Some(x), bound: Some(b), .. } => assert!(b <= m.objective_value(&x) + 1e-9),
            Outcome::Optimal(_) => {}
            _ => panic!("no point"),
        }
    }
}
