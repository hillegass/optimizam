# Proposals: the API and the problem and result formats

Written 2026-11-09 for the 0.2 release, from a review of the library, the command, the formats, and the skill as a
first-time user would meet them (about 110 small problem files were run through the command to see what it says),
and from the engine work of the same week. Each proposal says what gets in the way today, what to change, and what
it would cost. Nothing here is implemented.

Proposals 1 to 6 were implemented on 2026-11-10 and are no longer in this document: why a problem is infeasible, a
limit on the number of solutions, error messages that say where, silent traps as errors or warnings, result
metadata, and slack and duals. `CHANGELOG.md` says what landed. The others keep their numbers, so the list starts
at 7. Proposals 25 to 27 are new, and the ones marked *(revised)* changed: both came out of implementing the first
six. [What the first six left open](#what-the-first-six-left-open) lists it in one place.

The owner has no users yet and wants no backward compatibility. So nothing here is shaped to be additive any more:
where a proposal kept an alias or deprecated a name, it now renames or removes. (The table's "kind" column, which
said additive or breaking, is gone.)

| # | Proposal | Value | Effort |
|---|----------|-------|--------|
| 10 | Bounds from scalar parameters; bounds and interval fields as expressions | medium | small, then medium |
| 11 | Name the engines for what they do: `global` and `local` | medium | small |
| 12 | Command line: optional RESULT, `--check`, `--quiet`, built-in docs, `--nonzeros` | medium | small each |

These seven are the ones I would do before 0.2 (26 is new, and by value belongs first). Proposals 13 to 25 and 27
follow in number order, then comments on the 0.2 plan, what should stay as it is, and a list of small
inconsistencies with file and line.

## What the first six left open

Each item is taken up where the arrow points.

- **Conflicts.** The elastic variant ("which constraints must give, and by how much") isn't done, and a conflict
  lists constraints only: the variables' bounds always count and aren't named (22). `--explain` groups the
  instances of an indexed constraint by cutting their names at the first `[` (25).
- **Unbounded results** name the variables with an objective coefficient and no bound, which may not be the one
  that needs a bound (22).
- **The solution limit.** `"solutions": 1` is not `"one"` (papercuts).
- **Messages.** One error per run (26). `constraint!` can't accept a strict `<` (9). The error for ordering named
  elements can't suggest `pos` until it exists (8).
- **Warnings** exist for problem files only, are plain strings, and can't be silenced (12, 13).
- **Result metadata.** `SolveResult::engine()` returns a string (11). `statistics` has four numbers; what the
  search did is still text (23). The example result files are in the old, smaller shape (papercuts).
- **Constraints in results** are a flat list of names where variables are nested tables (25). Duals exist only for
  a linear program, solved without presolve (27). `Solution::slack` needs the option although the model could
  work it out on demand (13).
- **Found on the way:** numbers in problem files aren't read exactly, results print the last bit of floating-point
  noise, and with `--progress` an error still arrives as plain text (papercuts).

## 10. Bounds from scalar parameters; bounds and interval fields as expressions

A bound is a number or the name of a parameter "indexed by the same sets as the variable", strictly:
`{"name": "y", "index": ["T"], "upper": "n"}` with a scalar `n` fails (`bound or start parameter n must be indexed
by the same sets as the variable`), although the skill tells the agent to keep numbers in parameters. `cap[p]`
can't bound `make[p,t]`, and nothing can be computed (`"upper": "2 * n"`), so `examples/crew_schedule.json` carries
a hand-computed `latest_start`.

Step 1 (small): a scalar parameter bounds any variable. Step 2 (medium): an index entry may name its index, and
bounds, `start`, and interval fields may then be constant expressions in those names:

```json
{ "name": "start", "type": "integer", "index": ["t in TASKS"],
  "lower": "release[t]", "upper": "deadline[t] - length[t]" }
```

*(revised)* The reader now refuses a number in quotes as a bound (`"lower": "0"`: "write numbers without
quotes"), the commonest slip in generated files. With step 2 a bound's string is an expression and `"0"` is a
valid one; keep refusing a string that is nothing but a number.

Step 3 (small): `null` in a bound parameter means no bound for that element. A parameter can't hold infinity, so a
family of which only some elements are bounded can't be written with one parameter. `tools/mps_to_json.py` shows
the cost: it splits an MPS file's columns into one family per type and per combination of finite bounds
(`cont_lu[17]` is the 17th continuous column with both bounds finite), so the model's column order and names are
gone from the file.

## 11. Name the engines for what they do

`Engine::Nlp` and `--engine=nlp` mean "a local interior point solve, continuous variables only". The engine `auto`
uses for nonlinear problems, the global search, has no name and can't be asked for. The skill itself says `auto`
uses "NLP for problems with nonlinear terms" (`SKILL.md:184`), which in `--engine` terms is false, and
`--engine=nlp` on a linear knapsack answers that the problem "is nonlinear" (`src/solve.rs:210`).

*(revised)* `Engine::{Auto, Milp, Cp, Global, Local}` and `--engine=global|local`, with no alias for `nlp`; the
`mip` alias of `milp` goes too. Results now say which engine ran, as a string, because `Engine` can't say "the
global search": `SolveResult::engine()` returns `"milp"`, `"cp"`, `"global"`, `"nlp"`, or `"presolve"`. With the
rename it returns `Option<Engine>` (None when presolve settled the problem before any engine ran), and
`statistics.engine` writes `"local"` for `"nlp"`.

## 12. Command line: optional RESULT, `--check`, `--quiet`, built-in docs, `--nonzeros`

- `optimizam problem.json` is a usage error; default RESULT to stdout.
- `--check` parses and builds without solving and prints what was built (variables and constraint instances per
  name, the engine `auto` would pick, warnings): how one catches a `forall` that expanded to nothing or to far too
  much. In Rust, `Model::check()`. *(revised)* Most of it exists now: `Problem::warnings` reports the empty
  `forall`, and the model is converted once and kept, so `--check` is the counts, the engine choice, and an exit
  before the solve.
- *(revised)* Warnings now print on every run, and there is no way to turn them off. Of the 789 benchmark and
  example files, 72 draw "variable x appears in no constraint and not in the objective": true, and of no interest
  to whoever generated the file. `--quiet` drops them. In Rust they are strings (`Problem::warnings() ->
  Vec<String>`); a `Warning` with a kind, a place, and a message lets a caller filter.
- The help says the formats are documented "in the source repository"; after `cargo install` there is none. The
  two format documents are already compiled in for docs.rs: `optimizam --docs problem|result|cli` and
  `--schema problem|result` would print them.
- `--nonzeros` leaves zero entries out of indexed variables (a 50 by 50 assignment is 2,500 entries, nearly all 0)
  and marks the result `"omitted": "zeros"`. Off by default. *(revised)* The same for `--constraints`, which lists
  every comparison constraint with every solution: `--constraints=binding` for the ones with no slack.

## 13. Finish symbolic variables and loaded models in the Rust API

A Rust user can create symbolic variables but use them only in `is`, `is_not`, `equals`, and `all_different`:
`table` takes integer tuples, and `table_of` and `element_keyed` are crate-private. `constraint_named` searches
only `expr` constraints (`src/api/builder.rs:430`), so a loaded `all_different` or `no_overlap` can't be removed by
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
  other kind (`src/api/builder.rs:53`). Conflicts and results read both through one helper; `constraint_named`
  reads only the first. One store, behind `set_name` (proposal 25 goes further).
- Warnings are worked out for problem files only (`Problem::warnings`). A model built in Rust can also have a
  variable that nothing uses, two constraints with one name, or bounds that leave a variable no value:
  `Model::warnings()`.
- `Solution::slack(id)` is None unless the solve had `Options::constraints`, because the solution doesn't hold
  the constraints. The model does: `Model::slack(id, &Solution)` needs no option.

## 14. Reading solutions without knowing the model

`Solution` offers `sol[v]`, `value`, `eval`, `element`, and the string-typed `get("x", &["a", "3"])`. There is no
iteration, so a generic caller must call `to_json()` and parse it back. Add `Solution::iter()` (name, key, value),
`Solution::family(&VarFamily)`, `get(name, impl Key)` (`("a", 3)` instead of `&["a", "3"]`), and `Display` for
`println!`.

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

## 16. Dense arrays as parameter values

`"values": [[3, 2, 2], [2, 1, 4], [4, 3, 1]]` for range-indexed tables, nested one level per index set, lengths
checked. Today's keyed layout is three times the characters and a place to drop a key; a list is the first thing a
writer tries. Results keep the keyed layout.

*(revised)* The reader is written by hand now, so this is one more arm of `param_def` in `src/problem.rs`, and
the message a list gets today is at least clear: `parameters.cap: expected a number or a table {"index": [..],
"values": {..}}, found a list`. A dense table also has no keys to misspell, which is what the new key check is
for, and is smaller and faster to read: the set-covering benchmark's table of 600,000 entries is most of a 6.6 MB
file.

## 17. Interval accessors

`IntervalVar` has no methods, so callers carry start, length, and interval side by side, and in files
`begin[j,s] + length[j,s]` is repeated because `op[j,s]` can't be asked for its end. `Model::start_of`, `end_of`,
`duration_of`; in files `start(op[j,s])`, `end(op[j,s])`. And let `no_overlap` and `cumulative` take
`impl IntoIterator<Item = IntervalVar>` as `all_different` does.

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

## 21. Named expressions, reported in results

A quantity used twice, or wanted in the answer, must be an auxiliary variable with bounds and a defining row
(`examples/load_balance.json`'s `load[m]`). An `"expressions"` section whose entries may be used wherever an
expression may (substituted: no column, no bound to get wrong), and are reported per solution beside `variables`.

## 22. Later

Tuple sets and sparse families (`"ARCS": [["a","b"], ...]`, `flow` indexed by `ARCS`: large, and the biggest
change to the format's data model here); `circuit`; `count`; full reification (`"iff": "b"`); `Options::node_limit`
with a documented default (today the global search stops after 2,000 nodes only when there is *no* time limit, so
adding `--time` can make a run longer, and `docs/cli.md:35` says those statuses never occur without `--time`).

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

## 24. Read and write MPS

Every diagnosis in the MILP work began by giving the same model to HiGHS or SCIP and comparing root bounds and
node counts. That works only for a model that started as an MPS file. For one that starts in our format, the route
is `tools/highs_reference.py`, a second implementation of the format in Python, which lives in the repository and
not in the installed command. In the other direction, standard models arrive through `tools/mps_to_json.py`, which
scatters the columns over families by type and bounds (proposal 10, step 3) and puts the MPS names in a side file,
so a result can't be read against the model it came from.

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

## 25. Constraint families: a name and a key, not a formatted string

*(new)* A variable knows its family and its key: `make[('north', 3)]` is the family `make` at `('north', 3)`,
and results lay it out as a nested table. A constraint knows only a string. An instance of an indexed constraint
of a problem file is named `capacity[north]` when the file is read (`instance_label` in `src/api/convert.rs`),
and from then on that text is all there is. The first six proposals each paid for it:

- `--explain` tries the instances of an indexed constraint together before it tries them one by one. To find
  them it cuts every name at its first `[` (`groups` in `src/api/explain.rs:166`). An element that contains `[`
  or `,` breaks that, and a model built in Rust has no way to say that forty constraints belong together.
- Results list `"hours[finishing]": { "slack": 0 }` in one flat object beside `"make": { "chairs": 40 }`. The
  old proposal 6 asked for constraints nested by index like variables, and couldn't have it.
- A conflict that contains all 50 instances of `capacity` lists 50 names; it can't say `capacity`.
- Names sit in two stores (proposal 13), and about fifty messages write `constraint {label}` by hand. An unnamed
  constraint is called by its place, `constraints[3]`, so `Error::new` deletes the word "constraint" in front of
  it with a string replacement.

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
  to 1e-4 (`docs/cli.md:86`). HiGHS, Gurobi, and CPLEX stop at 1e-4 unless told otherwise. I would keep both
  defaults (the examples' expected objectives are compared at 1e-6, and `complete` should mean what a newcomer
  thinks it means), state them per engine in `docs/cli.md`, and have `--gap` override either. *(revised)*
  Results now print a `gap`, so the option has something to be compared with. `tools/miplib_benchmark.py` now
  runs HiGHS at 1e-6 too; its cached numbers, which the docs quote, are still from the 1e-4 run.
- **Warm starts.** The plan offers `Options::start(&Solution)` "or `Model::hint(v, value)`". The variable already
  has `VarSpec::start` and the file has `start`; keep that one word: `Model::set_start(v, value)` next to
  `set_bounds`, plus `Options::start(&Solution)` for re-solves.
- **`Problem`.** See proposal 15: give it the data, or remove it.
- **The CP bound.** *(revised)* The weak level-0 bound used to show only in progress events. It is now in every
  stopped result as `objective_bound` and `gap`, without qualification (a first gap of 19.5 on crew_schedule).
  The bound search of the plan's section 5 matters more for it; failing that, mark such bounds.
- **`Model: Clone` keeps the id**, so handles work across clones, but after a variable is added to each clone one
  handle means two different columns and the "another model" check can't see it. *(revised)* This now reaches
  problem files: `Problem::to_model` hands out clones of the one model it keeps, where each call used to build a
  model with an id of its own. Give clones a new id.
- CLAUDE.md says `Element` is `#[non_exhaustive]`; it isn't (`src/api/family.rs:14`).

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
- With `--progress`, warnings and the final `done` line are JSON objects, and an error is still `error: ...` in
  plain text (proposal 26).

Documentation against behavior:

- `docs/problem-format.md:99` allows `v != e` as a whole `expr`; the reader and the grammar at `:258` don't.
- `docs/cli.md:35` ("those three statuses never occur" without `--time`) against `:87` and `src/solve.rs:164`.
- `SKILL.md:184` (`auto` uses "NLP"); `:10` and the description say "continuous nonlinear programs" while `:199`
  allows integers; `docs/solver-design.md:6` says "continuous" too.
- `SKILL.md:17` tells the user to clone and `cargo install --path .`; the README has `cargo install optimizam`.
- `src/main.rs:112`: the `--help` text for `auto` omits nonlinear problems.
- `docs/cli.md:61` and the `done` line after it show a progress line's keys in an order the command doesn't
  write (it writes them sorted, `elapsed` first).
- The expected results in `examples/` are in older shapes: most come from the reference solvers, say version
  `"1.0"`, and none has `sense` or `statistics`. Four write an objective as `12.0` where the command writes `12`,
  and three carry a `"$schema"` key the command never writes. Have the reference tools write `sense` and the current
  version, or say in `examples/` that these are answers to compare with, not what the command prints.

Messages:

- `--engine=nlp` on a linear problem with integers reports that the problem "is nonlinear" (`src/solve.rs:210`).

Rust API (`src/api/builder.rs` unless another file is named):

- Result first or last: `min_of(value, items)`, `abs_of(value, of)` against `element(index, items, value)`.
- Slices against iterators: `table(&[Variable], ..)`, `no_overlap(&[IntervalVar])` against
  `all_different(impl IntoIterator)`.
- `VarSpec::min`/`max` against `lower`/`upper` in the format and in `set_bounds`; `min` then means three things.
- `VarSpec::binary` says bounds "are ignored"; a file with bounds on a binary is an error.
- `add_family` ignores `spec.name` silently; `add_var` and `add_vars` use it.
- `Progress::bound()` against `SolveResult::objective_bound()`.
- Three `to_json` signatures: `Result<String, Error>`, `String`, `Option<String>`.
- `Variable` has `==` (handle identity) and `.eq(rhs)` (a `Constraint`).
- One field in files (`"solutions"`), two methods in Rust that undo each other: `all_solutions(bool)` and
  `solution_limit(n)`.
- `SolveResult::engine()` returns a string; `Conflict::constraints()` and `Problem::warnings()` return strings
  where the model has ids and kinds (proposals 11, 25, 12).
- `Options::presolve`, `value_symmetry`, and `large_neighborhood_search` are documented as debugging switches;
  hide them or move them to environment variables.

Naming between the three surfaces (not all worth changing; listed so the choice is deliberate):

| Concept | JSON | Rust | CLI |
|---|---|---|---|
| Conditional constraint | `enforce_if` | `only_if` | |
| Bounds | `lower`, `upper` | `min`, `max`, `bounds`; `set_bounds` | |
| Allowed values | `domain` | `domain(values)`, `values(&set)` | |
| Aggregates | `min`, `max`, `abs` | `min_of`, `max_of`, `abs_of` | |
| Every solution, or N | `"solutions": "all"`, `N` | `all_solutions(true)`, `solution_limit(n)` | |
| Time limit | | `time_limit(Duration)`, `deadline(Instant)` | `--time=SECONDS` |
| Threads | | `threads(n)` | `--threads=N` |
| Why infeasible | `conflict` | `explain(true)`, `conflict()` | `--explain` |
| Slack and duals | `constraints`, `reduced_costs` | `constraints(true)`, `slack`, `dual`, `reduced_cost` | `--constraints` |
| Local nonlinear solve | `"engine": "nlp"` | `Engine::Nlp` | `--engine=nlp` |
| Global nonlinear search | `"engine": "global"` | `Engine::Auto` only | `--engine=auto` only |
| Bound on the optimum | `objective_bound` | `objective_bound()`, `Progress::bound()` | `"bound"` in `--progress` |
| How far from optimal | `gap` | `gap()` | `"gap"` in `--progress` |
