//! The command line: an optional RESULT, `--check`, `--quiet`, `--docs` and
//! `--schema`, `--nonzeros`, and `--constraints=binding`. And the same in
//! Rust: `Model::check`, typed warnings.

use optimizam::{Engine, Model, Options, WarningKind, constraint, var};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// Runs the command with `args`, the problem on stdin.
fn run(args: &[&str], problem: &Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_optimizam"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(problem.to_string().as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// A rota with things to warn about: a `forall` that matches nothing, a
/// parameter and a variable that nothing uses.
fn rota() -> Value {
    json!({
        "version": "1.3",
        "sets": { "W": ["ann", "bob", "cy"], "D": { "from": 1, "to": 4 } },
        "parameters": { "bonus": 5, "need": 2 },
        "variables": [
            { "name": "on", "type": "binary", "index": ["W", "D"] },
            { "name": "spare", "type": "binary" }
        ],
        "constraints": [
            { "name": "cover", "forall": "d in D", "expr": "sum(w in W) on[w,d] >= need" },
            { "name": "rest", "forall": "w in W, d in D: d > 4", "expr": "on[w,d] == 0" },
            { "forall": "w in W", "expr": "sum(d in D) on[w,d] <= 3" }
        ],
        "solve": { "method": "minimize", "objective": "sum(w in W, d in D) on[w,d]" }
    })
}

#[test]
fn the_result_goes_to_stdout_without_a_second_argument() {
    let with_dash = run(&["--threads=1", "-", "-"], &rota());
    let without = run(&["--threads=1", "-"], &rota());
    assert!(with_dash.status.success() && without.status.success());
    let result = |out: &Output| {
        let mut json: Value = serde_json::from_slice(&out.stdout).unwrap();
        json.as_object_mut().unwrap().remove("statistics");
        json
    };
    assert_eq!(result(&with_dash), result(&without));
    assert_eq!(result(&without)["solutions"][0]["objective"], 8.0);
    // No problem at all is still a usage error.
    let none = Command::new(env!("CARGO_BIN_EXE_optimizam")).output().unwrap();
    assert_eq!(none.status.code(), Some(2));
}

#[test]
fn warnings_have_kinds_and_can_be_silenced() {
    let out = run(&["-"], &rota());
    let lines: Vec<String> = text(&out.stderr).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines.iter().all(|line| line.starts_with("warning: ")), "{lines:?}");
    for flag in ["--quiet", "-q"] {
        let quiet = run(&[flag, "-"], &rota());
        assert!(quiet.status.success() && quiet.stderr.is_empty(), "{}", text(&quiet.stderr));
    }
    // As events, each says what kind it is and where.
    let out = run(&["--progress", "-", "-"], &rota());
    let events: Vec<Value> = text(&out.stderr).lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let warnings: Vec<(&str, &str)> = events
        .iter()
        .filter(|e| e["event"] == "warning")
        .map(|e| (e["kind"].as_str().unwrap(), e["place"].as_str().unwrap()))
        .collect();
    assert_eq!(
        warnings,
        [
            ("empty_forall", "constraints[1]"),
            ("unused_parameter", "parameters.bonus"),
            ("unused_variable", "variables[1]")
        ]
    );
    // In Rust: values to filter, which print and compare as their messages.
    let problem = Model::from_json(&rota().to_string()).unwrap();
    let warnings = problem.warnings();
    assert_eq!(warnings[1], "parameter bonus is never used");
    assert_eq!(warnings[1].to_string(), "parameter bonus is never used");
    let kinds: Vec<WarningKind> = warnings.iter().map(|w| w.kind()).collect();
    assert_eq!(kinds, [WarningKind::EmptyForall, WarningKind::UnusedParameter, WarningKind::UnusedVariable]);
    assert_eq!(warnings[2].place(), Some("variables[1]"));
    let loud: Vec<_> = warnings.iter().filter(|w| w.kind() != WarningKind::UnusedVariable).collect();
    assert_eq!(loud.len(), 2);
}

