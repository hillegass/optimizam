# Command line

```sh
optimizam [--time=SECONDS] [--engine=auto|milp|cp|nlp] [--incumbents=FILE] [--progress] PROBLEM RESULT
```

| Argument         | Meaning |
|------------------|---------|
| `PROBLEM`        | Path of the [problem file](problem-format.md) to read, or `-` to read it from stdin. |
| `RESULT`         | Path to write the [result file](result-format.md) to, or `-` to write it to stdout. An existing file is overwritten. |
| `--time=SECONDS` | Optional time limit: a positive number of seconds; decimals such as `0.5` are allowed. `--time 50` is accepted too. The clock starts when the program starts, so reading and parsing the problem count against the limit. |
| `--engine=ENGINE` | Optional: `auto` (default), `milp`, `cp`, or `nlp`. See below. |
| `--incumbents=FILE` | Optional: keep `FILE` up to date with the best solution so far. See below. |
| `--progress`      | Optional: write progress to stderr as JSON lines. See below. |
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

## Interrupting

Ctrl-C stops the solve and writes the result file as if the time limit had
been reached: the best solution found, with status `not_proven_optimal` (or
`timeout`, `incomplete`, or `locally_optimal`, as in
[result-format.md](result-format.md#status)), and exit code 0. SIGTERM (what
`kill` and most job schedulers send) and SIGHUP (a closed terminal) do the
same. A second Ctrl-C quits at once, with exit code 130 and no result file.
Interrupts while the problem is still being read quit at once too. SIGKILL
(`kill -9`) can't be caught: use `--incumbents` to keep the best solution
through that.

## Watching a long run

- `--incumbents=FILE` rewrites `FILE` with a result file each time a better
  solution is found (status `not_proven_optimal`, with `objective_bound` when
  the engine has one), and with the final result at the end. Each write goes
  to `FILE.tmp` first and is renamed into place, so `FILE` is always a whole
  result. If the run is killed (Ctrl-C, a closed terminal, a job scheduler's
  limit), `FILE` has the best solution found. Intermediate solutions are
  written only for `minimize` and `maximize` with `"solutions": "one"`; for
  other problems `FILE` gets just the final result.
- `--progress` writes one JSON object per line to stderr: each better
  solution, and about twice a second a better bound or a heartbeat, such as
  `{"event":"solution","elapsed":1.52,"objective":3173,"bound":3190.5,"gap":0.0055}`.
  The keys are `event` (`solution`, `bound`, or `heartbeat`), `elapsed`
  (seconds since the solve started), and, when known, `objective`, `bound`,
  and `gap` (`|objective - bound| / max(1, |objective|)`). The CP engine's
  bounds are often weak (see below).

## Engines

- `milp`: LP-based branch and bound (with presolve, cuts, and heuristics). It
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
  use a format 1.1 constraint kind such as `all_different`; and `milp`
  otherwise.

`milp` and `cp` give the same answers; they differ in speed. With `cp`, a time
limit on an optimization problem gives `not_proven_optimal` with an
`objective_bound` from the search's propagation rather than an LP. It is
valid but often far from the optimum (on a job shop, about the busiest
machine's load), so the gap overstates how far from optimal the solution is. `milp` and `cp` can't
solve problems with nonlinear terms, and asking for them is an error.

## Exit codes and errors

| Code | Meaning |
|------|---------|
| 0    | A result file was written. That includes `infeasible`, `unbounded`, and `timeout` results (these are answers, not failures), and results written after Ctrl-C. |
| 130  | Interrupted twice (or while reading the problem): nothing was written. |
| 1    | The problem couldn't be read or is invalid, or the result couldn't be written. |
| 2    | Bad command-line usage, such as a missing argument or a malformed `--time` value. |

Error messages go to stderr, never to stdout, so `RESULT` set to `-` always
gives clean JSON. When the exit code is nonzero, no result file is written.
