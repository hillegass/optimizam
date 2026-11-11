//! Problem files read into a `Model`: they solve to the examples' expected
//! results, survive a round trip through `Model::to_json`, and can be edited.

use optimizam::{Element, Model, Options, Status, constraint};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json") && !p.to_string_lossy().ends_with(".result.json"))
        .collect();
    files.sort();
    files
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * (1.0 + a.abs().max(b.abs()))
}

/// Every example, loaded as a model, solves to its expected status and
/// objective; and the loaded model's export (expanded: one constraint per
/// instance) solves to the same.
#[test]
fn examples_load_and_round_trip() {
    let options = Options::default().time_limit(std::time::Duration::from_secs(60));
    let mut checked = 0;
    for file in examples() {
        let name = file.file_stem().unwrap().to_string_lossy().to_string();
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(file.with_extension("result.json")).unwrap()).unwrap();
        let m = Model::from_file(&file).unwrap_or_else(|e| panic!("{name}: {e}"));
        let r = m.solve(&options).unwrap();
        assert_eq!(r.status().as_str(), expected["status"].as_str().unwrap(), "{name}");
        if let Some(objective) = expected["solutions"][0]["objective"].as_f64() {
            let got = r.objective().unwrap();
            assert!(close(got, objective), "{name}: {got} vs {objective}");
        }
        if expected["solutions"].as_array().unwrap().len() > 1 {
            assert_eq!(r.solutions().len(), expected["solutions"].as_array().unwrap().len(), "{name}");
        }
        let json = m.to_json().unwrap_or_else(|e| panic!("{name}: export: {e}"));
        // For the schema check (CLAUDE.md).
        if let Some(dir) = std::env::var_os("OPTIMIZAM_EXPORT_DIR") {
            std::fs::write(Path::new(&dir).join(format!("loaded-{name}.json")), &json).unwrap();
        }
        let again = Model::from_json(&json).unwrap_or_else(|e| panic!("{name}: {e}\n{json}")).solve(&options).unwrap();
        assert_eq!(again.status(), r.status(), "{name}: export");
        if let (Some(x), Some(y)) = (again.objective(), r.objective()) {
            assert!(close(x, y), "{name}: export {x} vs {y}");
        }
        checked += 1;
    }
    assert!(checked > 20);
}

/// A loaded model edited (a bound changed, a constraint removed, one
/// added), against brute force over every assignment.
#[test]
fn edited_loaded_model_matches_brute_force() {
    let mut m = Model::from_json(
        r#"{
        "version": "1.3",
        "sets": { "ITEMS": ["a", "b", "c", "d"] },
        "parameters": {
            "value": { "index": ["ITEMS"], "values": { "a": 6, "b": 5, "c": 4, "d": 3 } },
            "weight": { "index": ["ITEMS"], "values": { "a": 5, "b": 4, "c": 3, "d": 2 } }
        },
        "variables": [{ "name": "take", "type": "integer", "index": ["ITEMS"], "lower": 0, "upper": 2 }],
        "constraints": [
            { "name": "capacity", "expr": "sum(i in ITEMS) weight[i] * take[i] <= 12" },
            { "name": "pair", "forall": "i in ITEMS: i != 'd'", "expr": "take[i] + take['d'] <= 2" }
        ],
        "solve": { "method": "maximize", "objective": "sum(i in ITEMS) value[i] * take[i]" }
    }"#,
    )
    .unwrap();
    let take = m.family("take").unwrap();
    let (value, weight) = ([6.0, 5.0, 4.0, 3.0], [5.0, 4.0, 3.0, 2.0]);
    let best = |cap: f64, pair: bool, hi: [i64; 4], extra: &dyn Fn(&[i64; 4]) -> bool| -> f64 {
        let mut best = f64::NEG_INFINITY;
        for code in 0..81 {
            let x = [code % 3, code / 3 % 3, code / 9 % 3, code / 27 % 3];
            let w: f64 = (0..4).map(|k| weight[k] * x[k] as f64).sum();
            let ok =
                w <= cap && (!pair || (0..3).all(|k| x[k] + x[3] <= 2)) && (0..4).all(|k| x[k] <= hi[k]) && extra(&x);
            if ok {
                best = best.max((0..4).map(|k| value[k] * x[k] as f64).sum());
            }
        }
        best
    };
    let solve = |m: &Model| {
        let r = m.solve(&Options::default()).unwrap();
        assert_eq!(r.status(), Status::Complete);
        r.objective().unwrap()
    };
    assert_eq!(solve(&m), best(12.0, true, [2; 4], &|_| true));
    assert!(m.constraint_named("pair[b]").is_some() && m.constraint_named("pair[d]").is_none());

    m.set_bounds(take["c"], 0, 0);
    assert_eq!(solve(&m), best(12.0, true, [2, 2, 0, 2], &|_| true));
    m.remove(m.constraint_named("capacity").unwrap());
    m.add(constraint!(take["a"] * 5 + take["b"] * 4 + take["c"] * 3 + take["d"] * 2 <= 9).name("smaller"));
    assert_eq!(solve(&m), best(9.0, true, [2, 2, 0, 2], &|_| true));
    for k in ["a", "b", "c"] {
        m.remove(m.constraint_named(&format!("pair[{k}]")).unwrap());
    }
    m.add(constraint!(take["a"] + take["b"] >= 1));
    assert_eq!(solve(&m), best(9.0, false, [2, 2, 0, 2], &|x| x[0] + x[1] >= 1));
    // The variables keep their keys.
    let r = m.solve(&Options::default()).unwrap();
    let element = |s: &str| Element::from(s);
    assert!(take.get(element("d")).is_some());
    assert!(r.best().unwrap().get("take", &["d"]).is_some());
}

/// Errors in problem files are reported when reading them into a model.
#[test]
fn errors_are_reported_when_reading() {
    let bad = r#"{ "version": "1.3", "variables": [{ "name": "x", "type": "continuous" }],
        "constraints": [{ "name": "c", "expr": "x + y <= 1" }], "solve": { "method": "satisfy" } }"#;
    let e = Model::from_json(bad).unwrap_err();
    assert!(e.message().contains("constraint c: unknown name y"), "{}", e.message());
}
