---
name: optimizam
description: Solve optimization and constraint problems with the optimizam command-line solver. Use when the user wants to optimize, schedule, assign, allocate, pack, route, plan under constraints, fit a model to data, find a feasible configuration, or enumerate every solution of a puzzle (linear programs, mixed-integer programs, continuous nonlinear programs such as engineering design, portfolio, curve fitting, or equilibrium problems, and constraint programming problems such as job shops, timetables, Sudoku, or N-queens). Formulate the problem as an optimizam JSON problem file, run `optimizam`, and explain the result in the user's terms.
---

# Solving problems with optimizam

optimizam reads a JSON problem file and writes a JSON result file. It solves
linear programs (LP), mixed-integer programs (MILP), constraint programming
(CP) problems, and continuous nonlinear programs (NLP), choosing an engine
automatically.

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
7. **On an error** (exit code 1), read stderr: one `error:` line for each
   thing wrong, all found in the one run. Each names the place
   (`variables[1].type (carrier)`, `constraint capacity[north]`) and the
   problem, often with what to write instead. Fix them all, then rerun.
   To say which constraints limit the answer, rerun with `--constraints`:
   each solution then lists every comparison constraint's `slack` (0 means
   it binds), and for a linear program its `dual` (what one more unit of
   its right side is worth).
8. **On a warning** (a `warning:` line on stderr; the run still succeeds),
   check the model before trusting the result: a `forall` that matched
   nothing, a parameter or variable that nothing uses, or an index that hides
   another name usually means the file doesn't say what was meant.

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
  or non-integer powers; `"1.3"` when using string literals, the index
  functions (`mod`, `card`, `pos`, ...), or `abs`/`min`/`max` in expressions.
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
  `solutions` is `"one"` (default), `"all"` (every feasible solution, or
  every optimal one), or a number N (at most N of them: `complete` if those
  are all there are, `incomplete` if it stopped at N). `"all"` and N need
  every variable integer or binary. To learn whether a solution is the only
  one, ask for 2: one solution and `complete` means it is
  (`sudoku_unique.json`).

### Expressions

- Arithmetic: `+ - * /`, `^` with any exponent (`x^2`, `x^-1`, `x^2.5`,
  `2^x`), and the functions `ln`, `exp`, `sqrt`, `sin`, `cos`, `tan`,
  `atan`, `tanh` (radians; `ln` is the natural logarithm, and there is no
  `log`). Functions of constants (`exp(rate[i] * years) * x[i]`) are
  evaluated when the model is built and are fine anywhere.
- Nonlinear terms in variables (`x * y`, `x^2`, `1 / x`, `ln(x)`) need
  `"version": "1.2"`, and may appear only in `expr` constraints (without
  `enforce_if`) and the objective. Integer variables are allowed. The solver
  searches for the global optimum; see the statuses below.
- `sum(i in I, j in J: condition) term`, where the condition filters index
  combinations (`i != j`, `t > 1`, `cost[i,j] > 0`, with `and`, `or`, `not`).
  The body of a sum is one term: write `sum(i in I) (a[i] * x[i] + b[i])`.
- Index arithmetic: `x[t-1]`, `x[i+1]`, and (version `"1.3"`) functions of
  data, sets, and indexes: `mod(a, b)` and `div(a, b)` (rounded down),
  `floor`, `ceil`, `card(S)`, `first(S)`, `last(S)`, `at(S, k)` (the k-th
  element, from 1), `pos(i)` (the place of a bound index in its set), and
  `pos(e, S)`. The next element, wrapping around: `x[mod(t, card(T)) + 1]`;
  every day but the last: `d in DAYS: pos(d) < card(DAYS)`. Named elements
  have no order: compare `pos(d) < pos('tue', DAYS)`, not `d < 'tue'`.
- `abs(e)`, `min(a, b, ...)`, `max(a, b, ...)`, and over a set
  `max(j in JOBS) end[j]` (version `"1.3"`) take numbers or affine
  expressions with variables, in `expr` constraints, lists, and the
  objective: `sum(t in T) abs(make[t] - demand[t])`. The model stays linear,
  and no variable has to be declared for the term. An `abs` or `max` that is
  minimized or bounded above (a `min` maximized or bounded below) needs
  nothing more; anywhere else (`abs(e) >= 2`, an equation) the variables
  inside need finite bounds.
