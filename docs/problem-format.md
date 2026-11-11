# Problem file format

A problem is one JSON file. It is validated structurally by
[`schema/problem.schema.json`](https://github.com/hillegass/optimizam/blob/main/schema/problem.schema.json). Expressions are
strings that use the grammar below.

## Top-level fields

| Field         | Required | Meaning |
|---------------|----------|---------|
| `version`     | yes | Format version as `"major.minor"`: `"1.3"`, the version this document describes, and the only one optimizam reads. |
| `sets`        | no  | Named index sets: a list (`["a", "b"]`, `[1, 2, 3]`) or an inclusive range (`{"from": 1, "to": 8}`). |
| `parameters`  | no  | Named constants: a number, or `{"index": [sets...], "values": {...}, "default": n}`. `values` is nested one object level per index set, keyed by element. JSON keys are strings, so integer elements are written `"1"`. A key that isn't an element of its set is an error: it is probably misspelled, and the element it was meant for would take the default. A table's value may be `null`: no value, which as a variable's bound means no bound for that element, and which nothing can compute with. |
| `variables`   | yes | See below. |
| `intervals`   | no  | Named (optional) time intervals for scheduling constraints. See below. |
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
- `solutions`: `"one"` (default), `"all"`, or a number N of 1 or more.

| `method`                | `"one"` | `"all"` | N |
|-------------------------|---------|---------|---|
| `satisfy`               | any feasible solution | every feasible solution | at most N feasible solutions |
| `minimize` / `maximize` | one optimal solution  | every optimal solution  | at most N optimal solutions  |

A number stops an enumeration. If there are no more than N solutions, the
result has them all and its status is `complete`; otherwise it has N and the
status is `incomplete`. So `"solutions": 2` answers "is this solution the
only one?" without listing every other: one solution and `complete` means it
is. (With 1, a solution that wasn't shown to be the only one is `incomplete`;
`"one"` is what asks for just a solution.)

This is not the same as MiniZinc's `-a` flag. For optimization problems, `-a`
reports the improving solutions found along the way. `"all"` here means every
solution that reaches the optimal objective value.

`"all"` (like a number) is only well-defined when the relevant solution set is finite. In
practice that means every variable is binary, or integer with finite bounds,
or else the continuous variables are fully determined by the discrete ones.
A solver should report an error rather than enumerate an infinite set.

## Variables

```json
{ "name": "x", "type": "integer", "index": ["I", "J"], "lower": 0, "upper": "cap" },
{ "name": "start", "type": "integer", "index": ["t in TASKS"], "lower": "release[t]", "upper": "deadline[t] - length[t]" }
```

- `type`: `continuous`, `integer`, or `binary`. Binary variables are in {0, 1} and
  must not declare bounds or a domain.
- `index`: optional list of set names. `["I", "J"]` declares `x[i,j]` for every
  pair. If it is omitted, the variable is a scalar `x`. An entry may name its
  index, `"t in TASKS"`, for the bounds and the start to use.
- `lower` / `upper`: a number, or an expression (a string) in the parameters
  and the named indexes, worked out for each element: `"2 * n"`,
  `"deadline[t] - length[t]"`, `"cap[p]"` for a `make[p,t]`. The name of a
  parameter is the simplest one: a scalar bounds every element, and a table
  over the variable's own sets bounds each element with its entry (`"cap"`
  for `cap[i,j]`, needing no index names). If a bound is omitted, the variable is unbounded
  on that side; so is an element whose bound is a parameter entry that is
  `null`. There is no implicit `lower: 0`. A number in quotes (`"0"`) is an
  error, since it is most likely a slip.
- `domain`: integer variables only, used instead of `lower`/`upper`. Either a list
  of inclusive `[low, high]` ranges, sorted and non-overlapping, so a domain can
  have gaps: `[[1,3],[7,9]]` is {1, 2, 3, 7, 8, 9}, and `[[2,2],[5,5]]` is {2, 5};
  or the name of a set, which makes the variable set-valued (see below). The same domain applies to every element of an indexed variable.
  The schema can't check the sorting and overlap rules, so the loader must.
- `start`: optional starting value for local solvers of nonlinear problems,
  a number or an expression (the same rule as bounds). A start outside the
  bounds is moved inside them. Solvers that don't need one ignore it.

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
    constraint or as an `enforce_if` literal,
  - in a whole `expr` constraint `v == w` or `v != w` with another symbolic
    variable over the same set (`color[i] != color[j]`).

