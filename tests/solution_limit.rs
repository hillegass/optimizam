//! `Model::solution_limit` and `"solutions": N`: up to N solutions (optimal
//! ones, with an objective), `complete` if those are all there are and
//! `incomplete` if the search stopped at the Nth.

use optimizam::{Engine, ErrorKind, Expression, Model, Options, Problem, Solution, Status, StopReason, Variable};
use optimizam::{constraint, var};
use std::collections::HashSet;

/// n queens, one per column: `row[c]` is the row of column c's queen.
fn queens(n: usize) -> (Model, Vec<Variable>) {
    let mut m = Model::new();
    let row = m.add_vars(n, var().integer().bounds(0, n - 1).name("row"));
    m.all_different(&row);
    m.all_different((0..n).map(|c| row[c] + c));
    m.all_different((0..n).map(|c| row[c] - c));
    (m, row)
}

fn is_queens_solution(s: &Solution, row: &[Variable]) -> bool {
    let rows: Vec<i64> = row.iter().map(|&v| s[v] as i64).collect();
    let n = rows.len();
    (0..n).all(|a| (a + 1..n).all(|b| rows[a] != rows[b] && (rows[a] - rows[b]).abs() != (b - a) as i64))
}

/// Six queens have four solutions.
#[test]
fn a_limit_stops_an_enumeration() {
    for engine in [Engine::Auto, Engine::Cp, Engine::Milp] {
        let options = Options::default().engine(engine);
        for (limit, count, status) in [
            (1, 1, Status::Incomplete),
            (2, 2, Status::Incomplete),
            (4, 4, Status::Incomplete), // stopped at the fourth: no proof that there is no fifth
            (5, 4, Status::Complete),
            (100, 4, Status::Complete),
        ] {
            let (mut m, row) = queens(6);
            m.solution_limit(limit);
            let result = m.solve(&options).unwrap();
            assert_eq!((result.status(), result.solutions().len()), (status, count), "{engine:?}, limit {limit}");
            let reason = (status == Status::Incomplete).then_some(StopReason::SolutionLimit);
            assert_eq!(result.stop_reason(), reason, "{engine:?}, limit {limit}");
            assert!(result.solutions().iter().all(|s| is_queens_solution(s, &row)));
            let distinct: HashSet<Vec<i64>> =
                result.solutions().iter().map(|s| row.iter().map(|&v| s[v] as i64).collect()).collect();
            assert_eq!(distinct.len(), count, "{engine:?}, limit {limit}: a solution twice");
        }
    }
}

/// Eight items of weights 3, 3, 3, 3, 2, 2, 4, 4 and room for 9: twenty
/// ways to fill it exactly.
fn knapsack() -> (Model, Vec<Variable>, [i32; 8]) {
    let weights = [3, 3, 3, 3, 2, 2, 4, 4];
    let mut m = Model::new();
    let take = m.add_vars(8, var().binary().name("take"));
    let load: Expression = (0..8).map(|i| weights[i] * take[i]).sum();
    m.add(constraint!(load.clone() <= 9));
    m.maximize(load);
    (m, take, weights)
}

#[test]
fn a_limit_counts_optimal_solutions_only() {
    for engine in [Engine::Cp, Engine::Milp] {
        let options = Options::default().engine(engine);
        for (limit, count, status) in
            [(3, 3, Status::Incomplete), (20, 20, Status::Incomplete), (21, 20, Status::Complete)]
        {
            let (mut m, take, weights) = knapsack();
            m.solution_limit(limit);
            let result = m.solve(&options).unwrap();
            assert_eq!((result.status(), result.solutions().len()), (status, count), "{engine:?}, limit {limit}");
            for s in result.solutions() {
                let load: i32 = (0..8).map(|i| weights[i] * s[take[i]] as i32).sum();
                assert_eq!((load, s.objective()), (9, Some(9.0)), "{engine:?}: not an optimal packing");
            }
            let distinct: HashSet<Vec<i64>> =
                result.solutions().iter().map(|s| take.iter().map(|&v| s[v] as i64).collect()).collect();
            assert_eq!(distinct.len(), count);
        }
    }
}

