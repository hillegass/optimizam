//! optimizam: solve a constrained optimization problem described in JSON.
//! Command line: docs/cli.md.

use clap::Parser;
use optimizam::{CancelToken, Control, Engine, Event, Options, Problem};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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

The formats are documented in the tool itself: --docs problem, --docs result,
and --docs cli print the documents, and --schema problem and --schema result
the JSON schemas.";

const AFTER_LONG_HELP: &str = "\
Result statuses:
  complete            everything requested was found and proven
  incomplete          \"all\" or a number of solutions was requested; time ran out, or
                      that many were found (the solutions found are valid; more may exist)
  not_proven_optimal  time ran out with a solution that may not be optimal
  infeasible          no solution exists
  unbounded           the objective can improve without limit
  timeout             time ran out before any solution was found
  locally_optimal     nonlinear problems: a local optimum, not proven global in time
                      (a better one may exist; objective_bound bounds it)
  locally_infeasible  nonlinear problems: no feasible point found; converged to a local
                      minimum of the infeasibility

Interrupting:
  Ctrl-C (or SIGTERM, or SIGHUP) stops the solve and writes the result as if the time
  limit had been reached, with the best solution found. A second Ctrl-C quits at once
  (exit code 130) without writing a result.

Exit codes:
  0  a result file was written (infeasible and timeout are answers, not failures;
     so is a result written after Ctrl-C)
  1  the problem file is invalid or unsupported, or the result couldn't be written
     (the message is on stderr)
  2  bad command-line usage

Examples:
  optimizam problem.json result.json
  optimizam --time=30 problem.json            # print the result, stop after 30 s
  cat problem.json | optimizam -              # read stdin, write stdout
  optimizam --check problem.json              # what the model comes to, without solving
  optimizam --engine=milp problem.json -       # force the MILP engine
  optimizam --threads=1 problem.json -         # one thread: the run repeats exactly
  optimizam --engine=local problem.json -      # one local solve (continuous problems)
  optimizam --incumbents=best.json problem.json result.json
                                              # best.json: the best solution so far, if killed
  optimizam --progress problem.json result.json   # progress as JSON lines on stderr
  optimizam --explain problem.json -          # if infeasible: which constraints conflict
  optimizam --constraints problem.json -      # slack per constraint; duals for an LP
  optimizam --constraints=binding --nonzeros problem.json
                                              # only tight constraints and nonzero entries
  optimizam --docs problem | less             # the problem format";

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
    /// Threads a search may use. By default (and with 0) one per core, at
    /// most 8. With more than one, runs differ from one to the next.
    #[arg(long, value_name = "N")]
    threads: Option<usize>,
    /// Keep FILE up to date with the best solution so far: rewritten (whole,
    /// never half-written) at each better solution, and at the end with the
    /// final result. If the run is killed, FILE has the best answer found.
    /// Only for minimize and maximize problems with "solutions": "one".
    #[arg(long, value_name = "FILE")]
    incumbents: Option<PathBuf>,
    /// Write progress to stderr, one JSON object per line: each better
    /// solution, and about twice a second the bound or a heartbeat.
    #[arg(long)]
    progress: bool,
    /// Report the constraints with each solution: the slack it leaves in
    /// every comparison constraint, and for a linear program the dual
    /// values and the variables' reduced costs. With =binding, only the
    /// constraints left without slack.
    #[arg(long, value_enum, value_name = "WHICH", num_args = 0..=1, require_equals = true, default_missing_value = "all")]
    constraints: Option<Which>,
    /// Leave the zero entries out of indexed variables in the result (which
    /// then says "omitted": ["zeros"]): a missing entry is 0.
    #[arg(long)]
    nonzeros: bool,
    /// If the problem is infeasible, look for a small set of constraints
    /// that can't hold together and report it as the result's "conflict".
    /// Takes a solve per constraint (indexed ones together first), within
    /// the time limit.
    #[arg(long)]
    explain: bool,
    /// Don't solve: build the model and write what it comes to, as JSON
    /// (how many variables and constraints each name stands for, the solver
    /// model's size, the engine that would run, and the warnings).
    #[arg(long)]
    check: bool,
    /// Don't print warnings about the problem file.
    #[arg(long, short)]
    quiet: bool,
    /// Print one of the documents and exit: the problem format, the result
    /// format, or the command line.
    #[arg(long, value_enum, value_name = "WHICH")]
    docs: Option<Document>,
    /// Print one of the JSON schemas and exit.
    #[arg(long, value_enum, value_name = "WHICH")]
    schema: Option<Schema>,
    /// Problem file to read, or "-" for stdin.
    #[arg(required_unless_present_any = ["docs", "schema"])]
    problem: Option<String>,
    /// Result file to write; stdout if it is left out or "-".
    result: Option<String>,
}

