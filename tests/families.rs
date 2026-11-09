//! Index sets and variable families, symbolic variables, lookups, and
//! editing a model: keys, results, exports, and errors.

use optimizam::{
    Element, ErrorKind, Expression, Model, Options, Problem, Set, SolveResult, Status, Variable, constraint, var,
};
use std::path::Path;

fn solve(m: &Model) -> SolveResult {
    m.solve(&Options::default()).unwrap()
}

/// Solves the model's export with the problem-file path and checks that it
/// agrees with the model's own result; writes it to $OPTIMIZAM_EXPORT_DIR
/// (for the schema check in CLAUDE.md) when that is set.
fn round_trip(m: &Model, name: &str) {
    let json = m.to_json().unwrap();
    if let Some(dir) = std::env::var_os("OPTIMIZAM_EXPORT_DIR") {
        std::fs::write(Path::new(&dir).join(format!("families-{name}.json")), &json).unwrap();
    }
    let direct = solve(m);
    let exported = Problem::from_json(&json).unwrap_or_else(|e| panic!("{e}\n{json}")).solve(&Options::default());
    let exported = exported.unwrap_or_else(|e| panic!("{e}\n{json}"));
    assert_eq!(exported.status(), direct.status(), "{json}");
    assert_eq!(exported.objective(), direct.objective(), "{json}");
    assert_eq!(exported.solutions().len(), direct.solutions().len(), "{json}");
}

#[test]
fn keys_find_their_variables() {
    let plants = Set::new("PLANTS", ["north", "south"]);
    let hours = Set::range("HOURS", 1..=3);
    let mut m = Model::new();
    let x = m.add_family("x", &[&plants, &hours], var().binary());
    assert_eq!((x.len(), x.name(), x.sets().len()), (6, "x", 2));
    // Row-major: the last set varies fastest.
    assert_eq!(x[("north", 1)], x.vars()[0]);
    assert_eq!(x[("south", 3)], x.vars()[5]);
    assert_eq!(x[("south", 1i64)], x.vars()[3]);
    let south = String::from("south");
    assert_eq!(x[(&south, 2u32)], x.vars()[4]);
    assert_eq!(x[[Element::from("north"), Element::from(2)]], x.vars()[1]);
    assert_eq!(x[vec![Element::from("north"), Element::from(3)]], x.vars()[2]);
    let slice: &[Element] = &[Element::from("south"), Element::from(2)];
    assert_eq!(x[slice], x.vars()[4]);
    assert_eq!(x.get(("east", 1)), None);
    assert_eq!(x.get(("north", 4)), None);
    assert_eq!(x.get("north"), None, "too few elements");
    assert_eq!(x.get(("north", 1, 1)), None, "too many elements");
    let keys: Vec<String> = x.iter().map(|(k, _)| format!("{}/{}", k[0], k[1])).collect();
    assert_eq!(keys, ["north/1", "north/2", "north/3", "south/1", "south/2", "south/3"]);
    assert_eq!(plants.position("south"), Some(1));
    assert_eq!(hours.position(4), None);
    // One set: plain elements.
    let y = m.add_family("y", &[&plants], var());
    assert_eq!(y["south"], y.vars()[1]);
    assert_eq!(y[("north",)], y.vars()[0]);
}

#[test]
#[should_panic(expected = "no such key in family x")]
fn unknown_keys_panic() {
    let plants = Set::new("PLANTS", ["north", "south"]);
    let mut m = Model::new();
    let x = m.add_family("x", &[&plants], var());
    let _ = x["east"];
}

#[test]
#[should_panic(expected = "appears twice")]
fn sets_reject_repeats() {
    let _ = Set::new("S", [Element::from(3), Element::from("3")]);
}

