# Result file format

Solving a [problem file](https://github.com/hillegass/optimizam/blob/main/docs/problem-format.md) produces one JSON result file. Its
structure is validated by
[`schema/result.schema.json`](https://github.com/hillegass/optimizam/blob/main/schema/result.schema.json).

```json
{
  "version": "1.2",
  "status": "not_proven_optimal",
  "sense": "minimize",
  "solutions": [
    { "objective": 930, "variables": { "start": { "1": 0, "2": 84, "3": 31 } } }
  ],
  "objective_bound": 655,
  "gap": 0.2957,
  "stop_reason": "time_limit",
  "statistics": { "engine": "cp", "threads": 8, "time": 60.004, "nodes": 48211 }
}
```

| Field             | Present | Meaning |
|-------------------|---------|---------|
| `version`         | always | The format's version as `"major.minor"`: the writer's, whatever the result holds. optimizam writes `"1.2"`. |
| `status`          | always | How the solve ended. See below. |
| `sense`           | always | The problem's `solve.method`: `"minimize"`, `"maximize"`, or `"satisfy"`. It says which way `objective_bound` bounds. |
| `solutions`       | always | The solutions found. Empty when no solution was found. |
| `objective_bound` | sometimes | When the search stopped without proving its answer. See below. |
| `gap`             | sometimes | With a solution and an `objective_bound`: how far from optimal the solution could be. See below. |
| `stop_reason`     | sometimes | Why the search stopped before it finished. See below. |
| `conflict`        | sometimes | Only with `infeasible`: why. See below. |
| `unbounded`       | sometimes | Only with `unbounded`: where to look. See below. |
| `statistics`      | always | What the solve took. See below. |

Later versions add fields and never change the meaning of one, so a reader
should ignore fields it doesn't know. (The expected results in `examples/`
come from reference solvers, which write only `version`, `status`,
`solutions`, and `objective_bound`.)

## Status

| `status`             | Solutions | Meaning |
|----------------------|-----------|---------|
| `complete`           | 1 or more | Everything requested was found and proven. That means one feasible solution, every feasible solution, one proven-optimal solution, or every optimal solution, depending on `solve.method` and `solve.solutions`. |
| `incomplete`         | 1 or more | Only when `solutions` is `"all"` or a number. Time ran out before all of them were found, or as many as the number asked for were found. Every returned solution is valid: feasible for `satisfy`, proven optimal for `minimize`/`maximize`. More may exist. |
| `not_proven_optimal` | exactly 1 | Only for `minimize`/`maximize`. Time ran out after finding a feasible solution but before proving it optimal (for nonlinear problems: or the iteration limit was reached, or the solver stopped on numerical difficulties). It may or may not be optimal. The returned solution is the best one found. |
| `locally_optimal`    | exactly 1 | Only for `minimize`/`maximize` with nonlinear terms. The solution satisfies the constraints and the local optimality (KKT) conditions, within tolerance: no nearby feasible point is better. The search for the global optimum stopped (time or node limit) before proving it, or wasn't run (`--engine=nlp`); a better solution may exist elsewhere, and `objective_bound` (if given) bounds the optimum. (Proven global optima are `complete`.) |
| `infeasible`         | 0 | Proven: no solution satisfies the constraints. |
| `locally_infeasible` | 0 | Only for problems with nonlinear terms. No feasible point was found, and the search converged to a point that locally minimizes the constraint violation. A feasible solution may still exist elsewhere, and another start may find it. |
| `unbounded`          | 0 | Only for `minimize`/`maximize`. The objective can be improved without limit. |
| `timeout`            | 0 | Time ran out before any feasible solution was found (for nonlinear problems: or the solver stopped for one of the reasons above). Whether one exists is unknown. |

When `solutions` is `"one"`, a run that finds a solution ends as
`complete`, `locally_optimal`, or `not_proven_optimal`. Errors in the problem file itself, such as
a parse error, are reported separately and don't produce a result file.

### `objective_bound` and `gap`

When the search stops without proving its answer, it reports the best bound
it proved on the optimal value: an upper bound when maximizing, a lower bound
when minimizing (`sense` says which). With `not_proven_optimal` or
`locally_optimal`, the true optimum lies between the solution's `objective`
and the bound, and `gap` is their distance relative to the objective:

```text
gap = |objective - objective_bound| / max(1, |objective|)
```

A gap of 0.02 means the solution is within 2% of optimal. With `timeout`
there is no solution and so no gap, but the bound still says how good any
solution could be. A proven result (`complete`) has neither field: there is
nothing left between the solution and the optimum. The CP engine's bounds
come from propagation alone and are often weak on scheduling problems, so
its gap can overstate how far from optimal a schedule is.

### `stop_reason`

Why the search stopped before it finished; absent when it ran to its end
(always so for `complete`, `infeasible`, and `unbounded`).

| `stop_reason`    | Meaning |
|------------------|---------|
| `time_limit`     | The time limit (`--time`) was reached. |
| `canceled`       | The run was interrupted (Ctrl-C or a signal; in Rust, a `CancelToken`). |
| `callback`       | In Rust: a progress callback asked for the stop. |
| `node_limit`     | The global nonlinear search reached its node limit, which applies when there is no time limit. |
| `no_progress`    | The global nonlinear search stopped because its bound had stopped improving. |
| `solution_limit` | As many solutions as `"solutions": N` asked for were found. |

### `statistics`

- `engine`: the engine the problem went to (what `--engine=auto` chose):
  `"milp"`, `"cp"`, `"global"` (the global search for nonlinear problems),
  `"nlp"` (the local nonlinear solve of `--engine=nlp`), or `"presolve"` when
  the answer was found before any of them ran.
- `threads`: the threads its search could use.
- `time`: seconds from the start of the solve (after the problem file was
  read) to the result. With a time limit it can be somewhat more than the
  limit.
- `nodes`: the size of the search, in what the engine branches on:
  branch-and-bound nodes for `milp` (over all threads and restarts),
  conflicts for `cp`, boxes for `global`. Left out for `nlp`, which has no
  tree.

Two runs of one problem give different `time`s, and with more than one
thread different `nodes`.

### `conflict`

With `infeasible`, the result may say why:

```json
{
  "version": "1.2",
  "status": "infeasible",
  "sense": "minimize",
  "solutions": [],
  "conflict": { "constraints": ["floor[1]", "floor[2]", "budget"], "minimal": true }
}
```

- `constraints`: constraints that can't hold together, given the variables'
  bounds and types. An instance of an indexed constraint is written like
  `capacity[north]`; a constraint without a name is called by its place in
  the problem's list, counting from 0: `constraints[3]`.
- `variables` (left out when empty): variables whose own bounds or domain
  leave them no value, such as `"lower": 5, "upper": 1`, written like `x` or
  `x[north,3]`. Each is a cause by itself.
- `minimal`: true if every member is needed, so that without any one of them
  the rest can hold. Relaxing one member is then enough to get past this
  conflict (the problem may have others).

A plain run reports only what can be read off the problem: variables with no
value left, and constraints that can't hold within their variables' bounds,
which includes a constant one such as `0 >= 1` (what a sum whose condition
matches nothing gives). Several of those are each a cause alone, so `minimal`
is then false. With `--explain`, a search follows when there is nothing of
that kind: it solves the problem without each constraint in turn (the
instances of an indexed constraint together first) and keeps out the ones
whose absence leaves the rest infeasible. That takes many solves; at the time
limit it stops and reports what is left, with `minimal` false. The objective
plays no part.

### `unbounded`

With `unbounded`, `variables` lists the variables that have an objective
coefficient and no bound in the direction that improves the objective. One
of them is usually missing a bound:

```json
{ "version": "1.2", "status": "unbounded", "solutions": [], "unbounded": { "variables": ["batches"] } }
```

The field is left out when there is no such variable (the objective is then
unbounded through its nonlinear terms).

## Solutions

- `objective`: the objective value of the solution. It is present exactly when
  the problem's method is `minimize` or `maximize`.
- `variables`: a value for every variable in the problem, keyed by name.
  - A scalar variable maps to its value.
  - An indexed variable maps to a table, nested one object level per index
    set and keyed by element. This is the same layout as parameter `values`
    in the problem file, and integer elements are written as strings
    (`"q": {"1": {"1": 0, "2": 1, ...}, ...}`).
  - Integer and binary values are written as JSON integers.
  - A symbolic set-valued variable (format 1.1) has an element of its set as
    its value, written as in the set: a string, or an integer if the set mixes
    strings and integers (`"machine": {"t1": "mill", "t2": "lathe"}`).
  - Continuous values are written as JSON numbers, and are subject to the
    solver's numerical tolerance.

The order of solutions is unspecified.

## Examples

Each example problem has a matching result file: `examples/<name>.result.json`.
