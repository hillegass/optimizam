//! Bounds, starts, and interval fields as expressions over named indexes;
//! scalar parameters as bounds; `null` for no bound.

use optimizam::{Model, Options, Status};
use serde_json::{Value, json};

fn load(problem: &Value) -> Result<Model, String> {
    Model::from_json(&problem.to_string()).map_err(|e| e.to_string())
}

fn solve(problem: &Value) -> optimizam::SolveResult {
    load(problem).unwrap().solve(&Options::default().threads(1)).unwrap()
}

fn error(problem: &Value) -> String {
    match load(problem).and_then(|p| p.solve(&Options::default()).map_err(|e| e.to_string())) {
        Ok(result) => panic!("solved: {}", result.status()),
        Err(e) => e,
    }
}

fn base() -> Value {
    json!({
        "version": "1.3",
        "sets": { "P": ["a", "b", "c"], "T": { "from": 1, "to": 4 } },
        "parameters": {
            "n": 6,
            "cap": { "index": ["P"], "values": { "a": 5, "b": null, "c": 2 } },
            "len": { "index": ["P"], "values": { "a": 2, "b": 3, "c": 1 } },
            "floor": { "index": ["P", "T"], "default": 1, "values": { "a": { "2": 3 } } }
        },
        "variables": [
            { "name": "make", "type": "integer", "index": ["p in P", "t in T"], "lower": "floor", "upper": "len[p] + t" },
            { "name": "top", "type": "integer", "index": ["P"], "lower": 0, "upper": "cap" },
            { "name": "y", "type": "integer", "index": ["T"], "lower": "-n", "upper": "2 * n" },
            { "name": "begin", "type": "integer", "index": ["p in P"], "lower": "pos(p)", "upper": 20 },
            { "name": "on", "type": "binary", "index": ["P"] }
        ],
        "intervals": [
            { "name": "job", "index": ["p in P"], "start": "begin[p] + 1", "duration": "len[p] * 2", "presence": "on[p]" },
            { "name": "plain", "index": ["P"], "start": "begin", "duration": "len", "presence": "on" }
        ],
        "constraints": [
            { "name": "one", "no_overlap": "[job[p] | p in P]" },
            { "name": "also", "no_overlap": "[plain[p] | p in P]" },
            { "name": "lim", "forall": "p in P", "expr": "top[p] <= 9" },
            { "name": "all_on", "expr": "sum(p in P) on[p] == 3" }
        ],
        "solve": {
            "method": "maximize",
            "objective": "sum(p in P, t in T) make[p,t] + sum(p in P) top[p] + sum(t in T) y[t] - sum(p in P) begin[p]"
        }
    })
}

#[test]
fn bounds_are_worked_out_for_each_element() {
    let problem = base();
    assert!(load(&problem).unwrap().warnings().is_empty());
    let result = solve(&problem);
    assert_eq!(result.status(), Status::Complete);
    let s = result.best().unwrap();
    let value = |name: &str, key: &[&str]| s.get(name, key).unwrap();
    // make[p,t] <= len[p] + t, and a table over the variable's own sets by its name.
    for (p, len) in [("a", 2.0), ("b", 3.0), ("c", 1.0)] {
        for t in 1..=4 {
            assert_eq!(value("make", &[p, &t.to_string()]), len + f64::from(t));
        }
    }
    // cap[b] is null: no bound, so the constraint's 9.
    assert_eq!((value("top", &["a"]), value("top", &["b"]), value("top", &["c"])), (5.0, 9.0, 2.0));
    // A scalar parameter bounds every element.
    assert!((1..=4).all(|t| value("y", &[&t.to_string()]) == 12.0));
    // The intervals start one after begin and last twice len: a [2, 6), c [6, 8), b [8, 14).
    assert_eq!((value("begin", &["a"]), value("begin", &["c"]), value("begin", &["b"])), (1.0, 5.0, 7.0));
    assert_eq!(result.objective(), Some(54.0 + 16.0 + 48.0 - 13.0));

    // The lower bounds: minimize instead.
    let mut least = base();
    least["solve"] = json!({ "method": "minimize", "objective": "sum(p in P, t in T) make[p,t] + sum(t in T) y[t]" });
    let result = solve(&least);
    let s = result.best().unwrap();
    assert_eq!(s.get("make", &["a", "2"]), Some(3.0)); // the table's entry
    assert_eq!(s.get("make", &["b", "2"]), Some(1.0)); // its default
    assert_eq!(result.objective(), Some(11.0 + 3.0 - 24.0));
}

