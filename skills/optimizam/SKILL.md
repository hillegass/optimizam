---
name: optimizam
description: Solve optimization and constraint problems with the optimizam command-line solver. Use when the user wants to optimize, schedule, assign, allocate, pack, route, plan under constraints, find a feasible configuration, or enumerate every solution of a puzzle (linear programs, mixed-integer programs, and constraint programming problems such as job shops, timetables, Sudoku, or N-queens). Formulate the problem as an optimizam JSON problem file, run `optimizam`, and explain the result in the user's terms.
---

# Solving problems with optimizam

optimizam reads a JSON problem file and writes a JSON result file. It solves
linear programs (LP), mixed-integer programs (MILP), and constraint
programming (CP) problems, choosing an engine automatically.

## Workflow

1. **Check the tool**: `command -v optimizam`. If it's missing, tell the user
   to install it (see the README at https://github.com/hillegass/optimizam:
   clone it, then `cargo install --path .` in the checkout).
2. **Formulate** before writing JSON: name the index sets, the data
   (parameters), the decision variables with their types and bounds, the
   constraints, and the objective (or `satisfy` if any feasible answer will
   do). Confirm anything ambiguous with the user, such as whether the goal is
   cheapest, fastest, or just feasible.
3. **Write the problem file** (format below). Keep the data in `parameters`
   rather than hard-coding numbers in expressions, so the model reads clearly.
4. **Run it** with a time limit:
   `optimizam --time=60 problem.json result.json` (or `-` for stdout).
5. **Read the result** and check its `status` before using the values.
6. **Explain** the answer in the user's terms (who does what, when, at what
   cost), not as raw variable dumps. Mention whether it is proven optimal.
7. **On an error** (exit code 1), read the message on stderr. It names the
   constraint instance and the problem; fix the file and rerun.

## Problem file format

```json
{
  "version": "1.1",
  "sets": { "ITEMS": ["gold", "silver", "gem"], "DAYS": { "from": 1, "to": 7 } },
  "parameters": {
    "capacity": 7,
    "weight": { "index": ["ITEMS"], "values": { "gold": 4, "silver": 3, "gem": 2 } },
    "value":  { "index": ["ITEMS"], "values": { "gold": 10, "silver": 7, "gem": 5 }, "default": 0 }
  },
  "variables": [
    { "name": "take", "type": "binary", "index": ["ITEMS"] }
  ],
  "constraints": [
    { "name": "fits", "expr": "sum(i in ITEMS) weight[i] * take[i] <= capacity" }
  ],
  "solve": { "method": "maximize", "objective": "sum(i in ITEMS) value[i] * take[i]" }
}
```

- `version`: `"1.0"`; `"1.1"` when using the constraint kinds, set-valued
  domains, or intervals below; `"1.2"` when using functions (`ln`, `exp`, ...)
  or non-integer powers.
- `sets`: a list of elements (strings or integers) or an inclusive integer
  range `{"from": a, "to": b}`.
- `parameters`: a number, or a table `{"index": [sets], "values": {...},
  "default": n}` nested one JSON object level per index set. JSON keys are
  strings, so integer elements are written `"1"`.
- `variables`: `type` is `continuous`, `integer`, or `binary`; `index` lists
  sets (omit for a scalar). Bounds: `lower` / `upper` as a number or the name
  of a parameter indexed by the **same** sets. There is no implicit lower
  bound of 0. An integer variable may instead have a `domain`: ranges
  `[[1, 3], [7, 9]]`, or the name of a set (its values are the set's
  elements; a set with strings makes the variable **symbolic**: its values are
  names such as `"mill"`).
- `solve`: `method` is `minimize`, `maximize`, or `satisfy` (no objective);
  `solutions` is `"one"` (default) or `"all"` (every feasible solution, or
  every optimal one). `"all"` needs every variable integer or binary.

### Expressions

- Linear only: `3 * x[i] + y / 2 - 4` is fine; `x * y`, `x ^ 2`, `1 / x`,
  `ln(x)` (products, powers, or functions of variables) are rejected.
- Constants may use `^` with any exponent and the functions `ln`, `exp`,
  `sqrt`, `sin`, `cos`, `tan`, `atan`, `tanh` (radians; `ln` is the natural
  logarithm, and there is no `log`): `exp(rate[i] * years) * x[i]` and
  `(1 + r)^2.5` are fine, and are evaluated when the model is built.
- `sum(i in I, j in J: condition) term`, where the condition filters index
  combinations (`i != j`, `t > 1`, `cost[i,j] > 0`, with `and`, `or`, `not`).
  The body of a sum is one term: write `sum(i in I) (a[i] * x[i] + b[i])`.