#[test]
fn results_are_tables_keyed_by_element() {
    let plants = Set::new("PLANTS", ["north", "south"]);
    let hours = Set::range("HOURS", 1..=2);
    let capacity = |p: &str| if p == "north" { 4.0 } else { 7.0 };
    let mut m = Model::new();
    let make =
        m.add_family_with("make", &[&plants, &hours], |key| var().integer().bounds(0.0, capacity(&key[0].to_string())));
    m.maximize(make.vars().iter().copied().sum::<Expression>());
    let r = solve(&m);
    assert_eq!(r.objective(), Some(22.0));
    let best = r.best().unwrap();
    assert_eq!(best[make[("south", 2)]], 7.0);
    assert_eq!(best.get("make", &["north", "1"]), Some(4.0));
    let json: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
    assert_eq!(json["solutions"][0]["variables"]["make"]["south"]["1"], 7);
    // String keys are written as literals, in format 1.3.
    let json = m.to_json().unwrap();
    assert!(json.contains("\"version\": \"1.3\"") && json.contains("make['south', 2]"), "{json}");
    round_trip(&m, "string-keys");
}

#[test]
fn integer_families_export_with_their_sets() {
    let rows = Set::range("ROWS", 1..=3);
    let cols = Set::new("COLS", [10, 20]);
    let mut m = Model::new();
    let x = m.add_family_with("x", &[&rows, &cols], |key| {
        let (Element::Int(r), Element::Int(c)) = (&key[0], &key[1]) else { unreachable!() };
        var().integer().bounds(0, (r * c / 10) as i32).start(1)
    });
    for r in 1..=3 {
        m.add(constraint!(x[(r, 10)] + x[(r, 20)] <= 5).name(format!("row{r}")));
    }
    m.maximize((1..=3).map(|r| 3 * x[(r, 10)] + 2 * x[(r, 20)]).sum::<Expression>());
    let json = m.to_json().unwrap();
    for part in [
        "\"ROWS\": { \"from\": 1, \"to\": 3 }",
        "\"COLS\": [10, 20]",
        "\"_x_upper\"",
        "x[2, 20]",
        "\"version\": \"1.2\"",
    ] {
        assert!(json.contains(part), "{part} missing:\n{json}");
    }
    round_trip(&m, "integer");
}

#[test]
fn symbolic_variables_match_brute_force() {
    // A triangle and a pendant vertex, three colors, every coloring.
    let colors = Set::new("COLORS", ["red", "green", "blue"]);
    let mut m = Model::new();
    let c: Vec<Variable> = (0..4).map(|k| m.add_var(var().values(&colors).name(format!("c{k}")))).collect();
    for (a, b) in [(0, 1), (1, 2), (0, 2), (2, 3)] {
        m.all_different([c[a], c[b]]);
    }
    m.all_solutions(true);
    let r = solve(&m);
    assert_eq!(r.status(), Status::Complete);
    let mut found: Vec<String> = r
        .solutions()
        .iter()
        .map(|s| c.iter().map(|&v| s.element(v).unwrap().to_string()).collect::<Vec<_>>().join(","))
        .collect();
    found.sort();
    let names = ["blue", "green", "red"];
    let mut expected = Vec::new();
    for a in names {
        for b in names {
            for d in names {
                for e in names {
                    if a != b && b != d && a != d && d != e {
                        expected.push(format!("{a},{b},{d},{e}"));
                    }
                }
            }
        }
    }
    expected.sort();
    assert_eq!(found, expected);
    assert!(r.best().unwrap().get_element("c0", &[]).is_some());
    round_trip(&m, "coloring");
}

