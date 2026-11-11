//! `!=` and strict comparisons between integer-valued sides, in files and in
//! Rust, and literals as constraints.

mod common;

use common::Lcg;
use optimizam::{Constraint, Engine, Expression, Model, Options, Set, Status, constraint, var};
use serde_json::{Value, json};

fn solve(problem: &Value) -> optimizam::SolveResult {
    Model::from_json(&problem.to_string()).unwrap().solve(&Options::default().threads(1)).unwrap()
}

fn error(problem: &Value) -> String {
    match Model::from_json(&problem.to_string()).and_then(|p| p.solve(&Options::default())) {
        Ok(result) => panic!("solved: {}", result.status()),
        Err(e) => e.to_string(),
    }
}

/// Random models of three small integers and a binary under `!=`, `<`, and
/// `>` constraints, some enforced only when a literal holds: every engine
/// finds each solution once, and so does the model read back from its file.
#[test]
fn comparisons_agree_with_enumeration() {
    let mut rng = Lcg(9);
    let mut counts = Vec::new();
    for case in 0..120 {
        let mut m = Model::new();
        let x = m.add_vars(3, var().integer().bounds(0, 3).name("x"));
        let b = m.add_var(var().binary().name("b"));
        // (coefficients, relation, right side, condition)
        let mut rows = Vec::new();
        for _ in 0..2 + rng.below(3) {
            let coefs = [rng.below(5) as i64 - 2, rng.below(5) as i64 - 2, rng.below(3) as i64 - 1];
            let (relation, rhs, condition) = (rng.below(3), rng.below(7) as i64 - 3, rng.below(4));
            let lhs: Expression = (0..3).map(|i| x[i] * coefs[i]).sum();
            let c = match relation {
                0 => constraint!(lhs != rhs),
                1 => lhs.lt(rhs),
                _ => lhs.gt(rhs),
            };
            m.add(match condition {
                0 => c.only_if(b),
                1 => c.only_if(x[2].is(1)),
                2 => c.only_if(!b).only_if(x[0].is_not(2)),
                _ => c,
            });
            rows.push((coefs, relation, rhs, condition));
        }
        m.all_solutions(true);
        let mut expected = 0;
        for p in 0..128_i64 {
            let (point, b) = ([p % 4, p / 4 % 4, p / 16 % 4], p / 64);
            let holds = rows.iter().all(|&(coefs, relation, rhs, condition)| {
                let on = match condition {
                    0 => b == 1,
                    1 => point[2] == 1,
                    2 => b == 0 && point[0] != 2,
                    _ => true,
                };
                let lhs: i64 = (0..3).map(|i| coefs[i] * point[i]).sum();
                !on || match relation {
                    0 => lhs != rhs,
                    1 => lhs < rhs,
                    _ => lhs > rhs,
                }
            });
            expected += usize::from(holds);
        }
        for engine in [Engine::Cp, Engine::Milp] {
            let result = m.solve(&Options::default().threads(1).engine(engine)).unwrap();
            assert_eq!(result.solutions().len(), expected, "case {case}, {engine:?}: {rows:?}");
        }
        let file = m.to_json().unwrap();
        let reread = Model::from_json(&file).unwrap().solve(&Options::default().threads(1)).unwrap();
        assert_eq!(reread.solutions().len(), expected, "case {case}: {file}");
        counts.push(expected);
    }
    assert!(counts.iter().filter(|&&n| n == 0).count() > 3 && counts.iter().filter(|&&n| n > 10).count() > 30);
}

fn coloring(extra: Value) -> Value {
    let mut p = json!({
        "version": "1.3",
        "sets": { "N": { "from": 1, "to": 4 }, "COLORS": ["red", "green", "blue"], "SIZES": ["s", "m"] },
        "parameters": {
            "edge": { "index": ["N", "N"], "default": 0, "values": { "1": { "2": 1, "3": 1 }, "2": { "3": 1 }, "3": { "4": 1 } } },
            "banned": { "index": ["N"], "default": 0, "values": { "1": 3, "4": 9 } }
        },
        "variables": [
            { "name": "color", "type": "integer", "index": ["N"], "domain": "COLORS" },
            { "name": "size", "type": "integer", "domain": "SIZES" }
        ],
        "constraints": [
            { "name": "apart", "forall": "i in N, j in N: edge[i,j] == 1", "expr": "color[i] != color[j]" }
        ],
        "solve": { "method": "satisfy", "solutions": "all" }
    });
    p["constraints"].as_array_mut().unwrap().extend(extra.as_array().unwrap().iter().cloned());
    p
}

