//! `Options::relative_gap`, `Options::absolute_gap`, and `--gap`: the search
//! stops once its best solution is proven near enough to the optimum.

mod common;

use common::{JobShop, Knapsack};
use optimizam::{Control, Model, Options, Status, StopReason, constraint, var};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn limited() -> Options {
    Options::default().time_limit(Duration::from_secs(60))
}

#[test]
fn a_relative_gap_stops_the_search_within_it() {
    let k = Knapsack::new(150, 10);
    let optimum = k.model.solve(&limited()).unwrap();
    assert_eq!(optimum.status(), Status::Complete);
    let optimum = optimum.objective().unwrap();
    for threads in [1, 4] {
        let result = k.model.solve(&limited().relative_gap(0.005).threads(threads)).unwrap();
        assert_eq!(result.status(), Status::NotProvenOptimal, "{threads} threads");
        assert_eq!(result.stop_reason(), Some(StopReason::Gap));
        let best = k.check(result.best().unwrap());
        let bound = result.objective_bound().unwrap();
        // The gap is the one the result reports, and the bound is one: the
        // optimum lies between the solution and it (a maximization).
        assert!(result.gap().is_some_and(|gap| gap <= 0.005), "{:?}", result.gap());
        assert!(best <= optimum + 1e-6 && optimum <= bound + 1e-6, "{best} <= {optimum} <= {bound}");
        let json: serde_json::Value = serde_json::from_str(&result.to_json()).unwrap();
        assert_eq!((&json["status"], &json["stop_reason"]), (&"not_proven_optimal".into(), &"gap".into()));
    }
}

#[test]
fn an_absolute_gap_is_in_the_objectives_units() {
    let k = Knapsack::new(150, 10);
    let result = k.model.solve(&limited().absolute_gap(40.0).threads(1)).unwrap();
    assert_eq!((result.status(), result.stop_reason()), (Status::NotProvenOptimal, Some(StopReason::Gap)));
    let (best, bound) = (result.objective().unwrap(), result.objective_bound().unwrap());
    assert!(bound - best <= 40.0 && bound >= best, "{best} and {bound}");
    // Whichever of two gaps is reached first stops the search: a relative
    // gap nothing reaches leaves the absolute one to do it.
    let both = k.model.solve(&limited().absolute_gap(40.0).relative_gap(1e-12).threads(1)).unwrap();
    assert_eq!(both.stop_reason(), Some(StopReason::Gap));
    assert_eq!(both.objective(), result.objective());
}

#[test]
fn a_gap_no_wider_than_the_engines_own_changes_nothing() {
    let k = Knapsack::new(40, 3);
    let plain = k.model.solve(&limited().threads(1)).unwrap();
    for options in [limited().relative_gap(0.0), limited().absolute_gap(0.0), limited().relative_gap(1e-9)] {
        let result = k.model.solve(&options.threads(1)).unwrap();
        assert_eq!((result.status(), result.stop_reason()), (Status::Complete, None));
        assert_eq!(result.objective(), plain.objective());
    }
}

#[test]
fn a_gap_is_for_one_optimal_solution() {
    // Every optimal solution, and any feasible one: no gap to stop at.
    let mut m = Model::new();
    let x = m.add_vars(4, var().integer().bounds(0, 3).name("x"));
    m.add(constraint!(x[0] + x[1] + x[2] + x[3] == 6));
    m.maximize(x[0] + x[1]);
    m.all_solutions(true);
    let all = m.solve(&Options::default()).unwrap();
    let with_gap = m.solve(&Options::default().relative_gap(0.9)).unwrap();
    assert_eq!((with_gap.status(), with_gap.stop_reason()), (Status::Complete, None));
    assert_eq!(with_gap.solutions().len(), all.solutions().len());

    let mut m = Model::new();
    let q = m.add_vars(6, var().integer().bounds(1, 6).name("q"));
    m.all_different(&q);
    let any = m.solve(&Options::default().relative_gap(0.9).absolute_gap(100.0)).unwrap();
    assert_eq!((any.status(), any.stop_reason()), (Status::Complete, None));
}

#[test]
fn the_cp_engine_stops_at_a_gap_too() {
    // The CP engine's bound is weak on a job shop, so the gap is wide.
    let shop = JobShop::new(10, 10, 3);
    let result = shop.model.solve(&limited().relative_gap(0.6).threads(1)).unwrap();
    assert_eq!((result.status(), result.stop_reason()), (Status::NotProvenOptimal, Some(StopReason::Gap)));
    assert!(result.gap().is_some_and(|gap| gap <= 0.6), "{:?}", result.gap());
    shop.check(result.best().unwrap());
}

#[test]
fn a_callback_sees_the_solutions_before_the_gap_stops_the_search() {
    let k = Knapsack::new(150, 10);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let options = limited().threads(1).relative_gap(0.005).on_progress(move |p| {
        if let Some(objective) = p.objective() {
            record.lock().unwrap().push(objective);
        }
        Control::Continue
    });
    let result = k.model.solve(&options).unwrap();
    assert_eq!(result.stop_reason(), Some(StopReason::Gap));
    let seen = seen.lock().unwrap();
    // The last solution reported is the result's (a knapsack has no
    // objective terms that settle afterwards).
    assert_eq!(seen.last().copied(), result.objective());
}

#[test]
#[should_panic(expected = "a gap is a number of 0 or more")]
fn a_negative_gap_is_refused() {
    let _ = Options::default().relative_gap(-0.1);
}

#[test]
#[should_panic(expected = "a gap is a number of 0 or more")]
fn a_gap_that_is_no_number_is_refused() {
    let _ = Options::default().absolute_gap(f64::NAN);
}

#[test]
fn the_command_line_tool_takes_a_gap() {
    let k = Knapsack::new(150, 10);
    let dir = std::env::temp_dir().join(format!("optimizam-gap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("knapsack.json");
    std::fs::write(&file, k.model.to_json().unwrap()).unwrap();
    let tool =
        |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_optimizam")).args(args).arg(&file).arg("-").output().unwrap();
    let out = tool(&["--gap=0.005", "--threads=1", "--time=60"]);
    let usage = [tool(&["--gap=-1"]), tool(&["--gap=half"]), tool(&["--gap=inf"])];
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((&json["status"], &json["stop_reason"]), (&"not_proven_optimal".into(), &"gap".into()));
    assert!(json["gap"].as_f64().is_some_and(|gap| gap <= 0.005), "{}", json["gap"]);
    for out in usage {
        assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    }
}
