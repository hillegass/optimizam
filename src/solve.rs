//! Runs branch and bound on a model and maps what it found to a result
//! status (docs/solver-design.md, "Mapping to result statuses").

use crate::api::StopReason;
use crate::config::{Observer, Report};
use crate::cp;
use crate::encode::{Affine, Global, Interval};
use crate::lp::Lp;
use crate::milp::{self, End, Milp, Settings, tie_tolerance};
use crate::model::{LinearModel, Sense};
use crate::nlp;
use crate::presolve::{self, Presolved};
use crate::result::{Outcome, Status};
use crate::simplex;
use std::rc::Rc;
use std::time::Instant;

fn outcome(status: Status, solutions: Vec<Vec<f64>>) -> Outcome {
    Outcome { status, solutions, objective_bound: None }
}

/// A bound in a presolved problem's (minimizing) costs, in the model's
/// objective: the presolved LP leaves out the removed columns' costs.
fn model_objective(model: &LinearModel, post: &presolve::Postsolve, b: f64) -> f64 {
    let sign = if model.sense == Sense::Maximize { -1.0 } else { 1.0 };
    sign * (b + post.offset) + model.obj_offset
}

/// Reports a presolved problem's incumbents and bounds (the MILP and CP
/// engines') to the observer, in the model's columns and objective.
struct Restored<'a> {
    observer: Rc<dyn Observer>,
    model: &'a LinearModel,
    post: &'a presolve::Postsolve,
}

impl Report for Restored<'_> {
    fn solution(&self, x: &[f64]) {
        let y = self.post.restore(x);
        let objective = (self.model.sense != Sense::Satisfy).then(|| self.model.objective_value(&y));
        self.observer.solution(&y, objective);
    }

    fn bound(&self, bound: f64) {
        if self.model.sense != Sense::Satisfy {
            self.observer.bound(model_objective(self.model, self.post, bound));
        }
    }
}

/// Reports the global search's incumbents (in the model's columns) and
/// bounds (in the problem's sense) to the observer.
struct Direct<'a> {
    observer: Rc<dyn Observer>,
    model: &'a LinearModel,
}

impl Report for Direct<'_> {
    fn solution(&self, x: &[f64]) {
        let objective = (self.model.sense != Sense::Satisfy).then(|| self.model.objective_value(x));
        self.observer.solution(x, objective);
    }

    fn bound(&self, bound: f64) {
        self.observer.bound(bound);
    }
}

/// Runs the CP engine on the presolved problem and maps its result.
fn solve_cp(
    model: &LinearModel,
    problem: &crate::milp::Milp,
    native: &cp::Native,
    post: &presolve::Postsolve,
    deadline: Option<Instant>,
) -> Result<Outcome, String> {
    let (satisfy, want_all) = (model.sense == Sense::Satisfy, model.want_all);
    let reporter = crate::config::observer().map(|observer| Restored { observer, model, post });
    let report = reporter.as_ref().map(|r| r as &dyn Report);
    let settings = cp::Settings { want_all, satisfy, deadline, report, limit: model.solution_limit };
    crate::config::ran("cp");
    let r = cp::solve(problem, native, &settings)?;
    let solutions: Vec<Vec<f64>> = r.solutions.iter().map(|x| post.restore(x)).collect();
    let bound = r.bound.filter(|_| !satisfy).map(|b| model_objective(model, post, b));
    Ok(match (r.end, solutions.is_empty()) {
        (_, true) if r.end == cp::End::Complete => outcome(Status::Infeasible, vec![]),
        (_, true) => Outcome { status: Status::Timeout, solutions: vec![], objective_bound: bound },
        (cp::End::Complete, false) => outcome(Status::Complete, solutions),
        (cp::End::SolutionLimit, false) => {
            crate::config::stopped_by(StopReason::SolutionLimit);
            outcome(Status::Incomplete, solutions)
        }
        // Time ran out after finding solutions.
        (cp::End::TimeLimit, false) if want_all && (satisfy || r.optimum_proven) => {
            outcome(Status::Incomplete, solutions)
        }
        // The bound from level 0 of the search (none with a constant objective).
        (cp::End::TimeLimit, false) => {
            Outcome { status: Status::NotProvenOptimal, solutions: solutions[..1].to_vec(), objective_bound: bound }
        }
    })
}

