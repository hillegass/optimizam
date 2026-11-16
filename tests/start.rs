//! Starting values: `VarSpec::start`, `Model::set_start`, `start_from`, and
//! `start_from_result`, and `--start` on the command line. A start is where
//! a search looks first; it never changes the answer.

mod common;

use common::{JobShop, Knapsack, Lcg};
use optimizam::{Control, Engine, ErrorKind, Event, Model, Options, Set, Status, constraint, var};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn limited() -> Options {
    Options::default().time_limit(Duration::from_secs(60)).threads(1)
}

/// Options that record the objective of each solution as it is reported.
fn recording(options: Options) -> (Options, Arc<Mutex<Vec<f64>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let options = options.on_progress(move |p| {
        if p.event() == Event::Solution {
            record.lock().unwrap().push(p.objective().unwrap());
        }
        Control::Continue
    });
    (options, seen)
}

#[test]
fn the_milp_search_begins_at_a_solution_it_is_given() {
    let k = Knapsack::new(60, 5);
    let first = k.model.solve(&limited()).unwrap();
    assert_eq!(first.status(), Status::Complete);
    let optimum = first.objective().unwrap();

    // The same problem, made again: values are taken by name and key.
    let mut again = Knapsack::new(60, 5);
    assert_eq!(again.model.start_from(first.best().unwrap()), 60);
    let (options, seen) = recording(limited());
    let warm = again.model.solve(&options).unwrap();
    assert_eq!((warm.status(), warm.objective()), (Status::Complete, Some(optimum)));
    again.check(warm.best().unwrap());
    // The first solution reported is the one it was given: the optimum.
    assert_eq!(seen.lock().unwrap().first(), Some(&optimum));
}

#[test]
fn a_start_never_changes_the_answer() {
    let k = Knapsack::new(40, 4);
    let optimum = k.model.solve(&limited()).unwrap().objective().unwrap();
    let mut rng = Lcg(11);
    for round in 0..12 {
        let mut k = Knapsack::new(40, 4);
        // Starts of every sort: all of them or some, a solution or none
        // (everything taken overflows the knapsack), out of bounds.
        for (j, &x) in k.x.iter().enumerate() {
            match (round % 4, rng.below(4)) {
                (0, _) => k.model.set_start(x, 1),
                (1, 0 | 1) => k.model.set_start(x, rng.below(2) as f64),
                (1, _) => {}
                (2, _) => k.model.set_start(x, rng.below(2) as f64),
                (_, pick) => k.model.set_start(x, if j % 7 == 0 { 99.0 } else { pick as f64 - 1.5 }),
            }
        }
        let result = k.model.solve(&limited()).unwrap();
        assert_eq!((result.status(), result.objective()), (Status::Complete, Some(optimum)), "round {round}");
        k.check(result.best().unwrap());
    }
}

#[test]
fn a_solve_goes_on_from_the_last_one_after_an_edit() {
    let mut k = Knapsack::new(60, 5);
    let first = k.model.solve(&limited()).unwrap();
    let taken: Vec<_> = k.x.iter().copied().filter(|&x| first.best().unwrap()[x] > 0.5).collect();
    // The old solution is no solution of the new problem: two of the items
    // it took may no longer be taken together.
    k.model.add(constraint!(taken[0] + taken[1] <= 1));
    let cold = k.model.solve(&limited()).unwrap();
    assert_eq!(k.model.start_from(first.best().unwrap()), 60);
    let warm = k.model.solve(&limited()).unwrap();
    assert_eq!((warm.status(), warm.objective()), (Status::Complete, cold.objective()));
    assert!(warm.objective().unwrap() <= first.objective().unwrap());
    k.check(warm.best().unwrap());

    // A looser problem: the old solution still is one, and the search
    // begins at it.
    let mut k = Knapsack::new(60, 5);
    let x = k.x.clone();
    let tight = k.model.add(constraint!(x[0] + x[1] + x[2] + x[3] <= 1));
    let first = k.model.solve(&limited()).unwrap();
    k.model.remove(tight);
    k.model.start_from(first.best().unwrap());
    let (options, seen) = recording(limited());
    let warm = k.model.solve(&options).unwrap();
    assert_eq!(seen.lock().unwrap().first().copied(), first.objective());
    assert!(warm.objective().unwrap() >= first.objective().unwrap());
}

