//! `Options::constraints`: the slack each solution leaves in the model's
//! constraints, and a linear program's dual values and reduced costs.

mod common;

use common::Lcg;
use optimizam::{ConstraintId, Expression, Model, Options, SolveResult, Status, Variable, constraint, var};
use std::path::Path;

fn report(m: &Model) -> SolveResult {
    m.solve(&Options::default().constraints(true)).unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * (1.0 + a.abs().max(b.abs()))
}

/// Maximize 3x + 5y with x <= 4, 2y <= 12, 3x + 2y <= 18: the optimum is
/// (2, 6) with value 36, and the second and third constraints bind.
fn wyndor() -> (Model, [Variable; 2], [ConstraintId; 3]) {
    let mut m = Model::new();
    let x = m.add_var(var().min(0.0).name("doors"));
    let y = m.add_var(var().min(0.0).name("windows"));
    let ids = [
        m.add(constraint!(x <= 4).name("plant1")),
        m.add(constraint!(2 * y <= 12).name("plant2")),
        m.add(constraint!(3 * x + 2 * y <= 18).name("plant3")),
    ];
    m.maximize(3 * x + 5 * y);
    (m, [x, y], ids)
}

#[test]
fn a_linear_program_reports_slacks_duals_and_reduced_costs() {
    let (m, [x, y], [plant1, plant2, plant3]) = wyndor();
    let result = report(&m);
    assert_eq!((result.status(), result.objective().map(f64::round)), (Status::Complete, Some(36.0)));
    let best = result.best().unwrap();
    assert!(close(best[x], 2.0) && close(best[y], 6.0));
    // An hour more at plant 2 is worth 1.5, at plant 3 it is worth 1; plant 1 has two to spare.
    let slacks: Vec<f64> = [plant1, plant2, plant3].iter().map(|&c| best.slack(c).unwrap()).collect();
    let duals: Vec<f64> = [plant1, plant2, plant3].iter().map(|&c| result.dual(c).unwrap()).collect();
    for (got, want) in slacks.iter().zip([2.0, 0.0, 0.0]).chain(duals.iter().zip([0.0, 1.5, 1.0])) {
        assert!(close(*got, want), "slacks {slacks:?}, duals {duals:?}");
    }
    // Both variables are strictly inside their bounds.
    assert!(close(result.reduced_cost(x).unwrap(), 0.0) && close(result.reduced_cost(y).unwrap(), 0.0));

    let json: serde_json::Value = serde_json::from_str(&result.to_json()).unwrap();
    let solution = &json["solutions"][0];
    assert!(close(solution["constraints"]["plant2"]["dual"].as_f64().unwrap(), 1.5));
    assert!(close(solution["constraints"]["plant1"]["slack"].as_f64().unwrap(), 2.0));
    assert!(solution["reduced_costs"]["doors"].as_f64().is_some());
}

#[test]
fn nothing_is_reported_unless_asked() {
    let (m, [x, _], [plant1, ..]) = wyndor();
    let result = m.solve(&Options::default()).unwrap();
    assert_eq!((result.best().unwrap().slack(plant1), result.dual(plant1), result.reduced_cost(x)), (None, None, None));
    let json: serde_json::Value = serde_json::from_str(&result.to_json()).unwrap();
    assert!(json["solutions"][0].get("constraints").is_none() && json["solutions"][0].get("reduced_costs").is_none());
}

/// A variable at a bound has a reduced cost: what moving the bound is worth.
#[test]
fn reduced_costs_price_the_bounds() {
    let mut m = Model::new();
    let cheap = m.add_var(var().bounds(0.0, 10.0).name("cheap"));
    let dear = m.add_var(var().bounds(0.0, 100.0).name("dear"));
    let need = m.add(constraint!(cheap + dear >= 25).name("need"));
    m.minimize(2 * cheap + 5 * dear);
    let result = report(&m);
    assert!(close(result.objective().unwrap(), 2.0 * 10.0 + 5.0 * 15.0));
    // One more unit needed costs 5 (it comes from the dear source); one more
    // unit of the cheap source's limit saves 3.
    assert!(close(result.dual(need).unwrap(), 5.0));
    assert!(close(result.reduced_cost(cheap).unwrap(), -3.0) && close(result.reduced_cost(dear).unwrap(), 0.0));
    assert!(close(result.best().unwrap().slack(need).unwrap(), 0.0));
}