/// A linear program's duals, in the model's rows and columns and in the
/// problem's own sense: how much the optimal objective changes per unit
/// that a row's right side (`rows`) or a column's binding bound (`cols`,
/// the reduced costs) rises.
pub struct Duals {
    pub rows: Vec<f64>,
    pub cols: Vec<f64>,
}

/// Solves a linear program (continuous variables, linear rows, an
/// objective) for its optimum and its duals: by the simplex method alone,
/// without presolve, whose substitutions would leave rows without a dual.
/// None if the model isn't one, or doesn't end at an optimum (`solve` then
/// says what it is instead).
pub fn linear_program(model: &LinearModel, deadline: Option<Instant>) -> Option<(Outcome, Duals)> {
    let linear = model.nonlinear.is_none() && model.scheduling.is_empty() && !model.uses_cp_kinds;
    let continuous = !model.col_integer.iter().any(|&integer| integer);
    let settled = model.violated_constant_row.is_some() || model.num_cols() == 0;
    if !(linear && continuous) || settled || model.sense == Sense::Satisfy {
        return None;
    }
    let lp = Lp::from_model(model, false);
    let opts = simplex::Options { deadline, reduced_costs: true, ..simplex::Options::default() };
    let sol = simplex::Simplex::new(&lp).solve(&lp.col_lower, &lp.col_upper, None, &opts);
    if sol.status != simplex::LpStatus::Optimal || sol.row_dual.is_empty() {
        return None;
    }
    crate::config::ran("milp");
    crate::config::count_nodes(1);
    // The LP minimizes: a maximization's duals have the other sign.
    let sign = if model.sense == Sense::Maximize { -1.0 } else { 1.0 };
    let duals = Duals {
        rows: sol.row_dual.iter().map(|y| sign * y).collect(),
        cols: sol.col_dual.iter().map(|d| sign * d).collect(),
    };
    Some((outcome(Status::Complete, vec![sol.col_value]), duals))
}

/// Which engine solves the problem ([`Options::engine`](crate::Options::engine)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Engine {
    /// CP for problems it can take (every variable integer with finite
    /// bounds, rows with integer coefficients after scaling) that are satisfy,
    /// "all", or use a format 1.1 constraint kind; MILP otherwise.
    /// Problems with nonlinear terms are solved globally, by spatial branch
    /// and bound.
    #[default]
    Auto,
    /// LP-based branch and bound.
    Milp,
    /// Lazy clause generation (CDCL over integer bounds).
    Cp,
    /// Interior point method for continuous problems (with nonlinear terms
    /// or not); finds local optima. (auto solves nonlinear problems
    /// globally, with spatial branch and bound.)
    Nlp,
}

/// Without a time limit, the global search stops after this many nodes.
const GLOBAL_NODE_LIMIT: usize = 2_000;

/// Solves a nonlinear model globally (spatial branch and bound; integer
/// variables allowed).
fn solve_global(model: &LinearModel, deadline: Option<Instant>) -> Result<Outcome, String> {
    if model.want_all {
        return Err("unsupported: \"solutions\": \"all\" (or a number) for a problem with nonlinear terms".into());
    }
    let encoded;
    let model = if model.scheduling.is_empty() {
        model
    } else {
        encoded = {
            let mut m = model.clone();
            m.encode_scheduling()?;
            m
        };
        &encoded
    };
    if model.violated_constant_row.is_some() {
        return Ok(outcome(Status::Infeasible, vec![]));
    }
    let limit = if deadline.is_none() { Some(GLOBAL_NODE_LIMIT) } else { None };
    crate::config::ran("global");
    let reporter = crate::config::observer().map(|observer| Direct { observer, model });
    let report = reporter.as_ref().map(|r| r as &dyn Report);
    Ok(match nlp::global::solve(model, deadline, limit, report)? {
        nlp::global::Outcome::Optimal(x) => outcome(Status::Complete, vec![x]),
        nlp::global::Outcome::Infeasible => outcome(Status::Infeasible, vec![]),
        nlp::global::Outcome::LocallyInfeasible => outcome(Status::LocallyInfeasible, vec![]),
        nlp::global::Outcome::Stopped { x: Some(x), local_optimum, bound } => Outcome {
            status: if local_optimum { Status::LocallyOptimal } else { Status::NotProvenOptimal },
            solutions: vec![x],
            objective_bound: bound,
        },
        nlp::global::Outcome::Stopped { x: None, bound, .. } => {
            Outcome { status: Status::Timeout, solutions: vec![], objective_bound: bound }
        }
    })
}

