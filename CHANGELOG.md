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
- `SolveResult::stop_reason`: why a search stopped early (`TimeLimit`,
  `Canceled`, `Callback`, `NodeLimit`, `NoProgress`).
- The command's `--incumbents=FILE` keeps `FILE` up to date with the best
  solution so far (written whole, so a killed run leaves a valid result),
  and `--progress` writes progress to stderr as JSON lines.

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