/// Random small linear programs: each dual is a slope of the optimal
/// objective as a function of the constraint's right side (between its
/// slopes on the two sides, where they differ), and a constraint with slack
/// has dual zero.
#[test]
fn duals_are_the_slopes_of_the_optimum() {
    let mut rng = Lcg(31);
    let mut checked = 0;
    for case in 0..40 {
        let (n, rows) = (3 + rng.below(3) as usize, 3 + rng.below(3) as usize);
        let maximize = case % 2 == 0;
        let costs: Vec<f64> = (0..n).map(|_| 1.0 + rng.below(9) as f64).collect();
        let coefs: Vec<Vec<f64>> = (0..rows).map(|_| (0..n).map(|_| rng.below(6) as f64).collect()).collect();
        let rhs: Vec<f64> = (0..rows).map(|_| 10.0 + rng.below(30) as f64).collect();
        // Maximize value within capacities, or minimize cost meeting requirements.
        let build = |shift: Option<(usize, f64)>| {
            let mut m = Model::new();
            let x = m.add_vars(n, var().bounds(0.0, 20.0).name("x"));
            let mut ids = Vec::new();
            for (i, row) in coefs.iter().enumerate() {
                let lhs: Expression = row.iter().zip(&x).map(|(&a, &v)| a * v).sum();
                let bound = rhs[i] + shift.filter(|s| s.0 == i).map_or(0.0, |s| s.1);
                ids.push(m.add(if maximize { lhs.leq(bound) } else { lhs.geq(bound) }));
            }
            let objective: Expression = costs.iter().zip(&x).map(|(&c, &v)| c * v).sum();
            if maximize {
                m.maximize(objective)
            } else {
                m.minimize(objective)
            }
            (m, ids)
        };
        let (m, ids) = build(None);
        let result = report(&m);
        if result.status() != Status::Complete {
            continue; // the requirements can't be met within the bounds
        }
        let best = result.best().unwrap();
        for (i, &id) in ids.iter().enumerate() {
            let dual = result.dual(id).unwrap();
            let slack = best.slack(id).unwrap();
            assert!(slack >= -1e-7, "case {case}, row {i}: slack {slack}");
            if slack > 1e-6 {
                assert!(dual.abs() < 1e-7, "case {case}, row {i}: slack {slack} and dual {dual}");
            }
            // More capacity can't hurt a maximum; more required can't help a minimum.
            assert!(dual >= -1e-7, "case {case}, row {i}: dual {dual}");
            let step = 1e-3;
            let value = |delta: f64| build(Some((i, delta))).0.solve(&Options::default()).unwrap().objective();
            let (Some(below), Some(above)) = (value(-step), value(step)) else { continue };
            let here = result.objective().unwrap();
            let (left, right) = ((here - below) / step, (above - here) / step);
            let (low, high) = (left.min(right) - 1e-4, left.max(right) + 1e-4);
            assert!(
                (low..=high).contains(&dual),
                "case {case}, row {i}: dual {dual} outside the slopes {left}, {right}"
            );
            checked += 1;
        }
    }
    assert!(checked > 60, "{checked} duals checked");
}

