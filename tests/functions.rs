//! Functions in expressions: arithmetic on the model's data, sets, and
//! indexes (`mod`, `div`, `card`, `pos`, ...), and `abs`, `min`, and `max` of
//! expressions with variables.

mod common;

use common::Lcg;
use optimizam::{Engine, Expression, Model, Options, Problem, Status, Variable, constraint, max, min, sum, var};
use serde_json::{Value, json};

fn solve(problem: &Value) -> optimizam::SolveResult {
    Problem::from_json(&problem.to_string()).unwrap().solve(&Options::default().threads(1)).unwrap()
}

fn error(problem: &Value) -> String {
    match Problem::from_json(&problem.to_string()).and_then(|p| p.solve(&Options::default())) {
        Ok(result) => panic!("solved: {}", result.status()),
        Err(e) => e.to_string(),
    }
}

/// A problem whose one constraint fixes z to `expr`, solved for z.
fn value_of(expr: &str) -> Result<f64, String> {
    let problem = json!({
        "version": "1.3",
        "sets": { "DAYS": ["mon", "tue", "wed", "thu", "fri"], "T": { "from": 3, "to": 6 }, "MIX": [7, "x", 2] },
        "parameters": { "n": 5, "w": { "index": ["T"], "values": { "3": 1.5, "4": -2, "5": 0, "6": 9 } } },
        "variables": [{ "name": "z", "type": "continuous" }],
        "constraints": [{ "name": "c", "expr": format!("z == {expr}") }],
        "solve": { "method": "satisfy" }
    });
    let problem = Problem::from_json(&problem.to_string()).map_err(|e| e.to_string())?;
    let result = problem.solve(&Options::default()).map_err(|e| e.to_string())?;
    Ok(result.best().expect("a value").get("z", &[]).unwrap())
}

#[test]
fn arithmetic_on_data_sets_and_indexes() {
    for (expr, value) in [
        ("mod(7, 3)", 1.0),
        ("mod(-1, 5)", 4.0),
        ("mod(7, -3)", -2.0),
        ("mod(6, 3)", 0.0),
        ("div(7, 2)", 3.0),
        ("div(-7, 2)", -4.0),
        ("div(7, -2)", -4.0),
        ("div(-8, 2)", -4.0),
        ("mod(n + 2, n) + 1", 3.0),
        ("floor(2.5) + floor(-2.5)", -1.0),
        ("ceil(2.1) + ceil(-2.9)", 1.0),
        ("abs(-3) + abs(w[4])", 5.0),
        ("min(4, 2, 8) + max(w[3], w[4])", 3.5),
        ("max(t in T) w[t]", 9.0),
        ("min(t in T: t < 6) w[t]", -2.0),
        ("card(DAYS) + card(T)", 9.0),
        ("first(T) + 10 * last(T)", 63.0),
        ("at(T, 2)", 4.0),
        ("at(MIX, 1) + at(MIX, card(MIX))", 9.0),
        ("pos('thu', DAYS)", 4.0),
        ("pos(5, T) + pos(2, MIX)", 6.0),
        ("sum(d in DAYS) pos(d)", 15.0),
        ("sum(t in T) pos(t) * w[t]", 1.5 - 4.0 + 36.0),
        ("sum(d in DAYS: at(DAYS, mod(pos(d), card(DAYS)) + 1) == 'mon') pos(d)", 5.0),
        ("sum(d in DAYS, e in DAYS: pos(e) == pos(d) + 1 and e != last(DAYS)) 1", 3.0),
    ] {
        match value_of(expr) {
            Ok(v) => assert!((v - value).abs() < 1e-9, "{expr}: {v}, not {value}"),
            Err(e) => panic!("{expr}: {e}"),
        }
    }
    for (expr, why) in [
        ("mod(1.5, 2)", "mod takes integers, not 1.5"),
        ("div(3, 0)", "div(3, 0): division by zero"),
        ("at(T, 5)", "at(T, 5): T has elements 1 to 4"),
        ("at(T, 0)", "at(T, 0): T has elements 1 to 4"),
        ("pos(n)", "pos(i) takes an index bound by a forall, a sum, or a list"),
        ("pos('sat', DAYS)", "'sat' isn't in DAYS"),
        ("card(n)", "card takes the name of a set, as in card(S)"),
        ("floor(z)", "floor takes a number, not an expression with variables"),
        ("first(DAYS) + 1", "string \"mon\" used in arithmetic"),
        ("max(t in T: t > 9) w[t]", "a max over no elements has no value"),
        ("abs(first(DAYS))", "cannot take the abs of a string set element"),
        ("pos(1, 2, 3)", "pos takes 1 or 2 arguments, as in pos(i) or pos(e, S), not 3"),
    ] {
        let message = value_of(expr).expect_err(expr);
        assert!(message.contains(why), "{expr}: {message}");
    }
}

