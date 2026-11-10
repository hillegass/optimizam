//! The builder API: examples rebuilt in Rust must give the expected results
//! of their problem files, small CP models must agree with brute force, and
//! every model exported with `Model::to_json` must solve to the same answer.

use optimizam::{ErrorKind, Expression, Model, Options, Problem, SolveResult, Status, Variable, constraint, var};
use serde_json::Value;
use std::path::Path;

fn expected(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(format!("{name}.result.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * (1.0 + a.abs().max(b.abs()))
}

/// Solves the model's export and checks that it agrees with `result`.
///
/// With OPTIMIZAM_EXPORT_DIR set, the exports are also written there, to
/// check them against the schema (CLAUDE.md).
fn check_export(m: &Model, result: &SolveResult) {
    let json = m.to_json().unwrap();
    if let Some(dir) = std::env::var_os("OPTIMIZAM_EXPORT_DIR") {
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let k = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("{}-{k}.json", std::thread::current().name().unwrap_or("test").replace("::", "-"));
        std::fs::write(Path::new(&dir).join(name), &json).unwrap();
    }
    let exported = Problem::from_json(&json)
        .unwrap_or_else(|e| panic!("the export doesn't read back: {e}\n{json}"))
        .solve(&Options::default())
        .unwrap_or_else(|e| panic!("the export doesn't solve: {e}\n{json}"));
    assert_eq!(exported.status(), result.status(), "{json}");
    assert_eq!(exported.solutions().len(), result.solutions().len(), "{json}");
    match (exported.objective(), result.objective()) {
        (Some(a), Some(b)) => assert!(close(a, b), "export {a} vs builder {b}\n{json}"),
        (a, b) => assert_eq!(a, b),
    }
}

/// Solves the model and compares it with `examples/<name>.result.json`
/// (status, number of solutions, objective), then checks the export.
fn check_example(m: &Model, name: &str) -> SolveResult {
    let result = m.solve(&Options::default()).unwrap();
    let want = expected(name);
    assert_eq!(result.status().as_str(), want["status"].as_str().unwrap(), "{name}");
    let solutions = want["solutions"].as_array().unwrap();
    assert_eq!(result.solutions().len(), solutions.len(), "{name}");
    if let Some(objective) = solutions.first().and_then(|s| s["objective"].as_f64()) {
        let got = result.objective().unwrap();
        assert!(close(got, objective), "{name}: {got} vs {objective}");
    }
    check_export(m, &result);
    result
}

#[test]
fn knapsack_all_optima() {
    let (weight, value) = ([4, 3, 3, 2], [10, 7, 7, 5]);
    let mut m = Model::new();
    let take = m.add_vars(4, var().binary().name("take"));
    m.add(constraint!((0..4).map(|i| weight[i] * take[i]).sum::<Expression>() <= 7).name("capacity"));
    m.maximize((0..4).map(|i| value[i] * take[i]).sum::<Expression>());
    m.all_solutions(true);
    let result = check_example(&m, "knapsack");
    for s in result.solutions() {
        assert!(close(s.eval(&(0..4).map(|i| weight[i] * take[i]).sum()), 7.0) || s[take[3]] == 1.0);
        assert_eq!(s.get("take", &["0"]), Some(s[take[0]]));
    }
}

#[test]
fn production_lp() {
    let mut m = Model::new();
    let chairs = m.add_var(var().bounds(0, 40).name("chairs"));
    let tables = m.add_var(var().bounds(0, 30).name("tables"));
    m.add(constraint!(3 * chairs + 5 * tables <= 150));
    m.add(constraint!(chairs + 2 * tables <= 50));
    m.maximize(45 * chairs + 80 * tables);
    let result = check_example(&m, "production");
    let best = result.best().unwrap();
    assert!(close(45.0 * best[chairs] + 80.0 * best[tables], 2200.0));
    assert!(result.to_json().contains("\"chairs\""));
}

#[test]
fn queens_all_solutions() {
    let mut m = Model::new();
    let col = m.add_vars(8, var().integer().bounds(1, 8).name("col"));
    m.all_different(&col);
    m.all_different((0..8).map(|r| col[r] + r as i32));
    m.all_different((0..8).map(|r| col[r] - r as i32));
    m.all_solutions(true);
    let result = check_example(&m, "n_queens_alldiff");
    let mut seen = std::collections::HashSet::new();
    for s in result.solutions() {
        let cols: Vec<i64> = col.iter().map(|&c| s[c] as i64).collect();
        for a in 0..8 {
            for b in a + 1..8 {
                assert!(cols[a] != cols[b] && (cols[a] - cols[b]).abs() != (b - a) as i64);
            }
        }
        assert!(seen.insert(cols));
    }
}

#[test]
fn job_shop_intervals() {
    let machine = [[1, 2, 3], [2, 1, 3], [2, 3, 1]];
    let length = [[3, 2, 2], [2, 1, 4], [4, 3, 1]];
    let mut m = Model::new();
    let begin: Vec<Vec<Variable>> =
        (0..3).map(|j| m.add_vars(3, var().integer().bounds(0, 100).name(format!("begin{j}")))).collect();
    let makespan = m.add_var(var().integer().bounds(0, 100).name("makespan"));
    let op: Vec<Vec<_>> = (0..3).map(|j| (0..3).map(|s| m.interval(begin[j][s], length[j][s])).collect()).collect();
    for j in 0..3 {
        for s in 1..3 {
            m.add(constraint!(begin[j][s] >= begin[j][s - 1] + length[j][s - 1]));
        }
    }
    for mach in 1..=3 {
        let on: Vec<_> =
            (0..3).flat_map(|j| (0..3).map(move |s| (j, s))).filter(|&(j, s)| machine[j][s] == mach).collect();
        m.no_overlap(&on.iter().map(|&(j, s)| op[j][s]).collect::<Vec<_>>());
    }
    m.max_of(makespan, (0..3).flat_map(|j| (0..3).map(move |s| (j, s))).map(|(j, s)| begin[j][s] + length[j][s]));
    m.minimize(makespan);
    check_example(&m, "job_shop");
}

#[test]
fn load_balance_max_of() {
    let hours = [[4, 5, 6], [3, 3, 4], [6, 5, 5], [2, 3, 2], [5, 4, 6], [3, 4, 3]];
    let mut m = Model::new();
    let assign: Vec<Vec<Variable>> = (0..6).map(|_| m.add_vars(3, var().binary())).collect();
    let load = m.add_vars(3, var().integer().bounds(0, 40).name("load"));
    let makespan = m.add_var(var().integer().bounds(0, 40).name("makespan"));
    for row in &assign {
        m.add(constraint!(row.iter().sum::<Expression>() == 1));
    }
    for mach in 0..3 {
        m.add(constraint!(load[mach] == (0..6).map(|t| hours[t][mach] * assign[t][mach]).sum::<Expression>()));
    }
    m.max_of(makespan, &load);
    m.minimize(makespan);
    m.all_solutions(true);
    check_example(&m, "load_balance");
}

#[test]
fn hs071_nonlinear() {
    let mut m = Model::new();
    let x: Vec<Variable> = [1.0, 5.0, 5.0, 1.0].iter().map(|&s| m.add_var(var().bounds(1, 5).start(s))).collect();
    m.add(constraint!(x[0] * x[1] * x[2] * x[3] >= 25).name("product"));
    m.add(constraint!(x.iter().map(|&v| v.powi(2)).sum::<Expression>() == 40).name("sphere"));
    m.minimize(x[0] * x[3] * (x[0] + x[1] + x[2]) + x[2]);
    let result = check_example(&m, "hs071");
    let best = result.best().unwrap();
    assert!(best.eval(&(x[0] * x[1] * x[2] * x[3])) >= 25.0 - 1e-6);
}

#[test]
fn curve_fit_exp() {
    let t = [0.0, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 10.0];
    let y = [10.1, 7.48, 5.62, 4.39, 3.48, 2.31, 1.79, 1.52];
    let mut m = Model::new();
    let amplitude = m.add_var(var().start(5).name("amplitude"));
    let rate = m.add_var(var().min(0).start(0.5).name("rate"));
    let baseline = m.add_var(var().start(0).name("baseline"));
    m.minimize((0..8).map(|i| (amplitude * (-t[i] * rate).exp() + baseline - y[i]).powi(2)).sum::<Expression>());
    check_example(&m, "curve_fit");
}

#[test]
fn batch_sizing_mixed_integer_nonlinear() {
    let mut m = Model::new();
    let batches = m.add_var(var().integer().bounds(1, 20).name("batches"));
    let size = m.add_var(var().bounds(0, 400).name("size"));
    m.add(constraint!(batches * size >= 1000).name("meet_demand"));
    m.minimize(120 * batches + 2 * size + 0.01 * size.powi(2));
    check_example(&m, "batch_sizing");
}

/// Every assignment of x, y, z in their ranges, as (x, y, z).
fn points(lo: i64, hi: i64) -> Vec<[i64; 3]> {
    let r = lo..=hi;
    r.clone().flat_map(|a| r.clone().flat_map(move |b| (lo..=hi).map(move |c| [a, b, c]))).collect()
}

/// A model over three integers in lo..=hi, with every solution requested;
/// compares the solutions with brute force.
fn agree(lo: i64, hi: i64, build: impl Fn(&mut Model, [Variable; 3]), holds: impl Fn([i64; 3]) -> bool) {
    let mut m = Model::new();
    let v = m.add_vars(3, var().integer().bounds(lo as i32, hi as i32).name("v"));
    let v = [v[0], v[1], v[2]];
    build(&mut m, v);
    m.all_solutions(true);
    let result = m.solve(&Options::default()).unwrap();
    let mut got: Vec<[i64; 3]> =
        result.solutions().iter().map(|s| [s[v[0]] as i64, s[v[1]] as i64, s[v[2]] as i64]).collect();
    got.sort_unstable();
    let want: Vec<[i64; 3]> = points(lo, hi).into_iter().filter(|&p| holds(p)).collect();
    assert_eq!(got, want);
    let status = if want.is_empty() { Status::Infeasible } else { Status::Complete };
    assert_eq!(result.status(), status);
    check_export(&m, &result);
}

#[test]
fn element_against_brute_force() {
    let items = [3, -1, 2, 0];
    agree(
        -1,
        3,
        |m, [x, y, _]| {
            m.element(x, items, y);
        },
        |[x, y, _]| (0..4).contains(&x) && items[x as usize] as i64 == y,
    );
    // Items with variables: z == [y, y + 1, 2][x].
    agree(
        -1,
        3,
        |m, [x, y, z]| {
            m.element(x, [Expression::from(y), y + 1, Expression::from(2)], z);
        },
        |[x, y, z]| match x {
            0 => z == y,
            1 => z == y + 1,
            2 => z == 2,
            _ => false,
        },
    );
}

#[test]
fn table_against_brute_force() {
    let tuples = [[0, 1], [2, 2], [3, 0]];
    agree(
        0,
        3,
        |m, [x, y, _]| {
            m.table(&[x, y], tuples);
        },
        |[x, y, _]| tuples.contains(&[x, y]),
    );
    agree(
        0,
        3,
        |m, [x, y, _]| {
            m.forbidden_table(&[x, y], tuples);
        },
        |[x, y, _]| !tuples.contains(&[x, y]),
    );
}

#[test]
fn min_max_abs_against_brute_force() {
    agree(
        -2,
        2,
        |m, [x, y, z]| {
            m.max_of(z, [Expression::from(x), y + 1]);
        },
        |[x, y, z]| z == x.max(y + 1),
    );
    agree(
        -2,
        2,
        |m, [x, y, z]| {
            m.min_of(z, [x, y]);
        },
        |[x, y, z]| z == x.min(y),
    );
    agree(
        -2,
        2,
        |m, [x, y, z]| {
            m.abs_of(z, x - y);
        },
        |[x, y, z]| z == (x - y).abs(),
    );
}

#[test]
fn only_if_against_brute_force() {
    // With x as the switch: x == 2 forces y + z >= 3, x != 1 forces y <= z.
    agree(
        0,
        2,
        |m, [x, y, z]| {
            m.add(constraint!(y + z >= 3).only_if(x.is(2)));
            m.add(constraint!(y <= z).only_if(x.is_not(1)));
        },
        |[x, y, z]| (x != 2 || y + z >= 3) && (x == 1 || y <= z),
    );
    // Binary switches: b (= x here, in 0..=1) and !b.
    agree(
        0,
        1,
        |m, [b, y, z]| {
            m.add(constraint!(y == 1).only_if(b));
            m.add(constraint!(z == 0).only_if(!b));
        },
        |[b, y, z]| if b == 1 { y == 1 } else { z == 0 },
    );
}

#[test]
fn scheduling_against_brute_force() {
    // Starts x, y, z of tasks with durations 2, 1, 2 on one machine.
    agree(
        0,
        4,
        |m, [x, y, z]| {
            let ivs = [m.interval(x, 2), m.interval(y, 1), m.interval(z, 2)];
            m.no_overlap(&ivs);
        },
        |[x, y, z]| {
            let apart = |a: i64, da: i64, b: i64, db: i64| a + da <= b || b + db <= a;
            apart(x, 2, y, 1) && apart(x, 2, z, 2) && apart(y, 1, z, 2)
        },
    );
    // Capacity 3 with demands 2, 2, 1: x and y can't overlap.
    agree(
        0,
        3,
        |m, [x, y, z]| {
            let ivs = [m.interval(x, 2), m.interval(y, 2), m.interval(z, 1)];
            m.cumulative(&ivs, [2, 2, 1], 3);
        },
        |[x, y, _]| x + 2 <= y || y + 2 <= x,
    );
}

#[test]
fn optional_intervals() {
    // Two tasks of length 3 in a horizon of 4: at most one can run.
    let mut m = Model::new();
    let start = m.add_vars(2, var().integer().bounds(0, 1).name("start"));
    let run = m.add_vars(2, var().binary().name("run"));
    let ivs: Vec<_> = (0..2).map(|k| m.optional_interval(start[k], 3, run[k])).collect();
    m.no_overlap(&ivs);
    m.maximize(run[0] + run[1]);
    let result = m.solve(&Options::default()).unwrap();
    assert_eq!(result.status(), Status::Complete);
    assert_eq!(result.objective(), Some(1.0));
    check_export(&m, &result);
}

#[test]
fn gapped_domains_and_satisfy() {
    let mut m = Model::new();
    let x = m.add_var(var().domain([1, 4, 9]).name("x"));
    let y = m.add_var(var().domain([2, 3]).name("y"));
    m.add(constraint!(x + y == 7));
    m.all_solutions(true);
    let result = m.solve(&Options::default()).unwrap();
    let mut got: Vec<(f64, f64)> = result.solutions().iter().map(|s| (s[x], s[y])).collect();
    got.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(got, vec![(4.0, 3.0)]);
    assert_eq!(result.objective(), None);
    check_export(&m, &result);
}

#[test]
fn infeasible_and_unbounded_are_statuses() {
    let mut m = Model::new();
    let x = m.add_var(var().integer().bounds(0, 10));
    m.add(constraint!(2 * x == 7));
    let result = m.solve(&Options::default()).unwrap();
    assert_eq!(result.status(), Status::Infeasible);
    assert!(result.best().is_none());
    check_export(&m, &result);

    let mut m = Model::new();
    let x = m.add_var(var().min(0));
    m.maximize(x);
    assert_eq!(m.solve(&Options::default()).unwrap().status(), Status::Unbounded);
}

#[test]
fn ranged_and_named_constraints_export() {
    let mut m = Model::new();
    let x = m.add_var(var().name("x"));
    let y = m.add_var(var().name("y"));
    m.add((x + y).between(2.0, 5.0).name("window"));
    m.add(constraint!(x - y == 0.5).name("not an identifier"));
    m.maximize(x - 0.25 * y);
    let result = m.solve(&Options::default()).unwrap();
    assert!(close(result.objective().unwrap(), 2.75 - 0.25 * 2.25));
    let json = m.to_json().unwrap();
    assert!(json.contains("window_lower") && json.contains("window_upper") && json.contains("not an identifier"));
    check_export(&m, &result);
}

#[test]
fn errors() {
    // A variable of another model.
    let mut a = Model::new();
    let mut b = Model::new();
    let x = a.add_var(var().min(0));
    let _ = b.add_var(var().min(0));
    b.add(constraint!(x >= 1));
    let e = b.solve(&Options::default()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Invalid);
    assert!(e.message().contains("another model"), "{e}");
    assert!(b.to_json().is_err());

    // Nonlinear terms where only affine expressions go.
    let mut m = Model::new();
    let v = m.add_vars(2, var().integer().bounds(0, 3));
    m.all_different([v[0] * v[1], Expression::from(v[0])]);
    assert_eq!(m.solve(&Options::default()).unwrap_err().kind(), ErrorKind::Unsupported);
    let mut m = Model::new();
    let v = m.add_vars(2, var().binary());
    m.add(constraint!(v[0] * v[1] <= 0).only_if(v[0]));
    assert_eq!(m.solve(&Options::default()).unwrap_err().kind(), ErrorKind::Unsupported);

    // Names: identifiers, unique.
    let mut m = Model::new();
    m.add_var(var().name("2x"));
    assert_eq!(m.solve(&Options::default()).unwrap_err().kind(), ErrorKind::Invalid);
    let mut m = Model::new();
    m.add_var(var().name("x"));
    m.add_var(var().name("x"));
    assert!(m.solve(&Options::default()).unwrap_err().message().contains("two variables"));

    // Division by zero.
    let mut m = Model::new();
    let x = m.add_var(var());
    m.add(constraint!(x / 0 <= 1));
    assert_eq!(m.solve(&Options::default()).unwrap_err().kind(), ErrorKind::Invalid);

    // Every solution needs integer variables.
    let mut m = Model::new();
    m.add_var(var().bounds(0, 1));
    m.all_solutions(true);
    assert_eq!(m.solve(&Options::default()).unwrap_err().kind(), ErrorKind::Unsupported);

    // Problem files.
    assert_eq!(Problem::from_json("{").unwrap_err().kind(), ErrorKind::Parse);
    assert_eq!(Problem::from_file("no/such/file.json").unwrap_err().kind(), ErrorKind::Io);
}

#[test]
fn time_limits_and_logging() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let mut m = Model::new();
    let x = m.add_vars(3, var().integer().bounds(0, 5));
    m.add(constraint!(x[0] + 2 * x[1] + 3 * x[2] <= 7));
    m.maximize(x[0] + 3 * x[1] + 4 * x[2]);
    let options = Options::default()
        .time_limit(Duration::from_secs(30))
        .engine(optimizam::Engine::Milp)
        .log(move |line| sink.lock().unwrap().push(line.to_string()));
    let result = m.solve(&options).unwrap();
    assert_eq!(result.objective(), Some(10.0));
    assert!(!lines.lock().unwrap().is_empty());
    // Without a logger, nothing is collected (and nothing is printed).
    let before = lines.lock().unwrap().len();
    m.solve(&Options::default()).unwrap();
    assert_eq!(lines.lock().unwrap().len(), before);
}

#[test]
fn solves_on_several_threads() {
    let handles: Vec<_> = (1..=4)
        .map(|k| {
            std::thread::spawn(move || {
                let mut m = Model::new();
                let x = m.add_var(var().integer().bounds(0, 100));
                m.add(constraint!(3 * x <= 10 * k));
                m.maximize(x);
                m.solve(&Options::default()).unwrap().objective().unwrap()
            })
        })
        .collect();
    let got: Vec<f64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(got, vec![3.0, 6.0, 10.0, 13.0]);
}

#[test]
#[cfg(feature = "cli")]
fn export_runs_with_the_command() {
    let mut m = Model::new();
    let x = m.add_vars(3, var().integer().bounds(0, 5).name("x"));
    m.add(constraint!(x[0] + x[1] + x[2] == 7));
    m.all_different(&x);
    m.minimize(x[0] - x[2]);
    let result = m.solve(&Options::default()).unwrap();
    let dir = std::env::temp_dir().join(format!("optimizam-export-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("model.json");
    std::fs::write(&file, m.to_json().unwrap()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_optimizam")).arg(&file).arg("-").output().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let written: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(written["status"], "complete");
    assert_eq!(written["solutions"][0]["objective"].as_f64(), result.objective());
    // The builder's own result file has the same shape.
    let own: Value = serde_json::from_str(&result.to_json()).unwrap();
    assert_eq!(own["solutions"][0]["variables"], written["solutions"][0]["variables"]);
}

/// The CP kinds with items they can't take (continuous variables, fractional
/// coefficients, unbounded integers, empty lists) give errors or answers,
/// never panics, through the solver and through the export.
#[test]
fn odd_inputs_never_panic() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    type Build = fn(&mut Model, &[Variable]);
    let builds: &[(&str, Build)] = &[
        ("all_different continuous", |m, v| {
            m.all_different([v[0], v[1]]);
        }),
        ("all_different fractional", |m, v| {
            m.all_different([0.5 * v[2], Expression::from(v[3])]);
        }),
        ("all_different unbounded", |m, v| {
            m.all_different([v[4], v[5]]);
        }),
        ("all_different empty", |m, _| {
            m.all_different(Vec::<Expression>::new());
        }),
        ("table continuous", |m, v| {
            m.table(&[v[0], v[2]], [[1, 2]]);
        }),
        ("table unbounded", |m, v| {
            m.table(&[v[4]], [[1], [2]]);
        }),
        ("table empty", |m, _| {
            m.table(&[], Vec::<Vec<i64>>::new());
        }),
        ("element continuous index", |m, v| {
            m.element(v[0], [1, 2], v[2]);
        }),
        ("element fractional index", |m, v| {
            m.element(0.5 * v[2], [1, 2], v[3]);
        }),
        ("element unbounded", |m, v| {
            m.element(v[4], [Expression::from(v[5]), Expression::from(v[2])], v[5]);
        }),
        ("element empty", |m, v| {
            m.element(v[2], Vec::<Expression>::new(), v[3]);
        }),
        ("max continuous", |m, v| {
            m.max_of(v[0], [v[1], v[2]]);
        }),
        ("max unbounded", |m, v| {
            m.max_of(v[4], [v[5], v[2]]);
        }),
        ("min empty", |m, v| {
            m.min_of(v[2], Vec::<Expression>::new());
        }),
        ("abs continuous", |m, v| {
            m.abs_of(v[0], v[1]);
        }),
        ("abs unbounded", |m, v| {
            m.abs_of(v[4], v[5]);
        }),
        ("interval continuous", |m, v| {
            let iv = m.interval(v[0], 2);
            m.no_overlap(&[iv]);
        }),
        ("interval fractional", |m, v| {
            let iv = m.interval(0.5 * v[2], 2);
            m.no_overlap(&[iv]);
        }),
        ("interval unbounded", |m, v| {
            let a = m.interval(v[4], v[5]);
            let b = m.interval(v[2], 1);
            m.no_overlap(&[a, b]);
            m.cumulative(&[a, b], [1, 1], 1);
        }),
        ("interval negative", |m, v| {
            let a = m.interval(v[2], -1);
            m.no_overlap(&[a]);
        }),
        ("presence not binary", |m, v| {
            let a = m.optional_interval(v[2], 1, v[3]);
            m.no_overlap(&[a]);
        }),
        ("cumulative continuous demand", |m, v| {
            let a = m.interval(v[2], 1);
            m.cumulative(&[a], [v[0]], 2);
        }),
        ("cumulative mismatched demands", |m, v| {
            let a = m.interval(v[2], 1);
            let b = m.interval(v[3], 1);
            m.cumulative(&[a, b], [1], 2);
        }),
        ("cumulative empty", |m, _| {
            m.cumulative(&[], Vec::<Expression>::new(), 1);
        }),
        ("only_if continuous", |m, v| {
            m.add(constraint!(v[2] <= 1).only_if(v[0]));
        }),
        ("only_if integer", |m, v| {
            m.add(constraint!(v[2] <= 1).only_if(v[3]));
        }),
        ("only_if unbounded value", |m, v| {
            m.add(constraint!(v[2] <= 1).only_if(v[4].is(3)));
        }),
        ("only_if unbounded row", |m, v| {
            m.add(constraint!(v[4] <= 1).only_if(v[6]));
        }),
        ("powers of zero", |m, v| {
            m.add(constraint!((v[0] - v[0]).powf(-1.0) <= 1));
            m.minimize(v[1].powf(0.5) + v[2].pow(v[1]));
        }),
    ];
    for &(what, build) in builds {
        for all in [false, true] {
            let mut m = Model::new();
            // With every solution wanted, every variable must be integer.
            let real = if all { var().integer() } else { var() };
            let v = vec![
                m.add_var(real.clone().bounds(0, 3)),
                m.add_var(real.bounds(0, 3)),
                m.add_var(var().integer().bounds(0, 3)),
                m.add_var(var().integer().bounds(0, 3)),
                m.add_var(var().integer()),
                m.add_var(var().integer().min(0)),
                m.add_var(var().binary()),
            ];
            build(&mut m, &v);
            m.all_solutions(all);
            let options = Options::default().time_limit(std::time::Duration::from_millis(200));
            let solved = catch_unwind(AssertUnwindSafe(|| m.solve(&options).map(|r| r.status())));
            assert!(solved.is_ok(), "{what} (all: {all}): solve panicked");
            let exported = catch_unwind(AssertUnwindSafe(|| {
                m.to_json().map(|json| Problem::from_json(&json).map(|p| p.solve(&options).map(|r| r.status())))
            }));
            assert!(exported.is_ok(), "{what} (all: {all}): export panicked");
        }
    }
}

/// Numbers of any type as coefficients, right-hand sides, and bounds; `sum`
/// without a turbofish; borrowed keys; values read as integers.
#[test]
fn numbers_keys_and_reads_need_no_casts() {
    use optimizam::{Element, Set, sum};
    let plants = ["north", "south"];
    let set = Set::new("PLANTS", plants);
    let (limit, minimum, weights): (usize, u64, [i64; 2]) = (9, 2, [3, 4]);
    let mut m = Model::new();
    let make = m.add_family("make", &[&set], var().integer().bounds(0, limit));
    let spare = m.add_var(var().bounds(0u8, 2.5f32).name("spare"));
    // Keys as an iterator over a slice gives them (&&str), and a number of any type on the right.
    let load = sum(plants.iter().zip(weights).map(|(p, w)| make[p] * w));
    m.add(constraint!(load.clone() + spare <= 30_i64));
    m.add(constraint!(sum(plants.iter().map(|p| make[p])) >= minimum));
    m.add((make["north"] - make["south"]).between(-1, 1_u32));
    m.set_bounds(make["south"], 1_i64, limit);
    m.maximize(load + spare);
    let result = m.solve(&Options::default()).unwrap();
    let best = result.best().unwrap();
    // 3 n + 4 s + spare <= 30 with |n - s| <= 1: (4, 4) and spare 2.
    assert_eq!((best.int(make["north"]), best.int(make["south"])), (4, 4));
    assert!((best[spare] - 2.0).abs() < 1e-9 && !best.is_one(make["north"]));
    assert_eq!(result.objective(), Some(30.0));

    // A binary read as a truth value, and elements read as what they are.
    let mut k = Model::new();
    let pick = k.add_vars(3, var().binary());
    k.add(constraint!(sum(&pick) == 1));
    k.maximize(pick[1] * 2 + pick[2]);
    let s = k.solve(&Options::default()).unwrap();
    let picked: Vec<bool> = pick.iter().map(|&v| s.best().unwrap().is_one(v)).collect();
    assert_eq!(picked, [false, true, false]);
    assert_eq!((Element::from("north").as_str(), Element::from("north").as_int()), (Some("north"), None));
    assert_eq!((Element::from(7).as_str(), Element::from(7).as_int()), (None, Some(7)));
}
