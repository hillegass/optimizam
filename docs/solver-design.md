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

The format 1.1 constraint kinds are encoded in the front end
(`src/encode.rs`) as linear rows on extra columns, so both engines can solve
them. The common tool is a column's value encoding: a binary `[x = v]` per
value, with `sum_v [x = v] = 1` and `sum_v v [x = v] = x`, built once per
column and shared. It lets the CP engine remove values from inside a domain,
which bound literals alone can't express.

- `all_different`: for each value, at most one item takes it (an item over
  several columns first gets a column equal to it).
- `table`: a binary per distinct allowed tuple, exactly one chosen, and each
  variable equal to the chosen tuple's entry. Forbidden: for each tuple, the
  sum of its value binaries is at most n - 1.
- `element`: a selector binary per index combination of the array's
  comprehension (after its condition), exactly one chosen; each index equals
  the chosen combination's element, and the value equals the chosen item:
  exactly for constant items, by big-M pairs when items contain variables.
- `enforce_if`: with L the number of false literals (a binary, its negation,
  or a value binary `[x = v]`), the row is relaxed by M L on each side, M from
  the row's activity bounds.
- `max` (`min` mirrored): value >= every item, and a selector for the item
  that attains the maximum, with value <= that item (big-M otherwise).
- `abs`: value >= of and >= -of, and a sign binary choosing which bound is
  tight (big-M).
- Ties would leave selectors free and make `"all"` report a solution more
  than once, so for integer-valued items tie-breaking rows make them unique:
  `max`/`min` select the first extreme item, and `abs` has its sign binary 1
  exactly when of >= 0. `"all"` with items that aren't integer-valued is an
  error.
- Intervals: an interval's duration is at least 0 when it is present.
  `no_overlap`: for each pair, an order binary o, with o = 1 meaning the
  first ends by the time the second starts, and o = 0 meaning the second
  ends by the time the first starts and the first ends after the second
  starts; each applies only when both are present, and o is 0 otherwise. So
  o is unique, and the zero-length cases match CP-SAT. `cumulative`:
  time-indexed, with a binary per interval and time for "present, started,
  not ended", linearized exactly from the value binaries of the start (and
  end), and the demands at each time at most the capacity; a horizon of at
  most 10,000 time steps. These two are encoded only for the MILP engine;
  the CP engine propagates them natively (below).
- Symbolic values are their 1-based positions in their set; an element
  outside the set makes a tuple never match and a literal constant.

Columns may have at most 10,000 values each, and bounds must be finite where
a value encoding or a big-M needs them. Every added column is determined by
the original ones, so `"all"` enumerates each solution once, and they don't
appear in results. (The MILP engine's enumeration for `"all"` fixes one
column at a time, so it is slow on these encodings; auto picks CP for them.)

The CP engine propagates `no_overlap` and `cumulative` natively
(`src/cp/scheduling.rs`) instead of solving their encodings, so the model
keeps them aside (`LinearModel::scheduling`) and encodes them only when the
MILP engine is chosen. Presolve sees only the rows, so the columns they use
are frozen against dual fixing, and their instances are mapped through
presolve's column removal (fixed columns become constants, tasks fixed absent
are dropped). Each task is [start, end) with end = start + duration (an extra
variable when both vary); bounds pushed by these propagators are stored with
their explanations, which conflict analysis reads like any other reason.
Every rule also runs with time mirrored, to pull ends down.

- `cumulative`: timetabling over the compulsory parts [latest start,
  earliest end) of present tasks. A profile above the capacity is a
  conflict; otherwise it bounds a variable capacity from below. A task that
  would overload a time it covers at its earliest start must start after
  that time, explained by the tasks covering it, the capacity's bound, and
  the task's own bounds; an optional task that fits nowhere is absent.
- `no_overlap`: pairwise precedences (if a can't end by the time b starts,
  b ends by the time a starts; an optional task that fits neither way is
  absent), which are exact once tasks are fixed, zero-length ones included;
  and edge finding (for tasks within [a, b] with total duration P, another
  task that can't fit with them before b starts at a + P or later; a + P > b
  is a conflict). Edge finding is cubic in the number of tasks, so it is
  skipped above 200.

With scheduling constraints, branching until the first solution is
scheduling-aware: among the present tasks whose start isn't fixed, the one
that can start earliest (ties: end earliest) starts as early as it can.
That builds schedules left to right, which finds good first solutions
quickly. After the first solution the search uses VSIDS, whose phase saving
then starts from the solution; the fixed order is poor at proving optimality.

Optimization alternates, once there is a solution, between the main search
and large neighborhood search (`src/cp/lns.rs`, as in CP-SAT). The main
search runs until 1000 conflicts pass without a solution; then LNS rounds
spend up to as many conflicts again (halving after a phase without an
improvement, down to an eighth, and doubling after one with an improvement).
A round builds a fresh solver with `objective <= best - 1` and restrictions
that the incumbent satisfies, starts it from the incumbent's values, and
searches with a conflict limit (each round counts for at least a few
conflicts, so the phases end when every sub-problem fails at the root, as at a
proven optimum). An improvement tightens the main search's objective row and
resets its phases to the new solution. Only the main search proves
optimality. Neighborhoods, by start variable with scheduling constraints:
some tasks are relaxed (at random, a run in incumbent start order, or all
tasks of random resources), and the others keep their incumbent order but
not their times. On a `no_overlap` that means a chain `end <= start` through
them. On a `cumulative`, they are split into unit-capacity lanes (greedy
interval partitioning, a task of demand d on d lanes) with a chain per lane.
Without scheduling constraints, a random subset of columns is fixed. Each
neighborhood kind's relaxed fraction grows when its rounds finish within the
limit and shrinks when they don't. The random choices use a fixed seed, so
the search stays deterministic. `OPTIMIZAM_NO_LNS=1` turns LNS off.

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
