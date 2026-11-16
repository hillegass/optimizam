//! `Model::to_mps` and `--write-mps`: the file's text for a small model, and
//! every linear example read back from its file and solved to the same
//! answer. (`tools/mps_check.py` gives the files to HiGHS, which a test here
//! can't.)

use optimizam::{ErrorKind, Expression, Model, Options, Set, SolveResult, Status, Variable, constraint, sum, var};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// A column of a file: its name, whether it is an integer, and its entries
/// by the rows' names.
type Column = (String, bool, Vec<(String, f64)>);

/// A reader for the files `to_mps` writes (free format, one entry a line),
/// kept apart from the writer's code: it knows the format, not the writer.
fn read_mps(text: &str) -> Model {
    let mut m = Model::new();
    let (mut section, mut maximize, mut integer) = (String::new(), false, false);
    let mut rows: Vec<(String, char)> = Vec::new();
    let mut columns: Vec<Column> = Vec::new();
    let mut place: HashMap<String, usize> = HashMap::new();
    let (mut rhs, mut ranges) = (HashMap::<String, f64>::new(), HashMap::<String, f64>::new());
    let mut bounds: HashMap<String, (f64, f64)> = HashMap::new();
    let number = |s: &str| s.parse::<f64>().unwrap_or_else(|_| panic!("not a number: {s}"));
    for line in text.lines().filter(|l| !l.starts_with('*') && !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if !line.starts_with(' ') {
            section = fields[0].to_string();
            continue;
        }
        match section.as_str() {
            "OBJSENSE" => maximize = fields[0] == "MAX",
            "ROWS" => rows.push((fields[1].to_string(), fields[0].chars().next().unwrap())),
            "COLUMNS" if fields[1] == "'MARKER'" => integer = fields[2] == "'INTORG'",
            "COLUMNS" => {
                assert_eq!(fields.len(), 3, "{line}");
                let k = *place.entry(fields[0].to_string()).or_insert_with(|| {
                    columns.push((fields[0].to_string(), integer, Vec::new()));
                    columns.len() - 1
                });
                columns[k].2.push((fields[1].to_string(), number(fields[2])));
            }
            "RHS" => assert!(rhs.insert(fields[1].to_string(), number(fields[2])).is_none(), "{line}"),
            "RANGES" => assert!(ranges.insert(fields[1].to_string(), number(fields[2])).is_none(), "{line}"),
            "BOUNDS" => {
                assert!(place.contains_key(fields[2]), "a bound on an unknown column: {line}");
                let b = bounds.entry(fields[2].to_string()).or_insert((0.0, f64::INFINITY));
                match fields[0] {
                    "UP" => b.1 = number(fields[3]),
                    "LO" => b.0 = number(fields[3]),
                    "FX" => *b = (number(fields[3]), number(fields[3])),
                    "FR" => *b = (f64::NEG_INFINITY, f64::INFINITY),
                    "MI" => b.0 = f64::NEG_INFINITY,
                    "PL" => b.1 = f64::INFINITY,
                    other => panic!("bound type {other}"),
                }
            }
            other => panic!("section {other}"),
        }
    }
    assert_eq!(section, "ENDATA");
    let vars: Vec<Variable> = columns
        .iter()
        .map(|(name, integer, _)| {
            let (lower, upper) = bounds.get(name).copied().unwrap_or((0.0, f64::INFINITY));
            // Without names: a column's is no identifier (`make[north]`).
            let spec = var().bounds(lower, upper);
            m.add_var(if *integer { spec.integer() } else { spec })
        })
        .collect();
    let mut exprs: HashMap<&str, Vec<Expression>> = rows.iter().map(|r| (r.0.as_str(), Vec::new())).collect();
    assert_eq!(exprs.len(), rows.len(), "two rows share a name");
    for (k, (_, _, entries)) in columns.iter().enumerate() {
        for (row, value) in entries {
            exprs.get_mut(row.as_str()).unwrap_or_else(|| panic!("no row {row}")).push(*value * vars[k]);
        }
    }
    for (k, (name, kind)) in rows.iter().enumerate() {
        let expr = sum(exprs.remove(name.as_str()).unwrap());
        let b = rhs.get(name).copied().unwrap_or(0.0);
        match (kind, ranges.get(name)) {
            ('N', _) => {
                assert_eq!(k, 0, "a second objective");
                // The objective row's right-hand side is minus its constant.
                if maximize { m.maximize(expr - b) } else { m.minimize(expr - b) }
            }
            ('L', Some(range)) => drop(m.add(expr.between(b - range, b))),
            ('L', None) => drop(m.add(expr.leq(b))),
            ('G', None) => drop(m.add(expr.geq(b))),
            ('E', None) => drop(m.add(expr.eq(b))),
            other => panic!("row {other:?}"),
        }
    }
    m
}

