# Result file format

Solving a [problem file](problem-format.md) produces one JSON result file. Its
structure is validated by
[`schema/result.schema.json`](../schema/result.schema.json).

```json
{
  "version": "1.0",
  "status": "complete",
  "solutions": [
    { "objective": 17, "variables": { "take": { "gold": 1, "silver": 1, "bronze": 0, "gem": 0 } } },
    { "objective": 17, "variables": { "take": { "gold": 1, "silver": 0, "bronze": 1, "gem": 0 } } }
  ]
}
```

| Field             | Required | Meaning |
|-------------------|----------|---------|
| `version`         | yes | Format version as `"major.minor"`: `"1.0"`, or `"1.1"` when the status is `locally_optimal` or `locally_infeasible` (added in 1.1). |
| `status`          | yes | How the solve ended. See below. |
| `solutions`       | yes | The solutions found. Empty when no solution was found. |
| `objective_bound` | no  | Only with `not_proven_optimal`. See below. |

## Status

| `status`             | Solutions | Meaning |
|----------------------|-----------|---------|
| `complete`           | 1 or more | Everything requested was found and proven. That means one feasible solution, every feasible solution, one proven-optimal solution, or every optimal solution, depending on `solve.method` and `solve.solutions`. |
| `incomplete`         | 1 or more | Only when `solutions` is `"all"`. Time ran out before all of them were found. Every returned solution is valid: feasible for `satisfy`, proven optimal for `minimize`/`maximize`. More may exist. |
| `not_proven_optimal` | exactly 1 | Only for `minimize`/`maximize`. Time ran out after finding a feasible solution but before proving it optimal. It may or may not be optimal. The returned solution is the best one found. |
| `locally_optimal`    | exactly 1 | Only for `minimize`/`maximize` with nonlinear terms. The solution satisfies the constraints and the local optimality (KKT) conditions, within tolerance: no nearby feasible point is better. A better solution may exist elsewhere, and another start may find it. Added in 1.1. |
| `infeasible`         | 0 | Proven: no solution satisfies the constraints. |
| `locally_infeasible` | 0 | Only for problems with nonlinear terms. No feasible point was found, and the search converged to a point that locally minimizes the constraint violation. A feasible solution may still exist elsewhere, and another start may find it. Added in 1.1. |
| `unbounded`          | 0 | Only for `minimize`/`maximize`. The objective can be improved without limit. |
| `timeout`            | 0 | Time ran out before any feasible solution was found. Whether one exists is unknown. |

When `solutions` is `"one"`, a run that finds a solution ends as
`complete`, `locally_optimal`, or `not_proven_optimal`. Errors in the problem file itself, such as
a parse error, are reported separately and don't produce a result file.

### `objective_bound`

With `not_proven_optimal`, the solver may report the best bound it proved on
the optimal value. That's an upper bound when maximizing and a lower bound
when minimizing. The true optimum lies between the returned solution's
`objective` and this bound, so the difference between them is how far from
optimal the solution could be.

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
