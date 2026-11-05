# Command line

```sh
optimizam [--time=SECONDS] [--engine=auto|mip|cp|nlp] PROBLEM RESULT
```

| Argument         | Meaning |
|------------------|---------|
| `PROBLEM`        | Path of the [problem file](problem-format.md) to read, or `-` to read it from stdin. |
| `RESULT`         | Path to write the [result file](result-format.md) to, or `-` to write it to stdout. An existing file is overwritten. |
| `--time=SECONDS` | Optional time limit: a positive number of seconds; decimals such as `0.5` are allowed. `--time 50` is accepted too. The clock starts when the program starts, so reading and parsing the problem count against the limit. |
| `--engine=ENGINE` | Optional: `auto` (default), `mip`, `cp`, or `nlp`. See below. |
| `-h`, `--help`    | Print help and exit (`-h` is a summary; `--help` adds the statuses, exit codes, and examples). |
| `-V`, `--version` | Print the version and exit. |

Both positional arguments are required, and both can be `-` at the same time:

```sh
optimizam --time=50 problem.json result.json
optimizam problem.json -                    # print the result
cat problem.json | optimizam --time=5 - -   # pipe in, pipe out
```

## Time limit

- If `--time` is given and the limit is reached, the solver stops and writes
  whatever it has. The status is `timeout`, `not_proven_optimal`, or
  `incomplete`, as described in [result-format.md](result-format.md#status).
- If `--time` is omitted, there is no limit. The solver runs until it finishes,
  so those three statuses never occur.

## Engines

- `mip`: LP-based branch and bound (with presolve, cuts, and heuristics). It
  solves every problem in scope.
- `cp`: lazy clause generation, a CDCL search over integer bounds as in
  OR-Tools CP-SAT. It needs every variable to be integer with finite bounds
  (after presolve), and every row's coefficients to scale to integers. Asking
  for it on a problem it can't take is an error.
- `nlp`: an interior point method (after IPOPT) alone, for continuous
  problems with nonlinear terms or not: fast, and it finds a local optimum,
  reported as `locally_optimal`, or `complete` when it can prove it global
  (a linear problem, `satisfy`, or a root relaxation whose bound reaches the
  local optimum). It needs every variable to be continuous.
- `auto` solves problems with nonlinear terms globally: spatial branch and
  bound, with integer variables allowed, local solves for good points, and
  linear relaxations for bounds. Finishing proves the answer optimal (within
  a relative gap of 1e-4) or the problem infeasible: `complete` or
  `infeasible`. Stopped early (by the time limit; without one, after 2000
  nodes; or when the bound stops improving), it reports the best point as
  `locally_optimal` or `not_proven_optimal`, with an `objective_bound`. So a
  time limit is used to prove more, not only to find a point. Otherwise
  `auto` uses `cp` for problems it can take that are `satisfy`, `"all"`, or
  use a format 1.1 constraint kind such as `all_different`; and `mip`
  otherwise.

`mip` and `cp` give the same answers; they differ in speed. With `cp`, a time
limit on an optimization problem gives `not_proven_optimal` without an
`objective_bound`, since CP has no LP bound to report. `mip` and `cp` can't
solve problems with nonlinear terms, and asking for them is an error.

## Exit codes and errors

| Code | Meaning |
|------|---------|
| 0    | A result file was written. That includes `infeasible`, `unbounded`, and `timeout` results: these are answers, not failures. |
| 1    | The problem couldn't be read or is invalid, or the result couldn't be written. |
| 2    | Bad command-line usage, such as a missing argument or a malformed `--time` value. |

Error messages go to stderr, never to stdout, so `RESULT` set to `-` always
gives clean JSON. When the exit code is nonzero, no result file is written.