#[test]
fn the_cp_search_begins_at_a_schedule_it_is_given() {
    let shop = JobShop::new(10, 10, 5);
    let first = shop.model.solve(&limited().time_limit(Duration::from_millis(1500))).unwrap();
    let makespan = shop.check(first.best().unwrap());

    let mut again = JobShop::new(10, 10, 5);
    assert_eq!(again.model.start_from(first.best().unwrap()), 101);
    let (options, seen) = recording(limited().time_limit(Duration::from_millis(500)));
    let warm = again.model.solve(&options).unwrap();
    assert_eq!(warm.engine(), Some(Engine::Cp));
    // Its first schedule is the one it was given, and it ends no worse.
    assert_eq!(seen.lock().unwrap().first(), Some(&makespan));
    assert!(again.check(warm.best().unwrap()) <= makespan);

    // The same through the MILP engine, which encodes the schedule: the
    // start is a solution there too, once the encoding's columns follow.
    let (options, seen) = recording(limited().engine(Engine::Milp).time_limit(Duration::from_millis(500)));
    let warm = again.model.solve(&options).unwrap();
    assert_eq!(seen.lock().unwrap().first(), Some(&makespan));
    assert!(again.check(warm.best().unwrap()) <= makespan);
}

#[test]
fn any_solution_will_do_and_the_start_is_one() {
    // Eight queens by `all_different`, started at a solution that the
    // search doesn't find first by itself.
    let queens = |start: Option<&[i64]>, engine: Engine| {
        let mut m = Model::new();
        let q = m.add_vars(8, var().integer().bounds(1, 8).name("q"));
        m.all_different(&q);
        m.all_different(q.iter().enumerate().map(|(i, &q)| q + i as i32));
        m.all_different(q.iter().enumerate().map(|(i, &q)| q - i as i32));
        for (&q, &value) in q.iter().zip(start.unwrap_or(&[])) {
            m.set_start(q, value);
        }
        let result = m.solve(&limited().engine(engine)).unwrap();
        assert_eq!(result.status(), Status::Complete);
        q.iter().map(|&q| result.best().unwrap().int(q)).collect::<Vec<i64>>()
    };
    let given = [8, 4, 1, 3, 6, 2, 7, 5];
    for engine in [Engine::Cp, Engine::Milp] {
        assert_ne!(queens(None, engine), given, "{engine}");
        assert_eq!(queens(Some(&given), engine), given, "{engine}");
        // A start that is no solution is only a place to begin.
        let found = queens(Some(&[1, 1, 1, 1, 1, 1, 1, 1]), engine);
        assert!((0..8).all(|i| (0..i).all(|j| found[i] != found[j] && (found[i] - found[j]).abs() != (i - j) as i64)));
    }
}

#[test]
fn every_solution_is_found_whatever_the_start() {
    let count = |start: bool, engine: Engine| {
        let mut m = Model::new();
        let x = m.add_vars(4, var().integer().bounds(0, 3).name("x"));
        m.add(constraint!(x[0] + x[1] + x[2] + x[3] == 6));
        m.maximize(x[0] + 2 * x[1]);
        m.all_solutions(true);
        if start {
            for (&x, value) in x.iter().zip([3, 3, 0, 0]) {
                m.set_start(x, value);
            }
        }
        m.solve(&limited().engine(engine)).unwrap().solutions().len()
    };
    for engine in [Engine::Cp, Engine::Milp] {
        assert_eq!(count(true, engine), count(false, engine), "{engine}");
        assert_eq!(count(true, engine), 1);
    }
}

#[test]
fn symbolic_variables_start_at_elements() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plant_carriers.json");
    let model = Model::from_file(&example).unwrap();
    let first = model.solve(&limited()).unwrap();
    let mut again = Model::from_file(&example).unwrap();
    let from_solution = again.start_from(first.best().unwrap());
    let mut third = Model::from_file(&example).unwrap();
    let from_result = third.start_from_result(&first.to_json()).unwrap();
    assert!(from_solution > 0);
    assert_eq!(from_solution, from_result);
    for m in [&again, &third] {
        let warm = m.solve(&limited()).unwrap();
        assert_eq!((warm.status(), warm.objective()), (first.status(), first.objective()));
    }
    // The two ways give the same starts: the models are written the same.
    assert_eq!(again.to_json().unwrap(), third.to_json().unwrap());
    assert!(again.to_json().unwrap().contains("\"start\""));

    // An element the variable's set doesn't have is no start for it.
    let (wide, narrow) = (Set::new("WIDE", ["rail", "road", "air"]), Set::new("NARROW", ["road", "sea"]));
    let mut a = Model::new();
    let mode = a.add_var(var().values(&wide).name("mode"));
    let only_air = a.add(mode.is("air"));
    let by_air = a.solve(&limited()).unwrap();
    let mut b = Model::new();
    b.add_var(var().values(&narrow).name("mode"));
    assert_eq!(b.start_from(by_air.best().unwrap()), 0);
    a.remove(only_air);
    a.add(mode.is_not("air"));
    assert_eq!(a.start_from(by_air.best().unwrap()), 1);
    assert_eq!(a.solve(&limited()).unwrap().status(), Status::Complete);
}

