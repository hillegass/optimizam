# Proposals: the API and the problem and result formats

Written 2026-11-09 for the 0.2 release, from a review of the library, the command, the formats, and the skill as a
first-time user would meet them (about 110 small problem files were run through the command to see what it says),
and from the engine work of the same week. Each proposal says what gets in the way today, what to change, and what
it would cost. Nothing here is implemented.

Proposals 1 to 12, 26, and 28 were implemented on 2026-11-10 and are no longer in this document. 1 to 6: why a
problem is infeasible, a limit on the number of solutions, error messages that say where, silent traps as errors or
warnings, result metadata, and slack and duals. 26 and 7 to 12: every error in one run; number types, `sum()`,
borrowed keys, and typed reads in Rust; index arithmetic and inline `abs`/`min`/`max`; `!=` and strict
comparisons, and literals as constraints; bounds as expressions; the engines named `global` and `local`; and the
command line's optional RESULT, `--check`, `--quiet`, built-in documents, and `--nonzeros`. 28: the `min`, `max`,
and `abs` constraint kinds retired for the functions of those names. `CHANGELOG.md` says what landed. The others
keep their numbers, so the list starts at 13. Proposals 25 and 27 came out of implementing the first six, as did
the passages marked *(revised)*; proposal 29 (and 28), and the passages marked *(after 12)*, came out of
implementing 26 and 7 to 12.
[What the implemented proposals left open](#what-the-implemented-proposals-left-open) lists the rest in one
place.

The owner has no users yet and wants no backward compatibility. So nothing here is shaped to be additive: where a
proposal kept an alias or deprecated a name, it now renames or removes.

Proposals 13 to 25, 27, and 29 follow in number order, then comments on the 0.2 plan, what should stay as it is,
and a list of small inconsistencies.

What I would do next, and why these (each got its evidence from the last batch):

| # | Proposal | Why now | Effort |
|---|----------|---------|--------|
| 20 | One version for problem files; `description` at the top level | Four examples changed version for using a function or a computed bound; a second example had nowhere to put its description | small |
| 25 | Constraint families | `--check` is a second place that cuts a constraint's name at a `[` to find its family, and the two disagree | medium |
| 14 | Reading solutions | The last batch's own tests turned an integer key into a string to read an entry, four times in three files | small |
| 29 | Show what a constraint expanded to | `--check` says a `forall` came to 400 constraints, not what one of them is | small |
| 24 | Write MPS (the half that exports) | Twice in the last batch a file was rewritten by hand before a reference solver could check it | small to medium |

## What the implemented proposals left open

Each item is taken up where the number points, or is new here.

- **Conflicts.** The elastic variant ("which constraints must give, and by how much") isn't done, and a conflict
  lists constraints only: the variables' bounds always count and aren't named (22). `--explain` groups the
  instances of an indexed constraint by cutting their names at the first `[` (25), which puts every nameless
  constraint (`constraints[3]`) in one group; `check.rs`'s `family` cuts them right and could be shared.
- **Unbounded results** name the variables with an objective coefficient and no bound, which may not be the one
  that needs a bound (22).
- **The solution limit.** `"solutions": 1` is not `"one"` (papercuts).
- **Warnings** are values now and `--quiet` drops them, but only problem files have the lint: a model built in
  Rust gets the `infeasible` kind alone, from `Model::check` (13).
- **Result metadata.** `statistics` has four numbers; what the search did is still text (23). The example result
  files are in the old, smaller shape (papercuts).
- **Constraints in results** are a flat list of names where variables are nested tables (25). Duals exist only for
  a linear program, solved without presolve (27). `Solution::slack` needs the option although the model could
  work it out on demand (13).
- **Numbers in Rust.** `max([x, y + 1])` and `sum([x, y + 1])` don't compile: an array's items have one type, so
  a bare variable beside an expression needs `Expression::from(x)`. A `max!`/`sum!` macro, or two-argument
  methods, would take mixed items. And a number on the left of an operator is still an `i32` or an `f64`
  (`2 * x` must keep working): the batch's own tests wrote `coefs[i] * x[i]` with an `i64` twice and had to turn
  it round. Documented, and still what a user will type first.
- **`abs`, `min`, `max` terms.** Each occurrence gets its own column, so `abs(x) * (4 - abs(x))` has two for one
  value; equal terms should share (21). `abs` of a nonlinear argument is an error (it needs a column for the
  argument first). Progress events report an objective before the loosely held columns are settled, so for a
  solution that isn't optimal it can be above the result's. `tools/highs_reference.py` reads the terms only as
  constants (24).
- **The kinds' retirement (the old 28).** `v == max(..)` is the model the `max` kind was, with no column for the
  term, when the term's coefficient is 1 or -1 and there is no `enforce_if`; `2 * v == 4 * abs(..)` and an
  enforced equation still get a column and a row. A variable that is defined by such an equation and used
  nowhere but in an objective that pushes it one way could be held by rows alone, as the inline term is; it is
  held exactly, and the format document tells the writer to put the term in the objective instead.
  `tools/cp_benchmark.py`'s tournament keeps its `last_start` variable, as an equation, so that the benchmark's
  history stays comparable; with the maximum in the objective it ended at 1210 in 30 seconds with one thread
  and with eight, against 1180 and 1200, which is within what that instance varies by.
- **`!=`** between sides of several variables goes through `all_different`'s value binaries, one per value of
  each side: fine for colors, heavy for start times a thousand apart in the MILP engine (the CP engine takes
  those natively). A choice by one binary, as the `enforce_if` form already uses, would be lighter there.
  `constraint!` still can't take a strict `<` (`lt`, `gt`).
- **Bounds.** An error in a bound's expression stops the reading at the first one, where constraints report one
  each. A parameter's `default` can't be `null`. `Model::to_json` still writes per-element bounds as `_x_upper`
  tables, not as the expressions a loaded file had. `tools/mps_to_json.py` still splits columns into families by
  their finite bounds (24). A bound can be computed now, but a parameter can't: `examples/lot_sizing_milp.json`
  still carries `"big_m": 1435`, the sum of its demand table worked out by hand (21).
- **`--check`** counts; it doesn't show (29).
- **The result's `omitted`** is a list (`["zeros", "slack"]`), not the string the proposal had, since two things
  can be left out.
- **Found on the way:** numbers in problem files aren't read exactly, and results print the last bit of
  floating-point noise (papercuts). `examples/chemical_equilibrium.result.json` has said `complete` since the
  convexity proofs, which IPOPT can't confirm; the check in CLAUDE.md that ran it against IPOPT had been failing
  unnoticed, and now leaves it out.

## 13. Finish symbolic variables and loaded models in the Rust API

A Rust user can create symbolic variables but use them only in `is` and `is_not` (as conditions and as
constraints), `==` and `!=` between two of one set, and `all_different`:
`table` takes integer tuples, and `table_of` and `element_keyed` are crate-private. `constraint_named` searches
only `expr` constraints (`Model::constraint_named`), so a loaded `all_different` or `no_overlap` can't be removed by
name. `num_vars()` is the only inspector.

```rust
impl Model {
    pub fn table<E: Into<Element>>(&mut self, vars: .., tuples: ..) -> ConstraintId;      // also forbidden_table
    pub fn element_of<E: Into<Element>, X: Into<Expression>>(&mut self, index: Variable,
        items: impl IntoIterator<Item = (E, X)>, value: impl Into<Expression>) -> ConstraintId;
    pub fn set_name(&mut self, id: ConstraintId, name: impl Into<String>);                // every kind
    pub fn families(&self) -> impl Iterator<Item = VarFamily> + '_;
    pub fn constraints(&self) -> impl Iterator<Item = (ConstraintId, Option<&str>)> + '_;
}
```

*(revised)* Three more, from the first six:

- A constraint's name is kept in one of two places: `Constraint::name` for comparisons, `Model::labels` for every
  other kind (`src/api/builder.rs`). Conflicts and results read both through one helper; `constraint_named`
  reads only the first. One store, behind `set_name` (proposal 25 goes further).
- Warnings are worked out for problem files only (`Problem::warnings`). A model built in Rust can also have a
  variable that nothing uses, two constraints with one name, or bounds that leave a variable no value.
  *(after 12)* The place for them exists now: `Model::check` returns warnings, of the `infeasible` kind only.
  Give it the two that need no file (`unused_variable`, `shared_name`) instead of adding `Model::warnings()`.
- `Solution::slack(id)` is None unless the solve had `Options::constraints`, because the solution doesn't hold
  the constraints. The model does: `Model::slack(id, &Solution)` needs no option.

## 14. Reading solutions without knowing the model

`Solution` offers `sol[v]`, `value`, `eval`, `element`, and the string-typed `get("x", &["a", "3"])`. There is no
iteration, so a generic caller must call `to_json()` and parse it back. Add `Solution::iter()` (name, key, value),
`Solution::family(&VarFamily)`, `get(name, impl Key)` (`("a", 3)` instead of `&["a", "3"]`), and `Display` for
`println!`.

*(after 12)* `Solution::int` and `is_one` exist (the old proposal 7). The string-typed `get` is the part that
hurts: every test of the last batch that read a loaded problem's entry by an integer key wrote
`s.get("on", &["ann", &d.to_string()])`, four times in three files. `Key` already takes `("ann", 3)` for a
family; `get` should take the same.

## 15. Data apart from the model: `--data`, and a job for `Problem`

The format separates data (`sets`, `parameters`) from the model, but only inside one file, and in Rust
`Model::from_file` has already folded the parameters into coefficients. Let `Problem` be the symbolic form, where
data changes (`set_parameter`, `set_set`, `to_json`), and `Model` the expanded form, where structure changes; on
the command line, `--data week12.json` and `--set capacity[north]=50`.

*(revised)* `Problem` has since gained three small jobs: it converts its file once and keeps the model (so a
second `solve` reads no expression again), and it carries `warnings()` and `conflict()`. None of them needs a type
of its own: a `Model` could hold its warnings from the load. So the choice is still open, and with no one to
break it is cheap either way: give `Problem` the data-editing role above, or remove it and let
`Model::from_file` be the one way in.

*(after 12)* It gained a fourth, `check()`, which differs from `Model::check` in two things a `Model` could keep
from its load as well: the file's warnings, and the names of constraints whose `forall` matched nothing. But for
`warnings()`, every method `Problem` has, `Model` now has too. That argues for the second choice unless the
data-editing role is wanted.

## 16. Dense arrays as parameter values

`"values": [[3, 2, 2], [2, 1, 4], [4, 3, 1]]` for range-indexed tables, nested one level per index set, lengths
checked. Today's keyed layout is three times the characters and a place to drop a key; a list is the first thing a
writer tries. Results keep the keyed layout.

*(revised)* The reader is written by hand now, so this is one more arm of `param_def` in `src/problem.rs`, and
the message a list gets today is at least clear: `parameters.cap: expected a number or a table {"index": [..],
"values": {..}}, found a list`. A dense table also has no keys to misspell, which is what the new key check is
for, and is smaller and faster to read: the set-covering benchmark's table of 600,000 entries is most of a 6.6 MB
file. *(after 12)* A table's value may be `null` now (no bound); a dense list takes `null` in the same places.

## 17. Interval accessors

`IntervalVar` has no methods, so callers carry start, length, and interval side by side, and in files
`begin[j,s] + length[j,s]` is repeated because `op[j,s]` can't be asked for its end. `Model::start_of`, `end_of`,
`duration_of`; in files `start(op[j,s])`, `end(op[j,s])`. And let `no_overlap` and `cumulative` take
`impl IntoIterator<Item = IntervalVar>` as `all_different` does.

*(after 12)* Both halves got cheaper. Files have functions with names and arities now (`Builtin` in
`src/expr.rs`), so `start`, `end`, and `duration` are three more of them whose argument is an interval and whose
value is the affine expression the interval was declared with. And an interval's fields are expressions, so
`"start": "begin[j] + setup[j]"` no longer needs a variable made for it, which makes asking the interval for its
end the only way to get it without writing that sum again.

## 18. Lexicographic objectives

"Finish as early as possible, then with the least overtime" is the usual shape of a real scheduling request; today
it is two runs with a hand-edited constraint between them. `"objective": ["makespan", "sum(j in JOBS) late[j]"]`
and `Model::minimize_lex`, as a driver loop in `api/mod.rs` (solve, fix the level within tolerance, solve the
next), sharing the time limit. No engine change.

## 19. Status vocabulary

`timeout` means "no solution found and nothing proven", whatever the cause. *(revised)* Results carry stop
reasons now, so a file can read `"status": "timeout", "stop_reason": "node_limit"`, or `"canceled"`. I wrote
"leave the names alone" when names had to last; they don't: rename `timeout` to `unknown`, which is what it means
and what MiniZinc and CP-SAT call it. And add `Status::has_solution()` and `is_proven()`, so callers stop
matching on eight variants.

*(after 12)* The code agrees: `Model::check` runs the solve path up to the engine and stops, and what that
returns inside is `Status::Timeout`, for want of a status that says "nothing was searched".

## 20. Version and extensibility policy

*(revised)* The result side is done: every result says the writer's version, and the schema lets a reader ignore
top-level fields it doesn't know (a solution object still forbids them).

The problem side isn't. A file's `version` is one of four values chosen by which features it uses, the reader
ignores the choice (`"1.0"` with `all_different` loads; so does `"1.9"`), the examples carry all four, the format
document tags each feature with the version that added it, and `Model::to_json` threads a flag through every
expression it writes only to decide between `"1.2"` and `"1.3"`. With no old files to keep reading, a problem
file has one version, the current one, and all of that goes.

Unknown fields are still rejected everywhere, including a top-level `"description"`, which generated files often
carry: `examples/sudoku_unique.json` had to put its description on a parameter. Allow `name` and `description`
at the top level.

*(after 12)* Both halves came up again at once. Format 1.3, still unreleased, took in everything of the last
batch: functions, `abs`/`min`/`max`, `!=` and strict comparisons, named indexes, bounds as expressions, `null`.
Four examples went from `"1.1"` to `"1.3"`, three for a function or two and one for a computed bound; the
version's cell in the format document's table is a paragraph; the export's flag (`latest` in
`src/api/export.rs`) is set in three more places; and the format document says "added in 1.3" nine times, four
of them in the grammar. None of it changes what any reader does.
And `examples/line_fit.json`, written to show an objective of absolute values, has its explanation on the
`time` parameter, because a problem has no place for one. Do this before 0.2 is released: until then no file
in the world says `"1.3"` and means a particular set of features.

