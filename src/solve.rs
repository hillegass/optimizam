//! Runs branch and bound on a model and maps what it found to a result
//! status (docs/solver-design.md, "Mapping to result statuses").

use crate::lp::Lp;
use crate::mip::{self, End, Settings, tie_tolerance};
use crate::model::{LinearModel, Sense};
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
    let mut lp = Lp::from_model(model, satisfy);
    if !lp.tighten_integer_rows(&model.col_integer) {
        return Ok(outcome(Status::Infeasible, vec![]));
    }
    let settings = Settings { want_all: model.want_all, stop_at_first: satisfy && !model.want_all, deadline };
    let r = mip::branch_and_bound(&lp, model, &settings)?;

    match (r.end, r.incumbent) {
        (End::RootUnbounded, _) => {
            // Unbounded or infeasible: look for any integer-feasible point.
            let mut zero = Lp::from_model(model, true);
            zero.tighten_integer_rows(&model.col_integer);
            let search = Settings { want_all: false, stop_at_first: true, deadline };
            let z = mip::branch_and_bound(&zero, model, &search)?;
            Ok(match (z.end, z.incumbent) {
                (_, Some(_)) => outcome(Status::Unbounded, vec![]),
                (End::Complete, None) => outcome(Status::Infeasible, vec![]),
                (End::TimeLimit, None) => outcome(Status::Timeout, vec![]),
                (End::RootUnbounded, None) => {
                    return Err("internal error: LP with zero objective reported unbounded".into());
                }
            })
        }
        (_, None) if r.end == End::Complete => Ok(outcome(Status::Infeasible, vec![])),
        (_, None) => Ok(outcome(Status::Timeout, vec![])),
        (end, Some(inc)) => {
            let proven = end == End::Complete || satisfy || r.bound >= inc.objective - tie_tolerance(inc.objective);
            if model.want_all {
                let status = if end == End::Complete { Status::Complete } else { Status::Incomplete };
                if proven {
                    let solutions = if r.solutions.is_empty() { vec![inc.x] } else { r.solutions };
                    return Ok(outcome(status, solutions));
                }
            } else if proven {
                return Ok(outcome(Status::Complete, vec![inc.x]));
            }
            let (sign, offset) = (if model.sense == Sense::Maximize { -1.0 } else { 1.0 }, model.obj_offset);
            Ok(Outcome {
                status: Status::NotProvenOptimal,
                solutions: vec![inc.x],
                objective_bound: Some(sign * r.bound + offset),
            })
        }
    }
}