/// Solves a continuous model with the interior point method.
fn solve_nlp(model: &LinearModel, deadline: Option<Instant>) -> Result<Outcome, String> {
    if let Some(j) = model.col_integer.iter().position(|&i| i) {
        let var = model.vars.iter().find(|v| v.elements.iter().any(|e| e.1 == j)).map_or("?", |v| v.name.as_str());
        let place = model.nonlinear.as_ref().map_or("the problem".to_string(), |nl| nl.first_place());
        return Err(format!(
            "unsupported: nonlinear terms with integer or binary variables (mixed-integer nonlinear \
             programs): {place} is nonlinear and {var} is not continuous"
        ));
    }
    if model.violated_constant_row.is_some() {
        return Ok(outcome(Status::Infeasible, vec![]));
    }
    crate::config::ran("nlp");
    Ok(match nlp::solve(model, deadline)? {
        nlp::Outcome::Optimal { x, proven: true } => outcome(Status::Complete, vec![x]),
        nlp::Outcome::Optimal { x, proven: false } => outcome(Status::LocallyOptimal, vec![x]),
        nlp::Outcome::Feasible(x) => outcome(Status::NotProvenOptimal, vec![x]),
        nlp::Outcome::LocallyInfeasible => outcome(Status::LocallyInfeasible, vec![]),
        nlp::Outcome::Infeasible => outcome(Status::Infeasible, vec![]),
        nlp::Outcome::NoSolution => outcome(Status::Timeout, vec![]),
    })
}

/// The model's natively propagated constraints for the CP engine (its
/// deferred `no_overlap`, `cumulative`, and `all_different` instances), with
/// each original column mapped by `column` to a column of the problem it
/// solves or to a fixed value. Tasks fixed absent are left out.
fn native_for_cp(model: &LinearModel, column: &dyn Fn(usize) -> Result<usize, f64>) -> cp::Native {
    let map = |a: &Affine| -> Affine {
        let mut out = Affine { coefs: Vec::new(), constant: a.constant };
        for &(j, v) in &a.coefs {
            match column(j) {
                Ok(k) => out.coefs.push((k, v)),
                Err(x) => out.constant += v * x,
            }
        }
        out
    };
    let all_different = model
        .scheduling
        .iter()
        .filter_map(|g| match g {
            Global::AllDifferent { items, .. } => Some(items.iter().map(map).collect()),
            _ => None,
        })
        .collect();
    cp::Native { scheduling: scheduling_for_cp(model, &map), all_different }
}

/// The model's `no_overlap` and `cumulative` instances, with columns mapped by `map`.
fn scheduling_for_cp(model: &LinearModel, map: &dyn Fn(&Affine) -> Affine) -> Vec<cp::Scheduling> {
    let column = |j: usize| -> Result<usize, f64> {
        let a = map(&Affine { coefs: vec![(j, 1.0)], constant: 0.0 });
        a.coefs.first().map(|c| c.0).ok_or(a.constant)
    };
    let one = Affine { coefs: Vec::new(), constant: 1.0 };
    let task = |iv: &Interval, demand: &Affine| -> Option<cp::TaskDef> {
        let presence = match iv.presence.map(column) {
            None => None,
            Some(Ok(k)) => Some(k),
            Some(Err(x)) if x > 0.5 => None,
            Some(Err(_)) => return None, // absent
        };
        Some(cp::TaskDef { start: map(&iv.start), duration: map(&iv.duration), presence, demand: map(demand) })
    };
    model
        .scheduling
        .iter()
        .filter_map(|g| match g {
            Global::NoOverlap { intervals, .. } => Some(cp::Scheduling {
                tasks: intervals.iter().filter_map(|iv| task(iv, &one)).collect(),
                capacity: None,
            }),
            Global::Cumulative { intervals, demands, capacity, .. } => Some(cp::Scheduling {
                tasks: intervals.iter().zip(demands).filter_map(|(iv, d)| task(iv, d)).collect(),
                capacity: Some(map(capacity)),
            }),
            Global::AllDifferent { .. } => None,
            _ => unreachable!("only scheduling and all_different instances are deferred"),
        })
        .collect()
}

