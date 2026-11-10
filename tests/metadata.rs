//! What a result says besides its solutions: the sense, the bound and gap,
//! why the search stopped, and what it took (engine, threads, time, nodes).

mod common;

use common::{JobShop, Knapsack};
use optimizam::{Engine, Model, Options, Problem, SolveResult, Status, StopReason, constraint, var};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

fn json_of(result: &SolveResult) -> Value {
    serde_json::from_str(&result.to_json()).unwrap()
}

fn example(name: &str) -> Problem {
    Problem::from_file(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(name)).unwrap()
}

#[test]
fn a_proven_result_has_no_bound_gap_or_stop_reason() {
    let k = Knapsack::new(30, 3);
    let result = k.model.solve(&Options::default().engine(Engine::Milp)).unwrap();
    assert_eq!(result.status(), Status::Complete);
    assert_eq!((result.objective_bound(), result.gap(), result.stop_reason()), (None, None, None));
    assert_eq!(result.engine(), Some(Engine::Milp));
    assert!(result.nodes().is_some_and(|n| n >= 1));
    let json = json_of(&result);
    assert_eq!(
        (&json["version"], &json["status"], &json["sense"]),
        (&json!("1.2"), &json!("complete"), &json!("maximize"))
    );
    for absent in ["objective_bound", "gap", "stop_reason"] {
        assert!(json.get(absent).is_none(), "{absent}");
    }
    let stats = &json["statistics"];
    assert_eq!(stats["engine"], "milp");
    assert_eq!(stats["nodes"].as_u64(), result.nodes());
    assert!(stats["threads"].as_u64().is_some_and(|t| t >= 1));
    assert!(stats["time"].as_f64().is_some_and(|t| t >= 0.0 && (t - result.elapsed().as_secs_f64()).abs() < 0.001));
}

#[test]
fn a_stopped_result_says_how_far_it_got() {
    let k = Knapsack::new(150, 10);
    let limit = Duration::from_millis(200);
    let result = k.model.solve(&Options::default().threads(1).time_limit(limit)).unwrap();
    assert_eq!((result.status(), result.stop_reason()), (Status::NotProvenOptimal, Some(StopReason::TimeLimit)));
    let (objective, bound) = (result.objective().unwrap(), result.objective_bound().unwrap());
    let gap = (bound - objective) / objective;
    assert!(bound >= objective && result.gap().is_some_and(|g| (g - gap).abs() < 1e-12));
    assert!(result.elapsed() >= limit && result.elapsed() < limit * 5, "{:?}", result.elapsed());
    let json = json_of(&result);
    assert_eq!(json["stop_reason"], "time_limit");
    // (To the last digit or so: serde_json reads numbers a little loosely.)
    let near = |value: &Value, x: f64| value.as_f64().is_some_and(|v| (v - x).abs() <= 1e-12 * x.abs());
    assert!(near(&json["objective_bound"], bound) && near(&json["gap"], gap), "{json}");
    assert_eq!(json["statistics"]["threads"], 1);
    assert!(json["statistics"]["nodes"].as_u64().is_some_and(|n| n > 0));
}