/// Named colors: neighbors differ, written as `!=` between two symbolic
/// variables, and a variable differs from an element.
#[test]
fn symbolic_variables_differ_and_equal() {
    // A triangle 1-2-3 (6 ways) and node 4 unlike node 3 (2 ways), with either size.
    assert_eq!(solve(&coloring(json!([]))).solutions().len(), 24);
    let not_red = solve(&coloring(json!([{ "expr": "color[4] != 'red'" }])));
    assert_eq!(not_red.solutions().len(), 2 * (2 * 2 + 4)); // node 3 red: 2 x 2; otherwise 4 x 1
    let same = solve(&coloring(json!([{ "expr": "color[4] == color[1]" }])));
    assert_eq!(same.solutions().len(), 12);
    // An element that comes from the data and isn't a color never matches: nothing to keep apart.
    let data = solve(&coloring(json!([{ "forall": "i in N: banned[i] > 0", "expr": "color[i] != banned[i]" }])));
    assert_eq!(data.solutions().len(), 24);

    let message = error(&coloring(json!([{ "name": "typo", "expr": "color[4] != 'rad'" }])));
    assert!(
        message.contains("'rad' isn't one of the values of color") && message.contains("did you mean 'red'"),
        "{message}"
    );
    let message = error(&coloring(json!([{ "name": "mixed", "expr": "color[1] != size" }])));
    assert!(message.contains("symbolic variable") && message.contains("can't be used in arithmetic"), "{message}");
    let message = error(&coloring(json!([{ "name": "order", "expr": "color[1] < color[2]" }])));
    assert!(message.contains("can't be used in arithmetic"), "{message}");
}

/// Between real numbers the three comparisons mean nothing, and the message
/// says what to write.
#[test]
fn real_sides_are_refused() {
    let real = |expr: &str| {
        json!({
            "version": "1.3",
            "variables": [{ "name": "weight", "type": "continuous", "lower": 0, "upper": 5 }],
            "constraints": [{ "name": "c", "expr": expr }],
            "solve": { "method": "satisfy" }
        })
    };
    let message = error(&real("weight != 2"));
    assert!(message.contains("!= needs integer-valued sides"), "{message}");
    let message = error(&real("weight < 2"));
    assert!(message.contains("a strict comparison (< or >) needs integer-valued sides"), "{message}");
    assert!(message.ends_with("write <= or >="), "{message}");
    // A fractional coefficient on an integer isn't integer-valued either.
    let message = error(&json!({
        "version": "1.3",
        "variables": [{ "name": "n", "type": "integer", "lower": 0, "upper": 9 }],
        "constraints": [{ "name": "half", "expr": "0.5 * n > 1" }],
        "solve": { "method": "satisfy" }
    }));
    assert!(message.contains("needs integer-valued sides"), "{message}");

    let mut m = Model::new();
    let w = m.add_var(var().bounds(0, 5).name("w"));
    m.add(w.lt(2).name("under"));
    let message = m.solve(&Options::default()).unwrap_err().to_string();
    assert!(message.starts_with("constraint under: a strict comparison"), "{message}");
}

/// A literal is a constraint by itself, and `Variable::equals` is gone.
#[test]
fn literals_are_constraints() {
    let machines = Set::new("MACHINES", ["mill", "lathe", "drill"]);
    let mut m = Model::new();
    let machine = m.add_var(var().values(&machines).name("machine"));
    let other = m.add_var(var().values(&machines).name("other"));
    let shift = m.add_var(var().integer().bounds(1, 3).name("shift"));
    let open = m.add_var(var().binary().name("open"));
    m.add(machine.is_not("mill"));
    m.add(constraint!(machine != other));
    m.add(shift.is_not(2));
    m.add(!open);
    m.add(Constraint::from(other.is("drill")).only_if(shift.is(3)).name("late_drill"));
    m.all_solutions(true);
    // machine: lathe or drill; other: one of the two left; shift 1 or 3, and with 3 other is the drill.
    let count = |result: &optimizam::SolveResult| {
        assert_eq!(result.status(), Status::Complete);
        for s in result.solutions() {
            let (machine, other) = (s.element(machine).unwrap(), s.element(other).unwrap());
            assert!(machine.as_str() != Some("mill") && machine != other);
            assert!(s.int(shift) != 2 && !s.is_one(open));
            assert!(s.int(shift) != 3 || other.as_str() == Some("drill"));
        }
        result.solutions().len()
    };
    assert_eq!(count(&m.solve(&Options::default()).unwrap()), 4 + 1);
    assert_eq!(count(&m.solve(&Options::default().engine(Engine::Milp)).unwrap()), 5);
    // The file says the same, with the comparisons as they were written.
    let file = m.to_json().unwrap();
    assert!(file.contains("machine != 'mill'") && file.contains("machine != other"), "{file}");
    assert!(file.contains("shift != 2") && file.contains("\"1.3\""), "{file}");
    assert_eq!(Model::from_json(&file).unwrap().solve(&Options::default()).unwrap().solutions().len(), 5);

    // Outside the variable's set, a literal is a slip, as a constraint too.
    let mut bad = Model::new();
    let machine = bad.add_var(var().values(&machines).name("machine"));
    bad.add(machine.is_not("mil"));
    let message = bad.solve(&Options::default()).unwrap_err().to_string();
    assert!(message.contains("\"mil\" isn't one of the values of machine"), "{message}");
}

/// With `--constraints`, a `!=` says how far apart its sides are beyond the
/// one they must be.
#[test]
fn slack_of_a_difference() {
    let mut m = Model::new();
    let x = m.add_var(var().integer().bounds(0, 9).name("x"));
    let apart = m.add(constraint!(x != 4).name("apart"));
    let below = m.add(x.lt(8).name("below"));
    m.maximize(x);
    let result = m.solve(&Options::default().constraints(true)).unwrap();
    let best = result.best().unwrap();
    assert_eq!(best.int(x), 7);
    assert_eq!((best.slack(apart), best.slack(below)), (Some(2.0), Some(0.0)));
}
