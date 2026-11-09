# Problem file format

A problem is one JSON file. It is validated structurally by
[`schema/problem.schema.json`](https://github.com/hillegass/optimizam/blob/main/schema/problem.schema.json). Expressions are
strings that use the grammar below.

## Top-level fields

| Field         | Required | Meaning |
|---------------|----------|---------|
| `version`     | yes | Format version as `"major.minor"`: `"1.0"`; `"1.1"` for files that use set-valued variables, intervals, or the constraint kinds beyond `expr` (all added in 1.1); `"1.2"` for files that use functions (`ln`, `exp`, ...), powers with non-integer or non-constant exponents, or `start` (added in 1.2). |
| `sets`        | no  | Named index sets: a list (`["a", "b"]`, `[1, 2, 3]`) or an inclusive range (`{"from": 1, "to": 8}`). |
| `parameters`  | no  | Named constants: a number, or `{"index": [sets...], "values": {...}, "default": n}`. `values` is nested one object level per index set, keyed by element. JSON keys are strings, so integer elements are written `"1"`. |
| `variables`   | yes | See below. |
| `intervals`   | no  | Named (optional) time intervals for scheduling constraints. See below. Added in 1.1. |
| `constraints` | yes | See below. Can be empty. |
| `solve`       | yes | What to solve for. See below. |

The overall shape (a `version` field, a `solve` block, and integer domains as
lists of ranges) follows FlatZinc JSON. Unlike FlatZinc JSON and MathOptFormat,
which are flat solver-input formats, this format is model-level: it has index
sets, `sum`, and `forall`.

## Solve

```json
{ "method": "maximize", "objective": "sum(i in ITEMS) value[i] * take[i]", "solutions": "all" }
```

- `method`: `"satisfy"` (feasibility), `"minimize"`, or `"maximize"`.
- `objective`: an expression. It is required for `minimize` and `maximize`
  and not allowed for `satisfy`.
- `solutions`: `"one"` (default) or `"all"`.

| `method`                | `"one"` | `"all"` |
|-------------------------|---------|---------|
| `satisfy`               | any feasible solution | every feasible solution |
| `minimize` / `maximize` | one optimal solution  | every optimal solution  |

This is not the same as MiniZinc's `-a` flag. For optimization problems, `-a`
reports the improving solutions found along the way. `"all"` here means every
solution that reaches the optimal objective value.

`"all"` is only well-defined when the relevant solution set is finite. In
practice that means every variable is binary, or integer with finite bounds,
or else the continuous variables are fully determined by the discrete ones.
A solver should report an error rather than enumerate an infinite set.

## Variables

```json
{ "name": "x", "type": "integer", "index": ["I", "J"], "lower": 0, "upper": "cap" }
```

- `type`: `continuous`, `integer`, or `binary`. Binary variables are in {0, 1} and
  must not declare bounds or a domain.
- `index`: optional list of set names. `["I", "J"]` declares `x[i,j]` for every
  pair. If it is omitted, the variable is a scalar `x`.
- `lower` / `upper`: a number, or the name of a parameter indexed by the same
  sets as the variable, applied element by element. If a bound is omitted, the
  variable is unbounded on that side. There is no implicit `lower: 0`.
- `domain`: integer variables only, used instead of `lower`/`upper`. Either a list
  of inclusive `[low, high]` ranges, sorted and non-overlapping, so a domain can
  have gaps: `[[1,3],[7,9]]` is {1, 2, 3, 7, 8, 9}, and `[[2,2],[5,5]]` is {2, 5};
  or the name of a set, which makes the variable set-valued (see below; added
  in 1.1). The same domain applies to every element of an indexed variable.
  The schema can't check the sorting and overlap rules, so the loader must.
- `start`: optional starting value for local solvers of nonlinear problems,
  a number or a parameter indexed like the variable (the same rule as
  bounds). A start outside the bounds is moved inside them. Solvers that
  don't need one ignore it. Added in 1.2.

### Set-valued variables

```json
{ "name": "machine", "type": "integer", "index": ["TASKS"], "domain": "MACHINES" }
```

A variable whose `domain` names a set takes elements of that set as values.

- If every element of the set is an integer, the variable is an ordinary
  integer variable whose values are those integers: `"domain": "N"` with
  `N = {"from": 1, "to": 8}` is the same as `"domain": [[1, 8]]`.
- If the set has a string element, the variable is *symbolic*. Its values are
  the set's elements, and results report them as such (`"machine": {"t1":
  "mill", ...}`). A symbolic variable can't be used in arithmetic. It may
  appear only:
  - as an `element` index over its set,
  - in an `all_different` list of symbolic variables over the same set,
  - in a `table`, whose tuples then contain the set's elements,
  - in a comparison `v == e` or `v != e`, where `e` is a constant expression
    that evaluates to an element of the set, either as a whole `expr`
    constraint or as an `enforce_if` literal.

The grammar has no string literals, so a symbolic value is referred to
through a bound index: `"forall": "t in TASKS, m in MACHINES", "enforce_if":
"machine[t] == m"`.

## Intervals

Added in 1.1. An interval is a span of time for scheduling constraints
(`no_overlap`, `cumulative`), built from ordinary variables and parameters.

```json
{ "name": "task", "index": ["JOBS"], "start": "begin", "duration": "length", "presence": "scheduled" }
```

- `start` and `duration`: a number, or the name of a variable or parameter
  indexed by the same sets as the interval, applied element by element (the
  same rule as variable bounds). Both must be integer-valued.
- `presence` (optional): the name of a binary variable indexed by the same
  sets. Without it, the interval is always present.
- A present interval occupies [start, start + duration), and its duration must
  be at least 0. An absent interval (presence 0) constrains nothing.
- Intervals have no values of their own, so they don't appear in results.
- In a constraint, `task[j]` refers to one interval.

## Constraints

```json
{ "name": "capacity", "forall": "r in RESOURCES", "expr": "sum(p in PRODUCTS) usage[r,p] * make[p] <= available[r]" }
```

Every constraint has optional `name`, `description`, and `forall` fields, and
exactly one *kind* field, which says what the constraint is:

- `forall` (optional) repeats the constraint once for each binding, using the
  same binding syntax as `sum`.
- `name` (optional) names the constraint. An indexed constraint produces
  instances named `capacity[carpentry]` and so on.

The kinds other than `expr` were added in 1.1. A solver may not support every
kind; it reports an error rather than ignoring a constraint.

### `expr`: a linear (or other) relation

`expr` must contain exactly one relation: `<=`, `>=`, or `==`.

It may carry `enforce_if`, a conjunction of literals: the relation must hold
whenever all the literals are true, and is unconstrained otherwise. A literal
is a binary variable (`setup[p,t]`), its negation (`not idle[t]`), or a
comparison of an integer or set-valued variable with a constant element
(`machine[t] == m`, `shift[w] != 3`):

```json
{ "forall": "p in P, t in T", "expr": "make[p,t] <= capacity", "enforce_if": "setup[p,t]" },
{ "forall": "p in P, t in T", "expr": "make[p,t] == 0", "enforce_if": "not setup[p,t]" }
```

`enforce_if` is decided during the solve. It is different from the `:`
condition in `forall`, which selects index combinations when the model is
built.

### Lists

The other kinds take lists, written as strings in one of two forms:

- a comprehension, `"[x[i] | i in I: cost[i] > 0]"`, using the binding syntax
  of `sum`, in binding order (the last binding varies fastest);
- an explicit list, `"[a, b[2], c + 1]"`.

Lists can only be arguments of these constraint kinds, not part of arithmetic.
Unless stated otherwise, list items are affine expressions: a constant plus
constants times variables.

### `all_different`

```json
{ "name": "rows", "all_different": "[q[r] + r | r in N]" }
```

The listed expressions take pairwise different values. Items are integer-valued
affine expressions (integer or binary variables with integer coefficients), or
all symbolic variables over the same set.

### `element`

```json
{ "forall": "t in TASKS", "element": { "index": "machine[t]", "array": "[cost[t, m] | m in MACHINES]", "value": "c[t]" } }
```

`value` equals the `array` item selected by `index`.

- The array is a comprehension, and its bindings are matched to the indexes in
  order. Several indexes give a multi-dimensional lookup:
  `"index": ["x[t]", "x[t+1]"], "array": "[dist[i, j] | i in N, j in N]"`
  means `value == dist[x[t], x[t+1]]`.
- Each index takes values in its binding's set. An index over an integer set is
  an integer-valued affine expression (`pos[j] + 1`); an index over a symbolic
  set is a symbolic variable over that set.
- A condition on the comprehension restricts the index combinations allowed:
  with `"[cost[f] | f in F: allowed[f]]"`, the index must be an allowed f.
- The array items and `value` are affine expressions; items may contain
  variables.

### `table`

```json
{ "table": { "vars": "[shift[w], rest[w]]", "tuples": [[1, 0], [2, 1], [3, 1]] } }
```

The listed variables, in order, equal one of the tuples. With
`"forbidden": true`, they must equal none of them. Every tuple has one entry
per variable: an integer, or for a symbolic variable an element of its set.
Items are integer or symbolic variables, not other expressions.

### `min`, `max`, `abs`

```json
{ "max": { "value": "makespan", "of": "[begin[j] + length[j] | j in JOBS]" } },
{ "abs": { "value": "dev[d]", "of": "staff[d] - target[d]" } }
```

`value` equals the minimum or maximum of the non-empty list `of`, or the
absolute value of the expression `of`. All are affine expressions.

### `no_overlap`

```json
{ "forall": "m in MACHINES", "no_overlap": "[task[j] | j in JOBS: uses[j] == m]" }
```

The listed intervals don't overlap: for every pair of present intervals a and
b, end(a) <= start(b) or end(b) <= start(a). So intervals may touch, and two
zero-length intervals may share a point, but a zero-length interval can't lie
strictly inside another (as in OR-Tools CP-SAT).

### `cumulative`

```json
{ "cumulative": { "intervals": "[task[j] | j in JOBS]", "demands": "[crew[j] | j in JOBS]", "capacity": "workers" } }
```

At every time t, the demands of the present intervals with start <= t < end
add up to at most `capacity`. `demands` has one integer-valued affine
expression per interval, each at least 0; `capacity` is an integer-valued
affine expression. Zero-length intervals never count.

## Expression grammar

```ebnf
relation   = expr ("<=" | ">=" | "==") expr ;
expr       = term { ("+" | "-") term } ;
term       = factor { ("*" | "/") factor } ;
factor     = "-" factor | power ;
power      = atom [ "^" factor ] ;           (* 1.0, 1.1: the exponent is an integer *)
atom       = number
           | ref
           | func "(" expr ")"               (* added in 1.2 *)
           | "sum" "(" bindings ")" term      (* the body is one term: sum(i in I) a[i]*x[i] *)
           | "(" expr ")" ;