#[test]
fn literals_compare_with_elements() {
    let machines = Set::new("MACHINES", ["lathe", "mill"]);
    let mut m = Model::new();
    let machine = m.add_var(var().values(&machines).name("machine"));
    let hours = m.add_var(var().integer().bounds(0, 10).name("hours"));
    let size = m.add_var(var().integer().bounds(0, 3).name("size"));
    // The mill takes at least 6 hours, the lathe at least 2; size 3 needs 9.
    m.add(constraint!(hours >= 6).only_if(machine.is("mill")));
    m.add(constraint!(hours >= 2).only_if(machine.is_not("mill")));
    m.add(constraint!(hours >= 9).only_if(size.is(3)));
    m.add(constraint!(size >= 3));
    m.minimize(hours);
    let r = solve(&m);
    assert_eq!(r.objective(), Some(9.0));
    let json = m.to_json().unwrap();
    assert!(json.contains("machine == 'mill'") && json.contains("machine != 'mill'"), "{json}");
    round_trip(&m, "literals");

    let mut lathe_only = Model::new();
    let machine = lathe_only.add_var(var().values(&machines).name("machine"));
    let hours = lathe_only.add_var(var().integer().bounds(0, 10).name("hours"));
    lathe_only.add(constraint!(hours >= 6).only_if(machine.is("mill")));
    lathe_only.add(constraint!(hours <= 3));
    let r = solve(&lathe_only);
    assert_eq!(r.best().unwrap().element(machine), Some(Element::from("lathe")));

    // An element outside the set never matches, as in problem files.
    let mut outside = Model::new();
    let machine = outside.add_var(var().values(&machines).name("machine"));
    let hours = outside.add_var(var().integer().bounds(0, 10).name("hours"));
    outside.add(constraint!(hours <= 2).only_if(machine.is("drill")));
    outside.add(constraint!(hours >= 4).only_if(machine.is_not("drill")));
    outside.maximize(hours);
    assert_eq!(solve(&outside).objective(), Some(10.0));
    outside.minimize(hours);
    assert_eq!(solve(&outside).objective(), Some(4.0));
    round_trip(&outside, "outside");
}

#[test]
fn symbolic_misuse_is_an_error() {
    let colors = Set::new("COLORS", ["red", "green"]);
    let other = Set::new("OTHER", ["red", "green"]);
    let error = |build: &dyn Fn(&mut Model, Variable, Variable)| {
        let mut m = Model::new();
        let a = m.add_var(var().values(&colors).name("a"));
        let n = m.add_var(var().integer().bounds(0, 5).name("n"));
        build(&mut m, a, n);
        let e = m.solve(&Options::default()).unwrap_err();
        (e.kind(), e.message().to_string())
    };
    let (kind, message) = error(&|m, a, _| {
        m.add(constraint!(a + 1 <= 2));
    });
    assert_eq!(kind, ErrorKind::Invalid);
    assert!(message.contains("symbolic variable a can't be used in arithmetic"), "{message}");
    let (_, message) = error(&|m, a, _| m.minimize(a));
    assert!(message.contains("the objective"), "{message}");
    let (_, message) = error(&|m, a, n| {
        m.add(constraint!(n >= 1).only_if(n.is("red")));
        let _ = a;
    });
    assert!(message.contains("compared with a numeric variable (n)"), "{message}");
    // A table over a symbolic variable with numbers: no tuple matches (as in
    // problem files, where an element outside the set never matches).
    let mut m = Model::new();
    let a = m.add_var(var().values(&colors).name("a"));
    m.table(&[a], [[1]]);
    assert_eq!(solve(&m).status(), Status::Infeasible);
    let (_, message) = error(&|m, a, n| {
        m.all_different([a, n]);
    });
    assert!(message.contains("same set"), "{message}");
    let (_, message) = error(&|m, a, _| {
        let b = m.add_var(var().values(&other).name("b"));
        m.all_different([a, b]);
    });
    assert!(message.contains("same set"), "{message}");
    let (_, message) = error(&|m, a, _| m.set_bounds(a, 1, 1));
    assert!(message.contains("symbolic variable's bounds"), "{message}");
}

#[test]
fn every_spec_of_a_family_has_one_kind() {
    let s = Set::range("S", 0..2);
    let mut m = Model::new();
    m.add_family_with("x", &[&s], |k| if k[0] == Element::from(0) { var().integer() } else { var() });
    let e = m.solve(&Options::default()).unwrap_err();
    assert!(e.message().contains("same kind"), "{}", e.message());
}