#[test]
fn check_says_what_the_model_comes_to() {
    let out = run(&["--check", "-"], &rota());
    assert!(out.status.success() && out.stderr.is_empty(), "{}", text(&out.stderr));
    let check: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(check["variables"], json!({ "on": 12, "spare": 1 }));
    // The forall that matched nothing shows as 0; the nameless constraint goes by its place.
    assert_eq!(check["constraints"], json!({ "cover": 4, "rest": 0, "constraints[2]": 3 }));
    assert_eq!((&check["columns"], &check["rows"], &check["engine"]), (&json!(13), &json!(7), &json!("milp")));
    assert_eq!(check["warnings"].as_array().unwrap().len(), 3);
    assert_eq!(check["warnings"][0]["kind"], "empty_forall");
    // The engine is the one a solve would use: with --engine, that one, if it can.
    let forced: Value = serde_json::from_slice(&run(&["--check", "--engine=cp", "-"], &rota()).stdout).unwrap();
    assert_eq!(forced["engine"], "cp");
    let refused = run(&["--check", "--engine=global", "-"], &rota());
    assert_eq!(refused.status.code(), Some(1));
    assert!(text(&refused.stderr).contains("the global engine is for problems with nonlinear terms"));

    // The same in Rust.
    let problem = Model::from_json(&rota().to_string()).unwrap();
    let check = problem.check(&Options::default()).unwrap();
    assert_eq!(check.variables(), [("on".to_string(), 12), ("spare".to_string(), 1)]);
    assert_eq!(check.constraints()[1], ("rest".to_string(), 0));
    assert_eq!((check.columns(), check.rows(), check.engine()), (13, 7, Some(Engine::Milp)));
    // Checking doesn't solve: a model that would take long answers at once.
    let mut hard = Model::new();
    let x = hard.add_vars(60, var().integer().bounds(0, 59).name("x"));
    hard.all_different(&x);
    hard.all_different((0..60).map(|i| x[i] + i));
    hard.all_different((0..60).map(|i| x[i] - i));
    let started = std::time::Instant::now();
    let check = hard.check(&Options::default()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!((check.engine(), check.constraints().len()), (Some(Engine::Cp), 3));
    assert!(check.columns() > 60 && check.warnings().is_empty());
    // What a solve rejects, a check rejects; and what is infeasible on its face is a warning.
    let mut bad = Model::new();
    let w = bad.add_var(var().bounds(0, 5).name("w"));
    bad.add(w.lt(2));
    assert!(bad.check(&Options::default()).unwrap_err().to_string().contains("a strict comparison"));
    let mut stuck = Model::new();
    let n = stuck.add_var(var().integer().bounds(0, 5).name("n"));
    stuck.add(constraint!(n >= 7).name("far"));
    let check = stuck.check(&Options::default()).unwrap();
    assert_eq!(check.warnings().len(), 1);
    assert_eq!(check.warnings()[0].kind(), WarningKind::Infeasible);
    assert_eq!(check.engine(), None);
}

#[test]
fn documents_and_schemas_are_built_in() {
    let print = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_optimizam")).args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    assert!(print(&["--docs", "problem"]).starts_with("# Problem file format"));
    assert!(print(&["--docs=result"]).contains("## Statuses") || print(&["--docs=result"]).starts_with("# Result"));
    assert!(print(&["--docs", "cli"]).starts_with("# Command line"));
    for which in ["problem", "result"] {
        let schema: Value = serde_json::from_str(&print(&["--schema", which])).unwrap();
        assert!(schema["$schema"].as_str().unwrap().contains("json-schema.org"));
    }
    // The help points at them, not at a repository.
    let help = print(&["--help"]);
    assert!(help.contains("--docs problem") && !help.contains("source repository"), "{help}");
}

#[test]
fn zeros_and_slack_can_be_left_out() {
    let full: Value = serde_json::from_slice(&run(&["--threads=1", "--constraints", "-"], &rota()).stdout).unwrap();
    let out = run(&["--threads=1", "--constraints=binding", "--nonzeros", "-"], &rota());
    let brief: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(full.get("omitted").is_none());
    assert_eq!(brief["omitted"], json!(["zeros", "slack"]));
    let (all, some) = (&full["solutions"][0], &brief["solutions"][0]);
    // Every variable is still named; of its entries, the ones that aren't 0.
    assert_eq!(all["variables"]["spare"], some["variables"]["spare"]);
    let mut ones = 0;
    for (worker, days) in all["variables"]["on"].as_object().unwrap() {
        for (day, value) in days.as_object().unwrap() {
            let kept = some["variables"]["on"].get(worker).and_then(|days| days.get(day));
            assert_eq!(kept.is_some(), value == &json!(1), "{worker} {day}");
            ones += usize::from(kept.is_some());
        }
    }
    assert_eq!(ones, 8);
    // Of the constraints, the ones without slack, each family still a table by its index.
    let members = |solution: &Value, tight: bool| -> Vec<String> {
        let mut found = Vec::new();
        for (family, table) in solution["constraints"].as_object().unwrap() {
            for (key, c) in table.as_object().unwrap() {
                if !tight || c["slack"].as_f64() == Some(0.0) {
                    found.push(format!("{family}[{key}]"));
                }
            }
        }
        found
    };
    // (Read back, the names are in the reader's order, not the file's.)
    assert_eq!(all["constraints"].as_object().unwrap().keys().collect::<Vec<_>>(), ["constraints[2]", "cover"]);
    let (listed, tight) = (members(some, false), members(all, true));
    assert_eq!(listed, tight);
    assert!(listed.len() >= 4 && listed.len() < members(all, false).len());

    // In Rust the result keeps everything; only the file is shorter.
    let problem = Model::from_json(&rota().to_string()).unwrap();
    let result = problem.solve(&Options::default().threads(1).nonzeros(true)).unwrap();
    let zeros = (1..=4).filter(|d| result.best().unwrap().get("on", ("ann", d)) == Some(0.0)).count();
    assert!(zeros > 0);
    assert!(result.to_json().contains(r#""omitted": ["zeros"]"#));
}

#[test]
fn show_writes_out_what_a_name_came_to() {
    // The flag alone is a check; a plain check says nothing of "shown".
    let plain: Value = serde_json::from_slice(&run(&["--check", "-"], &rota()).stdout).unwrap();
    assert!(plain.get("shown").is_none());
    let out = run(&["--show", "cover", "--show=on", "--show", "rest", "--show", "constraints[2]", "-"], &rota());
    assert!(out.status.success(), "{}", text(&out.stderr));
    let check: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(check["constraints"], plain["constraints"]);
    let shown = check["shown"].as_array().unwrap();
    assert_eq!(shown.len(), 4);
    // A constraint: its first five members (here all four), as a file would write them.
    assert_eq!(
        (&shown[0]["name"], &shown[0]["kind"], &shown[0]["count"]),
        (&json!("cover"), &json!("constraint"), &json!(4))
    );
    assert_eq!(
        shown[0]["instances"],
        json!({
            "cover[1]": "on['ann', 1] + on['bob', 1] + on['cy', 1] >= 2",
            "cover[2]": "on['ann', 2] + on['bob', 2] + on['cy', 2] >= 2",
            "cover[3]": "on['ann', 3] + on['bob', 3] + on['cy', 3] >= 2",
            "cover[4]": "on['ann', 4] + on['bob', 4] + on['cy', 4] >= 2"
        })
    );
    // A variable: its first five elements of twelve.
    assert_eq!((&shown[1]["kind"], &shown[1]["count"]), (&json!("variable"), &json!(12)));
    assert_eq!(shown[1]["instances"].as_object().unwrap().len(), 5);
    assert_eq!(shown[1]["instances"]["on[ann,1]"], "binary");
    // A forall that matched nothing, and a constraint without a name.
    assert_eq!((&shown[2]["count"], &shown[2]["instances"]), (&json!(0), &json!({})));
    assert_eq!(
        shown[3]["instances"]["constraints[2][bob]"],
        "on['bob', 1] + on['bob', 2] + on['bob', 3] + on['bob', 4] <= 3"
    );
    // One member by its own name; a name that nothing has.
    let one: Value = serde_json::from_slice(&run(&["--show", "cover[3]", "-"], &rota()).stdout).unwrap();
    assert_eq!(one["shown"][0]["instances"], json!({ "cover[3]": "on['ann', 3] + on['bob', 3] + on['cy', 3] >= 2" }));
    let wrong = run(&["--show", "covers", "-"], &rota());
    assert_eq!(wrong.status.code(), Some(1));
    assert_eq!(
        text(&wrong.stderr).trim(),
        "error: nothing to show for covers: no constraint or variable has that name; did you mean cover?"
    );
}

#[test]
fn every_kind_of_constraint_can_be_shown() {
    use optimizam::Set;
    let machines = Set::new("MACHINES", ["lathe", "mill"]);
    let mut m = Model::new();
    let x = m.add_vars(12, var().integer().bounds(0, 20).name("x"));
    let on = m.add_var(var().binary().name("on"));
    let which = m.add_var(var().values(&machines).name("which"));
    let free = m.add_var(var().name("free"));
    m.add_var(var().integer().domain([1, 2, 5]).start(2).name("gaps"));
    m.set_bounds(x[3], 4, 4);
    let texts = |m: &Model, name: &str| -> Vec<String> {
        let check = m.check(&Options::default().show(name)).unwrap();
        check.shown()[0].instances().iter().map(|(_, text)| text.clone()).collect()
    };
    // A long sum is cut short, with its other side kept; limits on both sides are both written.
    m.add(constraint!(optimizam::sum(x.iter().map(|&v| 2 * v)) + 3 <= 40).name("long"));
    assert_eq!(
        texts(&m, "long"),
        [
            "2 * x[0] + 2 * x[1] + 2 * x[2] + 2 * x[3] + 2 * x[4] + 2 * x[5] + 2 * x[6] + 2 * x[7] + ... (4 more terms) <= 37"
        ]
    );
    m.add((x[0] - x[1]).between(-2, 2).name("near").only_if(!on).only_if(which.is("mill")));
    assert_eq!(texts(&m, "near"), ["x[0] - x[1] >= (-2) and x[0] - x[1] <= 2, enforce_if not on and which == 'mill'"]);
    m.add(x[0].ne(x[1]).name("apart"));
    m.add(optimizam::Constraint::from(which.is_not("lathe")).name("not_lathe"));
    assert_eq!(
        (texts(&m, "apart"), texts(&m, "not_lathe")),
        (vec!["x[0] != x[1]".to_string()], vec!["which != 'lathe'".to_string()])
    );
    let id = m.all_different(x.iter().map(|&v| v + 1));
    m.set_name(id, "distinct");
    assert_eq!(
        texts(&m, "distinct"),
        [
            "all_different([1 + x[0], 1 + x[1], 1 + x[2], 1 + x[3], 1 + x[4], 1 + x[5], 1 + x[6], 1 + x[7], ... (4 more)])"
        ]
    );
    let id = m.forbidden_table(&[x[0], x[1]], [[1, 2], [3, 4]]);
    m.set_name(id, "pairs");
    assert_eq!(texts(&m, "pairs"), ["forbidden_table([x[0], x[1]], [[1, 2], [3, 4]])"]);
    let id = m.element(x[2], [5 * on, x[4].into(), 7.into()], x[5]);
    m.set_name(id, "pick");
    assert_eq!(texts(&m, "pick"), ["x[5] == [5 * on, x[4], 7][x[2]]"]);
    let jobs = [m.interval(x[6], 3), m.optional_interval(x[7], x[8], on)];
    let id = m.no_overlap(&jobs);
    m.set_name(id, "machine");
    assert_eq!(texts(&m, "machine"), ["no_overlap([interval(x[6], 3), interval(x[7], x[8], on)])"]);
    let id = m.cumulative(&jobs, [2, 1], x[9]);
    m.set_name(id, "crew");
    assert_eq!(
        texts(&m, "crew"),
        ["cumulative(intervals: [interval(x[6], 3), interval(x[7], x[8], on)], demands: [2, 1], capacity: x[9])"]
    );
    // Variables: the type, then what bounds each element.
    assert_eq!(texts(&m, "x")[3], "integer, fixed at 4");
    assert_eq!(texts(&m, "x")[0], "integer, lower 0, upper 20");
    assert_eq!(texts(&m, "on"), ["binary"]);
    assert_eq!(texts(&m, "which"), ["one of MACHINES (2 values)"]);
    assert_eq!(texts(&m, "free"), ["continuous"]);
    assert_eq!(texts(&m, "gaps"), ["integer, domain [[1, 2], [5, 5]], start 2"]);
    // An unnamed constraint goes by its place, and a removed one isn't there.
    let last = m.add(constraint!(free >= 1.5));
    assert_eq!(
        texts(&m, &format!("constraints[{}]", m.check(&Options::default()).unwrap().constraints().len() - 1)),
        ["free >= 1.5"]
    );
    m.remove(last);
    assert!(m.check(&Options::default().show("constraints[9]")).is_err());
}