#[test]
fn what_is_wrong_with_a_bound_says_where() {
    let with = |change: &dyn Fn(&mut Value)| {
        let mut problem = base();
        change(&mut problem);
        error(&problem)
    };
    // A null has no value to compute with.
    let message = with(&|p| p["variables"][0]["upper"] = json!("cap[p] + t"));
    assert!(message.starts_with("variable make: upper: parameter cap[b] is null"), "{message}");
    // A table over other sets needs its subscripts.
    let message = with(&|p| p["variables"][0]["upper"] = json!("cap"));
    assert!(
        message.starts_with("variable make: upper: parameter cap is indexed by P, not by this item's sets"),
        "{message}"
    );
    assert!(message.ends_with("and write cap[i, ..]"), "{message}");
    // An index that the list doesn't name.
    let message = with(&|p| p["variables"][1]["upper"] = json!("cap[p]"));
    assert_eq!(message, "variable top: upper: unknown name p");
    // A bound can't depend on a variable, and a number in quotes is still a slip.
    let message = with(&|p| p["variables"][3]["upper"] = json!("on[p] + 3"));
    assert_eq!(message, "variable begin: upper: it must be a number, and on is a variable");
    let message = with(&|p| p["variables"][3]["upper"] = json!("y[1] + 3"));
    assert_eq!(message, "variable begin: upper: it must be a number, not an expression with variables");
    let message = with(&|p| p["variables"][3]["upper"] = json!("20"));
    assert!(
        message.contains("variables[3].upper (begin)") && message.contains("write numbers without quotes"),
        "{message}"
    );
    let message = with(&|p| p["variables"][3]["upper"] = json!("20 +"));
    assert!(message.starts_with("variable begin: upper: expected"), "{message}");
    // Index names: identifiers, each once, over known sets.
    let message = with(&|p| p["variables"][0]["index"] = json!(["p in P", "p in T"]));
    assert!(message.contains("variable make: two indexes are named p"), "{message}");
    let message = with(&|p| p["variables"][0]["index"] = json!(["p in Q", "t in T"]));
    assert!(message.contains("variable make is indexed by unknown set Q"), "{message}");
    // Interval fields.
    let message = with(&|p| p["intervals"][0]["presence"] = json!("begin[p] + 1"));
    assert_eq!(message, "interval job[a]: presence must be a binary variable");
    let message = with(&|p| p["intervals"][0]["duration"] = json!("begin[p] * begin[p]"));
    assert_eq!(message, "interval job[a]: duration: it must be affine");
    let message = with(&|p| p["intervals"][1]["start"] = json!("nothing"));
    assert_eq!(message, "interval plain[a]: start: unknown name nothing");
    // The place comes with the error, for a program.
    let mut problem = base();
    problem["variables"][3]["upper"] = json!("on[p] + 3");
    let e = Model::from_json(&problem.to_string()).and_then(|p| p.solve(&Options::default())).unwrap_err();
    assert_eq!((e.place(), e.name()), (Some("variables[3].upper"), Some("begin")));
}

/// A parameter used only in a bound isn't "never used", and an index name
/// isn't an unknown name.
#[test]
fn bounds_count_as_uses() {
    let problem = json!({
        "version": "1.3",
        "sets": { "T": { "from": 1, "to": 3 } },
        "parameters": { "most": { "index": ["T"], "values": { "1": 4, "2": 5, "3": 6 } }, "spare": 1, "first": 2 },
        "variables": [{ "name": "x", "type": "continuous", "index": ["t in T"], "lower": 0, "upper": "most[t] - first", "start": "t / 2" }],
        "constraints": [],
        "solve": { "method": "maximize", "objective": "sum(t in T) x[t]" }
    });
    assert_eq!(load(&problem).unwrap().warnings(), ["parameter spare is never used"]);
    assert_eq!(solve(&problem).objective(), Some(2.0 + 3.0 + 4.0));
}
