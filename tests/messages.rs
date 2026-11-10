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
        r#"variables[1].lower (y): expected a number or an expression, found "0" (write numbers without quotes)"#
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
    // log is ln, and a function says how it is called.
    let message = invalid(Box::new(expr(1, "log(y[1]) <= 3")));
    assert!(message.contains("use ln for the natural logarithm"), "{message}");
    let message = invalid(Box::new(expr(1, "y[1] <= mod(7)")));
    assert!(message.starts_with("constraints[1]: mod takes 2 arguments, as in mod(a, b), not 1"), "{message}");
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

/// A key of a parameter table that isn't an element of its index set is a
/// misspelling: the element it was meant for would silently take the default.
#[test]
fn parameter_keys_outside_their_sets_are_errors() {
    let invalid = |change: Change| error(ErrorKind::Invalid, change);
    assert_eq!(
        invalid(Box::new(|p| p["parameters"]["value"]["values"]["golf"] = json!(3))),
        "parameter value: the key \"golf\" isn't an element of ITEMS (gold, silver, gem)"
    );
    let pay = |values: Value| -> Change {
        Box::new(move |p| p["parameters"]["pay"] = json!({"index": ["T", "ITEMS"], "values": values, "default": 5}))
    };
    assert_eq!(
        invalid(pay(json!({"1": {"gold": 1}, "7": {"gem": 2}}))),
        "parameter pay: the key \"7\" isn't an element of T (1 to 3)"
    );
    assert_eq!(
        invalid(pay(json!({"1": {"gold": 1, "gems": 2}}))),
        "parameter pay: the key \"gems\" isn't an element of ITEMS (gold, silver, gem)"
    );
    // "01" isn't how 1 is written.
    assert!(invalid(pay(json!({"01": {"gold": 1}}))).contains("the key \"01\" isn't an element of T"));
    assert_eq!(invalid(pay(json!({"1": 4}))), "parameter pay values are not nested like its index");
}

/// A string literal compared with a symbolic variable must be one of its
/// values, and named elements have no order.
#[test]
fn literals_must_be_values_and_names_have_no_order() {
    let invalid = |change: Change| error(ErrorKind::Invalid, change);
    let carrier: fn(&mut Value) = |p| {
        p["sets"]["CARRIERS"] = json!(["road", "rail", "sea"]);
        p["variables"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "carrier", "type": "integer", "domain": "CARRIERS"}));
    };
    assert_eq!(
        invalid(Box::new(move |p| {
            carrier(p);
            p["constraints"][1] = json!({"name": "fee", "expr": "z >= 2", "enforce_if": "carrier == 'raill'"});
        })),
        "constraint fee: 'raill' isn't one of the values of carrier (road, rail, sea); did you mean 'rail'?"
    );
    assert_eq!(
        invalid(Box::new(move |p| {
            carrier(p);
            p["constraints"][1] = json!({"expr": "carrier == 'air'"});
        })),
        "constraints[1]: 'air' isn't one of the values of carrier (road, rail, sea)"
    );
    let message =
        invalid(Box::new(|p| p["constraints"][0]["expr"] = json!("sum(i in ITEMS: i < 'silver') take[i] <= 1")));
    assert!(
        message
            .starts_with("constraint capacity: 'gold' and 'silver' can't be ordered: named set elements have no order"),
        "{message}"
    );
    // ... and says what to write instead.
    assert!(message.ends_with("compare their places in the set instead: pos(i) < pos('silver', ITEMS)"), "{message}");
    // Equality between names is fine.
    let fine = |p: &mut Value| p["constraints"][0]["expr"] = json!("sum(i in ITEMS: i != 'silver') take[i] <= 1");
    let mut problem = base();
    fine(&mut problem);
    assert!(Problem::from_json(&problem.to_string()).unwrap().solve(&optimizam::Options::default()).is_ok());
}