/// With an integer variable there are slacks but no duals.
#[test]
fn integer_programs_report_slacks_only() {
    let mut m = Model::new();
    let x = m.add_var(var().integer().bounds(0, 10).name("x"));
    let y = m.add_var(var().bounds(0.0, 10.0).name("y"));
    let cap = m.add(constraint!(2 * x + y <= 9.5).name("cap"));
    let floor = m.add(constraint!(y >= 1).name("floor"));
    let range = m.add((x + y).between(2.0, 20.0).name("range"));
    let different = m.all_different([x]);
    m.maximize(3 * x + y);
    let result = report(&m);
    let best = result.best().unwrap();
    assert!(close(best[x], 4.0) && close(best[y], 1.5));
    assert!(close(best.slack(cap).unwrap(), 0.0) && close(best.slack(floor).unwrap(), 0.5));
    // A range's slack is to its nearer bound: 5.5 is 3.5 above 2.
    assert!(close(best.slack(range).unwrap(), 3.5));
    assert_eq!((best.slack(different), result.dual(cap), result.reduced_cost(x)), (None, None, None));
}

/// A constraint whose `only_if` literal is false in a solution doesn't bind it.
#[test]
fn a_constraint_that_is_not_enforced_has_no_slack() {
    let mut m = Model::new();
    let open = m.add_var(var().binary().name("open"));
    let load = m.add_var(var().integer().bounds(0, 10).name("load"));
    let closed = m.add(constraint!(load <= 0).only_if(!open).name("closed"));
    let limit = m.add(constraint!(load <= 8).only_if(open).name("limit"));
    m.maximize(load - 3 * open);
    let result = report(&m);
    let best = result.best().unwrap();
    assert_eq!((best[open], best[load]), (1.0, 8.0));
    assert_eq!((best.slack(closed), best.slack(limit)), (None, Some(0.0)));
    let json: serde_json::Value = serde_json::from_str(&result.to_json()).unwrap();
    let constraints = &json["solutions"][0]["constraints"];
    assert_eq!(constraints["closed"], serde_json::json!({"enforced": false}));
    assert_eq!(constraints["limit"], serde_json::json!({"slack": 0}));
}

/// Problem files: constraints are named by instance, and every solution of
/// an enumeration has its own slacks.
#[test]
fn problem_files_name_their_constraints() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let lp = Model::from_file(examples.join("production.json")).unwrap();
    let result = lp.solve(&Options::default().constraints(true)).unwrap();
    let json: serde_json::Value = serde_json::from_str(&result.to_json()).unwrap();
    let constraints = json["solutions"][0]["constraints"].as_object().unwrap();
    assert_eq!(constraints.keys().collect::<Vec<_>>(), ["hours[carpentry]", "hours[finishing]"]);
    assert!(close(constraints["hours[finishing]"]["dual"].as_f64().unwrap(), 40.0));
    assert!(json["solutions"][0]["reduced_costs"]["make"]["chairs"].as_f64().is_some());
    // The ids of the file's model give the same numbers.
    let finishing = lp.constraint_named("hours[finishing]").unwrap();
    assert!(close(result.dual(finishing).unwrap(), 40.0));

    let all = Model::from_file(examples.join("knapsack.json")).unwrap();
    let result = all.solve(&Options::default().constraints(true)).unwrap();
    assert!(result.solutions().len() > 1);
    let json: serde_json::Value = serde_json::from_str(&result.to_json()).unwrap();
    for solution in json["solutions"].as_array().unwrap() {
        assert!(solution["constraints"]["capacity"]["slack"].as_f64().is_some());
        assert!(solution["constraints"]["capacity"].get("dual").is_none() && solution.get("reduced_costs").is_none());
    }
}

/// `--constraints` on the command line.
#[cfg(feature = "cli")]
#[test]
fn the_command_reports_constraints() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/production.json");
    let run = |flag: Option<&str>| {
        let out =
            std::process::Command::new(env!("CARGO_BIN_EXE_optimizam")).args(flag).arg(&example).arg("-").output();
        serde_json::from_slice::<serde_json::Value>(&out.unwrap().stdout).unwrap()
    };
    assert!(run(None)["solutions"][0].get("constraints").is_none());
    let reported = run(Some("--constraints"));
    assert!(close(reported["solutions"][0]["constraints"]["hours[finishing]"]["dual"].as_f64().unwrap(), 40.0));
    assert!(close(reported["solutions"][0]["objective"].as_f64().unwrap(), 2200.0));
}