/// The columns that the model's deferred (natively propagated) instances use.
fn scheduling_columns(model: &LinearModel) -> Vec<bool> {
    let mut cols: Vec<usize> = Vec::new();
    for g in &model.scheduling {
        let (intervals, extra): (&[Interval], Vec<&Affine>) = match g {
            Global::NoOverlap { intervals, .. } => (intervals, vec![]),
            Global::Cumulative { intervals, demands, capacity, .. } => {
                (intervals, demands.iter().chain([capacity]).collect())
            }
            Global::AllDifferent { items, .. } => (&[], items.iter().collect()),
            _ => continue,
        };
        for iv in intervals {
            cols.extend(iv.start.coefs.iter().chain(&iv.duration.coefs).map(|c| c.0));
            cols.extend(iv.presence);
        }
        cols.extend(extra.into_iter().flat_map(|a| a.coefs.iter().map(|c| c.0)));
    }
    let mut used = vec![false; model.num_cols()];
    cols.into_iter().for_each(|j| used[j] = true);
    used
}

pub fn solve(model: &LinearModel, deadline: Option<Instant>, engine: Engine) -> Result<Outcome, String> {
    // Nonlinear problems go to the interior point method.
    match (&model.nonlinear, engine) {
        (_, Engine::Nlp) => return solve_nlp(model, deadline),
        (Some(_), Engine::Auto) => return solve_global(model, deadline),
        (Some(nl), Engine::Milp | Engine::Cp) => {
            return Err(format!(
                "the {} engine can't solve nonlinear problems ({} is nonlinear); use --engine=nlp or auto",
                if engine == Engine::Milp { "milp" } else { "cp" },
                nl.first_place()
            ));
        }
        (None, _) => {}
    }
    // Scheduling constraints are propagated natively by the CP engine, and
    // encoded as linear rows for the MILP engine.
    let native = !model.scheduling.is_empty()
        && match engine {
            Engine::Milp | Engine::Nlp => false,
            Engine::Cp => true,
            Engine::Auto => {
                let lp = Lp::from_model(model, model.sense == Sense::Satisfy);
                let milp = Milp { lp, integer: model.col_integer.clone(), domain: model.col_domain.clone() };
                cp::applicable(&milp, &native_for_cp(model, &|j| Ok(j))).is_ok()
            }
        };
    let encoded;
    let model = if model.scheduling.is_empty() || native {
        model
    } else {
        encoded = {
            let mut m = model.clone();
            m.encode_scheduling()?;
            m
        };
        &encoded
    };
    if model.violated_constant_row.is_some() {
        return Ok(outcome(Status::Infeasible, vec![]));
    }
    if model.num_cols() == 0 {
        return Ok(outcome(Status::Complete, vec![vec![]]));
    }

    let satisfy = model.sense == Sense::Satisfy;
    let milp_of = |zero_cost: bool| {
        let lp = Lp::from_model(model, zero_cost);
        Milp { lp, integer: model.col_integer.clone(), domain: model.col_domain.clone() }
    };
    let mut original = milp_of(satisfy);
    if !original.lp.tighten_integer_rows(&original.integer) {
        return Ok(outcome(Status::Infeasible, vec![]));
    }

    // Presolve. Dual reductions keep only some optimal solutions, so they are
    // off when all of them are wanted.
    let mut stats = presolve::Stats::default();
    let options = presolve::Options { dual: !model.want_all, frozen: scheduling_columns(model) };
    let presolved = presolve::presolve(&original, &options, &mut stats);
    if crate::config::debug() {
        crate::config::debug_log!("presolve: {stats:?}");
    }
    let (problem, post) = match presolved {
        Presolved::Infeasible => return Ok(outcome(Status::Infeasible, vec![])),
        Presolved::Reduced(problem, post) => (problem, post),
    };
    let native_cons = native_for_cp(model, &|j| post.column(j));
    if problem.lp.num_col == 0 && !native {
        // Presolve fixed every column (and dropped every row).
        return Ok(outcome(Status::Complete, vec![post.restore(&[])]));
    }

    let use_cp = match engine {
        Engine::Milp | Engine::Nlp => false,
        _ if native => {
            cp::applicable(&problem, &native_cons)
                .map_err(|why| format!("the CP engine can't solve this problem: {why}"))?;
            true
        }
        Engine::Cp => {
            cp::applicable(&problem, &native_cons)
                .map_err(|why| format!("the CP engine can't solve this problem: {why}"))?;
            true
        }
        Engine::Auto => {
            (satisfy || model.want_all || model.uses_cp_kinds) && cp::applicable(&problem, &native_cons).is_ok()
        }
    };
    if crate::config::debug() {
        crate::config::debug_log!("engine: {}", if use_cp { "cp" } else { "milp" });
    }
    if use_cp {
        return solve_cp(model, &problem, &native_cons, &post, deadline);
    }

    let reporter = crate::config::observer().map(|observer| Restored { observer, model, post: &post });
    let settings = Settings {
        want_all: model.want_all,
        solution_limit: model.solution_limit,
        stop_at_first: satisfy && !model.want_all,
        deadline,
        report: reporter.as_ref().map(|r| r as &dyn Report),
        ..Settings::default()
    };
    crate::config::ran("milp");
    let r = milp::branch_and_bound(&problem, &settings)?;
    let restore = |x: &[f64]| post.restore(x);

    match (r.end, r.incumbent) {
        (End::RootUnbounded, _) => {
            // Unbounded or infeasible: look for any integer-feasible point.
            // Presolve's reductions keep feasibility, so the reduced problem
            // with a zero objective decides it.
            let mut zero = problem.clone();
            zero.lp.col_cost.iter_mut().for_each(|c| *c = 0.0);
            let search = Settings { stop_at_first: true, deadline, ..Settings::default() };
            let z = milp::branch_and_bound(&zero, &search)?;
            Ok(match (z.end, z.incumbent) {
                (_, Some(_)) => outcome(Status::Unbounded, vec![]),
                (End::Complete, None) => outcome(Status::Infeasible, vec![]),
                (End::TimeLimit | End::NodeLimit | End::SolutionLimit, None) => outcome(Status::Timeout, vec![]),
                (End::RootUnbounded, None) => {
                    return Err("internal error: LP with zero objective reported unbounded".into());
                }
            })
        }
        (_, None) if r.end == End::Complete => Ok(outcome(Status::Infeasible, vec![])),
        // The bound of the nodes still open, if the root's LP got that far.
        (_, None) => Ok(Outcome {
            status: Status::Timeout,
            solutions: vec![],
            objective_bound: (!satisfy && r.bound.is_finite()).then(|| model_objective(model, &post, r.bound)),
        }),
        (end, Some(inc)) => {
            // Objectives in the original LP's costs.
            let (best, bound) = (inc.objective + post.offset, r.bound + post.offset);
            let proven = end == End::Complete || satisfy || bound >= best - tie_tolerance(best);
            if model.want_all {
                let status = if end == End::Complete { Status::Complete } else { Status::Incomplete };
                if proven {
                    let solutions = if r.solutions.is_empty() {
                        vec![restore(&inc.x)]
                    } else {
                        r.solutions.iter().map(|x| restore(x)).collect()
                    };
                    if end == End::SolutionLimit {
                        crate::config::stopped_by(StopReason::SolutionLimit);
                    }
                    return Ok(outcome(status, solutions));
                }
            } else if proven {
                return Ok(outcome(Status::Complete, vec![restore(&inc.x)]));
            }
            let (sign, offset) = (if model.sense == Sense::Maximize { -1.0 } else { 1.0 }, model.obj_offset);
            Ok(Outcome {
                status: Status::NotProvenOptimal,
                solutions: vec![restore(&inc.x)],
                objective_bound: Some(sign * bound + offset),
            })
        }
    }
}
