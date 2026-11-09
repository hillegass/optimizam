//! optimizam: solve a constrained optimization problem described in JSON.
//! Command line: docs/cli.md.

use clap::Parser;
use optimizam::{Engine, Options, Problem};
use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const LONG_ABOUT: &str = "\
Solve a constrained optimization problem described in JSON.

optimizam reads a problem file (variables, constraints, and an objective,
written with index sets, sums, and constraint kinds such as all_different or
no_overlap) and writes a JSON result file with the status and the solutions.
It solves linear programs, mixed-integer programs, constraint programming
problems, and nonlinear programs (with continuous and integer variables), with
an LP-based branch-and-bound engine, a lazy clause generation (CP) engine, and
spatial branch and bound with an interior point method for nonlinear problems.

The formats are documented in docs/problem-format.md and docs/result-format.md
in the source repository; schemas are in schema/.";

const AFTER_LONG_HELP: &str = "\
Result statuses:
  complete            everything requested was found and proven
  incomplete          \"all\" was requested; time ran out (the solutions found are valid)
  not_proven_optimal  time ran out with a solution that may not be optimal
  infeasible          no solution exists
  unbounded           the objective can improve without limit
  timeout             time ran out before any solution was found
  locally_optimal     nonlinear problems: a local optimum, not proven global in time
                      (a better one may exist; objective_bound bounds it)
  locally_infeasible  nonlinear problems: no feasible point found; converged to a local
                      minimum of the infeasibility

Exit codes:
  0  a result file was written (infeasible and timeout are answers, not failures)
  1  the problem file is invalid or unsupported, or the result couldn't be written
     (the message is on stderr)
  2  bad command-line usage

Examples:
  optimizam problem.json result.json
  optimizam --time=30 problem.json -          # print the result, stop after 30 s
  cat problem.json | optimizam - -            # read stdin, write stdout
  optimizam --engine=mip problem.json -       # force the MILP engine
  optimizam --engine=nlp problem.json -       # local solve only (continuous problems)";

#[derive(Parser)]
#[command(
    version,
    about = "Solve a constrained optimization problem described in JSON",
    long_about = LONG_ABOUT,
    after_long_help = AFTER_LONG_HELP
)]
struct Cli {
    /// Time limit in seconds (a positive number; decimals allowed). No limit if omitted.
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    time: Option<f64>,
    /// Which engine solves the problem.
    #[arg(long, value_enum, default_value = "auto")]
    engine: CliEngine,
    /// Problem file to read, or "-" for stdin.
    problem: String,
    /// Result file to write, or "-" for stdout.
    result: String,
}

/// `--engine` (the same as [`Engine`], whose docs these are).
#[derive(Clone, Copy, clap::ValueEnum)]
enum CliEngine {
    /// CP for problems it can take (every variable integer with finite
    /// bounds, rows with integer coefficients after scaling) that are satisfy,
    /// "all", or use a format 1.1 constraint kind; MILP otherwise.
    Auto,
    /// LP-based branch and bound.
    Mip,
    /// Lazy clause generation (CDCL over integer bounds).
    Cp,
    /// Interior point method for continuous problems (with nonlinear terms
    /// or not); finds local optima. (auto solves nonlinear problems
    /// globally, with spatial branch and bound.)
    Nlp,
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
    let problem = Problem::from_json(&text).map_err(|e| e.to_string())?;
    let engine = match cli.engine {
        CliEngine::Auto => Engine::Auto,
        CliEngine::Mip => Engine::Mip,
        CliEngine::Cp => Engine::Cp,
        CliEngine::Nlp => Engine::Nlp,
    };
    let mut options = Options::default().engine(engine);
    // The time limit counts from the start of the process, reading included.
    if let Some(t) = cli.time {
        options = options.deadline(start + Duration::from_secs_f64(t));
    }
    // Development switches (CLAUDE.md).
    let set = |name: &str| std::env::var_os(name).is_some();
    options = options
        .presolve(!set("OPTIMIZAM_NO_PRESOLVE"))
        .value_symmetry(!set("OPTIMIZAM_NO_SYMMETRY"))
        .large_neighborhood_search(!set("OPTIMIZAM_NO_LNS"));
    if set("OPTIMIZAM_DEBUG") {
        options = options.log_to_stderr();
    }
    let json = problem.solve(&options).map_err(|e| e.to_string())?.to_json();
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