- String literals (version `"1.3"`) name set elements, in **single quotes**
  (no escaping inside the JSON string; double a quote inside: `'o''hare'`):
  `x['north', t]`, `"enforce_if": "machine[t] == 'mill'"`,
  `sum(p in PLANTS: p != 'north') make[p]`. They can't be used in arithmetic.

### Constraints

Each constraint has optional `name`, `description`, and `forall` (bindings
like a sum's, which repeat the constraint), and exactly one kind:

| Kind | Example | Meaning |
|---|---|---|
| `expr` | `"sum(p in P) use[r,p] * make[p] <= avail[r]"` | a linear relation (`<=`, `>=`, `==`); between integer-valued sides also `!=`, `<`, `>` (version `"1.3"`): `color[i] != color[j]`, `start[a] < start[b]` |
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
optimizam --time=600 --incumbents=best.json problem.json result.json   # long run: best.json has the best so far
optimizam --help                              # statuses, exit codes, options
```

The result:

```json
{ "version": "1.2", "status": "complete", "sense": "maximize",
  "solutions": [ { "objective": 17, "variables": { "take": { "gold": 1, "silver": 1, "gem": 0 } } } ],
  "statistics": { "engine": "milp", "threads": 8, "time": 0.000412, "nodes": 1 } }
```

| `status` | Meaning |
|---|---|
| `complete` | proven: one feasible or optimal solution, or all of them with `"all"` |
| `not_proven_optimal` | time ran out; the solution is feasible but maybe not optimal. `gap` says how far from optimal it could be (0.02 is 2%), from `objective_bound`; `stop_reason` says why the search stopped |
| `incomplete` | `"all"` and time ran out, or `"solutions": N` and N were found; the solutions listed are valid, more may exist |
| `infeasible` | no solution exists. If the result has a `conflict`, it names the variables with no value left or the constraints that can't hold; if not, rerun with `--explain` (and a `--time`) to get a small set of constraints that clash. Then fix the model, or tell the user which requirements can't be met together |
| `unbounded` | the objective can improve forever: usually a missing bound or constraint. `unbounded.variables` lists the variables to look at |
| `timeout` | no solution found in time: raise `--time`, tighten bounds, or simplify |
| `locally_optimal` | nonlinear problems: the solution is feasible and no nearby point is better, but the global search ran out of time (or nodes) before proving it best; `objective_bound` says how much better the optimum could be. Say so; a larger `--time` or tighter bounds may prove it |
| `locally_infeasible` | nonlinear problems: no feasible point was found from this start; the problem may be infeasible, or need other `start` values |

For nonlinear problems, `complete` means proven globally optimal (within a
relative gap of 1e-4), and `infeasible` that no feasible point exists.
Finite, realistic bounds on every variable make such proofs possible; with
a variable unbounded inside a nonlinear term, only a local answer is
possible. The global search uses the whole `--time` if it keeps making
progress, so choose the limit accordingly.

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
- `--engine` is `auto` by default (NLP for problems with nonlinear terms; CP
  for satisfy, `"all"`, and problems using the constraint kinds; MILP
  otherwise). If a linear model is slow, try the other engine with
  `--engine=milp` or `--engine=cp`.
- The searches (MILP, global nonlinear, and a portfolio for scheduling and
  other CP optimization problems) use one thread per core, at most 8, so two
  runs can return different, equally good solutions and take different
  times. `--threads=N` sets the number; `--threads=1` makes a run repeat
  exactly. LPs use one thread.
- Nonlinear models: give variables realistic bounds and `start` values
  (`"start": 1` or a parameter name), especially where a function needs a
  positive argument (`ln`, `sqrt`, a divisor): the default start is 0, and
  if the functions can't be evaluated there the run fails with a message
  asking for a start. Prefer smooth formulations: no `abs`, `min`, or `max`
  of nonlinear terms (use the constraint kinds on affine terms instead).
  Mixed-integer nonlinear models are solved by branching on the integers
  too; keep their ranges small.
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
  (2-D element, `mod` and `card`), `job_shop.json` (intervals, no_overlap),
  `crew_schedule.json` (optional intervals, cumulative), `line_fit.json`
  (`abs` in the objective),
  `compound_growth.json` (functions of parameters in coefficients), and the
  nonlinear `hs071.json`, `portfolio_risk.json`, `curve_fit.json`,
  `chemical_equilibrium.json`, `batch_sizing.json` (integers), and
  `disk_infeasible.json`.