#[test]
fn lookups_by_name() {
    let s = Set::new("S", ["a", "b"]);
    let mut m = Model::new();
    let x = m.add_family("x", &[&s], var().binary());
    let y = m.add_var(var().integer().name("y"));
    let z = m.add_vars(3, var().name("z"));
    let cap = m.add(constraint!(x["a"] + x["b"] <= 1).name("cap"));
    assert_eq!(m.family("x").unwrap().vars(), x.vars());
    assert_eq!(m.family("x").unwrap()["b"], x["b"]);
    assert_eq!(m.family("y").unwrap()[()], y);
    assert_eq!(m.family("z").unwrap()[2], z[2]);
    assert_eq!(m.var_named("y"), Some(y));
    assert_eq!(m.var_named("x"), None, "x is a family");
    assert_eq!(m.family("w").map(|f| f.len()), None);
    assert_eq!(m.constraint_named("cap"), Some(cap));
    assert_eq!(m.constraint_named("nope"), None);
    m.remove(cap);
    assert_eq!(m.constraint_named("cap"), None);
}

#[test]
fn editing_a_model() {
    let k = Set::range("K", 0..4);
    let mut m = Model::new();
    let x = m.add_family("x", &[&k], var().integer().bounds(0, 10));
    let sum: Expression = x.vars().iter().copied().sum();
    let cap = m.add(constraint!(sum.clone() <= 12).name("cap"));
    m.add(constraint!(x[0] + x[1] <= 3));
    m.maximize(sum);
    assert_eq!(solve(&m).objective(), Some(12.0));
    round_trip(&m, "edit-before");

    m.set_bounds(x[2], 0, 2);
    m.set_bounds(x[3], 1, 1);
    assert_eq!(solve(&m).objective(), Some(6.0));
    round_trip(&m, "edit-bounds");

    // Removing the cap leaves 3 + 2 + 1; later ids still work.
    m.remove(cap);
    assert_eq!(solve(&m).objective(), Some(6.0));
    m.set_bounds(x[3], 0, 10);
    assert_eq!(solve(&m).objective(), Some(15.0));
    round_trip(&m, "edit-removed");

    // Binary bounds intersect 0..=1; a domain's values are clipped.
    let mut b = Model::new();
    let on = b.add_var(var().binary().name("on"));
    let size = b.add_var(var().domain([1, 4, 9, 16]).name("size"));
    b.set_bounds(on, 1, 5);
    b.set_bounds(size, 2, 10);
    b.maximize(on + size);
    let r = solve(&b);
    assert_eq!(r.objective(), Some(10.0));
    b.minimize(size);
    assert_eq!(solve(&b).objective(), Some(4.0));
    round_trip(&b, "edit-binary-domain");
    b.set_bounds(size, 5, 8);
    assert_eq!(solve(&b).status(), Status::Infeasible);
}

/// Elements with quotes, spaces, and digits survive the trip through a file.
#[test]
fn awkward_elements_round_trip() {
    let names = Set::new("NAMES", ["o'hare", "two words", "3", "it''s", "'"]);
    let mut m = Model::new();
    let x = m.add_family("x", &[&names], var().integer().bounds(0, 10));
    let pick = m.add_var(var().values(&names).name("pick"));
    // Each element's variable is at most its position + 1 when it is picked.
    for (k, (key, v)) in x.iter().enumerate() {
        m.add(constraint!(v <= k as f64 + 1.0).only_if(pick.is(key[0].clone())));
    }
    m.add(constraint!(x["it''s"] >= 4));
    m.maximize(x.vars().iter().copied().sum::<Expression>());
    let json = m.to_json().unwrap();
    assert!(json.contains("x['o''hare']") && json.contains("pick == ''''"), "{json}");
    round_trip(&m, "awkward");
    let r = Problem::from_json(&json).unwrap().solve(&Options::default()).unwrap();
    let best = r.best().unwrap();
    assert!(best.get("x", &["it''s"]).unwrap() >= 4.0);
    assert!(best.get_element("pick", &[]).is_some());
}

#[test]
fn set_names_must_agree() {
    let mut m = Model::new();
    m.add_family("x", &[&Set::range("S", 0..2)], var());
    m.add_family("y", &[&Set::range("S", 0..3)], var());
    let e = m.to_json().unwrap_err();
    assert!(e.message().contains("two different sets are named S"), "{}", e.message());
    let mut ok = Model::new();
    ok.add_family("x", &[&Set::range("S", 0..2)], var().bounds(0, 1));
    ok.add_family("y", &[&Set::range("S", 0..2)], var().bounds(0, 1));
    ok.to_json().unwrap();
}