- Index arithmetic: `x[t-1]`, `x[i+1]`.
- There are **no string literals**. Refer to an element of a string set
  through a bound index: `"forall": "m in MACHINES: slow[m] == 1"` then use
  `m`, or select it with a 0/1 parameter.

### Constraints

Each constraint has optional `name`, `description`, and `forall` (bindings
like a sum's, which repeat the constraint), and exactly one kind:

| Kind | Example | Meaning |
|---|---|---|
| `expr` | `"sum(p in P) use[r,p] * make[p] <= avail[r]"` | a linear relation (`<=`, `>=`, `==`) |
| `expr` + `enforce_if` | `"expr": "make[p] <= cap", "enforce_if": "on[p]"` | holds only when the literals hold: `b`, `not b`, `v == e`, `v != e`, joined by `and` |
| `all_different` | `"[col[r] + r \| r in N]"` | the items take pairwise different values |
| `element` | `{"index": "machine[t]", "array": "[cost[t, m] \| m in MACHINES]", "value": "c[t]"}` | value = the array item at the index (indexed by set elements; several indexes allowed) |
| `table` | `{"vars": "[x, y]", "tuples": [[1, 2], [2, 1]], "forbidden": false}` | the variables equal one of the tuples (or none, with `forbidden`) |
| `min` / `max` | `{"value": "makespan", "of": "[end[j] \| j in JOBS]"}` | value = min / max of the list |
| `abs` | `{"value": "dev[t]", "of": "make[t] - demand[t]"}` | value = absolute value |
| `no_overlap` | `"[task[j] \| j in JOBS: machine[j] == m]"` | the intervals don't overlap |
| `cumulative` | `{"intervals": "[task[j] \| j in J]", "demands": "[crew[j] \| j in J]", "capacity": "workers"}` | the demands of the running intervals never exceed the capacity |

Lists are `"[expr | bindings]"` or `"[a, b, c]"`.

### Intervals (scheduling)

```json
"intervals": [
  { "name": "task", "index": ["JOBS"], "start": "begin", "duration": "length", "presence": "done" }
]
```

`start` and `duration` are a number or the name of a variable or parameter
indexed like the interval; `presence` (optional) is a binary variable. A
present interval occupies [start, start + duration).

## Running and results

```sh
optimizam --time=60 problem.json result.json
optimizam --time=60 problem.json -            # print the result
optimizam --help                              # statuses, exit codes, options
```

The result:

```json
{ "version": "1.0", "status": "complete",
  "solutions": [ { "objective": 17, "variables": { "take": { "gold": 1, "silver": 1, "gem": 0 } } } ] }
```

| `status` | Meaning |
|---|---|
| `complete` | proven: one feasible or optimal solution, or all of them with `"all"` |
| `not_proven_optimal` | time ran out; the solution is feasible but maybe not optimal (`objective_bound` bounds the optimum when available) |
| `incomplete` | `"all"` and time ran out; the solutions listed are valid, more may exist |
| `infeasible` | no solution exists: check the constraints for a modeling error, or tell the user what can't be satisfied |
| `unbounded` | the objective can improve forever: usually a missing bound or constraint |
| `timeout` | no solution found in time: raise `--time`, tighten bounds, or simplify |

Values of symbolic variables are set elements (strings). Integer and binary
values are JSON integers.

## Modeling tips

- Give integer variables finite bounds (needed for `all_different`,
  `element`, `table`, intervals, and the CP engine) and keep them as tight as
  the problem allows.
- Prefer `enforce_if` to hand-written big-M constraints, and intervals with
  `no_overlap` / `cumulative` for scheduling.
- For "pick one of these options", use a symbolic variable over a set of
  names with `element` for costs, or binaries with a `sum == 1` row.
- `--engine` is `auto` by default (CP for satisfy, `"all"`, and problems using
  the constraint kinds; MILP otherwise). If a model is slow, try the other
  engine with `--engine=mip` or `--engine=cp`.
- Domains and time horizons are limited to 10,000 values each for the
  constraint kinds; continuous variables can't be used with `"all"`.

## Reference

Read these when you need more detail than this page gives:

- `references/problem-format.md`: the full problem format and grammar.
- `references/result-format.md`: the result format.
- `references/cli.md`: the command line, engines, and time limits.
- `references/examples/`: worked problems with their expected results, for example
  `knapsack.json` (binary, all optima), `production.json` (LP),
  `facility_location_milp.json` (MILP), `sudoku.json` and
  `n_queens_alldiff.json` (all_different), `machine_assignment.json`
  (symbolic variables, element, table, enforce_if), `tsp_tour.json`
  (2-D element), `job_shop.json` (intervals, no_overlap), and
  `crew_schedule.json` (optional intervals, cumulative), and
  `compound_growth.json` (functions of parameters in coefficients).
