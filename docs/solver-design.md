# Solver design: phase 1 (LP and MILP)

Phase 1 solves linear programs (LP) and mixed-integer linear programs (MILP),
in the style of [HiGHS](https://github.com/ERGO-Code/HiGHS). Problems outside
that scope are rejected with an error, as described below.

## Scope

A problem is in scope when, after `sum`/`forall` are expanded and parameter
values are substituted, every constraint and the objective is an affine
function of the variables: a constant times each variable, plus a constant.

- Allowed: `usage[r,p] * make[p]`, `x / 2`, `3 * (x + y)`, `cap * cap * x`.
  Any product of constants is fine.
- Rejected: `x * y`, `x ^ 2`, `1 / x`. This is decided on the expanded
  expression, not on the expression text.

### Errors

These are reported on stderr with exit code 1, and no result file is written
(see [cli.md](cli.md)). The message names the constraint instance, or the
objective, and the offending term, for example
`unsupported: nonlinear term x*y in constraint link[3]`.

- Any nonlinear term.
- `solve.solutions` is `"all"` and the problem has a `continuous` variable.

Integer variables without finite bounds are allowed with `"all"`. If such a
problem has infinitely many solutions, the run continues until the time
limit and reports `incomplete`. Without `--time`, it doesn't end.

## Numerics

The solver uses `f64` with tolerances, like HiGHS:

- Primal and dual feasibility tolerance: about 1e-7.
- Integrality tolerance: about 1e-6.
- When collecting all optimal solutions, a solution ties the optimum if its
  objective value is within the optimality tolerance of it.

In result files, integer and binary values are rounded to exact integers.

## Algorithms

1. **LP: bounded dual simplex.** It handles variable bounds directly, and it
   can quickly re-solve after bound changes, which branch and bound relies on.
   The first version uses dense matrices. Later it moves to a sparse LU
   factorization of the basis, refactored from scratch periodically, with our
   own update when a column is replaced (as HiGHS's `HFactor` does).
2. **MILP: branch and bound** on the LP solver. Each child node starts from its
   parent's final basis. The time limit is checked between nodes. Nodes are
   explored depth-first until the first integer solution, then best-bound.
   The branching variable is chosen by reliability branching: pseudocosts
   (average objective gain per unit bound change), with strong branching for
   variables that have too few observations. Before the search, each row
   whose variables are all integer, with integer coefficients, has its bounds
   rounded inward to multiples of the coefficients' gcd. An empty range proves
   infeasibility, which branch and bound alone may never show when the
   variables are unbounded (for example `2x - 4y = 7`).
3. **All optimal / all feasible solutions:** don't prune a node whose bound
   ties the best solution found, and collect every integer solution that
   reaches the optimum. A `satisfy` problem is solved with a zero objective.
4. **Cutting planes at the root,** in rounds until the bound stalls (`src/cuts/`):
   complemented MIR (c-MIR) cuts on aggregated rows, after Marchand and
   Wolsey and HiGHS's path separator, and Gomory mixed-integer cuts from the
   optimal tableau. Aggregation follows short paths through continuous
   variables, and for a few rows also projects all continuous variables out,
   which finds global counting cuts such as "total capacity covers total
   demand". Variable upper bounds such as `serve <= demand * open` are
   substituted into the MIR. Cuts no longer binding are removed between
   rounds, and the rest stay as rows for the whole search. Cuts are skipped
   with `"all"`, where a numerically unsafe cut could drop a tied optimum,
   and with a zero objective.
5. **Presolve** (`src/presolve.rs`), before branch and bound, in passes until
   nothing changes: fixed columns substituted out; empty, singleton,
   redundant, and forcing rows removed; implied bounds from row activities
   (integer bounds rounded and snapped to the domain); congruences on
   equality rows over integer columns (decimal coefficients scaled to
   integers; a small-range column keeps only values that satisfy the row
   modulo the gcd of the other coefficients); and coefficient
   tightening of binary columns in one-sided rows, which turns a loose big-M
   such as `make <= M * on` into `make <= U * on`. With `"one"`, dual
   reductions also fix a column at a finite bound when its cost doesn't
   favor leaving it and no row restricts it there. Only reductions with a
   trivial postsolve are used: a removed column has a fixed value, and a
   removed row is implied by what remains. Without dual reductions, presolve
   keeps every integer-feasible point, so it is safe with `"all"`.
6. **Primal heuristics** (`src/heuristics.rs`), to find good solutions early:
   simple rounding in lock-free directions; diving (fractional, coefficient,
   and guided toward the incumbent) with one backtrack; RENS at the root and
   RINS during the search, as sub-MIPs with a node limit solved by presolve
   and branch and bound themselves. They run at the root, then periodically,
   using at most about a fifth of the search time. Every solution they find
   is checked against the original rows.
7. **Bound propagation** (`src/propagation.rs`) at every node and in
   strong-branching probes, before the LP: implied bounds from row
   activities, with a trail of bound changes and their reasons. Conflict
   analysis by dual proofs was tried (branch `conflict-dual-proofs`) and not
   merged: correct, but no faster on our benchmarks.
8. **Deferred:** presolve reductions that need a real postsolve (doubleton
   equations, free column singletons, dominated columns, parallel rows),
   probing, and cuts below the root.

## CP engine

For problems whose variables are all integer with finite bounds, a second
engine (`src/cp/`) uses lazy clause generation, as OR-Tools CP-SAT does:

- Literals are integer bounds, `[x >= v]` and `[x <= v]`; the integer trail
  records each bound change with its reason and decision level, and can read
  back a bound as of any earlier trail position.
- Rows are scaled to integer coefficients and propagated on bounds in exact
  arithmetic (`i128` activities). Explanations are lazy: a bound implied at
  trail position p is explained by the other terms' bounds as of p. Rows of
  binaries such as `sum <= 1` need no special propagator: the explanation of
  each fixing is the single literal of the variable set to 1.
- CDCL search: 1-UIP conflict analysis with backjumping, VSIDS branching with
  phase saving (smallest value first), Luby restarts, and deletion of learned
  clauses by LBD.
- `"all"`: each solution is forbidden by a clause negating its decisions
  (propagation from them fixed every variable, so it is the only solution
  they allow), resolved as a conflict.
- Optimization: after each solution, the objective row is tightened to
  `<= value - 1`, and the search continues with its learned clauses.
  Optimization with `"all"` then enumerates every solution with objective
  `<=` the optimum in a fresh search, since the first search's learned
  clauses assumed a strictly better objective.

`all_different` (format 1.1) is encoded in the front end as linear rows, so
both engines can solve it: each item's column x gets a binary `[x = v]` per
value, with `sum_v [x = v] = 1` and `sum_v v [x = v] = x`, and for each value
at most one item takes it (an item over several columns first gets a column
equal to it). The value binaries let the CP engine remove values from inside
a domain, which bound literals alone can't express. Columns may have at most
10,000 values each. The binaries are determined by the columns, so `"all"`
enumerates each solution once, and they don't appear in results. (The MILP
engine's enumeration for `"all"` fixes one column at a time, so it is slow
on these encodings; auto picks CP for them.)

`--engine=auto` (the default) uses the CP engine for problems it can take
that are `satisfy`, `"all"`, or use a format 1.1 constraint kind, after
presolve, and the MILP engine otherwise;
`--engine=mip|cp` overrides (see [cli.md](cli.md)). CP has no LP bound, so a
time-limited optimization reports no `objective_bound`. OR-Tools CP-SAT is
its development reference.

## Mapping to result statuses

| Situation | `status` |
|-----------|----------|
| LP solved to optimality / B&B tree exhausted | `complete` |
| LP proven infeasible / B&B tree exhausted with no integer solution | `infeasible` |
| LP proven unbounded | `unbounded` |
| Time limit, MILP, incumbent found, gap not closed | `not_proven_optimal` (with `objective_bound`) |
| Time limit, `"all"` requested, some valid solutions found | `incomplete` |
| Time limit, no feasible solution found | `timeout` |

### MILP with an unbounded LP relaxation

When the LP relaxation is unbounded, the MILP is either unbounded or
infeasible. HiGHS can report this as "unbounded or infeasible". We always
decide which:

1. Run branch and bound with a zero objective to look for any
   integer-feasible point.
2. If one is found, report `unbounded`. With rational data, a feasible MILP
   whose relaxation is unbounded is itself unbounded (Meyer's theorem), and
   values parsed from JSON are rationals.
3. If the search proves that no such point exists, report `infeasible`.
4. If time runs out first, report `timeout`.

## Dependencies

- Runtime: `serde` (with `derive`), `serde_json`, and `clap` (with `derive`).
  Add no others without asking. All of the math is our own code.
- Possible later addition: `faer`, used only to refactor the basis
  periodically. It has no update for a replaced column, so we would write that.
- Development only: HiGHS (`highspy`) and OR-Tools (`ortools`, for CP-SAT) in
  `.venv` as references for comparing objective values and statuses. They are
  not dependencies of optimizam.