fn solved(m: &Model) -> SolveResult {
    m.solve(&Options::default().time_limit(std::time::Duration::from_secs(60))).unwrap()
}

/// The same answer: both without a solution in the same way, or both with
/// one and (when there is an objective) with the same objective.
fn assert_same(theirs: &SolveResult, ours: &SolveResult, what: &str) {
    match ours.status() {
        Status::Infeasible | Status::Unbounded => assert_eq!(theirs.status(), ours.status(), "{what}"),
        _ => {
            assert!(!theirs.solutions().is_empty(), "{what}: {:?}", theirs.status());
            assert_eq!(theirs.status(), Status::Complete, "{what}");
            match (ours.objective(), theirs.objective()) {
                (Some(a), Some(b)) => assert!((a - b).abs() <= 1e-6 * a.abs().max(1.0), "{what}: {a} and {b}"),
                // A satisfaction problem's file has an objective of nothing.
                (None, b) => assert!(b.is_none_or(|b| b == 0.0), "{what}"),
                (a, None) => panic!("{what}: {a:?} and no objective"),
            }
        }
    }
}

#[test]
fn a_small_model_is_written_so() {
    let mut m = Model::new();
    m.set_model_name("two plants");
    let plants = Set::new("PLANTS", ["north", "New York"]);
    let make = m.add_family("make", &[&plants], var().integer().min(0));
    let spare = m.add_var(var().bounds(-2, 7.5).name("spare"));
    let free = m.add_var(var().name("free"));
    let below = m.add_var(var().max(-1).name("below"));
    let held = m.add_var(var().bounds(3, 3).name("held"));
    let on = m.add_var(var().binary().name("on"));
    let unused = m.add_var(var().min(0).name("rhs"));
    m.add_indexed("cap", "north", constraint!(make["north"] + 2 * spare <= 40));
    m.add_indexed("cap", "New York", constraint!(make["New York"] - 100 * on <= 0));
    m.add(constraint!(make["north"] + make["New York"] >= 12.5).name("demand"));
    m.add(constraint!(free + below + held == 1));
    m.add((make["north"] - make["New York"]).between(-3, 4).name("balance"));
    m.maximize(3 * make["north"] + 2.5 * make["New York"] - spare + 10);
    let _ = unused;
    let expected = format!(
        "\
* Written by optimizam {}.
NAME two_plants
OBJSENSE
    MAX
ROWS
 N  objective
 L  cap[north]
 L  cap[New_York]
 G  demand
 E  constraints[3]
 L  balance
COLUMNS
    MARKER0 'MARKER' 'INTORG'
    make[north] objective 3
    make[north] cap[north] 1
    make[north] demand 1
    make[north] balance 1
    make[New_York] objective 2.5
    make[New_York] cap[New_York] 1
    make[New_York] demand 1
    make[New_York] balance -1
    MARKER1 'MARKER' 'INTEND'
    spare objective -1
    spare cap[north] 2
    free constraints[3] 1
    below constraints[3] 1
    held constraints[3] 1
    MARKER2 'MARKER' 'INTORG'
    on cap[New_York] -100
    MARKER3 'MARKER' 'INTEND'
    rhs_ objective 0
RHS
    RHS objective -10
    RHS cap[north] 40
    RHS demand 12.5
    RHS constraints[3] 1
    RHS balance 4
RANGES
    RNG balance 7
BOUNDS
 PL BND make[north]
 PL BND make[New_York]
 UP BND spare 7.5
 LO BND spare -2
 FR BND free
 UP BND below -1
 MI BND below
 FX BND held 3
 UP BND on 1
ENDATA
",
        env!("CARGO_PKG_VERSION")
    );
    let mps = m.to_mps().unwrap();
    assert_eq!(mps, expected, "{mps}");
    assert_same(&solved(&read_mps(&mps)), &solved(&m), "the small model");
}

