//! `Options::threads`: a portfolio's answers are as good as one thread's,
//! its progress arrives on the calling thread in improving order, and it
//! stops when asked.

mod common;

use common::JobShop;
use optimizam::{CancelToken, Control, Event, Model, Options, Status, StopReason, constraint, var};
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