/// A function's name is still free for a parameter or a variable: a call has
/// a parenthesis.
#[test]
fn function_names_are_not_reserved() {
    let result = solve(&json!({
        "version": "1.3",
        "sets": { "I": { "from": 1, "to": 3 } },
        "parameters": { "first": 2, "max": { "index": ["I"], "values": { "1": 4, "2": 5, "3": 6 } } },
        "variables": [{ "name": "pos", "type": "integer", "index": ["I"], "lower": 0, "upper": "max" }],
        "constraints": [{ "name": "c", "forall": "i in I", "expr": "pos[i] <= max[i] - first - pos(i) + max(0, first)" }],
        "solve": { "method": "maximize", "objective": "sum(i in I) pos[i]" }
    }));
    assert_eq!(result.objective(), Some(3.0 + 3.0 + 3.0));
}

/// Points off a line: the fit with the least total distance, and the one
/// with the least greatest distance, are linear programs although nothing
/// has bounds.
#[test]
fn absolute_values_to_minimize_are_a_linear_program() {
    let points = [(0.0, 1.0), (1.0, 3.1), (2.0, 4.9), (3.0, 7.0), (4.0, 20.0)];
    let mut m = Model::new();
    let (a, b) = (m.add_var(var().name("a")), m.add_var(var().name("b")));
    let off = |&(x, y): &(f64, f64)| (a * x + b - y).abs();
    m.minimize(sum(points.iter().map(off)));
    let total = m.solve(&Options::default()).unwrap();
    assert_eq!((total.status(), total.engine()), (Status::Complete, Some(Engine::Milp)));
    assert!((total.objective().unwrap() - 11.2).abs() < 1e-7, "{:?}", total.objective());
    assert_eq!(total.nodes(), Some(1));

    m.minimize(max(points.iter().map(off)));
    let worst = m.solve(&Options::default()).unwrap();
    // By hand: a variable above every distance.
    let mut by_hand = Model::new();
    let (a, b, t) = (by_hand.add_var(var()), by_hand.add_var(var()), by_hand.add_var(var()));
    for &(x, y) in &points {
        by_hand.add(constraint!(t >= a * x + b - y));
        by_hand.add(constraint!(t >= y - a * x - b));
    }
    by_hand.minimize(t);
    let expected = by_hand.solve(&Options::default()).unwrap().objective().unwrap();
    assert_eq!(worst.status(), Status::Complete);
    assert!((worst.objective().unwrap() - expected).abs() < 1e-7, "{:?} vs {expected}", worst.objective());
    // The same from a file, with duals: it is a linear program there too.
    let from_file = Problem::from_json(&m.to_json().unwrap()).unwrap();
    let result = from_file.solve(&Options::default().constraints(true)).unwrap();
    assert!((result.objective().unwrap() - expected).abs() < 1e-7);
}

/// The terms of one random expression, as the model's and as numbers.
struct Terms {
    coefs: [i32; 4],
    shape: u64,
}

impl Terms {
    fn new(rng: &mut Lcg) -> Terms {
        let mut coef = || rng.below(7) as i32 - 3;
        Terms { coefs: [coef(), coef(), coef(), coef()], shape: rng.below(4) }
    }

    fn expression(&self, x: &[Variable]) -> Expression {
        let [a, b, c, d] = self.coefs;
        let inner = match self.shape {
            0 => (x[0] - x[1]).abs(),
            1 => max([x[0] - 1, x[2] + 0, Expression::from(1)]),
            2 => min([x[1] + x[2], 3 - x[0]]),
            _ => max([(x[0] - 2).abs(), min([x[1], x[2]])]),
        };
        a * (x[0] - x[2]).abs()
            + b * max([x[0] + 0, x[1] * 2 - 1])
            + c * min([x[1] + 0, x[2] + 1, 2 - x[0]])
            + d * inner
    }

