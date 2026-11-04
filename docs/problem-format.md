# Problem file format

A problem is one JSON file. It is validated structurally by
[`schema/problem.schema.json`](../schema/problem.schema.json). Expressions are
strings that use the grammar below.

## Top-level fields

| Field         | Required | Meaning |
|---------------|----------|---------|
| `version`     | yes | Format version as `"major.minor"`, currently `"1.0"`. |
| `sets`        | no  | Named index sets: a list (`["a", "b"]`, `[1, 2, 3]`) or an inclusive range (`{"from": 1, "to": 8}`). |
| `parameters`  | no  | Named constants: a number, or `{"index": [sets...], "values": {...}, "default": n}`. `values` is nested one object level per index set, keyed by element. JSON keys are strings, so integer elements are written `"1"`. |
| `variables`   | yes | See below. |
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
- `domain`: integer variables only, used instead of `lower`/`upper`. It is a list
  of inclusive `[low, high]` ranges, sorted and non-overlapping, so a domain can
  have gaps: `[[1,3],[7,9]]` is {1, 2, 3, 7, 8, 9}, and `[[2,2],[5,5]]` is {2, 5}.
  The same domain applies to every element of an indexed variable. The schema
  can't check the sorting and overlap rules, so the loader must.

## Constraints

```json
{ "name": "capacity", "forall": "r in RESOURCES", "expr": "sum(p in PRODUCTS) usage[r,p] * make[p] <= available[r]" }
```

- `expr` must contain exactly one relation: `<=`, `>=`, or `==`.
- `forall` (optional) repeats the constraint once for each binding, using the
  same binding syntax as `sum`.
- `name` (optional) names the constraint. An indexed constraint produces
  instances named `capacity[carpentry]` and so on.

## Expression grammar

```ebnf
relation   = expr ("<=" | ">=" | "==") expr ;
expr       = term { ("+" | "-") term } ;
term       = factor { ("*" | "/") factor } ;
factor     = "-" factor | power ;
power      = atom [ "^" integer ] ;
atom       = number
           | ref
           | "sum" "(" bindings ")" term      (* the body is one term: sum(i in I) a[i]*x[i] *)
           | "(" expr ")" ;
ref        = ident [ "[" expr { "," expr } "]" ] ;
bindings   = binding { "," binding } [ ":" condition ] ;
binding    = ident "in" ident ;              (* index name in set name *)
condition  = disj ;
disj       = conj { "or" conj } ;
conj       = neg { "and" neg } ;
neg        = "not" neg | cmp | "(" condition ")" ;
cmp        = expr ("==" | "!=" | "<" | "<=" | ">" | ">=") expr ;
ident      = /[A-Za-z_][A-Za-z0-9_]*/ ;      (* not: sum, in, and, or, not *)
```

- A bare identifier refers to an index bound by an enclosing `sum` or `forall`,
  a scalar parameter, or a scalar variable, in that order of lookup.
- Inside `[...]`, an expression is evaluated to a set element. For example,
  `x[i+1]` is valid. Referring to an element outside a variable's index set
  is an error. Referring to one outside a parameter's index set uses that
  parameter's `default` if it has one.
- `sum(i in I, j in J: i != j) ...` iterates over the Cartesian product and
  keeps only the combinations that satisfy the condition. An empty sum is 0.
- The format allows nonlinear expressions such as `x*y`, `x^2`, and `1/x`.
  Whether a given solver accepts them is up to that solver.

## Examples

- [`examples/knapsack.json`](../examples/knapsack.json): binary variables,
  maximization, all optimal solutions. There are two, tied at value 17:
  {gold, silver} and {gold, bronze}.
- [`examples/four_queens.json`](../examples/four_queens.json): a feasibility
  problem with a 2-D indexed variable, conditional sums, and all solutions.
  There are exactly 2.
- [`examples/production.json`](../examples/production.json): continuous LP,
  a 2-D parameter table, and parameter-valued bounds.
- [`examples/supply_chain_lp.json`](../examples/supply_chain_lp.json): a larger,
  degenerate LP with 1,968 variables and 828 constraints. It has 3-D and 4-D
  indexed variables, conditional `forall` (`t == 1` / `t > 1`), index
  arithmetic (`t-1`), and inventory linking periods. Generated by
  `tools/generate_examples.py`.
- [`examples/facility_location_milp.json`](../examples/facility_location_milp.json):
  a MILP with binary, general-integer (0..4) and continuous variables, and a
  weak LP relaxation (7,802.8 vs. the integer optimum 8,181.2). Generated by
  `tools/generate_examples.py`.
- [`examples/lot_sizing_milp.json`](../examples/lot_sizing_milp.json):
  multi-product lot sizing with binary setups, continuous production and
  stock linked over 24 periods, and a deliberately loose big-M in
  `make <= big_m * setup`. Presolve tightens it to the capacity, which makes
  the LP relaxation much stronger. Generated by `tools/generate_examples.py`.
- [`examples/parity_infeasible.json`](../examples/parity_infeasible.json):
  `2 * give - 4 * change == 7` with integers unbounded in both directions.
  Infeasible because the left side is always even. Branch and bound alone
  never finishes on it, so the solver must prove this some other way.
- [`examples/unbounded_milp.json`](../examples/unbounded_milp.json): a MILP
  whose LP relaxation is unbounded and which has integer points, so it is
  unbounded (not "unbounded or infeasible").
- [`examples/domain_gaps.json`](../examples/domain_gaps.json): integer
  `domain`s with gaps (lot sizes 0, 3–4, 7–8) and all optimal solutions.
  There are two, tied at cost 40: (3, 7, 0) and (7, 3, 0).