/// The warnings for the base problem after `change`.
fn warnings(change: impl FnOnce(&mut Value)) -> Vec<String> {
    let mut problem = base();
    change(&mut problem);
    Problem::from_json(&problem.to_string()).unwrap().warnings().iter().map(|w| w.to_string()).collect()
}

#[test]
fn a_clean_problem_has_no_warnings() {
    assert_eq!(warnings(|_| {}), ["variable z appears in no constraint and not in the objective"]);
    assert!(warnings(|p| drop(p["variables"].as_array_mut().unwrap().pop())).is_empty());
}

#[test]
fn probable_mistakes_are_warned_about() {
    let unused_z = "variable z appears in no constraint and not in the objective".to_string();
    // A forall whose condition matches nothing.
    assert_eq!(
        warnings(|p| p["constraints"][1]["forall"] = json!("t in T: t > 3")),
        [
            "constraints[1]: its forall (\"t in T: t > 3\") matches nothing, so it adds no constraints".to_string(),
            unused_z.clone()
        ]
    );
    // Indexes that hide other names.
    assert_eq!(
        warnings(|p| p["constraints"][1] = json!({"name": "rows", "forall": "t in T", "expr": "sum(t in T) y[t] <= 9"})),
        [
            "constraint rows: the index t of a sum hides an index of the same name around it".to_string(),
            unused_z.clone()
        ]
    );
    assert_eq!(
        warnings(|p| p["constraints"][1]["expr"] = json!("sum(cap in T) y[cap] <= 9")),
        ["constraints[1]: the index cap of a sum hides the parameter of that name".to_string(), unused_z.clone()]
    );
    assert_eq!(
        warnings(|p| p["constraints"][1] = json!({"all_different": "[y[z] | z in T]"})),
        ["constraints[1]: the index z of a list hides the variable of that name".to_string(), unused_z.clone()]
    );
    // Two constraints with one name.
    assert_eq!(
        warnings(|p| p["constraints"][1]["name"] = json!("capacity")),
        ["2 constraints are named capacity: constraints[0], constraints[1]".to_string(), unused_z.clone()]
    );
    // A parameter nothing uses (a bound counts as a use).
    assert_eq!(
        warnings(|p| p["parameters"]["limit"] = json!(4)),
        ["parameter limit is never used".to_string(), unused_z.clone()]
    );
    assert_eq!(
        warnings(|p| {
            p["parameters"]["limit"] = json!(4);
            p["variables"][2]["upper"] = json!("limit");
        }),
        [unused_z.as_str()]
    );
    // Infeasible before any search.
    assert_eq!(
        warnings(|p| p["variables"][2]["lower"] = json!(9)),
        [
            unused_z.clone(),
            "variable z has no value within its bounds or domain: the problem is infeasible".to_string()
        ]
    );
    assert_eq!(
        warnings(|p| p["constraints"][1] = json!({"name": "much", "expr": "y[1] + z >= 11"})),
        ["constraint much can't hold within its variables' bounds: the problem is infeasible"]
    );
    // A problem with an error has no warnings: solving reports the error.
    assert!(warnings(expr(1, "w[t] <= 3")).is_empty());
}

/// The command prints warnings on stderr, and as events with `--progress`.
#[cfg(feature = "cli")]
#[test]
fn the_command_prints_warnings() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut problem = base();
    problem["constraints"][1]["forall"] = json!("t in T: t > 3");
    let run = |args: &[&str]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_optimizam"))
            .args(args)
            .args(["-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(problem.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["status"], "complete");
        String::from_utf8(out.stderr).unwrap()
    };
    let plain = run(&[]);
    assert_eq!(
        plain.lines().collect::<Vec<_>>(),
        [
            "warning: constraints[1]: its forall (\"t in T: t > 3\") matches nothing, so it adds no constraints",
            "warning: variable z appears in no constraint and not in the objective",
        ]
    );
    // With --progress every line of stderr is a JSON object.
    let events: Vec<Value> = run(&["--progress"]).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let warned: Vec<&Value> = events.iter().filter(|e| e["event"] == "warning").collect();
    assert_eq!(warned.len(), 2);
    assert!(warned[0]["message"].as_str().unwrap().starts_with("constraints[1]: its forall"));
}