    fn value(&self, x: &[i64]) -> i64 {
        let [a, b, c, d] = self.coefs.map(i64::from);
        let inner = match self.shape {
            0 => (x[0] - x[1]).abs(),
            1 => (x[0] - 1).max(x[2]).max(1),
            2 => (x[1] + x[2]).min(3 - x[0]),
            _ => (x[0] - 2).abs().max(x[1].min(x[2])),
        };
        a * (x[0] - x[2]).abs() + b * x[0].max(2 * x[1] - 1) + c * x[1].min(x[2] + 1).min(2 - x[0]) + d * inner
    }
}

/// Random models of three small integers with `abs`, `min`, and `max` terms
/// of either sign in a constraint (of any sense) and in the objective, by
/// each engine, against enumeration: the optimum, and the number of
/// solutions.
#[test]
fn piecewise_terms_agree_with_enumeration() {
    let mut rng = Lcg(2026);
    let (mut feasible, mut infeasible) = (0, 0);
    for case in 0..150 {
        let (row, objective) = (Terms::new(&mut rng), Terms::new(&mut rng));
        let (sense, rhs, maximize) = (rng.below(4), rng.below(9) as i32 - 3, rng.below(2) == 1);
        let mut m = Model::new();
        let x = m.add_vars(3, var().integer().bounds(0, 3).name("x"));
        let lhs = row.expression(&x);
        m.add(match sense {
            0 => constraint!(lhs <= rhs),
            1 => constraint!(lhs >= rhs),
            2 => constraint!(lhs == rhs),
            _ => lhs.between(rhs - 1, rhs + 2),
        });
        let holds = |x: &[i64]| {
            let (v, rhs) = (row.value(x), i64::from(rhs));
            match sense {
                0 => v <= rhs,
                1 => v >= rhs,
                2 => v == rhs,
                _ => (rhs - 1..=rhs + 2).contains(&v),
            }
        };
        let mut points = Vec::new();
        for p in 0..64_i64 {
            let point = [p % 4, p / 4 % 4, p / 16];
            if holds(&point) {
                points.push(point);
            }
        }
        let values = points.iter().map(|p| objective.value(p));
        let best = if maximize { values.max() } else { values.min() };
        if maximize {
            m.maximize(objective.expression(&x));
        } else {
            m.minimize(objective.expression(&x));
        }
        for engine in [Engine::Auto, Engine::Milp, Engine::Cp] {
            let result = m.solve(&Options::default().threads(1).engine(engine)).unwrap();
            let what = format!("case {case}, {engine:?}");
            match best {
                None => assert_eq!(result.status(), Status::Infeasible, "{what}"),
                Some(best) => {
                    assert_eq!(result.status(), Status::Complete, "{what}");
                    assert!(
                        (result.objective().unwrap() - best as f64).abs() < 1e-6,
                        "{what}: {:?}",
                        result.objective()
                    );
                    // The solution's own value of the expression is the objective.
                    let s = result.best().unwrap();
                    let point: Vec<i64> = x.iter().map(|&v| s.int(v)).collect();
                    assert!(holds(&point), "{what}: {point:?}");
                    assert_eq!(objective.value(&point), best, "{what}: {point:?}");
                }
            }
        }
        // Every solution, once each.
        let mut all = Model::new();
        let x = all.add_vars(3, var().integer().bounds(0, 3).name("x"));
        let lhs = row.expression(&x);
        all.add(match sense {
            0 => constraint!(lhs <= rhs),
            1 => constraint!(lhs >= rhs),
            2 => constraint!(lhs == rhs),
            _ => lhs.between(rhs - 1, rhs + 2),
        });
        all.all_solutions(true);
        let result = all.solve(&Options::default().threads(1)).unwrap();
        assert_eq!(result.solutions().len(), points.len(), "case {case}");
        if points.is_empty() {
            infeasible += 1;
        } else {
            feasible += 1;
        }
        // And through a file.
        let exported = m.to_json().unwrap();
        let reread = Problem::from_json(&exported).unwrap().solve(&Options::default().threads(1)).unwrap();
        assert_eq!(reread.objective().map(|v| v.round() as i64), best, "case {case}: {exported}");
    }
    assert!(feasible > 50 && infeasible > 5, "{feasible} feasible, {infeasible} infeasible");
}