func       = "ln" | "exp" | "sqrt" | "sin" | "cos" | "tan" | "atan" | "tanh" ;
ref        = ident [ "[" expr { "," expr } "]" ] ;
bindings   = binding { "," binding } [ ":" condition ] ;
binding    = ident "in" ident ;              (* index name in set name *)
condition  = disj ;
disj       = conj { "or" conj } ;
conj       = neg { "and" neg } ;
neg        = "not" neg | cmp | "(" condition ")" ;
cmp        = expr ("==" | "!=" | "<" | "<=" | ">" | ">=") expr ;
ident      = /[A-Za-z_][A-Za-z0-9_]*/ ;      (* not: sum, in, and, or, not *)

(* Added in 1.1 *)
list       = "[" expr "|" bindings "]"       (* comprehension *)
           | "[" expr { "," expr } "]" ;     (* explicit *)
literals   = literal { "and" literal } ;     (* enforce_if *)
literal    = "not" ref | ref [ ("==" | "!=") expr ] ;
```

- A bare identifier refers to an index bound by an enclosing `sum` or `forall`,
  a scalar parameter, or a scalar variable, in that order of lookup.
- Inside `[...]`, an expression is evaluated to a set element. For example,
  `x[i+1]` is valid. Referring to an element outside a variable's index set
  is an error. Referring to one outside a parameter's index set uses that
  parameter's `default` if it has one.
- `sum(i in I, j in J: i != j) ...` iterates over the Cartesian product and
  keeps only the combinations that satisfy the condition. An empty sum is 0.
- `^` is right-associative (`x^y^z` is `x^(y^z)`) and binds tighter than
  unary minus (`-x^2` is `-(x^2)`). Its exponent is a factor, so `x^-1` and
  `2^(t-1)` are allowed. Before 1.2, the exponent had to be an integer.
- The functions (added in 1.2) take one argument, in radians for the
  trigonometric ones: `ln` (natural logarithm; there is no `log`), `exp`,
  `sqrt`, `sin`, `cos`, `tan`, `atan`, and `tanh`. Function names aren't
  reserved: a parameter may still be named `exp` (and used as `exp` or
  `exp[i]`, without parentheses).
- `ln` and `sqrt` need a positive (for `sqrt`, non-negative) argument, `a/b`
  a nonzero `b`, and `a^b` a positive `a` unless `b` is an integer constant.
  A function of constants, such as `exp(rate[i] * years)`, is evaluated when
  the model is built, and it is an error if the result isn't a finite
  number. So functions of parameters can appear in linear models.
- The format allows nonlinear expressions in the variables, such as `x*y`,
  `x^2`, `1/x`, and `ln(x)`. Whether a given solver accepts them is up to
  that solver. optimizam accepts them in `expr` constraints (without
  `enforce_if`) and the objective, with continuous and integer variables,
  and searches for the global optimum: `complete` means proven (within a
  relative gap of 1e-4), and a search stopped early reports a local
  optimum (`locally_optimal`) with a bound. Finite bounds on the variables
  make proofs possible; `start` values help find good points sooner.

## Examples

### Models using the 1.1 constraint kinds

N-queens with one integer variable per row (the column of its queen), instead
of n² binaries:

```json
{
  "version": "1.1",
  "sets": { "N": { "from": 1, "to": 8 } },
  "variables": [{ "name": "col", "type": "integer", "index": ["N"], "domain": "N" }],
  "constraints": [
    { "name": "columns", "all_different": "[col[r] | r in N]" },
    { "name": "diagonals", "all_different": "[col[r] + r | r in N]" },
    { "name": "anti_diagonals", "all_different": "[col[r] - r | r in N]" }
  ],
  "solve": { "method": "satisfy", "solutions": "all" }
}
```

Assignment with named machines: each task goes to one machine, its cost
depends on the machine, and no machine takes two tasks.

```json
{
  "version": "1.1",
  "sets": { "TASKS": ["t1", "t2", "t3"], "MACHINES": ["lathe", "mill", "drill"] },
  "parameters": { "cost": { "index": ["TASKS", "MACHINES"], "values": {
    "t1": { "lathe": 4, "mill": 2, "drill": 8 },
    "t2": { "lathe": 4, "mill": 3, "drill": 7 },
    "t3": { "lathe": 3, "mill": 1, "drill": 6 } } } },
  "variables": [
    { "name": "machine", "type": "integer", "index": ["TASKS"], "domain": "MACHINES" },
    { "name": "c", "type": "integer", "index": ["TASKS"], "lower": 0, "upper": 100 }
  ],
  "constraints": [
    { "forall": "t in TASKS", "element": { "index": "machine[t]", "array": "[cost[t, m] | m in MACHINES]", "value": "c[t]" } },
    { "all_different": "[machine[t] | t in TASKS]" }
  ],
  "solve": { "method": "minimize", "objective": "sum(t in TASKS) c[t]" }
}
```

Job shop: each job's steps run in order, each machine runs one step at a time,
and the makespan is minimized.

```json
{
  "version": "1.1",
  "sets": { "JOBS": [1, 2, 3], "STEPS": [1, 2, 3], "MACHINES": [1, 2, 3] },
  "parameters": {
    "machine": { "index": ["JOBS", "STEPS"], "values": { "1": { "1": 1, "2": 2, "3": 3 }, "2": { "1": 2, "2": 1, "3": 3 }, "3": { "1": 2, "2": 3, "3": 1 } } },
    "length": { "index": ["JOBS", "STEPS"], "values": { "1": { "1": 3, "2": 2, "3": 2 }, "2": { "1": 2, "2": 1, "3": 4 }, "3": { "1": 4, "2": 3, "3": 1 } } }
  },
  "variables": [
    { "name": "begin", "type": "integer", "index": ["JOBS", "STEPS"], "lower": 0, "upper": 100 },
    { "name": "makespan", "type": "integer", "lower": 0, "upper": 100 }
  ],
  "intervals": [{ "name": "op", "index": ["JOBS", "STEPS"], "start": "begin", "duration": "length" }],
  "constraints": [
    { "forall": "j in JOBS, s in STEPS: s > 1", "expr": "begin[j,s] >= begin[j,s-1] + length[j,s-1]" },
    { "forall": "m in MACHINES", "no_overlap": "[op[j,s] | j in JOBS, s in STEPS: machine[j,s] == m]" },
    { "max": { "value": "makespan", "of": "[begin[j,s] + length[j,s] | j in JOBS, s in STEPS]" } }
  ],
  "solve": { "method": "minimize", "objective": "makespan" }
}
```

### Example files

- [`examples/hs071.json`](https://github.com/hillegass/optimizam/blob/main/examples/hs071.json) (1.2): Hock–Schittkowski
  problem 71, IPOPT's standard example: a product and a sum of squares as
  constraints, `start` values from a parameter. Local optimum 17.0140173.
- [`examples/portfolio_risk.json`](https://github.com/hillegass/optimizam/blob/main/examples/portfolio_risk.json) (1.2): a
  Markowitz portfolio: minimize the variance (a quadratic form over a
  covariance table) subject to a target return. 0.0102728.
- [`examples/curve_fit.json`](https://github.com/hillegass/optimizam/blob/main/examples/curve_fit.json) (1.2): least
  squares fit of an exponential decay `a exp(-r t) + b` to eight
  observations, unconstrained apart from `r >= 0`, from `start` values.
- [`examples/chemical_equilibrium.json`](https://github.com/hillegass/optimizam/blob/main/examples/chemical_equilibrium.json)
  (1.2): the chemical equilibrium problem of Bracken and McCormick: minimize
  the free energy `sum x_j (c_j + ln(x_j / sum x))` subject to element
  balances. Optimum -47.761.
- [`examples/batch_sizing.json`](https://github.com/hillegass/optimizam/blob/main/examples/batch_sizing.json) (1.2): a
  mixed-integer nonlinear problem: an integer number of batches times a
  continuous batch size must cover the demand (a bilinear constraint), with
  a quadratic cost. Optimum 1329.796 at 7 batches (by enumeration: 120 n +
  2000/n + 10000/n^2).
- [`examples/disk_infeasible.json`](https://github.com/hillegass/optimizam/blob/main/examples/disk_infeasible.json) (1.2):
  a disk and a half-plane that don't meet. Bound propagation (the disk
  puts both variables in [-1, 1]) proves it `infeasible`; a local solver
  alone could only report `locally_infeasible`.
- [`examples/compound_growth.json`](https://github.com/hillegass/optimizam/blob/main/examples/compound_growth.json) (1.2):
  an LP whose coefficients use functions of parameters: growth factors
  `exp(rate * years)` (continuous compounding) and `(1 + rate)^years` with a
  non-integer exponent, and `sqrt(variance)` in a risk limit. Optimum
  149,156.53.
- [`examples/knapsack.json`](https://github.com/hillegass/optimizam/blob/main/examples/knapsack.json): binary variables,
  maximization, all optimal solutions. There are two, tied at value 17:
  {gold, silver} and {gold, bronze}.
- [`examples/four_queens.json`](https://github.com/hillegass/optimizam/blob/main/examples/four_queens.json): a feasibility
  problem with a 2-D indexed variable, conditional sums, and all solutions.
  There are exactly 2.
- [`examples/production.json`](https://github.com/hillegass/optimizam/blob/main/examples/production.json): continuous LP,
  a 2-D parameter table, and parameter-valued bounds.
- [`examples/supply_chain_lp.json`](https://github.com/hillegass/optimizam/blob/main/examples/supply_chain_lp.json): a larger,
  degenerate LP with 1,968 variables and 828 constraints. It has 3-D and 4-D
  indexed variables, conditional `forall` (`t == 1` / `t > 1`), index
  arithmetic (`t-1`), and inventory linking periods. Generated by
  `tools/generate_examples.py`.
- [`examples/facility_location_milp.json`](https://github.com/hillegass/optimizam/blob/main/examples/facility_location_milp.json):
  a MILP with binary, general-integer (0..4) and continuous variables, and a
  weak LP relaxation (7,802.8 vs. the integer optimum 8,181.2). Generated by
  `tools/generate_examples.py`.
- [`examples/lot_sizing_milp.json`](https://github.com/hillegass/optimizam/blob/main/examples/lot_sizing_milp.json):
  multi-product lot sizing with binary setups, continuous production and
  stock linked over 24 periods, and a deliberately loose big-M in
  `make <= big_m * setup`. Presolve tightens it to the capacity, which makes
  the LP relaxation much stronger. Generated by `tools/generate_examples.py`.
- [`examples/parity_infeasible.json`](https://github.com/hillegass/optimizam/blob/main/examples/parity_infeasible.json):
  `2 * give - 4 * change == 7` with integers unbounded in both directions.
  Infeasible because the left side is always even. Branch and bound alone
  never finishes on it, so the solver must prove this some other way.
- [`examples/unbounded_milp.json`](https://github.com/hillegass/optimizam/blob/main/examples/unbounded_milp.json): a MILP
  whose LP relaxation is unbounded and which has integer points, so it is
  unbounded (not "unbounded or infeasible").
- [`examples/domain_gaps.json`](https://github.com/hillegass/optimizam/blob/main/examples/domain_gaps.json): integer
  `domain`s with gaps (lot sizes 0, 3–4, 7–8) and all optimal solutions.
  There are two, tied at cost 40: (3, 7, 0) and (7, 3, 0).
- [`examples/n_queens_alldiff.json`](https://github.com/hillegass/optimizam/blob/main/examples/n_queens_alldiff.json)
  (1.1): 8-queens with one integer per row, a domain naming a set, and
  `all_different` over affine items (`col[r] + r`). All 92 solutions.
- [`examples/sudoku.json`](https://github.com/hillegass/optimizam/blob/main/examples/sudoku.json) (1.1): `all_different` with
  `forall` over rows, columns, and boxes (a box is selected by a parameter in
  the comprehension's condition), and clues as `expr` rows. One solution.
- [`examples/room_assignment.json`](https://github.com/hillegass/optimizam/blob/main/examples/room_assignment.json) (1.1):
  symbolic set-valued variables (named rooms) in `all_different`; results
  report the rooms by name. All 6 solutions.
- [`examples/machine_assignment.json`](https://github.com/hillegass/optimizam/blob/main/examples/machine_assignment.json)
  (1.1): named machines with a symbolic `element` (hours by machine),
  `all_different`, `enforce_if` on a symbolic comparison
  (`machine[t] == m`, for the machine selected by a parameter), and a
  forbidden `table` of machine pairs, one naming a machine that doesn't exist.
  Two optimal assignments, at 11.
- [`examples/shift_rota.json`](https://github.com/hillegass/optimizam/blob/main/examples/shift_rota.json) (1.1): an allowed
  `table` of (works, early, late) combinations, `enforce_if` on a binary (no
  early shift after a late one), and a forbidden `table` over a comprehension
  (the two seniors don't both work late). Optimum 74.
- [`examples/tsp_tour.json`](https://github.com/hillegass/optimizam/blob/main/examples/tsp_tour.json) (1.1): a tour of six
  named cities with a two-dimensional symbolic `element`
  (`dist[city[t], city[t+1]]`), `all_different`, and a symbolic equality
  fixing the start. Optimum 201.
- [`examples/load_balance.json`](https://github.com/hillegass/optimizam/blob/main/examples/load_balance.json) (1.1): the
  makespan as the `max` of machine loads, minimized, with all optima. At the
  optimum (7) all three loads tie for the maximum, and it is reported once.
- [`examples/production_smoothing.json`](https://github.com/hillegass/optimizam/blob/main/examples/production_smoothing.json)
  (1.1): `abs` for deviations from demand and for period-to-period changes
  (bounded by the variable's upper bound), and `min` for the smallest batch,
  which the objective rewards. Optimum 16.
- [`examples/job_shop.json`](https://github.com/hillegass/optimizam/blob/main/examples/job_shop.json) (1.1): the job shop
  shown above: intervals, `no_overlap` per machine over a filtered list of
  intervals, precedences as `expr` rows, and the makespan as a `max`.
  Optimum 12.
- [`examples/crew_schedule.json`](https://github.com/hillegass/optimizam/blob/main/examples/crew_schedule.json) (1.1):
  optional intervals (`presence`) with release and deadline windows (as
  parameter bounds on the starts) sharing a crew through `cumulative`, and
  precedences that apply only when both tasks are done (`enforce_if`).
  Maximizes the value of the tasks done; optimum 36.