/// `--constraints`: every comparison constraint, or the binding ones.
#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
enum Which {
    All,
    Binding,
}

/// `--docs`.
#[derive(Clone, Copy, clap::ValueEnum)]
enum Document {
    Problem,
    Result,
    Cli,
}

/// `--schema`.
#[derive(Clone, Copy, clap::ValueEnum)]
enum Schema {
    Problem,
    Result,
}

/// `--engine` (the same as [`Engine`], whose docs these are).
#[derive(Clone, Copy, clap::ValueEnum)]
enum CliEngine {
    /// The global search for problems with nonlinear terms. Otherwise CP
    /// for problems it can take (every variable integer with finite
    /// bounds, rows with integer coefficients after scaling) that are satisfy,
    /// "all", or use a format 1.1 constraint kind; and MILP for the rest.
    Auto,
    /// LP-based branch and bound.
    Milp,
    /// Lazy clause generation (CDCL over integer bounds).
    Cp,
    /// Spatial branch and bound for problems with nonlinear terms: the
    /// global optimum. (What auto uses for them.)
    Global,
    /// One interior point solve of a continuous problem: a local optimum.
    Local,
}

fn parse_seconds(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
    if v.is_finite() && v > 0.0 { Ok(v) } else { Err("the time limit must be a positive number of seconds".into()) }
}

/// Why a run failed: the problem (which can have several errors), or
/// something else, such as a file that can't be written.
enum Failure {
    Problem(optimizam::Error),
    Other(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Failure {
        Failure::Other(message)
    }
}

impl From<optimizam::Error> for Failure {
    fn from(error: optimizam::Error) -> Failure {
        Failure::Problem(error)
    }
}

/// Writes the tool's output: to the result file, or to stdout.
fn write(result: Option<&str>, text: &str) -> Result<(), String> {
    match result {
        None | Some("-") => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(text.as_bytes()).and_then(|_| stdout.flush()).map_err(|e| format!("writing stdout: {e}"))
        }
        Some(path) => std::fs::write(path, text).map_err(|e| format!("writing {path}: {e}")),
    }
}

fn run(cli: &Cli, start: Instant) -> Result<(), Failure> {
    // The documents and schemas are compiled in: an installed tool has
    // no repository beside it.
    if let Some(document) = cli.docs {
        return Ok(write(
            None,
            match document {
                Document::Problem => include_str!("../docs/problem-format.md"),
                Document::Result => include_str!("../docs/result-format.md"),
                Document::Cli => include_str!("../docs/cli.md"),
            },
        )?);
    }
    if let Some(schema) = cli.schema {
        return Ok(write(
            None,
            match schema {
                Schema::Problem => include_str!("../schema/problem.schema.json"),
                Schema::Result => include_str!("../schema/result.schema.json"),
            },
        )?);
    }
    let path = cli.problem.as_deref().expect("clap requires PROBLEM");
    let text = if path == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| format!("reading stdin: {e}"))?;
        s
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?
    };
    let problem = Problem::from_json(&text)?;
    // What the file says but probably doesn't mean (as JSON lines with
    // --progress, whose readers expect nothing else on stderr). A check's
    // output has them.
    for warning in problem.warnings().iter().filter(|_| !cli.quiet && !cli.check) {
        if cli.progress {
            let mut event = serde_json::json!({ "event": "warning", "kind": warning.kind().as_str(), "message": warning.message() });
            if let Some(place) = warning.place() {
                event["place"] = place.into();
            }
            eprintln!("{event}");
        } else {
            eprintln!("warning: {warning}");
        }
    }
    let engine = match cli.engine {
        CliEngine::Auto => Engine::Auto,
        CliEngine::Milp => Engine::Milp,
        CliEngine::Cp => Engine::Cp,
        CliEngine::Global => Engine::Global,
        CliEngine::Local => Engine::Local,
    };
    // OPTIMIZAM_THREADS is for benchmark scripts, which don't pass options.
    let threads = cli.threads.or_else(|| std::env::var("OPTIMIZAM_THREADS").ok()?.parse().ok());
    let mut options = Options::default()
        .engine(engine)
        .explain(cli.explain)
        .constraints(cli.constraints.is_some())
        .binding_only(cli.constraints == Some(Which::Binding))
        .nonzeros(cli.nonzeros);
    if let Some(n) = threads {
        options = options.threads(n);
    }
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
    if cli.check {
        return Ok(write(cli.result.as_deref(), &problem.check(&options)?.to_json())?);
    }
    if cli.incumbents.is_some() || cli.progress {
        let (incumbents, progress) = (cli.incumbents.clone(), cli.progress);
        options = options.on_progress(move |p| {
            if progress {
                eprintln!("{}", progress_line(p));
            }
            if let Some(path) = &incumbents
                && p.event() == Event::Solution
                && let Some(json) = p.to_json()
                && let Err(e) = write_whole(path, &json)
            {
                eprintln!("optimizam: writing {}: {e}", path.display());
            }
            Control::Continue
        });
    }
    options = options.cancel_on(&interrupt_handler());
    let result = problem.solve(&options)?;
    if cli.progress {
        // The last line: how it ended, so a reader of the stream knows
        // whether the final solution was proven.
        let mut line = serde_json::json!({
            "event": "done",
            "elapsed": start.elapsed().as_secs_f64(),
            "status": result.status().as_str(),
        });
        for (key, value) in
            [("objective", result.objective()), ("bound", result.objective_bound()), ("gap", result.gap())]
        {
            if let Some(v) = value {
                line[key] = v.into();
            }
        }
        if let Some(reason) = result.stop_reason() {
            line["stop_reason"] = reason.as_str().into();
        }
        eprintln!("{line}");
    }
    let json = result.to_json();
    if let Some(path) = &cli.incumbents {
        write_whole(path, &json).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(write(cli.result.as_deref(), &json)?)
}

