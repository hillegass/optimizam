//! Why a model is infeasible or unbounded: what a solve reads off the model,
//! and the conflict that `Options::explain` and `Model::conflict` search for.

mod common;

use common::Lcg;
use optimizam::{Model, Options, Status, constraint, sum, var};
use std::time::Instant;

fn solve(m: &Model) -> optimizam::SolveResult {
    m.solve(&Options::default()).unwrap()
}

#[test]
fn crossed_bounds_name_the_variable() {
    let mut m = Model::new();
    let x = m.add_var(var().integer().bounds(0, 10).name("x"));
    m.add_var(var().bounds(5.0, 1.0).name("y"));
    m.add(constraint!(x >= 2).name("floor"));
    let result = solve(&m);
    assert_eq!(result.status(), Status::Infeasible);
    let conflict = result.conflict().expect("crossed bounds are an obvious cause");
    assert_eq!(conflict.variables(), ["y"]);
    assert!(conflict.constraints().is_empty() && conflict.constraint_ids().is_empty());
    assert!(conflict.is_minimal());
    assert!(result.to_json().contains(r#""conflict": { "variables": ["y"], "constraints": [], "minimal": true }"#));
}

#[test]
fn a_variable_with_no_value_left_is_named() {
    // An integer between 0.2 and 0.8, and a domain with nothing in the new bounds.
    let mut m = Model::new();
    m.add_var(var().integer().bounds(0.2, 0.8).name("count"));
    let sizes = m.add_family("size", &[&optimizam::Set::range("BOX", 1..=2)], var().integer().domain([0, 4, 8]));
    m.set_bounds(sizes[2], 5, 7);
    let result = solve(&m);
    assert_eq!(result.status(), Status::Infeasible);
    let conflict = result.conflict().unwrap();
    assert_eq!(conflict.variables(), ["count", "size[2]"]);
    // Each is a cause alone, so the pair isn't minimal.
    assert!(!conflict.is_minimal());
}

#[test]
fn a_constraint_that_cannot_hold_within_bounds_is_named() {
    let mut m = Model::new();
    let x = m.add_vars(3, var().bounds(0.0, 10.0).name("x"));
    m.add(constraint!(x[0] + x[1] <= 15).name("fine"));
    let id = m.add(constraint!(x[0] + 2 * x[1] - x[2] >= 31).name("too_much"));
    m.minimize(x[0]);
    let result = solve(&m);
    assert_eq!(result.status(), Status::Infeasible);
    let conflict = result.conflict().unwrap();
    assert_eq!((conflict.constraints(), conflict.constraint_ids()), (&["too_much".to_string()][..], &[id][..]));
    assert!(conflict.variables().is_empty() && conflict.is_minimal());
}

/// A sum whose condition matches nothing is the constant 0: `0 >= 1`.
#[test]
fn an_empty_sum_is_named_in_a_problem_file() {
    let problem = Model::from_json(
        r#"{"version": "1.3", "sets": {"T": {"from": 1, "to": 3}},
            "variables": [{"name": "y", "type": "binary", "index": ["T"]}],
            "constraints": [{"name": "ok", "forall": "t in T", "expr": "y[t] <= 1"},
                            {"name": "need", "expr": "sum(t in T: t > 5) y[t] >= 1"}],
            "solve": {"method": "satisfy"}}"#,
    )
    .unwrap();
    let result = problem.solve(&Options::default()).unwrap();
    assert_eq!(result.status(), Status::Infeasible);
    assert_eq!(result.conflict().unwrap().constraints(), ["need"]);
    let json: serde_json::Value = serde_json::from_str(&result.to_json()).unwrap();
    assert_eq!(json["version"], "1.2");
    assert_eq!(json["conflict"], serde_json::json!({"constraints": ["need"], "minimal": true}));
}

/// Six floors, six caps, a budget that two of the floors break, and a
/// constraint with no name.
const FLOORS_AND_A_BUDGET: &str = r#"{
    "version": "1.3", "sets": {"I": {"from": 1, "to": 6}},
    "variables": [{"name": "x", "type": "integer", "index": ["I"], "lower": 0, "upper": 10}],
    "constraints": [{"name": "floor", "forall": "i in I", "expr": "x[i] >= 3"},
                    {"name": "budget", "expr": "x[1] + x[2] <= 4"},
                    {"name": "cap", "forall": "i in I", "expr": "x[i] <= 9"},
                    {"expr": "x[5] + x[6] >= 7"}],
    "solve": {"method": "minimize", "objective": "sum(i in I) x[i]"}}"#;

