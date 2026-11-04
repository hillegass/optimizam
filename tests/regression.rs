//! Regression tests: solve every problem in `examples/` with the optimizam
//! binary and compare against the expected `examples/<name>.result.json`.
//!
//! Expected results come from brute force (small examples) or from HiGHS via
//! `tools/highs_reference.py` (larger examples). What is compared:
//! - `status` must match exactly.
//! - Objective values must agree within a relative tolerance.
//! - With `"solutions": "all"`, the set of solutions must match exactly
//!   (order-independent). Phase 1 only allows "all" for all-integer problems,
//!   so values are compared as integers.
//! - With `"solutions": "one"`, the optimum may not be unique, so variable
//!   values are not compared, only that every variable and index is present.
//!
//! Set OPTIMIZAM_BIN to run the suite against another solver, e.g.
//! `OPTIMIZAM_BIN=tools/highs.sh cargo test --test regression`.

use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Generous so that slow debug builds don't fail on the larger examples.
const TIME_LIMIT_SECONDS: &str = "600";
const REL_TOL: f64 = 1e-6;

macro_rules! regression_tests {
    ($($name:ident),* $(,)?) => {
        const LISTED: &[&str] = &[$(stringify!($name)),*];
        $( #[test] fn $name() { check_example(stringify!($name)); } )*
    };
}

#[rustfmt::skip]
regression_tests!(
    knapsack,
    four_queens,
    production,
    supply_chain_lp,
    facility_location_milp,
    parity_infeasible,
    unbounded_milp,
    domain_gaps,
    lot_sizing_milp,
    n_queens_alldiff,
    room_assignment,
    sudoku,
);

fn examples_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples"))
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

fn check_example(name: &str) {
    let problem_path = examples_dir().join(format!("{name}.json"));
    let problem = read_json(&problem_path);
    let expected = read_json(&examples_dir().join(format!("{name}.result.json")));

    // OPTIMIZAM_BIN runs the suite against another solver with the same CLI,
    // e.g. tools/highs.sh, to check the expected results and the test itself.
    let bin = std::env::var("OPTIMIZAM_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_optimizam").to_string());
    let output = Command::new(&bin)
        .arg(format!("--time={TIME_LIMIT_SECONDS}"))
        .arg(&problem_path)
        .arg("-")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {bin}: {e}"));
    assert!(
        output.status.success(),
        "{name}: optimizam exited with {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let actual: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("{name}: stdout is not JSON ({e}):\n{}", String::from_utf8_lossy(&output.stdout)));

    assert_eq!(actual["status"], expected["status"], "{name}: status");

    let want_all = problem["solve"]["solutions"] == "all";
    let exp_sols = expected["solutions"].as_array().expect("expected solutions array");
    let act_sols = actual["solutions"].as_array().unwrap_or_else(|| panic!("{name}: no solutions array"));

    // Statuses without solutions have no layout to compare against.
    let layout = exp_sols.first().map_or(Value::Null, |s| s["variables"].clone());
    for sol in act_sols {
        assert_same_shape(&layout, &sol["variables"], name, "variables");
    }
    if let Some(exp_obj) = exp_sols.first().and_then(|s| s["objective"].as_f64()) {
        for sol in act_sols {
            let obj = sol["objective"].as_f64().unwrap_or_else(|| panic!("{name}: solution lacks objective"));
            assert!(close(obj, exp_obj), "{name}: objective {obj}, expected {exp_obj}");
        }
    }

    if want_all {
        let canon = |sols: &[Value]| -> BTreeSet<String> {
            sols.iter().map(|s| canonical_integers(&s["variables"]).to_string()).collect()
        };
        assert_eq!(act_sols.len(), exp_sols.len(), "{name}: number of solutions");
        assert_eq!(canon(act_sols), canon(exp_sols), "{name}: solution sets differ");
    } else {
        assert_eq!(act_sols.len(), exp_sols.len().min(1), "{name}: number of solutions");
    }
}

/// Same nested keys; leaves are numbers on both sides, or strings on both
/// sides (values of symbolic set-valued variables).
fn assert_same_shape(expected: &Value, actual: &Value, name: &str, path: &str) {
    match expected {
        Value::Null => {}
        Value::Object(exp) => {
            let act = actual.as_object().unwrap_or_else(|| panic!("{name}: {path} should be an object"));
            let ek: BTreeSet<_> = exp.keys().collect();
            let ak: BTreeSet<_> = act.keys().collect();
            assert_eq!(ak, ek, "{name}: keys of {path}");
            for (k, v) in exp {
                assert_same_shape(v, &act[k], name, &format!("{path}.{k}"));
            }
        }
        Value::String(_) => assert!(actual.is_string(), "{name}: {path} should be a set element, got {actual}"),
        _ => assert!(actual.is_number(), "{name}: {path} should be a number, got {actual}"),
    }
}

/// Replace every number with the nearest integer so 17 and 17.0 compare equal.
fn canonical_integers(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), canonical_integers(v))).collect()),
        Value::Number(n) => Value::from(n.as_f64().expect("number").round() as i64),
        other => other.clone(),
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= REL_TOL * a.abs().max(b.abs()).max(1.0)
}

/// Every example problem must have an expected result and a test above.
#[test]
fn every_example_is_tested() {
    let mut missing = Vec::new();
    for entry in fs::read_dir(examples_dir()).expect("examples dir") {
        let file = entry.unwrap().file_name().into_string().unwrap();
        let Some(stem) = file.strip_suffix(".json") else {
            continue;
        };
        if stem.ends_with(".result") {
            continue;
        }
        if !examples_dir().join(format!("{stem}.result.json")).exists() {
            missing.push(format!("{stem}: no {stem}.result.json"));
        }
        if !LISTED.contains(&stem) {
            missing.push(format!("{stem}: not listed in regression_tests!"));
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}
