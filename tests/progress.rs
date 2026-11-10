//! Progress callbacks and stop reasons: every reported solution is feasible
//! and better than the last, the last one is the result's, bounds are valid,
//! and `Control::Stop` stops the solve.

mod common;

use common::{JobShop, Knapsack};
use optimizam::{Control, Event, Options, Problem, Solution, SolveResult, Status, StopReason};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a recording callback saw.
#[derive(Default)]
struct Seen {
    solutions: Vec<Solution>,
    /// (event, bound) of every call.
    events: Vec<(Event, Option<f64>)>,
    json: Vec<Option<String>>,
}

/// Options whose callback records every call (and stops when `stop` says).
fn recording(stop: impl Fn(&optimizam::Progress<'_>) -> bool + Send + Sync + 'static) -> (Options, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let log = seen.clone();
    let options = Options::default().on_progress(move |p| {
        let mut s = log.lock().unwrap();
        s.events.push((p.event(), p.bound()));
        if p.event() == Event::Solution {
            s.solutions.push(p.solution().unwrap().clone());
            s.json.push(p.to_json());
        }
        if stop(p) { Control::Stop } else { Control::Continue }
    });
    (options, seen)
}

/// Checks that the objectives strictly improve (`maximize`: increase), that
/// every bound is valid for the final objective, and that the last solution
/// reported is the result's.
fn check_sequence(seen: &Seen, result: &SolveResult, maximize: bool) {
    let objectives: Vec<f64> = seen.solutions.iter().map(|s| s.objective().unwrap()).collect();
    assert!(!objectives.is_empty(), "no solution reported");
    for w in objectives.windows(2) {
        assert!(if maximize { w[1] > w[0] } else { w[1] < w[0] }, "not improving: {objectives:?}");
    }
    let best = result.objective().unwrap();
    assert_eq!(objectives.last().copied(), Some(best));
    for &(_, bound) in &seen.events {
        if let Some(b) = bound {
            let valid = if maximize { b >= best - 1e-6 * best.abs() } else { b <= best + 1e-6 * best.abs() };
            assert!(valid, "bound {b} excludes the optimum {best}");
        }
    }
}

#[test]
fn milp_reports_every_improvement() {
    let k = Knapsack::new(40, 5);
    let (options, seen) = recording(|_| false);
    let result = k.model.solve(&options).unwrap();
    assert_eq!((result.status(), result.stop_reason()), (Status::Complete, None));
    let seen = seen.lock().unwrap();
    check_sequence(&seen, &result, true);
    for s in &seen.solutions {
        k.check(s);
    }
    let last = seen.solutions.last().unwrap();
    assert!(k.x.iter().all(|&x| last[x] == result.best().unwrap()[x]));
    // The root bound comes with the first solutions.
    assert!(seen.events.iter().any(|e| e.1.is_some()));
}

#[test]
fn stop_at_a_gap() {
    let k = Knapsack::new(150, 10);
    let (options, seen) = recording(|p| p.gap().is_some_and(|g| g < 0.005));
    let result = k.model.solve(&options.time_limit(Duration::from_secs(60))).unwrap();
    assert_eq!(result.status(), Status::NotProvenOptimal);
    assert_eq!(result.stop_reason(), Some(StopReason::Callback));
    let best = k.check(result.best().unwrap());
    let bound = result.objective_bound().unwrap();
    assert!(bound >= best && (bound - best) / best < 0.005, "objective {best}, bound {bound}");
    check_sequence(&seen.lock().unwrap(), &result, true);
}

#[test]
fn cp_reports_every_better_schedule() {
    let shop = JobShop::new(15, 15, 7);
    let (options, seen) = recording(|_| false);
    let result = shop.model.solve(&options.time_limit(Duration::from_millis(1500))).unwrap();
    assert_eq!(result.status(), Status::NotProvenOptimal);
    assert_eq!(result.stop_reason(), Some(StopReason::TimeLimit));
    let seen = seen.lock().unwrap();
    check_sequence(&seen, &result, false);
    for s in &seen.solutions {
        shop.check(s);
    }
    // Bounds from propagation (checked against the result's objective
    // above), and the result's own.
    assert!(seen.events.iter().any(|e| e.1.is_some()));
    assert!(result.objective_bound().unwrap() <= result.objective().unwrap());
}

/// The CP engine's bound never exceeds the optimum: small job shops,
/// stopped early, against their proven optima. (One thread: the limits are
/// chosen for what one search does in the time, whatever else is running.)
#[test]
fn cp_bounds_are_valid() {
    let mut checked = 0;
    for seed in 1..=12 {
        let shop = JobShop::new(6, 6, seed);
        let optimum = shop.model.solve(&Options::default()).unwrap();
        assert_eq!(optimum.status(), Status::Complete);
        let optimum = optimum.objective().unwrap();
        for limit in [1, 5, 20] {
            let options = Options::default().threads(1).time_limit(Duration::from_millis(limit));
            let r = shop.model.solve(&options).unwrap();
            if let Some(bound) = r.objective_bound() {
                assert!(bound <= optimum, "seed {seed}, {limit} ms: bound {bound} > optimum {optimum}");
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "no solve stopped early with a bound");
}

/// With nothing to report, the callback still hears from the solve about
/// twice a second: 11 pigeons in 10 holes, which takes clause learning a
/// long time to refute.
#[test]
fn quiet_searches_send_heartbeats() {
    let mut m = optimizam::Model::new();
    let x: Vec<Vec<_>> = (0..11).map(|_| m.add_vars(10, optimizam::var().binary())).collect();
    for p in &x {
        m.add(optimizam::constraint!(p.iter().copied().sum::<optimizam::Expression>() == 1));
    }
    for h in 0..10 {
        m.add(optimizam::constraint!(x.iter().map(|p| p[h]).sum::<optimizam::Expression>() <= 1));
    }
    let (options, seen) = recording(|_| false);
    let result = m.solve(&options.time_limit(Duration::from_millis(1600))).unwrap();
    assert_eq!((result.status(), result.stop_reason()), (Status::Timeout, Some(StopReason::TimeLimit)));
    let beats = seen.lock().unwrap().events.iter().filter(|e| e.0 == Event::Heartbeat).count();
    assert!((2..=4).contains(&beats), "{beats} heartbeats in 1.6 s");
}

#[test]
fn global_search_reports_incumbents_and_bounds() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hs071.json");
    let (options, seen) = recording(|_| false);
    let result = Problem::from_file(path).unwrap().solve(&options).unwrap();
    assert_eq!((result.status(), result.stop_reason()), (Status::Complete, None));
    let seen = seen.lock().unwrap();
    let last = seen.solutions.last().expect("no incumbent reported").objective().unwrap();
    // The result is the last incumbent, polished by a local solve.
    let best = result.objective().unwrap();
    assert!(best <= last + 1e-6 && last - best <= 1e-4 * best.abs(), "last {last}, result {best}");
}

#[test]
fn intermediate_results_are_result_files() {
    let k = Knapsack::new(40, 5);
    let (options, seen) = recording(|_| false);
    k.model.solve(&options).unwrap();
    for json in &seen.lock().unwrap().json {
        let r: serde_json::Value = serde_json::from_str(json.as_ref().unwrap()).unwrap();
        assert_eq!(r["status"], "not_proven_optimal");
        assert_eq!(r["solutions"].as_array().unwrap().len(), 1);
    }
    // A satisfaction problem's solutions aren't results of their own.
    let mut m = optimizam::Model::new();
    let x = m.add_var(optimizam::var().integer().bounds(0, 3));
    m.add(optimizam::constraint!(x >= 2));
    let (options, seen) = recording(|_| false);
    m.solve(&options).unwrap();
    let seen = seen.lock().unwrap();
    assert!(seen.json.iter().all(Option::is_none));
}

#[test]
fn stop_reasons() {
    let k = Knapsack::new(150, 10);
    // One thread: 200 ms is then enough for a solution and not for the proof.
    let timed = k.model.solve(&Options::default().threads(1).time_limit(Duration::from_millis(200))).unwrap();
    assert_eq!((timed.status(), timed.stop_reason()), (Status::NotProvenOptimal, Some(StopReason::TimeLimit)));
    let small = Knapsack::new(20, 2);
    let done = small.model.solve(&Options::default().time_limit(Duration::from_secs(60))).unwrap();
    assert_eq!((done.status(), done.stop_reason()), (Status::Complete, None));
    // A stop on the first event: before any solution, so a timeout.
    let (options, _) = recording(|_| true);
    let stopped = k.model.solve(&options).unwrap();
    assert_eq!(stopped.stop_reason(), Some(StopReason::Callback));
}

/// `--incumbents`: a run killed partway leaves the best solution so far.
#[cfg(feature = "cli")]
#[test]
fn killed_run_leaves_its_incumbent() {
    let k = Knapsack::new(150, 10);
    let dir = std::env::temp_dir().join(format!("optimizam-incumbents-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (problem, best, result) = (dir.join("problem.json"), dir.join("best.json"), dir.join("result.json"));
    std::fs::write(&problem, k.model.to_json().unwrap()).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_optimizam"))
        .arg(format!("--incumbents={}", best.display()))
        .args([&problem, &result])
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !best.exists() {
        assert!(start.elapsed() < Duration::from_secs(60), "no incumbent within 60 s");
        assert!(child.try_wait().unwrap().is_none(), "finished before it could be killed");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!result.exists());
    let r: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&best).unwrap()).unwrap();
    assert_eq!(r["status"], "not_proven_optimal");
    let objective = r["solutions"][0]["objective"].as_f64().unwrap();
    assert!(r["objective_bound"].as_f64().unwrap() >= objective);
    // The values are a feasible packing worth the objective.
    let x = &r["solutions"][0]["variables"]["x"];
    let mut model = k.model.clone();
    for (j, &v) in k.x.iter().enumerate() {
        let value = x[j.to_string()].as_f64().unwrap();
        model.add(optimizam::constraint!(v == value));
    }
    let fixed = model.solve(&Options::default()).unwrap();
    assert_eq!(fixed.status(), Status::Complete);
    assert_eq!(fixed.objective(), Some(objective));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Ctrl-C (SIGINT) and SIGTERM stop the command, which writes its best
/// result and exits with 0. (A second Ctrl-C quits at once with 130, but
/// that can't be tested reliably: two signals sent together arrive as one,
/// and a canceled solve stops and exits within milliseconds.)
#[cfg(all(unix, feature = "cli"))]
#[test]
fn interrupts_write_the_best_result() {
    use std::process::{Command, Stdio};
    let k = Knapsack::new(150, 10);
    let dir = std::env::temp_dir().join(format!("optimizam-interrupt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let problem = dir.join("problem.json");
    std::fs::write(&problem, k.model.to_json().unwrap()).unwrap();
    let signal = |pid: u32, name: &str| {
        assert!(Command::new("kill").args([format!("-{name}"), pid.to_string()]).status().unwrap().success());
    };
    for name in ["INT", "TERM"] {
        let (best, result) = (dir.join("best.json"), dir.join("result.json"));
        let _ = (std::fs::remove_file(&best), std::fs::remove_file(&result));
        let mut child = Command::new(env!("CARGO_BIN_EXE_optimizam"))
            .arg(format!("--incumbents={}", best.display()))
            .args([&problem, &result])
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Wait until it is solving (it has an incumbent), so the handler is installed.
        let start = std::time::Instant::now();
        while !best.exists() {
            assert!(start.elapsed() < Duration::from_secs(60), "no incumbent within 60 s");
            std::thread::sleep(Duration::from_millis(20));
        }
        signal(child.id(), name);
        assert_eq!(child.wait().unwrap().code(), Some(0), "SIG{name}");
        let r: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&result).unwrap()).unwrap();
        assert_eq!(r["status"], "not_proven_optimal", "SIG{name}");
        let objective = r["solutions"][0]["objective"].as_f64().unwrap();
        assert!(r["objective_bound"].as_f64().unwrap() >= objective);
        // The final result went to --incumbents too.
        assert_eq!(std::fs::read_to_string(&best).unwrap(), std::fs::read_to_string(&result).unwrap());
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
