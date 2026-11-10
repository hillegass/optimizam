//! `Options::threads`: the answers of the CP engine's portfolio, of the MILP
//! engine's workers, and of the global nonlinear search's are as good as one
//! thread's, their progress arrives on the calling thread in improving order,
//! and they stop when asked.

mod common;

use common::{JobShop, Knapsack};
use optimizam::{CancelToken, Control, Engine, Event, Model, Options, Status, StopReason, constraint, var};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn a_portfolio_proves_the_same_optimum() {
    for seed in [3, 11, 29] {
        let shop = JobShop::new(6, 5, seed);
        let one = shop.model.solve(&Options::default()).unwrap();
        let many = shop.model.solve(&Options::default().threads(4)).unwrap();
        assert_eq!((one.status(), many.status()), (Status::Complete, Status::Complete));
        assert_eq!(one.objective(), many.objective(), "seed {seed}");
        assert_eq!(shop.check(many.best().unwrap()), many.objective().unwrap());
        assert_eq!(many.stop_reason(), None);
    }
}

#[test]
fn a_portfolio_finds_one_solution_or_none() {
    // 8 values, all different, in 0..=7, the first and last adding up to 7.
    let mut m = Model::new();
    let x = m.add_vars(8, var().integer().bounds(0, 7));
    m.all_different(x.iter().copied());
    m.add(constraint!(x[0] + x[7] == 7));
    let result = m.solve(&Options::default().threads(4)).unwrap();
    assert_eq!(result.status(), Status::Complete);
    let s = result.best().unwrap();
    let mut values: Vec<i64> = x.iter().map(|&v| s[v] as i64).collect();
    assert_eq!(values[0] + values[7], 7);
    values.sort_unstable();
    assert_eq!(values, (0..8).collect::<Vec<i64>>());

    // Nine pigeons in eight holes.
    let mut m = Model::new();
    let x = m.add_vars(9, var().integer().bounds(0, 7));
    m.all_different(x.iter().copied());
    let result = m.solve(&Options::default().threads(4)).unwrap();
    assert_eq!(result.status(), Status::Infeasible);
}

#[test]
fn progress_comes_from_the_calling_thread_in_order() {
    let shop = JobShop::new(8, 6, 5);
    let caller = std::thread::current().id();
    let seen: Arc<Mutex<Vec<f64>>> = Arc::default();
    let log = seen.clone();
    let options = Options::default().threads(4).on_progress(move |p| {
        assert_eq!(std::thread::current().id(), caller, "a callback on a worker thread");
        if p.event() == Event::Solution {
            log.lock().unwrap().push(p.objective().unwrap());
        }
        Control::Continue
    });
    let result = shop.model.solve(&options).unwrap();
    assert_eq!(result.status(), Status::Complete);
    let objectives = seen.lock().unwrap();
    assert!(objectives.windows(2).all(|w| w[1] < w[0]), "not improving: {objectives:?}");
    assert_eq!(objectives.last().copied(), result.objective());
}

#[test]
fn a_portfolio_stops_when_canceled_or_out_of_time() {
    // Large enough not to be proven in the time allowed.
    let shop = JobShop::new(15, 12, 7);
    let started = Instant::now();
    let result = shop.model.solve(&Options::default().threads(4).time_limit(Duration::from_millis(300))).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert_eq!(result.status(), Status::NotProvenOptimal);
    assert_eq!(result.stop_reason(), Some(StopReason::TimeLimit));
    shop.check(result.best().unwrap());

    let token = CancelToken::new();
    let options = Options::default().threads(4).cancel_on(&token);
    let started = Instant::now();
    let result = std::thread::scope(|s| {
        let solve = s.spawn(|| shop.model.solve(&options));
        std::thread::sleep(Duration::from_millis(300));
        token.cancel();
        solve.join().unwrap()
    })
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert_eq!(result.status(), Status::NotProvenOptimal);
    assert_eq!(result.stop_reason(), Some(StopReason::Canceled));
    shop.check(result.best().unwrap());
}

