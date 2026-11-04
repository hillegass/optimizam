# Command line

```sh
optimizam [--time=SECONDS] [--engine=auto|mip|cp] PROBLEM RESULT
```

| Argument         | Meaning |
|------------------|---------|
| `PROBLEM`        | Path of the [problem file](problem-format.md) to read, or `-` to read it from stdin. |
| `RESULT`         | Path to write the [result file](result-format.md) to, or `-` to write it to stdout. An existing file is overwritten. |
| `--time=SECONDS` | Optional time limit: a positive number of seconds; decimals such as `0.5` are allowed. `--time 50` is accepted too. The clock starts when the program starts, so reading and parsing the problem count against the limit. |
| `--engine=ENGINE` | Optional: `auto` (default), `mip`, or `cp`. See below. |

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
- `auto` uses `cp` for `satisfy` and `"all"` problems that it can take, and
  `mip` otherwise.

Both engines give the same answers; they differ in speed. With `cp`, a time
limit on an optimization problem gives `not_proven_optimal` without an
`objective_bound`, since CP has no LP bound to report.

## Exit codes and errors

| Code | Meaning |
|------|---------|
| 0    | A result file was written. That includes `infeasible`, `unbounded`, and `timeout` results: these are answers, not failures. |
| 1    | The problem couldn't be read or is invalid, or the result couldn't be written. |
| 2    | Bad command-line usage, such as a missing argument or a malformed `--time` value. |

Error messages go to stderr, never to stdout, so `RESULT` set to `-` always
gives clean JSON. When the exit code is nonzero, no result file is written.
