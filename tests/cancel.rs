//! Canceling a solve: it must stop promptly and return what it found, as if
//! its time limit had been reached.

use optimizam::{CancelToken, Expression, Model, Options, Problem, SolveResult, Status, Variable, constraint, var};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A small deterministic generator (the tests mustn't depend on a crate).
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// Starts `solve` on another thread; once a log line starting with `marker`
/// shows that it has a solution, cancels it, and returns the result and how
/// long the solve took to stop after the cancel.
fn cancel_after_first_solution(
    marker: &'static str,
    solve: impl FnOnce(&Options) -> SolveResult + Send,
) -> (SolveResult, Duration) {
    let token = CancelToken::new();
    let (found, first) = mpsc::channel();
    let options = Options::default().cancel_on(&token).log(move |line| {
        if line.starts_with(marker) {
            let _ = found.send(());
        }
    });
    std::thread::scope(|s| {
        let handle = s.spawn(|| solve(&options));
        first.recv_timeout(Duration::from_secs(60)).expect("no solution within 60 s");
        assert!(!handle.is_finished(), "solved before the cancel: make the instance harder");
        token.cancel();
        let canceled = Instant::now();
        let result = handle.join().unwrap();
        (result, canceled.elapsed())
    })
}

/// A multidimensional knapsack with weak bounds (a large search tree), as in
/// `tools/milp_benchmark.py`.
fn knapsack(n: usize, m: usize) -> (Model, Vec<Variable>, Vec<Vec<i64>>, Vec<i64>, Vec<i64>) {
    let mut rng = Lcg(2);
    let w: Vec<Vec<i64>> = (0..m).map(|_| (0..n).map(|_| 5 + rng.below(56) as i64).collect()).collect();
    let v: Vec<i64> = (0..n).map(|j| (0..m).map(|r| w[r][j]).sum::<i64>() / m as i64 + rng.below(31) as i64).collect();
    let cap: Vec<i64> = w.iter().map(|row| row.iter().sum::<i64>() / 2).collect();
    let mut model = Model::new();
    let x = model.add_vars(n, var().binary().name("x"));
    for r in 0..m {
        model.add(constraint!(x.iter().zip(&w[r]).map(|(&x, &a)| a as f64 * x).sum::<Expression>() <= cap[r] as f64));
    }
    model.maximize(x.iter().zip(&v).map(|(&x, &c)| c as f64 * x).sum::<Expression>());
    (model, x, w, v, cap)
}

#[test]
fn canceled_milp_keeps_the_best_solution() {
    let (model, x, w, v, cap) = knapsack(150, 10);
    let (result, stop) = cancel_after_first_solution("incumbent", |o| model.solve(o).unwrap());
    assert!(stop < Duration::from_secs(2), "took {stop:?} to stop");
    assert_eq!(result.status(), Status::NotProvenOptimal);
    let best = result.best().unwrap();
    for r in 0..w.len() {
        let used: f64 = x.iter().zip(&w[r]).map(|(&x, &a)| a as f64 * best[x]).sum();
        assert!(used <= cap[r] as f64 + 1e-6, "row {r}: {used} > {}", cap[r]);
    }
    let value: f64 = x.iter().zip(&v).map(|(&x, &c)| c as f64 * best[x]).sum();
    let objective = result.objective().unwrap();
    assert!((value - objective).abs() < 1e-6);
    assert!(result.objective_bound().unwrap() >= objective - 1e-6);
}

#[test]
fn canceled_cp_search_keeps_the_best_schedule() {
    // A random 10x10 job shop: each job visits every machine once, in a
    // random order; minimize the makespan.
    let (jobs, machines) = (10, 10);
    let mut rng = Lcg(7);
    let mut m = Model::new();
    let horizon = 10 * 10 * 99;
    let makespan = m.add_var(var().integer().bounds(0, horizon).name("makespan"));
    let mut tasks = vec![vec![]; machines]; // (interval, start, duration) per machine
    let mut steps = vec![];
    for j in 0..jobs {
        let mut order: Vec<usize> = (0..machines).collect();
        for k in (1..machines).rev() {
            order.swap(k, rng.below(k as u64 + 1) as usize);
        }
        let mut previous: Option<(Variable, i64)> = None;
        for &machine in &order {
            let duration = 1 + rng.below(99) as i64;
            let start = m.add_var(var().integer().bounds(0, horizon).name(format!("s{j}_{machine}")));
            if let Some((p, d)) = previous {
                m.add(constraint!(p + d as f64 <= start));
            }
            tasks[machine].push((m.interval(start, duration as f64), start, duration));
            steps.push((start, duration));
            previous = Some((start, duration));
        }
        let (last, d) = previous.unwrap();
        m.add(constraint!(last + d as f64 <= makespan));
    }
    for on in &tasks {
        m.no_overlap(&on.iter().map(|t| t.0).collect::<Vec<_>>());
    }
    m.minimize(makespan);

    let (result, stop) = cancel_after_first_solution("cp: objective", |o| m.solve(o).unwrap());
    assert!(stop < Duration::from_secs(2), "took {stop:?} to stop");
    assert_eq!(result.status(), Status::NotProvenOptimal);
    let best = result.best().unwrap();
    for on in &tasks {
        for (a, &(_, sa, da)) in on.iter().enumerate() {
            for &(_, sb, db) in &on[a + 1..] {
                let (a0, b0) = (best[sa], best[sb]);
                assert!(a0 + da as f64 <= b0 || b0 + db as f64 <= a0, "tasks overlap");
            }
        }
    }
    let end = steps.iter().map(|&(s, d)| best[s] + d as f64).fold(0.0, f64::max);
    assert!(end <= best[makespan]);
    // The CP engine proves no bound before it finishes; others may.
    if let Some(bound) = result.objective_bound() {
        assert!(bound <= result.objective().unwrap());
    }
}

/// A token canceled before the solve stops it at the first check, exactly as
/// a deadline that has already passed does: same status, same solutions.
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
        let canceled = problem.solve(&Options::default().cancel_on(&token)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "{}: took {:?}", file.display(), started.elapsed());
        let expired = problem.solve(&Options::default().deadline(Instant::now())).unwrap();
        assert_eq!(canceled.to_json(), expired.to_json(), "{}", file.display());
    }
}

/// One token stops every solve that uses it, and a fresh token stops none.
#[test]
fn a_token_is_shared_by_its_clones() {
    let (model, ..) = knapsack(30, 3);
    let token = CancelToken::new();
    let options = Options::default().cancel_on(&token.clone());
    assert_eq!(model.solve(&options).unwrap().status(), Status::Complete);
    token.cancel();
    assert_ne!(model.solve(&options).unwrap().status(), Status::Complete);
    let fresh = Options::default().cancel_on(&CancelToken::new());
    assert_eq!(model.solve(&fresh).unwrap().status(), Status::Complete);
}
