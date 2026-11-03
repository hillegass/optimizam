//! optimizam: solve a constrained optimization problem described in JSON.
//! Command line: docs/cli.md.

mod expr;
mod lp;
mod mip;
mod model;
mod problem;
mod result;
mod simplex;
mod solve;

use clap::Parser;
use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(version, about = "Solve a constrained optimization problem described in JSON (see docs/cli.md)")]
struct Cli {
    /// Time limit in seconds (a positive number; decimals allowed). No limit if omitted.
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    time: Option<f64>,
    /// Problem file to read, or "-" for stdin.
    problem: String,
    /// Result file to write, or "-" for stdout.
    result: String,
}

fn parse_seconds(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
    if v.is_finite() && v > 0.0 { Ok(v) } else { Err("the time limit must be a positive number of seconds".into()) }
}

fn run(cli: &Cli, start: Instant) -> Result<(), String> {
    let text = if cli.problem == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| format!("reading stdin: {e}"))?;
        s
    } else {
        std::fs::read_to_string(&cli.problem).map_err(|e| format!("reading {}: {e}", cli.problem))?
    };
    let problem = problem::Problem::from_json(&text)?;
    let model = model::build(&problem)?;
    let deadline = cli.time.map(|t| start + Duration::from_secs_f64(t));
    let outcome = solve::solve(&model, deadline)?;
    let json = result::to_json(&model, &outcome);
    if cli.result == "-" {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(json.as_bytes()).and_then(|_| stdout.flush()).map_err(|e| format!("writing stdout: {e}"))
    } else {
        std::fs::write(&cli.result, json).map_err(|e| format!("writing {}: {e}", cli.result))
    }
}

fn main() -> ExitCode {
    let start = Instant::now();
    let cli = Cli::parse(); // exits with code 2 on usage errors
    match run(&cli, start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