#[test]
fn every_linear_example_reads_back_to_the_same_answer() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let (mut written, mut nonlinear) = (0, Vec::new());
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    for file in files {
        let name = file.file_name().unwrap().to_str().unwrap().to_string();
        if !name.ends_with(".json") || name.ends_with(".result.json") {
            continue;
        }
        let m = Model::from_file(&file).unwrap();
        match m.to_mps() {
            Ok(mps) => {
                assert_same(&solved(&read_mps(&mps)), &solved(&m), &name);
                written += 1;
            }
            Err(e) => {
                assert_eq!(e.kind(), ErrorKind::Unsupported, "{name}: {e}");
                assert!(e.message().contains("is nonlinear"), "{name}: {e}");
                nonlinear.push(name);
            }
        }
    }
    assert_eq!(
        nonlinear,
        ["batch_sizing", "chemical_equilibrium", "curve_fit", "disk_infeasible", "hs071", "portfolio_risk"]
            .map(|n| format!("{n}.json"))
    );
    assert!(written >= 23, "{written}");
}

#[test]
fn what_mps_cannot_say_is_written_as_rows_and_columns() {
    // A domain with gaps: a binary for each range, and no more solutions
    // than the model has.
    let mut m = Model::new();
    let x = m.add_var(var().integer().domain([1, 2, 6, 9, 10]).name("x"));
    let y = m.add_var(var().integer().bounds(0, 10).name("y"));
    m.add(constraint!(x + y == 11));
    m.add(constraint!(x <= 8));
    m.maximize(x - 0.5 * y);
    let mps = m.to_mps().unwrap();
    for line in [" E  _one_x\n", " G  _low_x\n", " L  _high_x\n", "    _in_x_1 _low_x -1\n", "    _in_x_2 _high_x -6\n"]
    {
        assert!(mps.contains(line), "{line:?} in\n{mps}");
    }
    let (theirs, ours) = (solved(&read_mps(&mps)), solved(&m));
    assert_eq!(ours.objective(), Some(3.5)); // x = 6, y = 5
    assert_same(&theirs, &ours, "a domain with gaps");

    // A constraint without variables that can't hold.
    let mut m = Model::new();
    let x = m.add_var(var().bounds(0, 1).name("x"));
    m.add(constraint!(0 * x >= 1));
    m.minimize(x);
    let mps = m.to_mps().unwrap();
    assert_eq!(solved(&m).status(), Status::Infeasible);
    assert_same(&solved(&read_mps(&mps)), &solved(&m), "a constraint that can't hold");

    // A satisfaction problem has an objective row with nothing in it.
    let mut m = Model::new();
    let q = m.add_vars(4, var().integer().bounds(1, 4).name("q"));
    m.all_different(&q);
    let mps = m.to_mps().unwrap();
    assert!(mps.contains(" N  objective\n") && !mps.contains("OBJSENSE"), "{mps}");
    assert_same(&solved(&read_mps(&mps)), &solved(&m), "all different");
}

#[test]
fn the_command_line_tool_writes_the_file_and_does_not_solve() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/knapsack.json");
    let tool = || Command::new(env!("CARGO_BIN_EXE_optimizam"));
    let out = tool().args(["--write-mps", "-"]).arg(&example).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mps = String::from_utf8(out.stdout).unwrap();
    assert_eq!(mps, Model::from_file(&example).unwrap().to_mps().unwrap());
    assert!(mps.contains("    take[gold] capacity 4\n") && mps.ends_with("ENDATA\n"), "{mps}");

    let dir = std::env::temp_dir().join(format!("optimizam-mps-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("model.mps");
    let out = tool().arg("--write-mps").arg(&file).arg(&example).output().unwrap();
    let written = std::fs::read_to_string(&file);
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(out.status.success() && out.stdout.is_empty());
    assert_eq!(written.unwrap(), mps);

    // A result file beside it is a mistake in the arguments, and a
    // nonlinear problem is refused.
    let out = tool().args(["--write-mps", "-"]).arg(&example).arg("result.json").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    let nonlinear = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hs071.json");
    let out = tool().args(["--write-mps", "-"]).arg(&nonlinear).output().unwrap();
    let message = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success() && out.stdout.is_empty());
    assert!(message.contains("an MPS file holds a linear model, and constraint product is nonlinear"), "{message}");
}
