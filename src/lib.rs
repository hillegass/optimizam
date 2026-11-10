//! Linear, mixed-integer, constraint, and nonlinear optimization in pure
//! Rust.
//!
//! optimizam solves:
//!
//! - **linear and mixed-integer programs** (dual simplex, presolve, cutting
//!   planes, heuristics, branch and bound);
//! - **constraint programs**: scheduling, timetabling, assignment, puzzles
//!   (a lazy clause generation engine, with `all_different`, `element`,
//!   `table`, `min`/`max`/`abs`, conditional constraints, and intervals with
//!   `no_overlap` and `cumulative`);
//! - **nonlinear programs** with continuous and integer variables, globally
//!   (spatial branch and bound, with an interior point method for local
//!   solves and exact derivatives of `+ - * /`, powers, `ln`, `exp`, `sqrt`,
//!   and the trigonometric functions). It proves global optima when it can,
//!   and says when it couldn't;
//! - one optimal solution, or all of them, and feasibility problems.
//!
//! It has no dependencies beyond serde and serde_json (and clap and ctrlc,
//! for the command), and no C or C++ code.
//!
//! There are two ways in: build a [`Model`] in Rust, or read a [`Problem`]
//! in the JSON problem format, the format of the `optimizam` command. Both
//! go through the same solver, and a `Model` can be written as a problem
//! file with [`Model::to_json`].
//!
//! # Building a model
//!
//! Variables come from [`var`] and [`Model::add_var`]; expressions are built
//! with the arithmetic operators; constraints come from the
//! [`constraint!`] macro or from [`Expression::leq`], [`geq`](Expression::geq),
//! and [`eq`](Expression::eq):
//!
//! ```
//! use optimizam::{Model, Options, Status, constraint, var};
//!
//! let mut m = Model::new();
//! let chairs = m.add_var(var().integer().bounds(0, 40).name("chairs"));
//! let tables = m.add_var(var().integer().bounds(0, 30).name("tables"));
//! m.add(constraint!(3 * chairs + 5 * tables <= 150).name("carpentry"));
//! m.add(constraint!(chairs + 2 * tables <= 50).name("finishing"));
//! m.maximize(45 * chairs + 80 * tables);
//!
//! let result = m.solve(&Options::default())?;
//! assert_eq!(result.status(), Status::Complete);
//! let best = result.best().unwrap();
//! println!("{} chairs, {} tables, profit {}", best[chairs], best[tables], result.objective().unwrap());
//! # Ok::<(), optimizam::Error>(())
//! ```
//!
//! Nonlinear terms need nothing special:
//!
//! ```
//! use optimizam::{Expression, Model, Options, Status, constraint, var};
//!
//! // Hock–Schittkowski problem 71.
//! let mut m = Model::new();
//! let x: Vec<_> = [1.0, 5.0, 5.0, 1.0].iter().map(|&s| m.add_var(var().bounds(1, 5).start(s))).collect();
//! m.add(constraint!(x[0] * x[1] * x[2] * x[3] >= 25));
//! m.add(constraint!(x.iter().map(|&v| v.powi(2)).sum::<Expression>() == 40));
//! m.minimize(x[0] * x[3] * (x[0] + x[1] + x[2]) + x[2]);
//!
//! let result = m.solve(&Options::default())?;
//! assert_eq!(result.status(), Status::Complete); // proven globally optimal
//! assert!((result.objective().unwrap() - 17.0140173).abs() < 1e-6);
//! # Ok::<(), optimizam::Error>(())
//! ```
//!
//! The constraint programming kinds are methods of [`Model`]:
//! [`all_different`](Model::all_different), [`table`](Model::table),
//! [`element`](Model::element), [`min_of`](Model::min_of),
//! [`max_of`](Model::max_of), [`abs_of`](Model::abs_of), intervals
//! ([`interval`](Model::interval), [`optional_interval`](Model::optional_interval))
//! with [`no_overlap`](Model::no_overlap) and
//! [`cumulative`](Model::cumulative), and conditional constraints with
//! [`Constraint::only_if`].
//!
//! Variables indexed by sets of names or numbers come from
//! [`Model::add_family`]: `x[("north", 3)]`, reported in results as a table
//! keyed by element (see [`VarFamily`]). A [`Set`] with string elements can
//! also be a variable's values ([`VarSpec::values`]), making it symbolic.
//! Models can be changed after they are built: [`Model::set_bounds`],
//! [`Model::remove`], and lookups by name ([`Model::family`],
//! [`Model::constraint_named`]).
//!
//! # Problem files
//!
//! [`Problem`] reads the JSON format that the `optimizam` command takes,
//! with index sets, parameters, sums, and indexed constraints; see
//! [`format::problem`]. Results can be written in the result format
//! ([`format::result`]) with [`SolveResult::to_json`]. To change a problem
//! file's model before solving it, read it into a [`Model`] with
//! [`Model::from_file`] (or [`Problem::to_model`]): its variables are
//! families, its constraints named by instance (`capacity[north]`).
//!
//! ```no_run
//! use optimizam::{Options, Problem};
//! use std::time::Duration;
//!
//! let problem = Problem::from_file("examples/job_shop.json")?;
//! let result = problem.solve(&Options::default().time_limit(Duration::from_secs(30)))?;
//! println!("{}", result.to_json());
//! # Ok::<(), optimizam::Error>(())
//! ```
//!
//! # Results
//!
//! To watch a long solve, pass a callback to [`Options::on_progress`]: it
//! hears of each better solution as it is found, with the bound and gap, and
//! can stop the solve. [`SolveResult::stop_reason`] says whether a limit cut
//! the search short.
//!
//! A solve that ends is a success, whatever it found: infeasible problems and
//! time limits are [`Status`]es, and [`Error`]s are for problems that are
//! malformed or that the solver can't take.
//!
//! - [`Status::Complete`]: optimal (for nonlinear problems, a proven global
//!   optimum), feasible for a satisfaction problem, or every solution when
//!   all were requested ([`Model::all_solutions`]).
//! - [`Status::NotProvenOptimal`] and [`Status::LocallyOptimal`]: a solution
//!   that may not be optimal; [`SolveResult::objective_bound`] bounds how
//!   much better one could be.
//! - [`Status::Infeasible`], [`Status::Unbounded`], [`Status::Timeout`],
//!   [`Status::Incomplete`], [`Status::LocallyInfeasible`].
//!
//! Constraints hold to a tolerance of about 1e-6 (relative), and optimal
//! objectives are proven to about the same; integer variables have
//! integral values in solutions.
//!
//! # Engines
//!
//! [`Engine::Auto`] (the default) picks the engine from the problem:
//!
//! - nonlinear terms: spatial branch and bound (global);
//! - every variable integer with finite bounds, and the problem a
//!   satisfaction problem, an all-solutions problem, or one with
//!   constraint programming kinds: the CP engine;
//! - otherwise: LP-based branch and bound (for an LP, the dual simplex).
//!
//! [`Options::engine`] overrides the choice; [`Engine::Nlp`] is a local
//! solve only (an interior point method), which is much faster on large
//! continuous problems but proves nothing about global optimality.
//!
//! # Limitations
//!
//! A solve uses one thread unless [`Options::threads`] allows more, and
//! then only the CP engine does (a portfolio of searches). Some MILPs with
//! weak root bounds need large search trees (there is no probing, conflict
//! analysis, or cutting below the root). The global
//! search struggles with nonlinear terms of unbounded variables: give
//! nonlinear variables bounds. Without a time limit, hard problems can take
//! arbitrarily long; set [`Options::time_limit`], or stop the solve from
//! another thread with a [`CancelToken`] (both keep the best solution found).
//!
//! # Features
//!
//! - `cli` (default): the `optimizam` command. Libraries can use
//!   `default-features = false` to leave out clap and ctrlc.

#![warn(missing_docs)]

mod api;
mod config;
mod cp;
mod cuts;
mod encode;
mod expr;
mod heuristics;
mod lp;
mod milp;
mod model;
mod nlp;
mod presolve;
mod problem;
mod propagation;
mod result;
mod simplex;
mod solve;
mod symmetry;

pub use api::{
    CancelToken, Constraint, ConstraintId, Control, Element, Error, ErrorKind, Event, Expression, IntervalVar, Key,
    KeyPart, Literal, Model, Options, Problem, Progress, Set, Solution, SolveResult, StopReason, VarFamily, VarSpec,
    Variable, var,
};
pub use result::Status;
pub use solve::Engine;

/// The JSON formats of the `optimizam` command.
pub mod format {
    #[doc = include_str!("../docs/problem-format.md")]
    pub mod problem {}

    #[doc = include_str!("../docs/result-format.md")]
    pub mod result {}
}
