# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Phase 1 (LP/MILP) is implemented and passes the regression tests. Modules in `src/`:

- `problem.rs` reads the JSON; `expr.rs` parses expression strings; `model.rs` expands sets/sums into a flat `LinearModel` and rejects nonlinear terms.
- `lp.rs` converts it to a HiGHS-style `Lp` (minimization, row/column bounds, CSC + CSR).
- `simplex/` is the bounded dual simplex (`mod.rs`, after HiGHS `HEkkDual`) with its LU factorization (`factor.rs`).
- `mip.rs` is branch and bound (DFS until an incumbent, then best-bound; reliability branching, i.e. pseudocosts seeded by strong branching; `want_all` splits integral nodes until every variable is fixed).
- `cuts/` adds root cutting planes: `cmir.rs` (c-MIR on row aggregations, with variable-bound substitution), `gmi.rs` (Gomory mixed-integer cuts from the tableau), and `mod.rs` (rounds, numerical cleanup, selection, removing cuts that are no longer binding).
- `solve.rs` maps the search outcome to a result status; `result.rs` writes the result file.

Set `OPTIMIZAM_DEBUG=1` to print cut and branch-and-bound statistics to stderr.

Known limitations: there's no presolve or primal heuristic yet. Before branch and bound, `Lp::tighten_integer_rows` rounds all-integer rows to multiples of their coefficients' gcd, which proves cases like `2x - 4y = 7` infeasible. But an infeasible MILP with unbounded integer variables whose infeasibility needs several rows combined can still run forever without `--time`.

- Rust edition: **2024** (requires a toolchain of Rust 1.85 or newer)
- Single binary target. Dependencies: `serde`, `serde_json`, `clap`.

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

HiGHS (`highspy` 1.15.1) is installed in the project's `.venv` (gitignored) as a development-only reference. Optimizam itself must not depend on it. To recreate: `python3 -m venv .venv && .venv/bin/pip install highspy jsonschema`.

- `tools/highs_reference.py` is an independent Python implementation of the problem format (parser + linearization) that solves with HiGHS and writes our result format. `tools/highs.sh` wraps it with optimizam's CLI. It encodes integers one-hot for gapped domains and for "all" (enumerated with no-good cuts), so those need finite bounds.
- To produce an expected result: `tools/highs.sh examples/<name>.json examples/<name>.result.json`. The small examples' results were brute-forced instead, and agree with HiGHS.
- To check the expected files and the test itself: `OPTIMIZAM_BIN=tools/highs.sh cargo test --test regression`, which must pass.
- The large examples come from `tools/generate_examples.py` (fixed seeds). If you regenerate them, regenerate their result files too.
- `tools/compare_random.py` is a differential test: it solves random MILPs from fixed seeds (facility-location-shaped and unstructured) with both the release binary and HiGHS, and reports any status or objective mismatch. Disagreeing problems are saved to `compare_random_failures/` (gitignored), and it exits with status 1. Run it after changing the simplex, branching, or cuts: `cargo build --release && python3 tools/compare_random.py` (about a minute; `--count`, `--seed`, and `--kind` adjust it).

## Problem file format

Problems are JSON files; solving one produces a JSON result file (`schema/result.schema.json`, `docs/result-format.md`). `schema/problem.schema.json` (JSON Schema 2020-12) defines the structure. `docs/problem-format.md` defines what the fields mean and the expression grammar. Constraint and objective expressions are strings (e.g. `sum(i in I) w[i]*x[i] <= cap`), so the JSON Schema can't validate them; they need a parser in the Rust code. Keep the schema, the doc, and `examples/` in sync when changing the format. To check the examples against the schema:

```sh
python3 -c "import json,glob,jsonschema; s=json.load(open('schema/problem.schema.json')); [jsonschema.validate(json.load(open(f)), s) for f in glob.glob('examples/*.json') if not f.endswith('.result.json')]; r=json.load(open('schema/result.schema.json')); [jsonschema.validate(json.load(open(f)), r) for f in glob.glob('examples/*.result.json')]"
```
