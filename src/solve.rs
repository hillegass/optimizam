//! Runs branch and bound on a model and maps what it found to a result
//! status (docs/solver-design.md, "Mapping to result statuses").

use crate::lp::Lp;
use crate::mip::{self, End, Mip, Settings, tie_tolerance};
use crate::model::{LinearModel, Sense};
use crate::presolve::{self, Presolved};
use crate::result::{Outcome, Status};
use std::time::Instant;

fn outcome(status: Status, solutions: Vec<Vec<f64>>) -> Outcome {
    Outcome { status, solutions, objective_bound: None }
}

pub fn solve(model: &LinearModel, deadline: Option<Instant>) -> Result<Outcome, String> {
    if model.violated_constant_row.is_some() {
        return Ok(outcome(Status::Infeasible, vec![]));
    }
    if model.num_cols() == 0 {
        return Ok(outcome(Status::Complete, vec![vec![]]));
    }

    let satisfy = model.sense == Sense::Satisfy;
    let mip_of = |zero_cost: bool| {
        let lp = Lp::from_model(model, zero_cost);
        Mip { lp, integer: model.col_integer.clone(), domain: model.col_domain.clone() }
    };
    let mut original = mip_of(satisfy);
    if !original.lp.tighten_integer_rows(&original.integer) {
        return Ok(outcome(Status::Infeasible, vec![]));
    }

    // Presolve. Dual reductions keep only some optimal solutions, so they are
    // off when all of them are wanted.
    let mut stats = presolve::Stats::default();
    let presolved = presolve::presolve(&original, &presolve::Options { dual: !model.want_all }, &mut stats);
    if std::env::var_os("OPTIMIZAM_DEBUG").is_some() {
        eprintln!("presolve: {stats:?}");
    }
    let (problem, post) = match presolved {
        Presolved::Infeasible => return Ok(outcome(Status::Infeasible, vec![])),
        Presolved::Reduced(problem, post) => (problem, post),
    };
    if problem.lp.num_col == 0 {
        // Presolve fixed every column (and dropped every row).
        return Ok(outcome(Status::Complete, vec![post.restore(&[])]));
    }

    let settings = Settings {
        want_all: model.want_all,
        stop_at_first: satisfy && !model.want_all,
        deadline,
        ..Settings::default()
    };
    let r = mip::branch_and_bound(&problem, &settings)?;
    let restore = |x: &[f64]| post.restore(x);

    match (r.end, r.incumbent) {
        (End::RootUnbounded, _) => {
            // Unbounded or infeasible: look for any integer-feasible point.
            // Presolve's reductions keep feasibility, so the reduced problem
            // with a zero objective decides it.
            let mut zero = problem.clone();
            zero.lp.col_cost.iter_mut().for_each(|c| *c = 0.0);
            let search = Settings { stop_at_first: true, deadline, ..Settings::default() };
            let z = mip::branch_and_bound(&zero, &search)?;
            Ok(match (z.end, z.incumbent) {
                (_, Some(_)) => outcome(Status::Unbounded, vec![]),
                (End::Complete, None) => outcome(Status::Infeasible, vec![]),
                (End::TimeLimit | End::NodeLimit, None) => outcome(Status::Timeout, vec![]),
                (End::RootUnbounded, None) => {
                    return Err("internal error: LP with zero objective reported unbounded".into());
                }
            })
        }
        (_, None) if r.end == End::Complete => Ok(outcome(Status::Infeasible, vec![])),
        (_, None) => Ok(outcome(Status::Timeout, vec![])),
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
