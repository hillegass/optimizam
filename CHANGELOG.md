# Changelog

All notable changes to optimizam are recorded here. The project follows
[semantic versioning](https://semver.org); before 1.0, minor versions may
change the API.

## Unreleased

- `CancelToken` and `Options::cancel_on`: stop a solve from another thread.
  It returns normally, with the best solution found so far, as if its time
  limit had been reached.
- `Options::on_progress`: a callback with each better solution and, about
  twice a second, the bound or a heartbeat (`Progress`: objective, bound,
  gap, the best solution, and a result file for it). Returning
  `Control::Stop` stops the solve, for example at a gap.
- The CP engine reports a bound when stopped early (in results and progress
  events), from propagation at decision level 0: valid, but often weak.
- Index sets and variable families: `Set` (named, of integers or strings),
  `Model::add_family` and `add_family_with` (per-element specs), and
  `VarFamily`, indexed by keys such as `x[("north", 3)]`; results report
  families as tables keyed by element.
- Symbolic variables: `VarSpec::values` with a set of strings,
  `Variable::is("mill")` literals, `Solution::element`.
- Lookups and editing: `Model::family`, `var_named`, `constraint_named`,
  `set_bounds`, and `remove`.
- Breaking: `Variable::is` and `is_not` take any element (`impl
  Into<Element>`), and `Literal` is no longer `Copy`.
- `Model::to_json` writes integer-keyed families over their sets, and now
  rejects models that solving would reject.
- Breaking: the MILP engine is `Engine::Milp` and `--engine=milp` (it was
  `Mip`/`mip`, which suggests mixed-integer nonlinear problems too). The
  command still accepts `--engine=mip`.
- Problem format 1.3: string literals in expressions, in single quotes
  (`x['north', 3]`, `"enforce_if": "machine[t] == 'mill'"`; `'o''hare'`
  for a quote). `Model::to_json` uses them for string keys and comparisons
  with strings, writing version 1.3 only then.
- Builder literals on an element outside a symbolic variable's set follow
  the format: `is` never holds and `is_not` always does (they were errors).
- `SolveResult::stop_reason`: why a search stopped early (`TimeLimit`,
  `Canceled`, `Callback`, `NodeLimit`, `NoProgress`).
- The command's `--incumbents=FILE` keeps `FILE` up to date with the best
  solution so far (written whole, so a killed run leaves a valid result),
  and `--progress` writes progress to stderr as JSON lines.
- The command handles Ctrl-C, SIGTERM, and SIGHUP: the solve stops and the
  result is written with the best solution found (exit code 0). A second
  Ctrl-C quits at once (exit code 130). New dependency of the command (not
  of the library): `ctrlc`.

## 0.1.0 (2026-11-09)

The first release on crates.io, as a library and a command.

- The library API: `Model` (variables from `var()`, expressions with the
  arithmetic operators and `ln`, `exp`, `sqrt`, the trigonometric functions,
  and powers; constraints from `constraint!` or `leq`/`geq`/`eq`/`between`;
  `all_different`, `table`, `element`, `min_of`, `max_of`, `abs_of`,
  intervals with `no_overlap` and `cumulative`, and `Constraint::only_if`),
  `Problem` for JSON problem files, `Options`, and `SolveResult`/`Solution`.
- `Model::to_json` writes a model as a problem file (format 1.2).
- The `optimizam` command, behind the default `cli` feature. Its behavior is
  unchanged.
- The solver: LP/MILP (dual simplex, presolve, cuts, heuristics, branch and
  bound), CP (lazy clause generation with native scheduling and
  `all_different` propagators), and global nonlinear optimization (spatial
  branch and bound with an interior point method).
