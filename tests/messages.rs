//! What a mistake in a problem file is told: structural errors name the
//! place by its JSON path (with the name of the variable or constraint it is
//! in), and the usual slips in expressions come with a hint.

use optimizam::{ErrorKind, Problem};
use serde_json::{Value, json};

fn base() -> Value {
    json!({
        "version": "1.3",
        "sets": {"ITEMS": ["gold", "silver", "gem"], "T": {"from": 1, "to": 3}},
        "parameters": {
            "value": {"index": ["ITEMS"], "values": {"gold": 10, "silver": 6, "gem": 8}},
            "weight": {"index": ["ITEMS"], "values": {"gold": 5, "silver": 4, "gem": 3}},
            "cap": 9
        },
        "variables": [
            {"name": "take", "type": "binary", "index": ["ITEMS"]},
            {"name": "y", "type": "integer", "index": ["T"], "lower": 0, "upper": 5},
            {"name": "z", "type": "integer", "lower": 0, "upper": 5}
        ],
        "constraints": [
            {"name": "capacity", "expr": "sum(i in ITEMS) weight[i] * take[i] <= cap"},
            {"forall": "t in T", "expr": "y[t] <= 4"}
        ],
        "solve": {"method": "maximize", "objective": "sum(i in ITEMS) value[i] * take[i]"}
    })
}

/// A change to the base problem.
type Change = Box<dyn FnOnce(&mut Value)>;

/// The error for the base problem after `change`, which must be of `kind`.
fn error(kind: ErrorKind, change: impl FnOnce(&mut Value)) -> String {
    let mut problem = base();
    change(&mut problem);
    let text = problem.to_string();
    let e = match Problem::from_json(&text) {
        Err(e) => e,
        Ok(p) => p.solve(&optimizam::Options::default()).expect_err("the problem should be rejected"),
    };
    assert_eq!(e.kind(), kind, "{e}");
    e.message().to_string()
}

fn structure(change: impl FnOnce(&mut Value)) -> String {
    let message = error(ErrorKind::Parse, change);
    message.strip_prefix("invalid problem file: ").unwrap_or_else(|| panic!("{message}")).to_string()
}

/// Sets constraint `i`'s expression.
fn expr(i: usize, text: &'static str) -> impl FnOnce(&mut Value) {
    move |p| p["constraints"][i]["expr"] = json!(text)
}

#[test]
fn the_base_problem_is_fine() {
    let result = Problem::from_json(&base().to_string()).unwrap().solve(&optimizam::Options::default()).unwrap();
    assert_eq!(result.objective(), Some(18.0));
}