fn floors_and_a_budget() -> Model {
    Model::from_json(FLOORS_AND_A_BUDGET).unwrap()
}

#[test]
fn a_conflict_among_several_constraints_takes_a_search() {
    let m = floors_and_a_budget();
    // Nothing can be read off the model.
    let plain = solve(&m);
    assert_eq!((plain.status(), plain.conflict()), (Status::Infeasible, None));
    // The search finds the three that clash, with any engine.
    for engine in [optimizam::Engine::Auto, optimizam::Engine::Milp, optimizam::Engine::Cp] {
        let options = Options::default().engine(engine);
        let explained = m.solve(&options.clone().explain(true)).unwrap();
        let conflict = explained.conflict().unwrap();
        assert_eq!(conflict.constraints(), ["floor[1]", "floor[2]", "budget"], "{engine:?}");
        assert!(conflict.is_minimal() && conflict.variables().is_empty());
        assert_eq!(m.conflict(&options).unwrap().as_ref(), Some(conflict));
        // The ids are the model's: without any one of them the rest can hold.
        for &id in conflict.constraint_ids() {
            let mut relaxed = m.clone();
            relaxed.remove(id);
            assert_eq!(relaxed.solve(&options).unwrap().status(), Status::Complete);
            assert_eq!(relaxed.conflict(&options).unwrap(), None);
        }
    }
}

#[test]
fn a_feasible_model_has_no_conflict() {
    let mut m = floors_and_a_budget();
    m.remove(m.constraint_named("budget").unwrap());
    assert_eq!(m.conflict(&Options::default()).unwrap(), None);
    let result = m.solve(&Options::default().explain(true)).unwrap();
    assert_eq!((result.status(), result.conflict()), (Status::Complete, None));
}

/// With no time to try anything, there is nothing to report (the whole
/// model would be a conflict, and says nothing).
#[test]
fn a_search_with_no_time_reports_nothing_minimal() {
    let m = floors_and_a_budget();
    let conflict = m.conflict(&Options::default().deadline(Instant::now())).unwrap();
    assert!(conflict.is_none_or(|c| !c.is_minimal()));
}

