# Command line

```sh
optimizam [--time=SECONDS] [--engine=auto|milp|cp|nlp] [--threads=N] [--incumbents=FILE] [--progress] [--explain] PROBLEM RESULT
```

| Argument         | Meaning |
|------------------|---------|
| `PROBLEM`        | Path of the [problem file](problem-format.md) to read, or `-` to read it from stdin. |
| `RESULT`         | Path to write the [result file](result-format.md) to, or `-` to write it to stdout. An existing file is overwritten. |
| `--time=SECONDS` | Optional time limit: a positive number of seconds; decimals such as `0.5` are allowed. `--time 50` is accepted too. The clock starts when the program starts, so reading and parsing the problem count against the limit. |
| `--engine=ENGINE` | Optional: `auto` (default), `milp`, `cp`, or `nlp`. See below. |
| `--threads=N`     | Optional: how many threads a search may use. The default (also `0`) is one per core, at most 8. See below. |
| `--incumbents=FILE` | Optional: keep `FILE` up to date with the best solution so far. See below. |
| `--explain`       | Optional: if the problem is infeasible, look for a small set of constraints that can't hold together. See below. |
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
  bounds are often weak (see below). Warnings about the problem file come
  first, as `{"event":"warning","message":"..."}`, so that every line of
  stderr is a JSON object. The last line says how the solve ended:
  `{"event":"done","elapsed":60.0,"status":"not_proven_optimal","objective":3173,"bound":3190.5,"gap":0.0055,"stop_reason":"time_limit"}`.

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

## Threads

By default the searches use one thread per core of the machine, at most 8.
`--threads=N` sets the number (`--threads=0` is the default again). With more
than one thread:

- `cp` runs a portfolio when it optimizes or looks for one solution: N
  searches with different settings side by side, sharing the best solution
  and the short clauses they learn, the first proof ending them all.
- `milp` shares one branch-and-bound tree among N workers, each with an LP
  solver of its own, and solves the root's strong-branching LPs side by side.
- The global nonlinear search shares its tree of boxes the same way, and
  tightens the root's bounds a share of the variables per thread.

Hard problems are typically solved several times faster than with one
thread. The answers are as valid, but which of several equally good solutions
is returned, and how long the run takes, vary from run to run. `--threads=1`
makes a run repeat exactly (unless the time limit cuts it off); use it for
that, or when several solves run side by side. LPs, `--engine=nlp`'s local
solve, and enumerating every solution (`"all"`) use one thread whatever
`--threads` says.

`milp` and `cp` give the same answers; they differ in speed. With `cp`, a time
limit on an optimization problem gives `not_proven_optimal` with an
`objective_bound` from the search's propagation rather than an LP. It is
valid but often far from the optimum (on a job shop, about the busiest
machine's load), so the gap overstates how far from optimal the solution is. `milp` and `cp` can't
solve problems with nonlinear terms, and asking for them is an error.

## Infeasible problems

An `infeasible` result says why when the cause can be read off the problem: a
variable whose bounds leave it no value, or a constraint that can't hold
within its variables' bounds. Its [`conflict`](result-format.md#conflict)
field names them.

When several constraints clash only together, `--explain` finds them: after
an infeasible solve it solves the problem again without each constraint in
turn (the instances of an indexed constraint together first), keeping out
those whose absence leaves the rest infeasible. What remains is a set of
constraints that can't hold together, each of them needed for that:

```sh
optimizam --explain --time=60 problem.json -
```

```json
{ "version": "1.2", "status": "infeasible", "solutions": [],
  "conflict": { "constraints": ["floor[1]", "floor[2]", "budget"], "minimal": true } }
```

It takes about one solve per constraint, within the time limit; cut short,
it reports what is left with `"minimal": false`. Give the constraints names,
or the conflict can only call them by their places, `constraints[3]` and
`constraints[7]`.

An `unbounded` result lists, under [`unbounded`](result-format.md#unbounded),
the variables with an objective coefficient and no bound in the improving
direction: usually one of them is missing a bound.

## Exit codes and errors

| Code | Meaning |
|------|---------|
| 0    | A result file was written. That includes `infeasible`, `unbounded`, and `timeout` results (these are answers, not failures), and results written after Ctrl-C. |
| 130  | Interrupted twice (or while reading the problem): nothing was written. |
| 1    | The problem couldn't be read or is invalid, or the result couldn't be written. |
| 2    | Bad command-line usage, such as a missing argument or a malformed `--time` value. |

An error in the problem file says where it is, as a path into the file
counting from 0, with the name of the variable or constraint it is in:

```
error: invalid problem file: variables[1].type (carrier): expected "continuous", "integer", or "binary", found "int"
error: constraint capacity: unknown name gold: gold is an element of ITEMS, so write it in single quotes: 'gold'
error: constraints[4][3]: y[4] is outside the variable's index sets: 4 isn't in T (1 to 3); if the subscript is computed, give the forall or sum a condition, as in "t in T: t < 3"
```

A constraint without a name is called by its place (`constraints[4]`, and
`constraints[4][3]` for its instance at 3).

What the file says but probably doesn't mean is a warning, also on stderr,
and the solve goes on (the [format](problem-format.md#warnings) lists them):

```
warning: constraint late: its forall ("t in T: t > 3") matches nothing, so it adds no constraints
warning: parameter bonus is never used
```

Error messages go to stderr, never to stdout, so `RESULT` set to `-` always
gives clean JSON. When the exit code is nonzero, no result file is written.