/// One reading reports everything that is wrong: the structure's errors,
/// then the checks between sections, then each constraint's, in the file's
/// order, and nothing that only echoes an error already given.
#[test]
fn every_error_comes_in_one_run() {
    let mut problem = base();
    problem["parameters"]["cap"] = json!("9"); // a number in quotes
    problem["variables"][1]["lowr"] = json!(0); // a misspelled field
    problem["variables"][2] = json!({"name": "z", "type": "integer", "domain": "CARRIERS"}); // no such set
    problem["constraints"][0]["expr"] = json!("sum(i in ITEMS) wieght[i] * take[i] <= 9");
    problem["constraints"][1]["expr"] = json!("y[t+1] >= y[t]");
    problem["constraints"].as_array_mut().unwrap().push(json!({"name": "pick", "expr": "take[gold] == 1"}));
    problem["constraints"].as_array_mut().unwrap().push(json!({"name": "fine", "expr": "y[1] >= 0"}));
    problem["constraints"].as_array_mut().unwrap().push(json!({"name": "echo", "expr": "z + cap >= 0"}));
    problem["solve"]["method"] = json!("maximise");
    let error = Problem::from_json(&problem.to_string()).unwrap_err();
    let found: Vec<(Option<&str>, Option<&str>, &str)> =
        error.errors().map(|e| (e.place(), e.name(), e.message())).collect();
    let expected = [
        (
            Some("parameters.cap"),
            None,
            "invalid problem file: parameters.cap: expected a number or a table {\"index\": [..], \"values\": {..}}, \
             found \"9\" (write numbers without quotes)",
        ),
        (
            Some("variables[1]"),
            Some("y"),
            "invalid problem file: variables[1] (y): unknown field \"lowr\" (the fields are \"name\", \"description\", \
             \"type\", \"index\", \"lower\", \"upper\", \"domain\", and \"start\")",
        ),
        (
            Some("solve.method"),
            None,
            "invalid problem file: solve.method: expected \"satisfy\", \"minimize\", or \"maximize\", found \"maximise\"",
        ),
        (Some("variables[2]"), Some("z"), "variable z: domain refers to unknown set CARRIERS"),
        (Some("constraints[0]"), Some("capacity"), "constraint capacity: unknown name wieght; did you mean weight?"),
        (
            Some("constraints[1]"),
            Some("constraints[1]"),
            "constraints[1][3]: y[4] is outside the variable's index sets: 4 isn't in T (1 to 3); if the subscript is \
             computed, give the forall or sum a condition, as in \"t in T: t < 3\"",
        ),
        (
            Some("constraints[2]"),
            Some("pick"),
            "constraint pick: unknown name gold: gold is an element of ITEMS, so write it in single quotes: 'gold'",
        ),
    ];
    // (`echo` uses z and cap, which didn't read: nothing is said about it.)
    assert_eq!(found, expected);
    // The first error is the error; printing it prints them all, one to a line.
    assert_eq!((error.kind(), error.message()), (ErrorKind::Parse, expected[0].2));
    assert_eq!(error.to_string().lines().count(), expected.len());
    assert_eq!(error.omitted(), 0);
}