#[test]
fn every_engine_is_named() {
    // Job shops go to the CP engine; its nodes are conflicts.
    let shop = JobShop::new(6, 5, 3);
    let cp = shop.model.solve(&Options::default()).unwrap();
    assert_eq!((cp.status(), cp.engine()), (Status::Complete, Some(Engine::Cp)));
    assert_eq!(json_of(&cp)["sense"], "minimize");
    // A nonlinear problem: the global search by default, the local solve on request.
    let hs071 = example("hs071.json");
    let global = hs071.solve(&Options::default()).unwrap();
    assert_eq!((global.status(), global.engine()), (Status::Complete, Some(Engine::Global)));
    assert!(global.nodes().is_some_and(|boxes| boxes > 0));
    let local = hs071.solve(&Options::default().engine(Engine::Local)).unwrap();
    assert_eq!((local.engine(), local.nodes()), (Some(Engine::Local), None));
    assert!(local.to_json().contains(r#""engine": "local""#));
    assert!(json_of(&local)["statistics"].get("nodes").is_none());
    // Asked for by name, the global search is what auto ran; and the names are the command's.
    let asked = hs071.solve(&Options::default().engine(Engine::Global)).unwrap();
    assert_eq!((asked.status(), asked.engine()), (Status::Complete, Some(Engine::Global)));
    assert_eq!((Engine::Global.to_string(), Engine::Local.name()), ("global".to_string(), "local"));
    // Each engine says what it is for when asked for something else.
    let message = shop.model.solve(&Options::default().engine(Engine::Global)).unwrap_err().to_string();
    assert!(message.starts_with("the global engine is for problems with nonlinear terms"), "{message}");
    let message = shop.model.solve(&Options::default().engine(Engine::Local)).unwrap_err().to_string();
    assert!(message.contains("the local engine solves continuous problems only"), "{message}");
    let message = hs071.solve(&Options::default().engine(Engine::Cp)).unwrap_err().to_string();
    assert!(message.starts_with("the cp engine can't solve nonlinear problems"), "{message}");
    // Settled before any engine ran.
    let mut m = Model::new();
    let x = m.add_var(var().integer().bounds(0, 5));
    m.add(constraint!(2 * x == 7));
    let none = m.solve(&Options::default()).unwrap();
    assert_eq!((none.status(), none.engine(), none.nodes()), (Status::Infeasible, None, Some(0)));
    assert!(none.to_json().contains(r#""engine": "presolve""#));
    // A satisfaction problem has no objective to bound.
    let mut m = Model::new();
    let x = m.add_var(var().integer().bounds(0, 5));
    m.add(constraint!(x >= 2));
    let any = m.solve(&Options::default()).unwrap();
    assert_eq!(json_of(&any)["sense"], "satisfy");
    assert_eq!((any.objective(), any.gap()), (None, None));
}

#[test]
fn stop_reasons_have_names() {
    let names = [
        (StopReason::TimeLimit, "time_limit"),
        (StopReason::Canceled, "canceled"),
        (StopReason::Callback, "callback"),
        (StopReason::NodeLimit, "node_limit"),
        (StopReason::NoProgress, "no_progress"),
        (StopReason::SolutionLimit, "solution_limit"),
    ];
    for (reason, name) in names {
        assert_eq!((reason.as_str(), reason.to_string().as_str()), (name, name));
    }
    // In a result file.
    let mut m = Model::new();
    let x = m.add_vars(3, var().integer().bounds(1, 3));
    m.all_different(&x);
    m.solution_limit(2);
    let json = json_of(&m.solve(&Options::default()).unwrap());
    assert_eq!((&json["status"], &json["stop_reason"]), (&json!("incomplete"), &json!("solution_limit")));
}

/// A search that stops before its first solution still reports the bound it
/// proved, when it has one.
#[test]
fn a_timeout_can_carry_a_bound() {
    let shop = JobShop::new(15, 15, 7);
    for millis in [1, 3, 10, 30] {
        let options = Options::default().threads(1).engine(Engine::Milp).time_limit(Duration::from_millis(millis));
        let result = shop.model.solve(&options).unwrap();
        let json = json_of(&result);
        assert_eq!(json["objective_bound"].as_f64().is_some(), result.objective_bound().is_some());
        if result.status() == Status::Timeout {
            // No solution, so no gap; the bound says how good one could be.
            assert_eq!((result.gap(), json.get("gap")), (None, None));
            assert!(result.objective_bound().is_none_or(|b| b >= 0.0));
        }
    }
}

/// `--progress` ends with a line that says how the solve ended.
#[cfg(feature = "cli")]
#[test]
fn the_progress_stream_ends_with_done() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/knapsack.json");
    let out =
        std::process::Command::new(env!("CARGO_BIN_EXE_optimizam")).arg("--progress").arg(example).arg("-").output();
    let out = out.unwrap();
    assert!(out.status.success());
    let lines: Vec<Value> =
        String::from_utf8(out.stderr).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let done = lines.last().unwrap();
    assert_eq!(
        (&done["event"], &done["status"], &done["objective"]),
        (&json!("done"), &json!("complete"), &json!(17.0))
    );
    assert!(done["elapsed"].as_f64().is_some() && done.get("stop_reason").is_none());
    assert_eq!(lines.iter().filter(|l| l["event"] == "done").count(), 1);
}