/// Two is enough to tell whether a solution is the only one.
#[test]
fn two_solutions_decide_uniqueness() {
    let mut m = Model::new();
    let x = m.add_vars(4, var().integer().bounds(1, 4).name("x"));
    m.all_different(&x);
    m.add(constraint!(x[0] + x[1] == 3));
    m.add(constraint!(x[2] <= x[3] - 1));
    m.solution_limit(2);
    // (1, 2, 3, 4) and (2, 1, 3, 4).
    let several = m.solve(&Options::default()).unwrap();
    assert_eq!((several.status(), several.solutions().len()), (Status::Incomplete, 2));
    m.add(constraint!(x[0] <= x[1] - 1));
    let unique = m.solve(&Options::default()).unwrap();
    assert_eq!((unique.status(), unique.solutions().len(), unique.stop_reason()), (Status::Complete, 1, None));
    m.add(constraint!(x[3] <= 3));
    assert_eq!(m.solve(&Options::default()).unwrap().status(), Status::Infeasible);
}

#[test]
fn all_solutions_and_a_limit_replace_each_other() {
    let (mut m, _) = queens(6);
    m.solution_limit(2);
    m.all_solutions(true);
    assert_eq!(m.solve(&Options::default()).unwrap().solutions().len(), 4);
    m.solution_limit(3);
    assert_eq!(m.solve(&Options::default()).unwrap().solutions().len(), 3);
    m.all_solutions(false);
    let one = m.solve(&Options::default()).unwrap();
    assert_eq!((one.status(), one.solutions().len()), (Status::Complete, 1));
}

#[test]
fn bad_limits_are_errors() {
    let (mut m, _) = queens(4);
    m.solution_limit(0);
    let zero = m.solve(&Options::default()).unwrap_err();
    assert_eq!(zero.kind(), ErrorKind::Invalid);
    assert!(zero.message().contains("1 or more"), "{zero}");
    // Enumerating needs every variable to be integer.
    let mut m = Model::new();
    let x = m.add_var(var().bounds(0.0, 1.0).name("x"));
    m.add(constraint!(x >= 0.5));
    m.solution_limit(2);
    assert_eq!(m.solve(&Options::default()).unwrap_err().kind(), ErrorKind::Unsupported);
}

/// `"solutions": 2` in a problem file, and through `to_json` and back.
#[test]
fn problem_files_take_a_number() {
    let file = |solutions: &str| {
        format!(
            r#"{{"version": "1.3", "sets": {{"I": {{"from": 1, "to": 4}}}},
                 "variables": [{{"name": "x", "type": "integer", "index": ["I"], "lower": 1, "upper": 4}}],
                 "constraints": [{{"all_different": "[x[i] | i in I]"}},
                                 {{"expr": "x[1] + x[2] == 3"}}, {{"expr": "x[3] <= x[4] - 1"}}],
                 "solve": {{"method": "satisfy", "solutions": {solutions}}}}}"#
        )
    };
    let two = Problem::from_json(&file("2")).unwrap().solve(&Options::default()).unwrap();
    assert_eq!((two.status(), two.solutions().len()), (Status::Incomplete, 2));
    let json: serde_json::Value = serde_json::from_str(&two.to_json()).unwrap();
    assert_eq!((&json["status"], json["solutions"].as_array().unwrap().len()), (&serde_json::json!("incomplete"), 2));
    let three = Problem::from_json(&file("3")).unwrap().solve(&Options::default()).unwrap();
    assert_eq!((three.status(), three.solutions().len()), (Status::Complete, 2));

    // A model keeps its limit when written out.
    let exported = Model::from_json(&file("2")).unwrap().to_json().unwrap();
    let written: serde_json::Value = serde_json::from_str(&exported).unwrap();
    assert_eq!(written["solve"]["solutions"], 2);
    let again = Problem::from_json(&exported).unwrap().solve(&Options::default()).unwrap();
    assert_eq!((again.status(), again.solutions().len()), (Status::Incomplete, 2));

    for bad in ["0", "-1", "2.5", r#""two""#] {
        let e = Problem::from_json(&file(bad)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Parse, "{bad}");
        assert!(e.message().contains(r#""one", "all", or a number"#), "{bad}: {e}");
    }
}