/// Where a term has to be exact, the variables inside it need bounds, and
/// the message says so; where rows are enough they don't.
#[test]
fn exact_terms_need_bounds() {
    let problem = |expr: &str| {
        json!({
            "version": "1.3",
            "variables": [{ "name": "x", "type": "continuous" }, { "name": "y", "type": "continuous", "lower": 0, "upper": 4 }],
            "constraints": [{ "name": "far", "expr": expr }],
            "solve": { "method": "minimize", "objective": "y" }
        })
    };
    let message = error(&problem("abs(x - y) >= 2"));
    assert!(
        message.starts_with("constraint far: this abs needs finite bounds on the variables inside it"),
        "{message}"
    );
    let message = error(&problem("y >= min(x, 3)"));
    assert!(
        message.starts_with("constraint far: this min needs finite bounds on the variables inside it"),
        "{message}"
    );
    for fine in ["abs(x - y) <= 2", "y >= max(x, 1) - 3", "min(x, y) >= 1", "y - 2 * abs(x) >= -8"] {
        assert_eq!(solve(&problem(fine)).status(), Status::Complete, "{fine}");
    }
    // A product of terms is a nonlinear model, and the terms are exact in it.
    let result = solve(&json!({
        "version": "1.3",
        "variables": [{ "name": "x", "type": "continuous", "lower": -3, "upper": 2 }],
        "constraints": [],
        "solve": { "method": "maximize", "objective": "abs(x) * (4 - abs(x))" }
    }));
    assert_eq!(result.status(), Status::Complete);
    assert!((result.objective().unwrap() - 4.0).abs() < 1e-6, "{:?}", result.objective());
}

/// A makespan written in the objective, with the constraint kind for
/// comparison; lists of the other kinds take the terms too.
#[test]
fn terms_inline_and_as_constraint_kinds() {
    let problem = |objective: &str, extra: Value| {
        let mut p = json!({
            "version": "1.3",
            "sets": { "J": { "from": 1, "to": 4 } },
            "parameters": { "len": { "index": ["J"], "values": { "1": 3, "2": 5, "3": 2, "4": 4 } } },
            "variables": [
                { "name": "start", "type": "integer", "index": ["J"], "lower": 0, "upper": 20 },
                { "name": "makespan", "type": "integer", "lower": 0, "upper": 40 }
            ],
            "intervals": [{ "name": "job", "index": ["J"], "start": "start", "duration": "len" }],
            "constraints": [{ "name": "one_machine", "no_overlap": "[job[j] | j in J: j <= 2]" },
                            { "name": "other_machine", "no_overlap": "[job[j] | j in J: j >= 3]" }],
            "solve": { "method": "minimize", "objective": objective }
        });
        p["constraints"].as_array_mut().unwrap().extend(extra.as_array().unwrap().iter().cloned());
        p
    };
    let inline = solve(&problem("max(j in J) (start[j] + len[j])", json!([])));
    let kind = solve(&problem(
        "makespan",
        json!([{ "name": "last", "max": { "value": "makespan", "of": "[start[j] + len[j] | j in J]" } }]),
    ));
    assert_eq!((inline.status(), inline.objective()), (Status::Complete, Some(8.0)));
    assert_eq!(kind.objective(), Some(8.0));
    // In a list: the distances from 10 all differ.
    let apart = solve(&problem(
        "sum(j in J) start[j]",
        json!([{ "name": "apart", "all_different": "[abs(start[j] - 10) | j in J]" }]),
    ));
    assert_eq!(apart.status(), Status::Complete);
    let s = apart.best().unwrap();
    let mut distances: Vec<i64> =
        (1..=4).map(|j| (s.get("start", &[&j.to_string()]).unwrap() as i64 - 10).abs()).collect();
    distances.sort_unstable();
    distances.dedup();
    assert_eq!(distances.len(), 4);
}
