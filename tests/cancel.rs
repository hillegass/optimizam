//! Canceling a solve: it must stop promptly and return what it found, as if
//! its time limit had been reached.

mod common;

use common::{JobShop, Knapsack};
use optimizam::{CancelToken, Control, Event, Model, Options, Problem, SolveResult, Status, StopReason};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Solves `model` on another thread; once it reports its first solution,
/// cancels it, and returns the result and how long the solve took to stop
/// after the cancel.
fn cancel_after_first_solution(model: &Model) -> (SolveResult, Duration) {
    let token = CancelToken::new();
    let (found, first) = mpsc::channel();
    let found = std::sync::Mutex::new(found);
    let options = Options::default().cancel_on(&token).on_progress(move |p| {
        if p.event() == Event::Solution {
            let _ = found.lock().unwrap().send(());
        }
        Control::Continue
    });
    std::thread::scope(|s| {
        let handle = s.spawn(|| model.solve(&options).unwrap());
        first.recv_timeout(Duration::from_secs(60)).expect("no solution within 60 s");
        assert!(!handle.is_finished(), "solved before the cancel: make the instance harder");
        token.cancel();
        let canceled = Instant::now();
        let result = handle.join().unwrap();
        (result, canceled.elapsed())
    })
}

#[test]
fn canceled_milp_keeps_the_best_solution() {
    let k = Knapsack::new(150, 10);
    let (result, stop) = cancel_after_first_solution(&k.model);
    assert!(stop < Duration::from_secs(2), "took {stop:?} to stop");
    assert_eq!(result.status(), Status::NotProvenOptimal);
    assert_eq!(result.stop_reason(), Some(StopReason::Canceled));
    let value = k.check(result.best().unwrap());
    assert!(result.objective_bound().unwrap() >= value - 1e-6);
}

#[test]
fn canceled_cp_search_keeps_the_best_schedule() {
    let shop = JobShop::new(10, 10, 7);
    let (result, stop) = cancel_after_first_solution(&shop.model);
    assert!(stop < Duration::from_secs(2), "took {stop:?} to stop");
    assert_eq!(result.status(), Status::NotProvenOptimal);
    assert_eq!(result.stop_reason(), Some(StopReason::Canceled));
    let makespan = shop.check(result.best().unwrap());
    assert!(result.objective_bound().unwrap() <= makespan);
}

/// A token canceled before the solve stops it at the first check, exactly as
/// a deadline that has already passed does: same status, same solutions.
/// (On one thread, where a run repeats; a portfolio's workers each get some
/// way before their first check, and not the same way twice.)
#[test]
fn canceled_before_the_start_is_an_expired_deadline() {
    let token = CancelToken::new();
    token.cancel();
    assert!(token.clone().is_canceled());
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut files: Vec<_> = std::fs::read_dir(&examples)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json") && !p.to_string_lossy().ends_with(".result.json"))
        .collect();
    files.sort();
    assert!(files.len() > 20);
    for file in files {
        let problem = Problem::from_file(&file).unwrap();
        let started = Instant::now();
        let canceled = problem.solve(&Options::default().threads(1).cancel_on(&token)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "{}: took {:?}", file.display(), started.elapsed());
        let expired = problem.solve(&Options::default().threads(1).deadline(Instant::now())).unwrap();
        assert_eq!(canceled.to_json(), expired.to_json(), "{}", file.display());
        let (a, b) = (canceled.stop_reason(), expired.stop_reason());
        assert!(a.is_none() || a == Some(StopReason::Canceled), "{}: {a:?}", file.display());
        assert!(b.is_none() || b == Some(StopReason::TimeLimit), "{}: {b:?}", file.display());
        assert_eq!(a.is_none(), b.is_none(), "{}", file.display());
    }
}

/// One token stops every solve that uses it, and a fresh token stops none.
#[test]
fn a_token_is_shared_by_its_clones() {
    let k = Knapsack::new(30, 3);
    let token = CancelToken::new();
    let options = Options::default().cancel_on(&token.clone());
    let finished = k.model.solve(&options).unwrap();
    assert_eq!((finished.status(), finished.stop_reason()), (Status::Complete, None));
    token.cancel();
    let stopped = k.model.solve(&options).unwrap();
    assert_ne!(stopped.status(), Status::Complete);
    assert_eq!(stopped.stop_reason(), Some(StopReason::Canceled));
    let fresh = Options::default().cancel_on(&CancelToken::new());
    assert_eq!(k.model.solve(&fresh).unwrap().status(), Status::Complete);
}