## 21. Named expressions, reported in results

A quantity used twice, or wanted in the answer, must be an auxiliary variable with bounds and a defining row
(`examples/load_balance.json`'s `load[m]`). An `"expressions"` section whose entries may be used wherever an
expression may (substituted: no column, no bound to get wrong), and are reported per solution beside `variables`.

*(after 12)* Two more reasons, and one constraint on the design:

- **Computed parameters are the same thing without variables.** A bound can be an expression now, a parameter
  can't: `examples/lot_sizing_milp.json` has `"big_m": 1435`, the sum of its demand table, and
  `examples/crew_schedule.json` had a `latest_start` table of deadline minus length until bounds could compute
  it. An entry of `"expressions"` with no variable in it is a parameter worked out once, usable in bounds, in
  conditions, and in subscripts; with named indexes (`"index": ["t in TASKS"]`, as variables have) it is a table.
- **Reported values that aren't variables.** With `abs` and `max` inline, a model's interesting quantities (each
  period's shortfall, the makespan) are no longer variables, so results no longer show them:
  `examples/line_fit.json`'s result has the line and not the distances, and since the kinds were retired
  `examples/production_smoothing.json`'s has the six batches and neither what each missed by nor the smallest.
  Reporting named expressions is how they come back.
- **One term, one column.** An `abs` or a `max` gets a column where it is lowered, one for each time it is
  written. A named expression that is such a term must be one column however often it is used, so substitution
  has to be by reference to the term, not by copying its text. (In Rust this is already the natural shape: an
  `Expression` holds its terms behind `Arc`s, and lowering can key columns by the pointer.)

## 22. Later

Tuple sets and sparse families (`"ARCS": [["a","b"], ...]`, `flow` indexed by `ARCS`: large, and the biggest
change to the format's data model here); `circuit`; `count`; full reification (`"iff": "b"`); `Options::node_limit`
with a documented default (today the global search stops after 2,000 nodes only when there is *no* time limit, so
adding `--time` can make a run longer, and the "Time limit" section of `docs/cli.md` says those statuses never
occur without `--time`).

*(after 12)* From the functions and the `abs`/`min`/`max` terms:

- **Rounding and division of integer expressions.** `div(day[j] - 1, 7)` for the week a job falls in is the next
  thing after index arithmetic, and today it is an error that says the function takes a number. For an integer
  expression and a constant divisor it is one integer column and one row (`7q <= e <= 7q + 6`); `mod`, `floor`,
  and `ceil` follow from it.
- **A piecewise linear function of one variable** (`"piecewise(x, [[0, 0], [10, 50], [30, 90]])"`: tiered prices,
  overtime rates, a cost curve from a table). The machinery of the last batch does most of it: lowering already
  works out, from where a term stands, whether rows alone can hold it (a convex cost that is minimized) or it
  needs binaries, and gives the column its bounds. `abs` is the two-piece case.

From the MILP work:

- **`node_limit` is also what makes a stopped run repeatable.** With one thread a run that finishes is
  deterministic, but one cut off by `--time` isn't: where the clock stops it varies. Comparing two versions of the
  solver meant comparing node counts inside a time window (13 nodes against 1,300 in 10 seconds, on one model), and
  a test of a result stopped early can't assert anything exact. The MILP and global searches already take a node
  limit internally (`milp::Settings::node_limit`); it only needs to be public, and `--nodes=N` on the command.
- **A search emphasis** (`--emphasis=solutions|proof`, as Gurobi's `MIPFocus` and CPLEX's MIP emphasis). Every
  step of the MILP work traded one for the other: more cut rounds and restarts at the root raised the number of
  MIPLIB models proven in 30 seconds and left a worse best solution on some that weren't (tr12-30 ended 7% to
  13% above the optimum, from 1.9%). The default was chosen by proofs. Worth an option only if a benchmark of
  solution quality at short limits shows that one default can't serve both.

*(revised)* From explaining infeasible and unbounded results:

- **Which constraints must give, and by how much.** A conflict says which constraints can't hold together; the
  next question is the cheapest way out. For linear rows: add a slack to each, minimize the total, and report the
  rows that used theirs. (This was the third stage of the old proposal 1.)
- **Bounds in conflicts.** The deletion filter removes constraints and leaves every variable's bounds in place, so
  a conflict of one constraint means "this constraint, against some bounds", without saying whose. Run the same
  filter over the bounds of the variables in the conflict.
- **The variable that runs away.** An unbounded result lists the variables that have an objective coefficient and
  no bound in the improving direction. The one that needs a bound can be another: maximize `profit` with
  `profit == 3 * units - waste` and no upper bound on `units`, and the result names `profit`. The simplex has the
  direction along which the objective improves without end; its variables are the ones to list.

## 23. Solver statistics as data

*(revised)* The four numbers of the old proposal 5 exist: `statistics` has the engine, the threads, the time,
and the node count. What the search did is still available only as text on stderr (`OPTIMIZAM_DEBUG=1`, or
`Options::log`):

```
cuts: Stats { rounds: 26, cmir_added: 1446, gmi_added: 501, removed: 1351, initial_objective: 13924.174518328724, final_objective: 124593.8243903784 }, rows 1342
incumbent 1188 (RENS) at node 1, 2.54s
nodes 3622, node LP iterations 61711, node LP seconds 5.03, restarts 0
```

The first line is a Rust `Debug` print. The MILP work ran on these lines: four throwaway scripts read them with
regular expressions to compare root bounds with HiGHS's, to time the cut loop, and to see which models restart, and
two of the lines changed shape while that work was going on. A user has the same questions. Is the root bound weak,
or the tree large? When was the best solution found, and would more time or more threads have helped? Was most of
the limit spent before the first node?

Opt-in, so default results stay small: `optimizam --statistics`, and `SolveResult::statistics()` in Rust. The
four numbers stay where they are, and the engine's own go under the engine's name:

```json
"statistics": {
  "engine": "milp", "threads": 1, "time": 10.0, "nodes": 13,
  "milp": {
    "root_bound": 983.17, "root_bound_with_cuts": 1089.23, "root_time": 6.68,
    "cut_rounds": 81, "restarts": 1,
    "incumbents": [
      { "objective": 1247, "time": 2.54, "node": 1, "found_by": "coefficient_dive" },
      { "objective": 1188, "time": 2.54, "node": 1, "found_by": "rens" },
      { "objective": 1187, "time": 6.68, "node": 2, "found_by": "rens" }
    ]
  }
}
```

(The values are from one 10-second run on MIPLIB's aflow30a during that work: two thirds of the limit went to the
root, which is the kind of thing this is for.) Document the section as informational: fields are added over time,
and values change from version to version, so nothing should assert on them. In Rust, a `Statistics` struct with
accessors and `#[non_exhaustive]` enums, like the rest of the API.

Effort: small to medium. The engines collect all of it already (`cuts::Stats`, `heuristics::Stats`, the MILP
search's `Totals`, the CP solver's conflict and restart counts). *(revised)* The engine name and the node count
reach the result through two solve-scoped values in `config.rs` (`config::ran`, `config::count_nodes`), since
`solve.rs` returns only the outcome; a struct of statistics would travel the same way, or `Outcome` could carry
it. The debug text can then be printed from the same structs. Under `"cp"`, call the count what it is: the base
`nodes` of a CP run is its conflicts.

*(after 12)* `--check` reports the solver model's columns and rows before presolve, and a result reports
neither. The pair a user needs to see together, the size before and what presolve left, is in no one place.
Put both in `statistics` (presolve's `Stats` has the second), and say in it how many
`abs`, `min`, and `max` terms were held by rows alone and how many took binaries, since that is what decides
whether such a model is a linear program.

## 24. Read and write MPS

Every diagnosis in the MILP work began by giving the same model to HiGHS or SCIP and comparing root bounds and
node counts. That works only for a model that started as an MPS file. For one that starts in our format, the route
is `tools/highs_reference.py`, a second implementation of the format in Python, which lives in the repository and
not in the installed command. In the other direction, standard models arrive through `tools/mps_to_json.py`, which
scatters the columns over families by type and bounds and puts the MPS names in a side file, so a result can't be
read against the model it came from. (*(after 12)* The scattering is no longer forced: a bound table may hold
`null` for "no bound", so one family can have every column. The converter wasn't changed, because column order
decides search paths and every converted benchmark would move.)

- **Read:** `Model::from_mps(&str)` and `from_mps_file(path)`; the command reads MPS when PROBLEM ends in `.mps`,
  or with `--format=mps` (needed for stdin). Columns become one family `x` keyed by the MPS column names, rows keep
  their names, so the result reads `"x": {"C0001": 1, ...}`. The Python converter is the specification, including
  its one convention that readers disagree on (a negative `UP` bound with no lower bound), *(revised)* which
  should come out as a warning the way a problem file's do. Gzip needs a crate, so compressed files go through
  `gunzip -c file.mps.gz | optimizam --format=mps - result.json`.
- **Write:** `Model::to_mps()` and `optimizam --write-mps FILE`, for linear models after lowering (so the CP kinds
  are written as their encodings, and anything nonlinear is an `Unsupported` error). This is the half that makes a
  bug report or a comparison with another solver possible for a model built in Rust or written in our format.

Medium: the reader is about 300 lines following the converter, the writer about half that; both can be tested by
round trips on the Netlib and MIPLIB files the benchmarks already download. *(revised)* With `--constraints` an
MPS model's result then carries duals under the MPS row names, which is how they can be checked against HiGHS's.

*(after 12)* The write half is now the more urgent one, and for the project's own sake. Each format feature of
the last batch had to be written a second time in Python before an example using it could be checked, and not
all of it was: `tools/highs_reference.py` refuses `abs`, `min`, and `max` of variables and `!=`. So
`examples/line_fit.json`'s optimum was confirmed by writing the same fit by hand, with a variable above each
distance, and giving that to HiGHS; a test file with a `null` bound had the `null` replaced by a number before
CP-SAT, which needs finite bounds, could be asked; and the list of examples that the HiGHS reference must skip
is 19 `--skip` flags in a paragraph of CLAUDE.md. `optimizam --write-mps` after lowering would let HiGHS check
every linear model the format can say, whatever was added to the format, with no second implementation to keep
up.

## 25. Constraint families: a name and a key, not a formatted string

*(new)* A variable knows its family and its key: `make[('north', 3)]` is the family `make` at `('north', 3)`,
and results lay it out as a nested table. A constraint knows only a string. An instance of an indexed constraint
of a problem file is named `capacity[north]` when the file is read (`instance_label` in `src/api/convert.rs`),
and from then on that text is all there is. The first six proposals each paid for it:

- `--explain` tries the instances of an indexed constraint together before it tries them one by one. To find
  them it cuts every name at its first `[` (`groups` in `src/api/explain.rs`). An element that contains `[`
  or `,` breaks that, and a model built in Rust has no way to say that forty constraints belong together.
- Results list `"hours[finishing]": { "slack": 0 }` in one flat object beside `"make": { "chairs": 40 }`. The
  old proposal 6 asked for constraints nested by index like variables, and couldn't have it.
- A conflict that contains all 50 instances of `capacity` lists 50 names; it can't say `capacity`.
- Names sit in two stores (proposal 13), and about fifty messages write `constraint {label}` by hand. An unnamed
  constraint is called by its place, `constraints[3]`, so `Error::new` deletes the word "constraint" in front of
  it with a string replacement.
- *(after 12)* `--check` counts the constraints that each name stands for, so it cuts names too (`family` in
  `src/api/check.rs`), and not the way `--explain` does: an instance of a nameless constraint is
  `constraints[3][north]`, which belongs to `constraints[3]`, and `--explain`'s cut at the first `[` puts every
  nameless constraint of a file in one group. Two cutters, two answers. A warning, meanwhile, says where it is
  by position in the file (`constraints[3]`) and a conflict by name, because neither has the family to say
  both.

Give constraints what variables have: the model keeps a family name and a key for each (a constraint added alone
is a family of one), and one function turns that into text for messages.

```rust
impl Model {
    pub fn add_indexed(&mut self, family: &str, key: impl Key, constraint: Constraint) -> ConstraintId;
    pub fn constraint_family(&self, name: &str) -> Option<ConstraintFamily>;   // iterate, index by key, remove
}
```

`convert.rs` has the key when it names an instance (the `forall`'s bound values), so files need no change. Then
results nest `constraints` like `variables`, a conflict can name a whole family, `--explain`'s groups are the
families, and proposal 13's `constraints()` and `set_name` have one thing to iterate and set. Medium.

## 27. Sensitivity beyond a linear program

*(new)* `--constraints` reports slack for every problem, and duals and reduced costs only for a linear program
(continuous variables, linear rows, an objective), which it solves by the simplex method directly, without
presolve. Four extensions, each useful alone:

1. **Mixed-integer programs (small).** What PuLP and Gurobi users mean by a MILP's duals: fix the integer
   variables at the solution's values and report the duals of the linear program that is left. One LP solve after
   the search. Say what it is in the result (`"dual_of": "fixed"`), since it prices the continuous part only.
2. **Duals through presolve (medium).** Asking for duals changes how the LP is solved: presolve substitutes
   columns out through equality rows, which would leave those rows without a dual, so it is skipped. On an LP
   that presolve helps, that is the difference between maros-r7's 0.4 s and 2.3 s. The substitutions (doubleton
   equations, implied free columns) have standard dual postsolves; with them the option costs nothing.
3. **Ranging (medium).** A dual holds over a range of the right side, and a solution stays optimal over a range
   of each cost: "how far can this price move before the plan changes?" Both come from the final basis.
4. **Nonlinear problems (small).** The interior point method ends with multipliers, which are the duals at a
   local optimum; report them for a local solve.

The sign convention is settled (the change of the optimal objective per unit of the right side, in the problem's
own sense) and has a test that every extension can reuse: `tests/constraints.rs` compares each dual with the
change in the optimum when the right side is moved a little, on random linear programs.

*(after 12)* A model with `abs` or `max` terms that rows alone hold is a linear program and gets its duals; one
term that needs a binary, or one `!=`, makes it mixed-integer and they vanish without a word. Extension 1 covers
it, and until then the result should say why it has slacks and no duals.

## 29. Show what a constraint expanded to

*(new)* `--check` answers "how many": `"cover": 4, "rest": 0`. The question after it is "of what". A `forall`
with the wrong condition, or a sum over the wrong set, usually comes to the right number of constraints with
the wrong contents, and the only way to see one today is `Model::to_json` in Rust, which writes all of them.

```sh
optimizam --check --show cover problem.json
```
```json
"instances": {
  "cover[1]": "on[ann,1] + on[bob,1] + on[cy,1] >= 2",
  "cover[2]": "on[ann,2] + on[bob,2] + on[cy,2] >= 2"
}
```

The first few instances of the named constraint (or of each, without a name), as the flat text that the
exporter already writes (`Writer::constraint` in `src/api/export.rs`), with the count of the rest. For a
variable, its first elements with their bounds, which is what shows a bound expression that computed the wrong
thing. In Rust, `Check::instances(name)`. Small, since both writers exist; it wants proposal 25's families to
find a constraint's instances without cutting strings a third time.

## On the 0.2 plan and its open decisions

- **Threads.** The default is one thread per core, at most 8 (the owner's direction, 2026-11-10; the rule is
  provisional). Cores, not CPUs: `available_parallelism()` counts hardware threads, so it is halved where cores
  have two (Linux says; x86 is assumed to elsewhere). At most 8, because nothing has been run above 10 threads.
  No example is slower with it than with one thread. Still to do: benchmark 4, 7, 10, and 14 threads on the
  14-CPU development machine, and decide whether efficiency cores should count.
- **Repeatability** is a developer's tool (`--threads=1`, which the tests and benchmark scripts pin), not
  something users are promised, and there is no `seed` option. Giving it up gains nothing by itself, in speed or
  memory. One thread repeats only because the algorithms read no clock and use fixed seeds, which costs nothing,
  and several threads never repeated. What it allows is work that can't repeat: root heuristics beside the cut
  loop on idle threads (nw04 takes 2.6 s with one thread and with ten, all of it at the root), effort scaled to
  the time limit (with 10 seconds, aflow30a spent 7 at the root), and racing engines or LP algorithms against
  each other.
- **Gaps.** The plan says the engines' fixed gaps become the defaults of `relative_gap` and `--gap`, but they
  aren't one number: the MILP search proves to 1e-6 (`OPTIMALITY_TOLERANCE`, `src/milp.rs`), the global search
  to 1e-4 (`docs/cli.md`, "Engines"). HiGHS, Gurobi, and CPLEX stop at 1e-4 unless told otherwise. I would keep both
  defaults (the examples' expected objectives are compared at 1e-6, and `complete` should mean what a newcomer
  thinks it means), state them per engine in `docs/cli.md`, and have `--gap` override either. *(revised)*
  Results now print a `gap`, so the option has something to be compared with. `tools/miplib_benchmark.py` now
  runs HiGHS at 1e-6 too; its cached numbers, which the docs quote, are still from the 1e-4 run.
- **Warm starts.** The plan offers `Options::start(&Solution)` "or `Model::hint(v, value)`". The variable already
  has `VarSpec::start` and the file has `start`; keep that one word: `Model::set_start(v, value)` next to
  `set_bounds`, plus `Options::start(&Solution)` for re-solves.
- **`Problem`.** See proposal 15: give it the data, or remove it.
- **The docs pass.** *(after 12)* The plan's last step has more to do than it had. `docs/problem-format.md` grew
  by a paragraph wherever each feature touched it: the functions of data and indexes are a table inside a bullet
  of the grammar's notes, bounds are explained under variables and again under intervals, and "added in 1.x"
  runs through all of it. It wants one pass that arranges it by what a writer is doing (data, variables,
  expressions with one table of every function, constraints), which proposal 20 makes shorter by removing the
  version notes. The document is also the tool's `--docs problem` now, so it is read in a terminal: keep its
  lines short and its tables narrow.
- **Four places per flag.** *(after 12)* Each command-line flag is described in `docs/cli.md`, in `--help`
  (`src/main.rs`), in `SKILL.md`, and often in the README, and each function in the format document, in
  `SKILL.md`, and in the parser's own message. The last batch edited all of them by hand and CLAUDE.md asks
  for that in prose. A test can hold most of it: every `--flag` that `--help` prints appears in `docs/cli.md`,
  and every name in `Func::ALL` and `Builtin::ALL` appears in the format document and in `SKILL.md`.
- **The CP bound.** *(revised)* The weak level-0 bound used to show only in progress events. It is now in every
  stopped result as `objective_bound` and `gap`, without qualification (a first gap of 19.5 on crew_schedule).
  The bound search of the plan's section 5 matters more for it; failing that, mark such bounds.
- **`Model: Clone` keeps the id**, so handles work across clones, but after a variable is added to each clone one
  handle means two different columns and the "another model" check can't see it. *(revised)* This now reaches
  problem files: `Problem::to_model` hands out clones of the one model it keeps, where each call used to build a
  model with an id of its own. Give clones a new id.
- CLAUDE.md says `Element` is `#[non_exhaustive]`; it isn't (`src/api/family.rs`).

## What should stay as it is

- Statuses are answers, errors are for malformed input: exit code 0 for `infeasible` and `timeout`, messages on
  stderr only, so `-` always gives clean JSON.
- Expression strings, not a JSON syntax tree, with single-quoted literals inside JSON strings.
- Sets, parameters, `sum`, `forall`, named constraint instances (`capacity[north]`), `description` fields.
- One front end: files become a `Model`, and a `Model` exports to a file that reads back to the same answers.
- `enforce_if`/`only_if` instead of big-M; intervals built from ordinary variables; symbolic values by name.
- No implicit lower bound of 0, said plainly.
- `Variable` as a `Copy` handle, operator overloading, `constraint!`, `#[must_use]`, family keys such as
  `x[("north", 3)]`, private fields, chained options, `#[non_exhaustive]` enums.
- `CancelToken`, `Control::Stop`, stop reasons apart from the status, `--incumbents` written whole through a rename.
- Solver settings on the command line, not in the problem file.
- Explanations worked out on the model, above the engines (`explain.rs`, `constraints.rs`, `lint.rs`): they hold
  for every engine and for files and Rust alike, and the engines stay free to change.
- *(after 12)* The modeler writes `abs`, `max`, or `!=` and doesn't choose an encoding: lowering picks rows or
  binaries from where the term stands. No option for it.
- *(after 12)* A bare name as a bound (`"upper": "cap"` for a table over the variable's own sets), although
  `"upper": "cap[i,j]"` over named indexes now says the same: it is the common case, every example uses it, and
  it has one meaning.
- *(after 12)* What a result file leaves out (`--nonzeros`, `--constraints=binding`) is decided where the file is
  written and said in the file (`omitted`); the result in memory is always whole.
- *(after 12)* `--check` finds the engine by running the solve path to the point where an engine would start,
  not by a second copy of the choice, so the two can't come apart.

## Inconsistencies and papercuts

Behavior:

- The time limit is soft, and how soft isn't documented. In the MIPLIB benchmark with `--time 30` and ten
  threads, 7 of 58 runs took more than 30.3 s, the longest 32.7 s (mik-250-20-75-4) and 31.4 s (gen-ip054); with
  one thread the longest was 30.4 s. Not diagnosed: candidates are a batch of strong-branching probes that has no
  deadline check inside it, and freeing a tree of millions of open nodes at the end. Results now show it
  (`statistics.time`). Fix it, or give `docs/cli.md` a sentence on the margin.
- `statistics.time` and `--time` run on different clocks: the limit counts from the start of the process, the
  reported time from the start of the solve, after the file is read (0.4 s for a 14 MB file).
- Numbers in a problem file aren't read exactly. `serde_json` without its `float_roundtrip` feature can be one
  unit in the last place off: a test read `4056.0968936499276` back as `4056.096893649928`.
  `tools/mps_to_json.py` writes coefficients with `repr` "so they round-trip exactly", and through our reader
  they don't. The feature costs parsing time (its documentation says about twice, for numbers); measure a load of
  `target/miplib/json/nw04.json` with it before deciding.
- Results print the last bit of floating-point noise: `examples/production.json` gives `"tables":
  5.000000000000015` and objective `2200.0000000000014`. Write continuous values to 15 significant digits.
- `"solutions": 1` is not `"one"`: it stops an enumeration, so one solution that wasn't shown to be the only one
  is `incomplete`. Consistent, and a trap. Reject 1 with a message that says to write `"one"` or 2.
- `Model::conflict` returns None both for a model that has a solution and for one it couldn't decide in time.
- *(after 12)* `--constraints binding`, with a space, takes `binding` for the problem file and fails on reading
  it; only `--constraints=binding` works. Clap can't tell an optional value from the next argument. The message
  (`reading binding: No such file or directory`) could say so.
- *(after 12)* `--nonzeros` drops zeros from `reduced_costs` as well as from `variables`. A reduced cost of 0
  says something a value of 0 doesn't (the variable is basic, or the optimum is degenerate there), and its
  absence now looks the same as "not reported".
- *(after 12)* A slack is "how far from the nearer limit"; for `!=` it is how far the two sides are beyond the
  one they must differ by, and for a variable compared with a named element it is always 0. The word covers
  three things.

Documentation against behavior:

- `docs/cli.md`'s "Time limit" section ("those three statuses never occur" without `--time`) against its own
  "Engines" section and `GLOBAL_NODE_LIMIT` in `src/solve.rs`: the global search stops after 2,000 nodes when
  there is no time limit.
- `SKILL.md:10` and its description say "continuous nonlinear programs" while its expressions section allows
  integers; `docs/solver-design.md:6` says "continuous" too.
- `SKILL.md`'s first step tells the user to clone and `cargo install --path .`; the README has
  `cargo install optimizam`.
- The progress lines shown in `docs/cli.md` ("Watching a long run") have their keys in an order the command
  doesn't write: it writes them sorted (`bound`, `elapsed`, `event`, `gap`, `objective`).
- The expected results in `examples/` are in older shapes: most come from the reference solvers, say version
  `"1.0"`, and none has `sense` or `statistics`. Four write an objective as `12.0` where the command writes `12`,
  and three carry a `"$schema"` key the command never writes. Have the reference tools write `sense` and the current
  version, or say in `examples/` that these are answers to compare with, not what the command prints.
- *(after 12)* CLAUDE.md's check of the nonlinear examples against IPOPT named `chemical_equilibrium`, whose
  expected status became `complete` on 2026-11-05, with the convexity proofs; IPOPT can only say
  `locally_optimal`, so the check couldn't pass from then on, which was found on 2026-11-10. The lists of which
  reference can check which example are prose there; `tests/regression.rs` could hold them, one column per
  reference, and run what is installed.

Rust API (`src/api/builder.rs` unless another file is named):

- Slices against iterators: `table(&[Variable], ..)`, `no_overlap(&[IntervalVar])` against
  `all_different(impl IntoIterator)`.
- `VarSpec::min`/`max` against `lower`/`upper` in the format and in `set_bounds`; `min` then means three things.
- `VarSpec::binary` says bounds "are ignored"; a file with bounds on a binary is an error.
- `add_family` ignores `spec.name` silently; `add_var` and `add_vars` use it.
- `Progress::bound()` against `SolveResult::objective_bound()`.
- Three `to_json` signatures: `Result<String, Error>`, `String`, `Option<String>`.
- `Variable` has `==` (handle identity) and `.eq(rhs)` (a `Constraint`). *(after 12)* And now `!=` (identity) and
  `.ne(rhs)` (a `Constraint`), and `x.is(3)` beside `x.eq(3)`: one is a condition that is also a constraint, the
  other a constraint only. `x.is(3)` could be the one spelling for a variable against a value.
- *(after 12)* `constraint!(x != y)` works and `constraint!(x < y)` is a compile error that points to `x.lt(y)`:
  four comparisons in the macro, two as methods only.
- *(after 12)* `Options::constraints(true)`, `binding_only(true)`, and `nonzeros(true)`: three booleans, of which
  the second does nothing without the first. `Options::constraints(Which::Binding)`, as the command line has it.
- *(after 12)* `Check::variables()` and `constraints()` return `&[(String, usize)]`; with proposal 25 they would
  return the families.
- One field in files (`"solutions"`), two methods in Rust that undo each other: `all_solutions(bool)` and
  `solution_limit(n)`.
- `Conflict::constraints()` returns strings where the model has ids (25).
- `Options::presolve`, `value_symmetry`, and `large_neighborhood_search` are documented as debugging switches;
  hide them or move them to environment variables.

Naming between the three surfaces (not all worth changing; listed so the choice is deliberate):

| Concept | JSON | Rust | CLI |
|---|---|---|---|
| Conditional constraint | `enforce_if` | `only_if` | |
| Bounds | `lower`, `upper` | `min`, `max`, `bounds`; `set_bounds` | |
| Allowed values | `domain` | `domain(values)`, `values(&set)` | |
| Differs | `!=` | `ne`, `constraint!(a != b)`; `is_not` | |
| Strictly less | `<` | `lt` (not in `constraint!`) | |
| What a model comes to | | `check()` | `--check` |
| Every solution, or N | `"solutions": "all"`, `N` | `all_solutions(true)`, `solution_limit(n)` | |
| Time limit | | `time_limit(Duration)`, `deadline(Instant)` | `--time=SECONDS` |
| Threads | | `threads(n)` | `--threads=N` |
| Why infeasible | `conflict` | `explain(true)`, `conflict()` | `--explain` |
| Slack and duals | `constraints`, `reduced_costs` | `constraints(true)`, `slack`, `dual`, `reduced_cost` | `--constraints` |
| What a result leaves out | `omitted` | `nonzeros(true)`, `binding_only(true)` | `--nonzeros`, `--constraints=binding` |
| Bound on the optimum | `objective_bound` | `objective_bound()`, `Progress::bound()` | `"bound"` in `--progress` |
| How far from optimal | `gap` | `gap()` | `"gap"` in `--progress` |