/// Prints why the run failed: each error on a line (`error: ...`), or with
/// `--progress`, where every line of stderr is a JSON object, as an event.
fn report(failure: &Failure, progress: bool) {
    let line = |message: &str, place: Option<&str>, name: Option<&str>| {
        if !progress {
            return eprintln!("error: {message}");
        }
        let mut event = serde_json::json!({ "event": "error", "message": message });
        for (key, value) in [("place", place), ("name", name)] {
            if let Some(v) = value {
                event[key] = v.into();
            }
        }
        eprintln!("{event}");
    };
    match failure {
        Failure::Other(message) => line(message, None, None),
        Failure::Problem(error) => {
            for e in error.errors() {
                line(e.message(), e.place(), e.name());
            }
            if error.omitted() > 0 {
                line(&format!("and {} more errors", error.omitted()), None, None);
            }
        }
    }
}

/// A token that Ctrl-C (or SIGTERM or SIGHUP) cancels: the solve stops and
/// the result, with the best solution found, is written as usual. A second
/// signal quits at once, writing nothing.
fn interrupt_handler() -> CancelToken {
    let token = CancelToken::new();
    let signaled = token.clone();
    let installed = ctrlc::set_handler(move || {
        if signaled.is_canceled() {
            std::process::exit(130);
        }
        eprintln!("optimizam: interrupted; writing the best result so far (interrupt again to quit now)");
        signaled.cancel();
    });
    if let Err(e) = installed {
        eprintln!("optimizam: can't handle interrupts ({e}); Ctrl-C will quit without a result");
    }
    token
}

/// One `--progress` line.
fn progress_line(p: &optimizam::Progress<'_>) -> String {
    let event = match p.event() {
        Event::Solution => "solution",
        Event::Bound => "bound",
        _ => "heartbeat",
    };
    let mut line = serde_json::json!({ "event": event, "elapsed": p.elapsed().as_secs_f64() });
    for (key, value) in [("objective", p.objective()), ("bound", p.bound()), ("gap", p.gap())] {
        if let Some(v) = value {
            line[key] = v.into();
        }
    }
    line.to_string()
}

/// Writes `text` to `path` through a temporary file in the same directory
/// and a rename, so that readers (and a killed run) never see half a file.
fn write_whole(path: &Path, text: &str) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let temporary = path.with_file_name(name);
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

fn main() -> ExitCode {
    let start = Instant::now();
    let cli = Cli::parse(); // exits with code 2 on usage errors
    match run(&cli, start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            report(&failure, cli.progress);
            ExitCode::from(1)
        }
    }
}
