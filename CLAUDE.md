# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Phase 1 (LP/MILP) is implemented and passes the regression tests. Modules in `src/`:

- `problem.rs` reads the JSON; `expr.rs` parses expression strings; `model.rs` expands sets/sums into a flat `LinearModel` and rejects nonlinear terms.
- `lp.rs` converts it to a HiGHS-style `Lp` (minimization, row/column bounds, CSC + CSR).
- `simplex/` is the bounded dual simplex (`mod.rs`, after HiGHS `HEkkDual`) with its LU factorization (`factor.rs`).
- `mip.rs` is branch and bound (DFS until an incumbent, then best-bound; reliability branching, i.e. pseudocosts seeded by strong branching; `want_all` splits integral nodes until every variable is fixed).
- `cuts/` adds root cutting planes: `cmir.rs` (c-MIR on row aggregations, with variable-bound substitution), `gmi.rs` (Gomory mixed-integer cuts from the tableau), and `mod.rs` (rounds, numerical cleanup, selection, removing cuts that are no longer binding).
- `presolve.rs` reduces the problem before the search (bound and coefficient tightening, row and column removal, dual fixing with `"one"`); its postsolve only re-inserts fixed columns.
- `propagation.rs` is the domain store shared by the engines: bound propagation over linear rows with a trail of bound changes and their reasons (for conflict analysis). The MILP search propagates at every node and in strong-branching probes before solving the LP.
- `heuristics.rs` finds solutions early: rounding, dives, and RENS/RINS sub-MIPs (which call presolve and branch and bound recursively, with `Settings::sub_mip`).
- `cp/` is the CP engine (lazy clause generation, after OR-Tools CP-SAT) for all-integer problems with finite bounds: `domains.rs` (integer trail of bound literals with reasons), `linear.rs` (exact integer bound propagation with lazy explanations), `clauses.rs` (two-watched-literal clauses), `solver.rs` (CDCL: 1-UIP, VSIDS, Luby restarts, clause deletion; earliest-start task branching until the first solution when there are scheduling constraints), `scheduling.rs` (native `no_overlap` and `cumulative` propagators: timetabling, pairwise precedences, edge finding, with stored explanations), `lns.rs` (large neighborhood search between slices of the main search when optimizing: relaxed tasks re-solved with the others kept in incumbent order, or random columns fixed), and `mod.rs` (integer scaling and the satisfy/all/optimize modes).
- `nlp/` is for nonlinear programs: `graph.rs` is the expression graph that `model.rs` builds for the nonlinear part of rows and the objective (`LinearModel::nonlinear`; hash-consed nodes, constant folding), with an `Evaluator` for values, gradients (reverse mode), and the Hessian of the Lagrangian (forward-over-reverse, with the pattern's columns colored to share directions).
- `solve.rs` runs presolve, picks the engine (`--engine=auto|mip|cp`; auto uses CP for `satisfy` and `"all"` problems it can take), and maps the outcome to a result status; `result.rs` writes the result file.

Set `OPTIMIZAM_DEBUG=1` to print presolve, cut, heuristic, and branch-and-bound statistics (including when each incumbent was found, and by what) to stderr. Set `OPTIMIZAM_NO_PRESOLVE=1` to skip presolve, for comparisons or to isolate a presolve bug. Set `OPTIMIZAM_NO_LNS=1` to turn off the CP engine's large neighborhood search.

Known limitations: no probing, conflict analysis, or cuts below the root, so some MILPs with weak root bounds still need large trees (lot sizing with many periods is the slowest class tried). Before branch and bound, `Lp::tighten_integer_rows` rounds all-integer rows to multiples of their coefficients' gcd, and presolve's congruence check handles equality rows with decimal coefficients and small-range columns; together they prove cases like `2x - 4y = 7` or `2x - 4y + 8.25z = 9.5` (z binary) infeasible. But an infeasible MILP with unbounded integer variables whose infeasibility needs several rows combined can still run forever without `--time`.

- Rust edition: **2024** (requires a toolchain of Rust 1.85 or newer)
- Single binary target. Dependencies: `serde`, `serde_json`, `clap`.

## Users

`README.md` is for users: installing the binary (`cargo install --path .`) and the Claude Code skill in `skills/optimizam/` (`SKILL.md`, plus `references/` with symlinks to `docs/problem-format.md`, `docs/result-format.md`, `docs/cli.md`, and `examples/`). Keep `SKILL.md`'s summary of the format and statuses, and `--help` in `src/main.rs`, in sync with the docs when the format or CLI changes.

## Commands

```sh
cargo build                      # build (debug)
cargo run -- --time=50 examples/knapsack.json -   # solve, print result (CLI: docs/cli.md)
cargo run --release -- problem.json result.json   # optimized build and run
cargo test                       # run all tests
cargo test <name_substring>      # run tests whose name matches
cargo test <name> -- --exact     # run one test by its full path, e.g. module::tests::foo
cargo test --release --test regression           # solve every example, compare to expected results
cargo test --test regression knapsack            # one example
cargo clippy --all-targets       # lint
cargo fmt                        # format with rustfmt.toml (120 columns); cargo fmt --check must pass
```

## Solver

Phase 1 is an LP/MILP solver (dual simplex + branch and bound, HiGHS-style). Scope, error cases, numerics, status mapping, and the dependency policy (no crates beyond serde/serde_json/clap without asking) are in `docs/solver-design.md`.

## Regression tests and reference solver

`tests/regression.rs` runs the binary on each `examples/<name>.json` and compares against `examples/<name>.result.json`: status exactly; objectives within 1e-6 relative tolerance; the full solution set for `"all"`; only the variable layout for `"one"`, since optima need not be unique. A new example must be added to the `regression_tests!` list, and `every_example_is_tested` enforces that.

HiGHS (`highspy` 1.15.1), OR-Tools (`ortools` 9.15, for CP-SAT), SymPy (1.14, for derivatives), and CasADi (3.8.2, whose wheel bundles IPOPT, for nonlinear problems) are installed in the project's `.venv` (gitignored) as development-only references. Optimizam itself must not depend on them. To recreate: `python3 -m venv .venv && .venv/bin/pip install highspy ortools jsonschema sympy casadi`.

- `tools/sympy_derivatives.py` writes `tests/data/sympy_derivatives.json`: random format 1.2 expressions over x1..x3 with SymPy's values, gradients, and Hessians at random points, which `nlp::tests::derivatives_match_sympy` checks through the model builder. Regenerate it with `.venv/bin/python tools/sympy_derivatives.py tests/data/sympy_derivatives.json` after changing the generator.

- `tools/highs_reference.py` is an independent Python implementation of the problem format (parser + linearization) that solves with HiGHS and writes our result format. `tools/highs.sh` wraps it with optimizam's CLI. It encodes integers one-hot for gapped domains and for "all" (enumerated with no-good cuts), so those need finite bounds.
- To produce an expected result: `tools/highs.sh examples/<name>.json examples/<name>.result.json` for a format 1.0 example, `tools/cpsat.sh ...` (the same arguments) for one using format 1.1. The small examples' results were brute-forced instead, and agree with HiGHS.
- To check the expected files and the test itself: `OPTIMIZAM_BIN=tools/highs.sh cargo test --test regression -- --skip n_queens_alldiff --skip room_assignment --skip sudoku --skip machine_assignment --skip shift_rota --skip tsp_tour --skip load_balance --skip production_smoothing --skip job_shop --skip crew_schedule` (HiGHS reads only format 1.0), and `OPTIMIZAM_BIN=tools/cpsat.sh cargo test --test regression -- n_queens_alldiff room_assignment sudoku machine_assignment shift_rota tsp_tour load_balance production_smoothing job_shop crew_schedule four_queens knapsack domain_gaps` (the all-integer examples); both must pass.
- `tools/cpsat_reference.py` (wrapped by `tools/cpsat.sh`) is the CP engine's reference: it reuses `highs_reference.py`'s parser and evaluator (extended for 1.1 lists and set domains; `highspy` is imported lazily there, since OR-Tools bundles its own HiGHS and the two clash) and builds a native CP-SAT model (`AddAllDifferent`, `AddAllowedAssignments`/`AddForbiddenAssignments`, `AddElement`, `AddMin/Max/AbsEquality`, interval variables with `AddNoOverlap`/`AddCumulative`, `OnlyEnforceIf`, set domains), so it shares nothing with `encode.rs`. Integer variables with finite bounds only.
- The large examples come from `tools/generate_examples.py` (fixed seeds). If you regenerate them, regenerate their result files too.
- `tools/milp_benchmark.py` is the MILP performance benchmark: seven generated instances (facility location, generalized assignment, two lot sizing, multidimensional knapsack, set covering, time-window scheduling) from fixed seeds, each stressing a different part of the solver. `cargo build --release && python3 tools/milp_benchmark.py` prints time, status, objective and bound per instance (about 50s); `--highs` adds HiGHS's times and flags disagreeing optima, and naming instances runs only those. Run it before and after performance work.
- `tools/mps_to_json.py` converts MPS (fixed or free format, optionally gzipped) to our problem format: `python3 tools/mps_to_json.py in.mps.gz out.json [--names names.json] [--pretty]`. Columns become integer-indexed variables grouped by type and finite bounds (the format has no string literals or infinite parameters); ranged rows become two constraints. One convention differs between readers: a negative UP bound with no lower bound sets the lower bound to -infinity here (with a warning), while HiGHS 1.15 keeps 0.
- `tools/netlib_lp_benchmark.py` is the LP benchmark: it downloads the 91 main Netlib LPs that the mirror has (gzipped MPS from the COIN-OR Data-Netlib mirror) into `target/netlib_lp/`, converts them, solves them, and checks each against HiGHS reading the original MPS and against Netlib's published optimum. `cargo build --release && python3 tools/netlib_lp_benchmark.py` (add `--check-converter` to also solve the converted JSON with HiGHS, which separates converter bugs from solver bugs). The Netlib files are downloaded on demand and never committed.
- `tools/cp_benchmark.py` is the CP benchmark: published instances downloaded on demand into `target/cp_benchmark/` (job shop from JSPLIB, RCPSP j30 from PSPLIB via the MiniZinc benchmarks, DIMACS graph coloring) plus generated Golomb rulers, Sudoku, N-queens, and a sports tournament schedule, each checked against its known optimum. `cargo build --release && python3 tools/cp_benchmark.py` (`--time`, default 60 s per instance; `--cpsat` adds OR-Tools CP-SAT's times and answers; naming instances or prefixes runs only those).
- `tools/compare_random.py` is a differential test: it solves random MILPs from fixed seeds (facility-location-shaped, unstructured, and medium instances of classic classes that exercise presolve and the heuristics) with both the release binary and HiGHS, and reports any status or objective mismatch. Disagreeing problems are saved to `compare_random_failures/` (gitignored), and it exits with status 1. Run it after changing the simplex, branching, cuts, presolve, or heuristics: `cargo build --release && python3 tools/compare_random.py` (about a minute; `--count`, `--seed`, and `--kind` adjust it).

## Problem file format

Problems are JSON files; solving one produces a JSON result file (`schema/result.schema.json`, `docs/result-format.md`). `schema/problem.schema.json` (JSON Schema 2020-12) defines the structure. `docs/problem-format.md` defines what the fields mean and the expression grammar. Constraint and objective expressions are strings (e.g. `sum(i in I) w[i]*x[i] <= cap`), so the JSON Schema can't validate them; they need a parser in the Rust code. Keep the schema, the doc, and `examples/` in sync when changing the format.

Format 1.1 adds set-valued variables (`domain` naming a set; symbolic when the set has strings), an `intervals` section, `enforce_if` on `expr` constraints, and the constraint kinds `all_different`, `element`, `table`, `min`, `max`, `abs`, `no_overlap`, and `cumulative`, which take list arguments (`"[x[i] | i in I]"`). Implemented so far: lists, set-valued variables (symbolic ones in lists, `table`, `element` indexes, and `v == e` / `v != e` comparisons), `all_different`, `table`, `element`, `enforce_if`, `min`, `max`, `abs`, intervals, `no_overlap`, and `cumulative`: all of format 1.1. `model.rs` evaluates each instance to an `encode::Global`, and `encode.rs` encodes it as linear rows on extra columns (shared value binaries `[x = v]`, selectors, big-M) so both engines solve it; auto picks CP for problems using 1.1 kinds. The exceptions are `no_overlap` and `cumulative`: the model keeps them in `LinearModel::scheduling`, the CP engine propagates them natively (`cp/scheduling.rs`), and `solve.rs` encodes them only when the MILP engine is chosen. Other kinds could get native propagators the same way if their encodings prove slow. OR-Tools CP-SAT is the CP engine's reference, as HiGHS is for MILP; verify the CP engine with brute-force tests and OR-Tools, not by comparing it against HiGHS.

Format 1.2 adds the functions `ln`, `exp`, `sqrt`, `sin`, `cos`, `tan`, `atan`, `tanh` (`expr::Func`), powers with any exponent (`Expr::PowE`; integer constant exponents stay `Expr::Pow`), and `start` on variables; result format 1.1 adds the statuses `locally_optimal` and `locally_infeasible` (written with result version "1.1"). Implemented so far: functions and powers of constants, evaluated when the model is built, and nonlinear terms in the variables, which `model.rs` keeps in an expression graph (`nlp/graph.rs`); `solve.rs` still rejects models with nonlinear terms (a dense interior point solver after IPOPT is planned). To check the examples against the schema:

```sh
python3 -c "import json,glob,jsonschema; s=json.load(open('schema/problem.schema.json')); [jsonschema.validate(json.load(open(f)), s) for f in glob.glob('examples/*.json') if not f.endswith('.result.json')]; r=json.load(open('schema/result.schema.json')); [jsonschema.validate(json.load(open(f)), r) for f in glob.glob('examples/*.result.json')]"
```