/// Random small integer models, most of them infeasible: the conflict has
/// no solution, and (being minimal) has one without any one of its members.
#[test]
fn conflicts_are_irreducible() {
    const VARS: usize = 4;
    const VALUES: i64 = 3; // each variable in 0..3
    let mut rng = Lcg(2026);
    let (mut infeasible, mut with_several) = (0, 0);
    for case in 0..60 {
        // Rows `coefs . x <= rhs` with coefficients in -2..=2.
        let rows: Vec<(Vec<i64>, i64)> =
            (0..7).map(|_| ((0..VARS).map(|_| rng.below(5) as i64 - 2).collect(), rng.below(5) as i64 - 3)).collect();
        let holds = |subset: &[usize]| {
            (0..VALUES.pow(VARS as u32)).any(|code| {
                let x: Vec<i64> = (0..VARS).map(|j| code / VALUES.pow(j as u32) % VALUES).collect();
                subset.iter().all(|&r| rows[r].0.iter().zip(&x).map(|(a, v)| a * v).sum::<i64>() <= rows[r].1)
            })
        };
        let mut m = Model::new();
        let x = m.add_vars(VARS, var().integer().bounds(0, VALUES - 1).name("x"));
        let mut ids = Vec::new();
        for (coefs, rhs) in &rows {
            let lhs = sum(coefs.iter().zip(&x).map(|(&a, &v)| v * a));
            ids.push(m.add(constraint!(lhs <= *rhs)));
        }
        let all: Vec<usize> = (0..rows.len()).collect();
        let found = m.solve(&Options::default().explain(true)).unwrap();
        if holds(&all) {
            assert_eq!((found.status(), found.conflict()), (Status::Complete, None), "case {case}");
            continue;
        }
        infeasible += 1;
        assert_eq!(found.status(), Status::Infeasible, "case {case}");
        let conflict = found.conflict().unwrap_or_else(|| panic!("case {case}: no conflict"));
        // Unnamed constraints are called by their place: constraints[0], ...
        let place = |name: &String| name.strip_prefix("constraints[")?.strip_suffix(']')?.parse::<usize>().ok();
        let members: Vec<usize> = conflict.constraints().iter().map(|name| place(name).unwrap()).collect();
        assert!(members.iter().zip(conflict.constraint_ids()).all(|(&k, &id)| ids[k] == id));
        assert!(!members.is_empty() && !holds(&members), "case {case}: {members:?} can hold");
        with_several += usize::from(members.len() > 1);
        if conflict.is_minimal() {
            for skip in 0..members.len() {
                let rest: Vec<usize> = members.iter().copied().filter(|&r| r != members[skip]).collect();
                assert!(holds(&rest), "case {case}: {members:?} without {} still can't hold", members[skip]);
            }
        } else {
            // Only when several constraints can each not hold alone.
            assert!(members.iter().all(|&r| !holds(&[r])), "case {case}: {members:?} not minimal");
        }
    }
    assert!(infeasible >= 20 && with_several >= 10, "{infeasible} infeasible, {with_several} with several members");
}

#[test]
fn an_unbounded_objective_names_its_free_variables() {
    let mut m = Model::new();
    let profit = m.add_var(var().name("profit"));
    let units = m.add_family("units", &[&optimizam::Set::new("PLANTS", ["north", "south"])], var().min(0.0));
    let waste = m.add_var(var().bounds(0.0, 5.0).name("waste"));
    m.add(constraint!(profit - 3 * units["north"] - 2 * units["south"] + waste == 0));
    // Up along profit and units[south], down along nothing: waste is bounded.
    m.maximize(profit + units["south"] - waste);
    let result = solve(&m);
    assert_eq!(result.status(), Status::Unbounded);
    assert_eq!(result.unbounded_variables(), ["profit", "units[south]"]);
    assert!(result.to_json().contains(r#""unbounded": { "variables": ["profit", "units[south]"] }"#));
    // Bound them, and there is an optimum and nothing to name.
    m.set_bounds(profit, -100, 100);
    m.set_bounds(units["south"], 0, 10);
    let bounded = solve(&m);
    assert_eq!(bounded.status(), Status::Complete);
    assert!(bounded.unbounded_variables().is_empty() && bounded.conflict().is_none());
}

/// `--explain` puts the conflict in the result file.
#[cfg(feature = "cli")]
#[test]
fn the_command_explains() {
    let dir = std::env::temp_dir().join(format!("optimizam-explain-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let problem = dir.join("problem.json");
    std::fs::write(&problem, FLOORS_AND_A_BUDGET).unwrap();
    let run = |args: &[&str]| {
        let out =
            std::process::Command::new(env!("CARGO_BIN_EXE_optimizam")).args(args).arg(&problem).arg("-").output();
        let out = out.unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let plain = run(&[]);
    assert_eq!((&plain["status"], plain.get("conflict")), (&serde_json::json!("infeasible"), None));
    let explained = run(&["--explain"]);
    assert_eq!(explained["status"], "infeasible");
    assert_eq!(explained["conflict"]["constraints"], serde_json::json!(["floor[1]", "floor[2]", "budget"]));
    assert_eq!(explained["conflict"]["minimal"], true);
    std::fs::remove_dir_all(&dir).unwrap();
}