#[test]
fn a_result_file_is_read_for_what_it_has() {
    let k = Knapsack::new(30, 3);
    let whole = k.model.solve(&limited()).unwrap();
    let sparse = k.model.solve(&limited().nonzeros(true)).unwrap();
    let taken = k.x.iter().filter(|&&x| whole.best().unwrap()[x] > 0.5).count();
    assert!(taken < 30 && sparse.to_json().contains("\"omitted\""));
    // Without its zeros, a result still gives every element a value.
    let mut a = Knapsack::new(30, 3);
    assert_eq!(a.model.start_from_result(&whole.to_json()).unwrap(), 30);
    let mut b = Knapsack::new(30, 3);
    assert_eq!(b.model.start_from_result(&sparse.to_json()).unwrap(), 30);
    assert_eq!(a.model.to_json().unwrap(), b.model.to_json().unwrap());

    // What isn't a result with a solution is an error; a result of another
    // problem gives nothing.
    let mut m = Knapsack::new(30, 3).model;
    assert_eq!(m.start_from_result("not json").unwrap_err().kind(), ErrorKind::Parse);
    let none = r#"{"version": "1.3", "status": "infeasible", "solutions": []}"#;
    let e = m.start_from_result(none).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Invalid);
    assert!(e.message().contains("no solution to start from"), "{e}");
    let other = r#"{"version": "1.3", "status": "complete", "solutions": [{"variables": {"y": 3, "x": 2}}]}"#;
    assert_eq!(m.start_from_result(other).unwrap(), 0);
}

#[test]
#[should_panic(expected = "a start is a finite number")]
fn a_start_is_a_number() {
    let mut m = Model::new();
    let x = m.add_var(var().bounds(0, 1).name("x"));
    m.set_start(x, f64::INFINITY);
}

#[test]
fn the_command_line_tool_starts_from_a_result_file() {
    let shop = JobShop::new(10, 10, 5);
    let dir = std::env::temp_dir().join(format!("optimizam-start-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (problem, last, other) = (dir.join("shop.json"), dir.join("last.json"), dir.join("other.json"));
    std::fs::write(&problem, shop.model.to_json().unwrap()).unwrap();
    std::fs::write(&other, r#"{"version": "1.3", "status": "complete", "solutions": [{"variables": {"y": 3}}]}"#)
        .unwrap();
    let tool = |args: &[&str], out: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_optimizam"));
        command.args(["--threads=1", "--progress"]).args(args).arg(&problem).arg(out);
        command.output().unwrap()
    };
    let first = tool(&["--time=1.5"], &last);
    let start = |file: &Path| format!("--start={}", file.display());
    let warm = tool(&["--time=0.5", &start(&last)], Path::new("-"));
    let unmatched = tool(&["--time=0.2", &start(&other)], Path::new("-"));
    let missing = tool(&[&start(&dir.join("none.json"))], Path::new("-"));
    let objective =
        |text: &str| serde_json::from_str::<serde_json::Value>(text).unwrap()["solutions"][0]["objective"].clone();
    let before = objective(&std::fs::read_to_string(&last).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(first.status.success() && warm.status.success());
    // The first solution of the second run is the last of the first.
    let events = String::from_utf8(warm.stderr).unwrap();
    let solution = events.lines().find(|l| l.contains("\"solution\"")).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(solution).unwrap()["objective"].as_f64(), before.as_f64());
    assert!(objective(&String::from_utf8(warm.stdout).unwrap()).as_f64() <= before.as_f64());
    let warned = String::from_utf8(unmatched.stderr).unwrap();
    assert!(warned.contains("has a value for none of the problem's variables"), "{warned}");
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8(missing.stderr).unwrap().contains("reading"));
}