A symbolic value is written as a string literal, `"enforce_if":
"machine[t] == 'mill'"`, or referred to through a bound index:
`"forall": "t in TASKS, m in MACHINES", "enforce_if": "machine[t] == m"`.
A literal must be one of the variable's values: `machine[t] == 'mil'` is an
error. An element that arrives through an index, and isn't one of the
values, is not: that comparison never holds (`==` is false, so there is
nothing to enforce or the constraint can't be met; `!=` is true).

## Intervals

An interval is a span of time for scheduling constraints
(`no_overlap`, `cumulative`), built from ordinary variables and parameters.

```json
{ "name": "task", "index": ["JOBS"], "start": "begin", "duration": "length", "presence": "scheduled" }
```

- `index`: as for variables; an entry may name its index (`"j in JOBS"`).
- `start` and `duration`: a number, or an affine expression in the
  variables, the parameters, and the named indexes: `"begin[j] + setup[j]"`.
  The name of a variable or a parameter indexed by the same sets as the
  interval stands for its element (the same rule as variable bounds). Both must be integer-valued.
- `presence` (optional): a binary variable, by name if it is indexed by the
  same sets, or as a reference (`"on[j]"`). Without it, the interval is
  always present.
- A present interval occupies [start, start + duration), and its duration must
  be at least 0. An absent interval (presence 0) constrains nothing.
- Intervals have no values of their own, so they don't appear in results.
- In a constraint, `task[j]` refers to one interval.

## Constraints

```json
{ "name": "capacity", "forall": "r in RESOURCES", "expr": "sum(p in PRODUCTS) usage[r,p] * make[p] <= available[r]" }
```

Every constraint has optional `name`, `description`, and `forall` fields, and
exactly one *kind* field, which says what the constraint is. Names are worth
giving: error messages and an infeasible result's `conflict` call a constraint
by its name (`capacity`, or `capacity[north]` for one instance of an indexed
one), and one without a name only by its place in this list, counting from 0
(`constraints[3]`). The kinds:

- `forall` (optional) repeats the constraint once for each binding, using the
  same binding syntax as `sum`.
- `name` (optional) names the constraint. An indexed constraint produces
  instances named `capacity[carpentry]` and so on.

A solver may not support every kind; it reports an error rather than ignoring a constraint.

### `expr`: a linear (or other) relation

`expr` must contain exactly one relation: `<=`, `>=`, `==`, `!=`, `<`, or `>`. The last three are for integer-valued sides
(integer or binary variables with whole coefficients, and whole constants),
where `a < b` is `a <= b - 1` and `a != b` leaves a gap of at least one;
between real numbers they are errors, since no solver can hold `x < 3`
apart from `x <= 3`. `x[i] != x[j]` is the same as an `all_different` of
the two.

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

### Absolute values, minima, and maxima

These aren't kinds: `abs`, `min`, and `max` are functions in expressions
(see the grammar's notes), written where the value is used.

```json
{ "expr": "abs(staff[d] - target[d]) <= 2" },
{ "expr": "makespan == max(j in JOBS) (begin[j] + length[j])" }
```

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
relation   = expr ("<=" | ">=" | "==" | "!=" | "<" | ">") expr ;
expr       = term { ("+" | "-") term } ;
term       = factor { ("*" | "/") factor } ;
factor     = "-" factor | power ;
power      = atom [ "^" factor ] ;
atom       = number
           | string                          (* a set element *)
           | ref
           | func "(" expr ")"
           | builtin "(" expr { "," expr } ")"
           | ("min" | "max") "(" bindings ")" term   (* over a set, like sum *)
           | "sum" "(" bindings ")" term      (* the body is one term: sum(i in I) a[i]*x[i] *)
           | "(" expr ")" ;
func       = "ln" | "exp" | "sqrt" | "sin" | "cos" | "tan" | "atan" | "tanh" ;
builtin    = "mod" | "div" | "floor" | "ceil" | "abs" | "min" | "max"
           | "card" | "first" | "last" | "at" | "pos" ;
string     = "'" { any character but "'" | "''" } "'" ;   (* '' is one quote: 'o''hare' *)
ref        = ident [ "[" expr { "," expr } "]" ] ;
bindings   = binding { "," binding } [ ":" condition ] ;
binding    = ident "in" ident ;              (* index name in set name *)
condition  = disj ;
disj       = conj { "or" conj } ;
conj       = neg { "and" neg } ;
neg        = "not" neg | cmp | "(" condition ")" ;
cmp        = expr ("==" | "!=" | "<" | "<=" | ">" | ">=") expr ;
ident      = /[A-Za-z_][A-Za-z0-9_]*/ ;      (* not: sum, in, and, or, not *)

(* Constraint kinds *)
list       = "[" expr "|" bindings "]"       (* comprehension *)
           | "[" expr { "," expr } "]" ;     (* explicit *)
literals   = literal { "and" literal } ;     (* enforce_if *)
literal    = "not" ref | ref [ ("==" | "!=") expr ] ;
```

- A string literal is a set element: `'north'`. Single quotes
  keep it free of escapes inside the JSON string; a quote inside it is
  doubled (`'o''hare'`). It may appear wherever an element may: in a
  subscript (`x['north', t]`), in a comparison in a condition
  (`sum(p in PLANTS: p != 'north') ...`), in an `enforce_if` literal or a
  whole `expr` constraint comparing a symbolic variable
  (`machine['t1'] == 'mill'`), and in lists. It can't be used in
  arithmetic. The literal `'3'` is the string element `"3"`, not the integer
  3.
- In a condition, numbers compare with `==`, `!=`, `<`, `<=`, `>`, `>=`.
  Named (string) elements compare with `==` and `!=` only: they have no
  order, and `d < 'tue'` is an error (alphabetical order would put `'fri'`
  first). To order them, compare their places in the set:
  `pos(d) < pos('tue', DAYS)`.
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
  `2^(t-1)` are allowed.
- The functions take one argument, in radians for the
  trigonometric ones: `ln` (natural logarithm; there is no `log`), `exp`,
  `sqrt`, `sin`, `cos`, `tan`, `atan`, and `tanh`. Function names aren't
  reserved: a parameter may still be named `exp` (and used as `exp` or
  `exp[i]`, without parentheses).
- The functions of data, sets, and indexes are worked out
  when the model is built, so their arguments have no variables:

  | Function | Value |
  |----------|-------|
  | `mod(a, b)`, `div(a, b)` | The remainder and the quotient of integers, rounded down: `div(7, 2)` is 3, `div(-7, 2)` is -4, and `mod` has the sign of `b` (`mod(-1, 5)` is 4), so `a == b * div(a, b) + mod(a, b)`. |
  | `floor(a)`, `ceil(a)` | `a` rounded down, and up. |
  | `abs(a)`, `min(a, b, ...)`, `max(a, b, ...)` | The absolute value; the smallest and the largest of one or more values. |
  | `min(i in I: cond) term`, `max(i in I: cond) term` | The smallest and the largest of a term over index combinations, written like a `sum`. Over no combinations it is an error. |
  | `card(S)` | The number of elements of the set `S`. |
  | `first(S)`, `last(S)`, `at(S, k)` | The set's first, last, and k-th element (counting from 1). |
  | `pos(i)` | The place (counting from 1) of the index `i` in the set it runs over, in a `forall`, a `sum`, or a list. |
  | `pos(e, S)` | The place of the element `e` in `S`: `pos('tue', DAYS)`. |

  They do the index arithmetic that otherwise needs a parameter table:
  `x[mod(t, card(T)) + 1]` is the element after `t`, wrapping around;
  `3 * div(r - 1, 3) + div(c - 1, 3) + 1` is a Sudoku cell's box;
  `d in DAYS: pos(d) < card(DAYS)` is every day but the last; and
  `at(DAYS, pos(d) + 1)` is the day after `d`. Like the other functions,
  their names aren't reserved.
- `abs`, `min`, and `max` (both forms) also take affine expressions with
  variables, in `expr` constraints, lists, and the objective:
  `sum(t in T) abs(make[t] - demand[t])`, `max(j in JOBS) end[j]`,
  `abs(x[i] - x[j]) >= 2`. Such a term is piecewise linear, and the model
  stays linear (or integer): the solver adds a variable for the term, which
  results don't show. Where only one side of the term matters, rows are
  enough and nothing else is asked of the model: an `abs` or a `max` that is
  minimized or bounded above (`abs(e) <= 3`, a makespan), a `min` that is
  maximized or bounded below. Anywhere else (`abs(e) >= 2`, an equation, a
  term inside a product) the term needs a binary variable, and the
  variables inside it need finite bounds. So write a term where it is used:
  `"objective": "max(j in JOBS) end[j]"` is a smaller model than a variable
  `makespan`, the constraint `makespan == max(j in JOBS) end[j]`, and the
  objective `makespan`, which must hold the variable to the maximum exactly
  because the solver can't know where else it is used.
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

## Warnings

Some things are allowed but are probably mistakes. They don't stop a solve;
the command-line tool prints each as a `warning:` line on stderr (unless
`--quiet`), `--check` lists them, and `Model::warnings` returns them in
Rust. Each has a kind:

- `empty_forall`: a `forall` that matches nothing, so that its constraint
  doesn't exist;
- `hidden_name`: an index of a `sum`, a list, or a `forall` with the name of
  an index around it, of a parameter, or of a variable, which it hides;
- `shared_name`: two constraints with one name;
- `unused_parameter` and `unused_variable`: a parameter that nothing uses,
  and a variable that appears in no constraint and not in the objective;
- `infeasible`: a variable whose bounds leave it no value, and a constraint
  that can't hold within its variables' bounds: the problem is infeasible
  before any search.

## Examples

### Models using the constraint kinds

N-queens with one integer variable per row (the column of its queen), instead
of n² binaries:

```json
{
  "version": "1.3",
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
  "version": "1.3",
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
  "version": "1.3",
  "sets": { "JOBS": [1, 2, 3], "STEPS": [1, 2, 3], "MACHINES": [1, 2, 3] },
  "parameters": {
    "machine": { "index": ["JOBS", "STEPS"], "values": { "1": { "1": 1, "2": 2, "3": 3 }, "2": { "1": 2, "2": 1, "3": 3 }, "3": { "1": 2, "2": 3, "3": 1 } } },
    "length": { "index": ["JOBS", "STEPS"], "values": { "1": { "1": 3, "2": 2, "3": 2 }, "2": { "1": 2, "2": 1, "3": 4 }, "3": { "1": 4, "2": 3, "3": 1 } } }
  },
  "variables": [
    { "name": "begin", "type": "integer", "index": ["JOBS", "STEPS"], "lower": 0, "upper": 100 }
  ],
  "intervals": [{ "name": "op", "index": ["JOBS", "STEPS"], "start": "begin", "duration": "length" }],
  "constraints": [
    { "forall": "j in JOBS, s in STEPS: s > 1", "expr": "begin[j,s] >= begin[j,s-1] + length[j,s-1]" },
    { "forall": "m in MACHINES", "no_overlap": "[op[j,s] | j in JOBS, s in STEPS: machine[j,s] == m]" }
  ],
  "solve": { "method": "minimize", "objective": "max(j in JOBS, s in STEPS) (begin[j,s] + length[j,s])" }
}
```

### Example files

- [`examples/hs071.json`](https://github.com/hillegass/optimizam/blob/main/examples/hs071.json): Hock–Schittkowski
  problem 71, IPOPT's standard example: a product and a sum of squares as
  constraints, `start` values from a parameter. Local optimum 17.0140173.
- [`examples/portfolio_risk.json`](https://github.com/hillegass/optimizam/blob/main/examples/portfolio_risk.json): a
  Markowitz portfolio: minimize the variance (a quadratic form over a
  covariance table) subject to a target return. 0.0102728.
- [`examples/curve_fit.json`](https://github.com/hillegass/optimizam/blob/main/examples/curve_fit.json): least
  squares fit of an exponential decay `a exp(-r t) + b` to eight
  observations, unconstrained apart from `r >= 0`, from `start` values.
- [`examples/chemical_equilibrium.json`](https://github.com/hillegass/optimizam/blob/main/examples/chemical_equilibrium.json): the chemical equilibrium problem of Bracken and McCormick: minimize
  the free energy `sum x_j (c_j + ln(x_j / sum x))` subject to element
  balances. Optimum -47.761.
- [`examples/batch_sizing.json`](https://github.com/hillegass/optimizam/blob/main/examples/batch_sizing.json): a
  mixed-integer nonlinear problem: an integer number of batches times a
  continuous batch size must cover the demand (a bilinear constraint), with
  a quadratic cost. Optimum 1329.796 at 7 batches (by enumeration: 120 n +
  2000/n + 10000/n^2).
- [`examples/disk_infeasible.json`](https://github.com/hillegass/optimizam/blob/main/examples/disk_infeasible.json):
  a disk and a half-plane that don't meet. Bound propagation (the disk
  puts both variables in [-1, 1]) proves it `infeasible`; a local solver
  alone could only report `locally_infeasible`.
- [`examples/compound_growth.json`](https://github.com/hillegass/optimizam/blob/main/examples/compound_growth.json):
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
- [`examples/n_queens_alldiff.json`](https://github.com/hillegass/optimizam/blob/main/examples/n_queens_alldiff.json): 8-queens with one integer per row, a domain naming a set, and
  `all_different` over affine items (`col[r] + r`). All 92 solutions.
- [`examples/sudoku.json`](https://github.com/hillegass/optimizam/blob/main/examples/sudoku.json): `all_different` with
  `forall` over rows, columns, and boxes (a cell's box is worked out with
  `div` in the comprehension's condition), and clues as `expr` rows. One
  solution.
- [`examples/room_assignment.json`](https://github.com/hillegass/optimizam/blob/main/examples/room_assignment.json):
  symbolic set-valued variables (named rooms) in `all_different`; results
  report the rooms by name. All 6 solutions.
- [`examples/machine_assignment.json`](https://github.com/hillegass/optimizam/blob/main/examples/machine_assignment.json): named machines with a symbolic `element` (hours by machine),
  `all_different`, `enforce_if` on a symbolic comparison
  (`machine[t] == m`, for the machine selected by a parameter), and a
  forbidden `table` of machine pairs, one naming a machine that doesn't exist.
  Two optimal assignments, at 11.
- [`examples/shift_rota.json`](https://github.com/hillegass/optimizam/blob/main/examples/shift_rota.json): an allowed
  `table` of (works, early, late) combinations, `enforce_if` on a binary (no
  early shift after a late one, on every day but `last(D)`), and a forbidden
  `table` over a comprehension (the two seniors don't both work late).
  Optimum 74.
- [`examples/tsp_tour.json`](https://github.com/hillegass/optimizam/blob/main/examples/tsp_tour.json): a tour of six
  named cities with a two-dimensional symbolic `element` (the distance from
  `city[t]` to `city[mod(t, card(STOPS)) + 1]`, which for the last stop is
  the first), `all_different`, and a symbolic equality fixing the start.
  Optimum 201.
- [`examples/load_balance.json`](https://github.com/hillegass/optimizam/blob/main/examples/load_balance.json): the
  `max` of the machine loads as the objective, minimized, with all optima.
  At the optimum (7) all three loads tie for the maximum, and it is reported
  once.
- [`examples/production_smoothing.json`](https://github.com/hillegass/optimizam/blob/main/examples/production_smoothing.json): one variable family and the rest in expressions: `abs` for the
  deviations from demand and for the period-to-period changes (bounded by
  `abs(..) <= 3`), and `min` for the smallest batch, which the objective
  rewards. Every term is pushed one way, so the model is a small
  mixed-integer program with no binaries for them. Optimum 16.
- [`examples/job_shop.json`](https://github.com/hillegass/optimizam/blob/main/examples/job_shop.json): the job shop
  shown above: intervals, `no_overlap` per machine over a filtered list of
  intervals, precedences as `expr` rows, and the makespan as a `max` in the
  objective. Optimum 12.
- [`examples/crew_schedule.json`](https://github.com/hillegass/optimizam/blob/main/examples/crew_schedule.json):
  optional intervals (`presence`) with release and deadline windows (the
  starts' bounds: a parameter, and `deadline[t] - length[t]` over a named
  index) sharing a crew through `cumulative`, and
  precedences that apply only when both tasks are done (`enforce_if`).
  Maximizes the value of the tasks done; optimum 36.
- [`examples/plant_carriers.json`](https://github.com/hillegass/optimizam/blob/main/examples/plant_carriers.json):
  string literals: a subscript (`make['north', 2] == 0`), a sum's condition
  (`p != 'east'`), and `enforce_if` on a symbolic variable
  (`carrier == 'road'`) choosing between a shipping limit and a fee.
  Optimum 800, by road.
- [`examples/sudoku_unique.json`](https://github.com/hillegass/optimizam/blob/main/examples/sudoku_unique.json):
  a 4 by 4 Sudoku with `"solutions": 2`, to learn whether its solution is
  the only one (it is: one solution, `complete`).
- [`examples/line_fit.json`](https://github.com/hillegass/optimizam/blob/main/examples/line_fit.json): a line through
  measurements with the least total distance, `sum(r in RUNS) abs(...)` in
  the objective: a linear program, with unbounded variables. Optimum 9.171.