#[test]
fn structural_errors_say_where() {
    let cases: Vec<(Change, &str)> = vec![
        (
            Box::new(|p| p["sets"]["T"] = json!("1..3")),
            r#"sets.T: expected a list of elements or {"from": .., "to": ..}, found "1..3""#,
        ),
        (
            Box::new(|p| p["sets"]["T"] = json!({"range": [1, 3]})),
            r#"sets.T: unknown field "range" (the fields are "from" and "to")"#,
        ),
        (Box::new(|p| p["sets"]["ITEMS"][1] = json!(2.5)), "sets.ITEMS[1]: expected a whole number, found 2.5"),
        (
            Box::new(|p| drop(p["variables"][1].as_object_mut().unwrap().remove("type"))),
            r#"variables[1] (y): missing "type""#,
        ),
        (
            Box::new(|p| p["variables"][1]["type"] = json!("int")),
            r#"variables[1].type (y): expected "continuous", "integer", or "binary", found "int""#,
        ),
        (
            Box::new(|p| p["variables"][1]["domain"] = json!([[1, 3], [7]])),
            "variables[1].domain[1] (y): expected a range [low, high], found a list",
        ),
        (
            Box::new(|p| drop(p["parameters"]["value"].as_object_mut().unwrap().remove("values"))),
            r#"parameters.value: missing "values""#,
        ),
        (
            Box::new(|p| p["parameters"]["cap"] = json!([1, 2])),
            r#"parameters.cap: expected a number or a table {"index": [..], "values": {..}}, found a list"#,
        ),
        (
            Box::new(|p| p["constraints"][0]["expr"] = json!(5)),
            "constraints[0].expr (capacity): expected a string, found 5",
        ),
        (
            Box::new(|p| p["constraints"][1] = json!({"table": {"vars": "[y[1], y[2]]", "tuples": [[1, 2], 3]}})),
            "constraints[1].table.tuples[1]: expected a tuple (a list of elements), found 3",
        ),
        (
            Box::new(|p| p["solve"]["method"] = json!("max")),
            r#"solve.method: expected "satisfy", "minimize", or "maximize", found "max""#,
        ),
        (
            Box::new(|p| p["solve"]["solutions"] = json!(0)),
            r#"solve.solutions: expected "one", "all", or a number of solutions (1 or more), found 0"#,
        ),
        (Box::new(|p| drop(p.as_object_mut().unwrap().remove("solve"))), r#"missing "solve""#),
        (
            Box::new(|p| p["intervals"] = json!([{"name": "job", "duration": 3}])),
            r#"intervals[0] (job): missing "start""#,
        ),
    ];
    for (change, message) in cases {
        assert_eq!(structure(change), message);
    }
    // Not an object at all, and not JSON at all (where a line and column help).
    let e = Problem::from_json("[1, 2]").unwrap_err();
    assert!(e.message().starts_with("invalid problem file: expected an object with \"version\""), "{e}");
    let e = Problem::from_json(r#"{"version": "1.0", "#).unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::Parse, "invalid problem file: EOF while parsing a value at line 1 column 19")
    );
}

#[test]
fn misspelled_fields_are_suggested() {
    let renamed = |p: &mut Value| {
        let variables = p.as_object_mut().unwrap().remove("variables").unwrap();
        p["variabls"] = variables;
    };
    assert_eq!(structure(renamed), r#"unknown field "variabls"; did you mean "variables"?"#);
    let message = structure(|p| p["variables"][2]["domian"] = json!([[0, 3]]));
    assert_eq!(message, r#"variables[2] (z): unknown field "domian"; did you mean "domain"?"#);
    // Nothing close: the fields there are listed.
    let message = structure(|p| p["constraints"][1] = json!({"kind": "all_different", "args": ["[y[t] | t in T]"]}));
    assert!(
        message.starts_with(
            r#"constraints[1]: unknown field "args" (the fields are "name", "description", "forall", "expr","#
        ),
        "{message}"
    );
    let message = structure(|p| p["description"] = json!("a knapsack"));
    assert!(message.starts_with(r#"unknown field "description" (the fields are "$schema", "version","#), "{message}");
}

#[test]
fn numbers_in_quotes_are_pointed_out() {
    let message = structure(|p| p["variables"][1]["lower"] = json!("0"));
    assert_eq!(
        message,
        r#"variables[1].lower (y): expected a number or a name, found "0" (write numbers without quotes)"#
    );
    let message = structure(|p| p["parameters"]["cap"] = json!("9"));
    assert!(message.ends_with(r#"found "9" (write numbers without quotes)"#), "{message}");
}

#[test]
fn unnamed_constraints_are_called_by_their_place() {
    let invalid = |change: fn(&mut Value)| error(ErrorKind::Invalid, change);
    assert_eq!(
        invalid(|p| drop(p["constraints"][1].as_object_mut().unwrap().remove("expr"))),
        "constraints[1] has no kind field such as expr"
    );
    assert_eq!(
        invalid(|p| drop(p["constraints"][0].as_object_mut().unwrap().remove("expr"))),
        "constraint capacity has no kind field such as expr"
    );
    // An instance of an unnamed indexed constraint.
    assert_eq!(error(ErrorKind::Invalid, expr(1, "w[t] <= 3")), "constraints[1][1]: unknown name w");
}

#[test]
fn expression_slips_come_with_hints() {
    let invalid = |change: Change| error(ErrorKind::Invalid, change);
    // A sum takes one term.
    let message = invalid(Box::new(expr(0, "sum(i in ITEMS) weight[i]*take[i] + value[i]*take[i] <= cap")));
    assert!(message.starts_with("constraint capacity: unknown name i: i is bound only inside its sum"), "{message}");
    assert!(message.contains("put parentheses around a longer one"), "{message}");
    let message =
        invalid(Box::new(|p| p["solve"]["objective"] = json!("sum(i in ITEMS) value[i]*take[i] - weight[i]")));
    assert!(message.starts_with("objective: unknown name i: i is bound only inside its sum"), "{message}");
    // A set element without quotes, or in double quotes.
    assert_eq!(
        invalid(Box::new(expr(0, "take[gold] == 1"))),
        "constraint capacity: unknown name gold: gold is an element of ITEMS, so write it in single quotes: 'gold'"
    );
    let message = invalid(Box::new(expr(0, r#"take["gold"] == 1"#)));
    assert!(message.ends_with("string literals use single quotes, as in x['north']"), "{message}");
    // One = for equality.
    let message = invalid(Box::new(expr(1, "y[t] = 2")));
    assert!(
        message.starts_with("constraints[1]: unexpected character '='") && message.ends_with("use == for equality"),
        "{message}"
    );
    // min, max, abs are constraint kinds, and log is ln.
    let message = invalid(Box::new(expr(1, "max(y[1], y[2]) <= 3")));
    assert!(
        message.contains("unknown function max") && message.contains("use the constraint kind of the same name"),
        "{message}"
    );
    let message = invalid(Box::new(expr(1, "log(y[1]) <= 3")));
    assert!(message.contains("use ln for the natural logarithm"), "{message}");
}

#[test]
fn subscripts_outside_their_sets_say_which() {
    let invalid = |change: Change| error(ErrorKind::Invalid, change);
    // A computed subscript runs off the end of its set.
    assert_eq!(
        invalid(Box::new(expr(1, "y[t+1] >= y[t]"))),
        "constraints[1][3]: y[4] is outside the variable's index sets: 4 isn't in T (1 to 3); if the subscript is \
         computed, give the forall or sum a condition, as in \"t in T: t < 3\""
    );
    let message = invalid(Box::new(expr(1, "y[t-1] >= y[t]")));
    assert!(
        message.starts_with("constraints[1][1]: y[0] is outside") && message.ends_with("\"t in T: t > 1\""),
        "{message}"
    );
    // The string '1' isn't the integer 1, and is shown as written.
    assert_eq!(
        invalid(Box::new(expr(1, "y['1'] <= 4"))),
        "constraints[1][1]: y['1'] is outside the variable's index sets: '1' isn't in T (1 to 3)"
    );
    assert_eq!(
        invalid(Box::new(expr(0, "take['bronze'] == 1"))),
        "constraint capacity: take['bronze'] is outside the variable's index sets: 'bronze' isn't in ITEMS"
    );
    // The wrong number of subscripts isn't a range error.
    assert_eq!(
        invalid(Box::new(expr(0, "take['gold', 1] == 1"))),
        "constraint capacity: take['gold',1]: take takes one subscript (ITEMS), not 2"
    );
    assert_eq!(
        invalid(Box::new(expr(1, "z[t] <= 4"))),
        "constraints[1][1]: z[1]: z is a scalar variable and takes no subscript"
    );
    assert_eq!(invalid(Box::new(expr(1, "y <= 4"))), "constraints[1][1]: y: y takes one subscript (T), not 0");
}