#[test]
fn branch_and_bound_on_threads_proves_the_same_optimum() {
    for (n, m) in [(30, 4), (36, 5), (24, 3)] {
        let k = Knapsack::new(n, m);
        let one = k.model.solve(&Options::default()).unwrap();
        let many = k.model.solve(&Options::default().threads(4)).unwrap();
        assert_eq!((one.status(), many.status()), (Status::Complete, Status::Complete));
        assert_eq!(one.objective(), many.objective(), "{n} items, {m} rows");
        assert_eq!(k.check(many.best().unwrap()), many.objective().unwrap());
        assert_eq!(many.stop_reason(), None);
    }
    // A job shop on the MILP engine: big-M rows, a deep tree.
    let shop = JobShop::new(4, 3, 9);
    let one = shop.model.solve(&Options::default()).unwrap();
    let many = shop.model.solve(&Options::default().engine(Engine::Milp).threads(4)).unwrap();
    assert_eq!((one.status(), many.status()), (Status::Complete, Status::Complete));
    assert_eq!(one.objective(), many.objective());
    shop.check(many.best().unwrap());
}

#[test]
fn branch_and_bound_on_threads_reports_in_order_and_stops() {
    let k = Knapsack::new(60, 6);
    let caller = std::thread::current().id();
    let seen: Arc<Mutex<Vec<f64>>> = Arc::default();
    let log = seen.clone();
    let options = Options::default().threads(4).time_limit(Duration::from_millis(400)).on_progress(move |p| {
        assert_eq!(std::thread::current().id(), caller, "a callback on a worker thread");
        if p.event() == Event::Solution {
            log.lock().unwrap().push(p.objective().unwrap());
        }
        Control::Continue
    });
    let started = Instant::now();
    let result = k.model.solve(&options).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    let objectives = seen.lock().unwrap();
    assert!(objectives.windows(2).all(|w| w[1] > w[0]), "not improving: {objectives:?}");
    assert_eq!(objectives.last().copied(), result.objective());
    k.check(result.best().unwrap());
    if result.status() == Status::NotProvenOptimal {
        assert_eq!(result.stop_reason(), Some(StopReason::TimeLimit));
        // The bound is valid for a maximization: no less than the best value.
        assert!(result.objective_bound().unwrap() >= result.objective().unwrap());
    }
}

/// The global nonlinear search on four threads proves what one thread
/// proves: the nonlinear examples' optima (one has an integer variable, one
/// is infeasible), and a nonconvex packing problem that takes a tree of boxes.
#[test]
fn the_global_search_on_threads_proves_the_same() {
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-5 * a.abs().max(1.0);
    for name in ["hs071", "portfolio_risk", "batch_sizing", "disk_infeasible"] {
        let model = Model::from_file(format!("{}/examples/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let one = model.solve(&Options::default()).unwrap();
        let many = model.solve(&Options::default().threads(4)).unwrap();
        assert_eq!(one.status(), many.status(), "{name}");
        assert_eq!(many.stop_reason(), None, "{name}");
        match (one.objective(), many.objective()) {
            (Some(a), Some(b)) => assert!(close(a, b), "{name}: {a} with one thread, {b} with four"),
            (a, b) => assert_eq!(a, b, "{name}"),
        }
    }
    // Four points in the unit square, as far apart as they can be: the
    // corners, at distance 1.
    let packing = r#"{
        "version": "1.2",
        "sets": { "P": { "from": 1, "to": 4 } },
        "variables": [
            { "name": "x", "type": "continuous", "index": ["P"], "lower": 0, "upper": 1 },
            { "name": "y", "type": "continuous", "index": ["P"], "lower": 0, "upper": 1 },
            { "name": "d", "type": "continuous", "lower": 0, "upper": 2 }
        ],
        "constraints": [
            { "forall": "i in P, j in P : i < j", "expr": "(x[i] - x[j])^2 + (y[i] - y[j])^2 >= d" }
        ],
        "solve": { "method": "maximize", "objective": "d" }
    }"#;
    let model = Model::from_json(packing).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let options = Options::default().threads(4).on_progress(move |p| {
        if p.event() == Event::Solution {
            sink.lock().unwrap().push(p.objective().unwrap());
        }
        Control::Continue
    });
    let many = model.solve(&options).unwrap();
    let events = seen.lock().unwrap().clone();
    assert_eq!(many.status(), Status::Complete);
    assert!(close(many.objective().unwrap(), 1.0), "{:?}", many.objective());
    // Solutions are reported in improving order.
    assert!(!events.is_empty() && events.windows(2).all(|w| w[1] > w[0]), "{events:?}");
}