/// What couldn't be read isn't blamed again where it is used.
#[test]
fn errors_are_not_echoed() {
    // The set T can't be read: y, which is indexed by it, and the constraint
    // over it are not reported too.
    let error = error_of(|p| p["sets"]["T"] = json!("1..3"));
    assert_eq!(
        messages(&error),
        [r#"invalid problem file: sets.T: expected a list of elements or {"from": .., "to": ..}, found "1..3""#]
    );
    // A variable without a type is taken as continuous, and its uses pass.
    let error = error_of(|p| drop(p["variables"][1].as_object_mut().unwrap().remove("type")));
    assert_eq!(messages(&error), [r#"invalid problem file: variables[1] (y): missing "type""#]);
    // Two variables of one name: the second is the error, once.
    let error = error_of(|p| p["variables"][2]["name"] = json!("y"));
    assert_eq!(messages(&error), ["name y is declared more than once"]);
    assert_eq!((error.kind(), error.place(), error.name()), (ErrorKind::Invalid, Some("variables[2]"), Some("y")));
}

/// At most 20 errors are kept; the rest are counted.
#[test]
fn a_flood_of_errors_is_capped() {
    let error = error_of(|p| {
        let constraints = p["constraints"].as_array_mut().unwrap();
        for k in 0..50 {
            constraints.push(json!({"name": format!("c{k}"), "expr": "nosuch >= 1"}));
        }
    });
    assert_eq!((error.errors().count(), error.omitted()), (20, 30));
    assert!(error.to_string().ends_with("\n(and 30 more)"));
}

/// The error for the base problem after `change`: from reading it, or from
/// reading its expressions (which waits until the model is wanted).
fn error_of(change: impl FnOnce(&mut Value)) -> optimizam::Error {
    let mut problem = base();
    change(&mut problem);
    Problem::from_json(&problem.to_string()).and_then(|p| p.to_model()).expect_err("the problem should be rejected")
}

fn messages(error: &optimizam::Error) -> Vec<&str> {
    error.errors().map(|e| e.message()).collect()
}

/// Names that are a slip away from a known one are suggested.
#[test]
fn close_names_are_suggested() {
    let invalid = |change: Change| error(ErrorKind::Invalid, change);
    assert_eq!(
        invalid(Box::new(expr(0, "sum(i in ITEMS) wieght[i] * take[i] <= cap"))),
        "constraint capacity: unknown name wieght; did you mean weight?"
    );
    assert_eq!(
        invalid(Box::new(expr(0, "sum(i in ITEM) weight[i] * take[i] <= cap"))),
        "constraint capacity: unknown set ITEM; did you mean ITEMS?"
    );
    assert_eq!(
        invalid(Box::new(|p| p["variables"][1]["index"] = json!(["TT"]))),
        "variable y is indexed by unknown set TT"
    );
    assert_eq!(
        invalid(Box::new(|p| p["variables"][1]["upper"] = json!("capp"))),
        "variable y: upper: unknown name capp; did you mean cap?"
    );
    // Short names are a slip away from too much: no guess.
    assert_eq!(invalid(Box::new(expr(1, "w[t] <= 3"))), "constraints[1][1]: unknown name w");
}

/// The command prints each error on a line, and as events with `--progress`.
#[cfg(feature = "cli")]
#[test]
fn the_command_prints_every_error() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut problem = base();
    problem["variables"][1]["type"] = json!("int");
    problem["constraints"][0]["expr"] = json!("sum(i in ITEMS) wieght[i] * take[i] <= cap");
    let run = |args: &[&str]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_optimizam"))
            .args(args)
            .args(["-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(problem.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!((out.status.code(), out.stdout.len()), (Some(1), 0));
        String::from_utf8(out.stderr).unwrap()
    };
    assert_eq!(
        run(&[]).lines().collect::<Vec<_>>(),
        [
            r#"error: invalid problem file: variables[1].type (y): expected "continuous", "integer", or "binary", found "int""#,
            "error: constraint capacity: unknown name wieght; did you mean weight?",
        ]
    );
    let events: Vec<Value> = run(&["--progress"]).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event"], "error");
    assert_eq!((&events[0]["place"], &events[0]["name"]), (&json!("variables[1].type"), &json!("y")));
    assert_eq!((&events[1]["place"], &events[1]["name"]), (&json!("constraints[0]"), &json!("capacity")));
    assert!(events[1]["message"].as_str().unwrap().ends_with("did you mean weight?"));
}
