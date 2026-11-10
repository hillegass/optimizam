# Proposals: the API and the problem and result formats

Written 2026-11-09 for the 0.2 release, from a review of the library, the command, the formats, and the skill as a
first-time user would meet them (about 110 small problem files were run through the command to see what it says),
and from the engine work of the same week. Nothing here is implemented except where it says so. Each proposal says
what gets in the way today, what to change, whether that breaks anything, and what it would cost. They are ranked:
the first twelve are the ones I would do before 0.2.

Problem format 1.3 and result format 1.2 are both unreleased (0.1.0 shipped 1.2 and 1.1), so additions can still go
into them without another version.

| # | Proposal | Value | Effort | Kind |
|---|----------|-------|--------|------|
| 1 | Say why a problem is infeasible, and what is unbounded | high | medium, in stages | additive |
| 2 | A limit on the number of solutions (`"solutions": 2`) | high | small | additive |
| 3 | Structural errors with a JSON path; hints for the common expression mistakes | high | small to medium | messages |
| 4 | Turn silent traps into errors or warnings | high | small | partly breaking |
| 5 | Result metadata: one version, engine, time, sense, gap, stop reason | high | small | additive |
| 6 | Report constraints: slack, then duals and reduced costs for LPs | high | small, then medium | additive |
| 7 | Rust: any number type, a `sum()` function, borrowed keys, typed reads | medium-high | small | additive |
| 8 | Expressions: `mod`, `div`, `card`, `first`, `last`, `pos`; inline `abs`/`min`/`max` | medium-high | small, then medium | additive |
| 9 | `!=` and strict comparisons in `expr`; literals as constraints in Rust | medium | small to medium | additive |
| 10 | Bounds from scalar parameters; bounds and interval fields as expressions | medium | small, then medium | additive |
| 11 | Name the engines for what they do: `global` and `local` | medium | small | breaking rename |
| 12 | Command line: optional RESULT, `--check`, built-in docs, `--nonzeros` | medium | small each | additive |

Proposals 13 to 22 follow, then comments on the 0.2 plan, what should stay as it is, and a list of small
inconsistencies with file and line.

## 1. Say why a problem is infeasible, and what is unbounded

`infeasible` is the result a modeler most needs help with, and it comes with nothing:

```json
{ "version": "1.0", "status": "infeasible", "solutions": [] }
```

That is the whole output for a row that can't be met, for `"lower": 5, "upper": 1`, and for a sum whose condition
matched nothing (`0 >= 1`). `skills/optimizam/SKILL.md` tells the agent to "check the constraints for a modeling
error", with nothing to go on. `unbounded` names no variable either.

Three stages, each useful alone:

1. **Free reasons (small).** When the model is refuted before any search (crossed bounds, a violated constant row, a
   row that can't hold within its columns' bounds), name the variable or constraint. For `unbounded`, list the
   variables with an objective coefficient and no bound in the improving direction: the usual cause is a forgotten
   bound.
2. **A conflict by deletion (medium)**, in `api/` on top of `Model::remove`, so it works for every engine: drop the
   objective, then try removing each named constraint group (a `forall` family is one group), keeping it removed
   when the rest is still infeasible; refine by instance inside the surviving groups if time remains. Opt-in and
   time-boxed, since it costs a solve per group.
3. **Later:** an elastic variant for linear rows ("which constraints must give, and by how much").

```rust
impl Model {
    /// A subset of the constraints that is infeasible by itself; None if the model is feasible.
    pub fn conflict(&self, options: &Options) -> Result<Option<Conflict>, Error>;
}
```

```sh
optimizam --explain --time=60 problem.json result.json
```

```json
{ "version": "1.2", "status": "infeasible", "solutions": [],
  "conflict": { "constraints": ["need_gold", "light"], "minimal": true } }
```

## 2. A limit on the number of solutions

`"solutions"` is `"one"` or `"all"`. Fourteen free binaries produce 16,384 solutions and a 2.9 MB result inside a
2-second limit, and the README's own example ("tell me whether the solution is unique") needs two solutions, not
all of them. `"solutions": 5` is rejected with `expected value at line 1 column 382`.

```json
"solve": { "method": "satisfy", "solutions": 2 }
```

At most N solutions (for `minimize`/`maximize`: at most N optimal ones). If the search ends first the status is
`complete`; if it stops at N, `incomplete` with stop reason `solution_limit`. In Rust, `Model::solution_limit(n)`
next to `all_solutions(bool)`. The enumeration exists; this is a counter and a stop reason.

## 3. Structural errors with a JSON path, and hints for the common expression mistakes

Expression errors are good: they name the constraint instance, the token, and the text. Structural errors come
straight from serde, and since generated files are usually one line, "line 1 column 283" locates nothing. Writing
a test file for this review, I guessed `{"range": [1, 3]}` for a set and got:

```
error: invalid problem file: data did not match any variant of untagged enum SetDef at line 1 column 45
```

and a variable without `"type"` gave `missing field type at line 1 column 300`. What a writer needs is
`sets.T: expected a list of elements or {"from": .., "to": ..}` and `variables[1] (carrier): missing "type"`.

- Read the file into `serde_json::Value` first and convert by hand (or hand-write `Deserialize` for the two
  untagged enums and the top level), so every structural error carries its path.
- Add hints for the mistakes a newcomer or an LLM makes:

| Written | Message today | Hint to add |
|---|---|---|
| `sum(i in I) a[i]*x[i] + b[i]*y[i]` | `unknown name i` | "i is bound only inside the sum's term; use parentheses" |
| `take[gold] == 1` | `unknown name gold` | "gold is an element of ITEMS; write `take['gold']`" |
| `take["gold"]` | `unexpected character '"'` | "string literals use single quotes" |
| `y[1] = 2` | `unexpected character '='` | "use `==`" |
| `y[t+1]` at the last t | `y[4] is outside the variable's index sets` | "T is 1..3; add a condition such as `: t < 3`" |
| `max(y[1], y[2]) <= 3` | `unknown function max` | "use the `max` constraint kind" |
| `"lower": "0"` | `refers to unknown parameter 0` | "write numbers without quotes" |

- One convention for unnamed constraints (`constraints[1]`) instead of both `(unnamed)` (`src/problem.rs:225`) and
  `#2` (`src/api/convert.rs:504`).
- In Rust, `constraint!(x != y)` fails with "recursion limit reached while expanding `constraint!`" (it falls through
  every arm). Add `compile_error!` arms for `!=`, `<`, `>`, and for no comparison at all.

The skill's repair loop runs on these messages, which is why this ranks third.

## 4. Turn silent traps into errors or warnings

Each of these ran to `complete` or `infeasible` with nothing on stderr:

- **Parameter keys outside their set are ignored.** `"pay": {"index": ["T"], "values": {"1": 1, "2": 2, "7": 9},
  "default": 5}` with T = 1..3 loads; a misspelled key silently takes the default and the answer is wrong.
- **Literal typos never match.** `"enforce_if": "carrier == 'raill'"` drops the constraint (an open decision in
  CLAUDE.md).
- **`<` between string elements is alphabetical** (`src/api/convert.rs:311`), not the set's order: with days
  `mon..sun`, `d < 'tue'` is true for `fri`.
- **An empty `forall`** adds no constraints; **crossed bounds** are `infeasible` with no mention of the variable;
  two constraints may share a name; an inner binding may shadow an outer one or a parameter; unused variables and
  parameters pass without comment.

Errors for what is almost certainly a mistake: a parameter key outside its index set; a string *literal* outside the
set of the symbolic variable it is compared with (elements that arrive through a binding or a table keep "never
matches": they are data, a literal is code; in the builder `v.is("dril")` is the literal case); `<`, `<=`, `>`, `>=`
between two strings, pointing to `pos(i)` (proposal 8). Warnings for the rest, on stderr from the command and
through `Options::log` or a `Problem::warnings()` in Rust. The three errors break files that relied on the old
behavior; I doubt any exist.

## 5. Result metadata: one version, and enough to read a result without the problem

The plan's step 6 adds `stop_reason` and `statistics: {time, nodes}`. Beyond that:

- `version` varies with the status (`"1.1"` for the two nonlinear statuses, else `"1.0"`: `src/result.rs:65`), so a
  reader can't use it to know which fields may be present.
- Whether `objective_bound` is an upper or lower bound depends on `solve.method`, which is in the other file.
- The bound appears only with two statuses; a `timeout` or `incomplete` result can't carry the bound the search
  proved.
- The result schema has `additionalProperties: false` at the top, so "ignorable by readers of 1.1" is false for any
  reader that validates.
- `--progress` has no last line: a stream consumer can't tell that the final objective was proven.
- With `--threads` (now implemented for CP and MILP), which engine ran and on how many threads is worth recording.

```json
{
  "version": "1.2",
  "status": "not_proven_optimal",
  "sense": "minimize",
  "solutions": [ { "objective": 930, "variables": { } } ],
  "objective_bound": 655,
  "gap": 0.2957,
  "stop_reason": "time_limit",
  "statistics": { "engine": "cp", "threads": 10, "time": 60.0, "nodes": 48211 }
}
```

`version` is always the writer's version; `gap` is as `Progress::gap` defines it; `statistics.engine` is what `auto`
chose; `stop_reason` covers every `StopReason` (add `StopReason::as_str()`); `objective_bound` whenever there is one
and no proof. One policy sentence in the format document ("minor versions only add fields; readers ignore fields
they don't know"), and the schema's top-level `additionalProperties: false` goes. In Rust:
`SolveResult::engine()`, `gap()`, `elapsed()`, `nodes()`. `--progress` ends with `{"event": "done", ...}`.

## 6. Report constraints: slack, then duals and reduced costs for LPs

Nothing in a result is about constraints. "Which resource is the bottleneck?" and "what is an extra hour worth?"
are the first questions after an LP solve. PuLP, JuMP, and HiGHS all answer them.

1. **Activity and slack of named `expr` constraints** (small: evaluate the stored rows at the solution), opt-in so
   default results stay small: `optimizam --constraints`, `Solution::slack(ConstraintId)`.

   ```json
   "constraints": { "carpentry": { "slack": 0 }, "capacity": { "north": { "slack": 0 }, "south": { "slack": 12 } } }
   ```

2. **Duals and reduced costs** (medium), for continuous linear problems first: `"dual"` next to `"slack"`,
   `SolveResult::dual(ConstraintId)`, `reduced_cost(Variable)`. The simplex has them; the LP presolve now
   substitutes columns out, so the simple route is to solve without presolve when duals are asked for. Define the
   sign once: the change of the optimal objective per unit of the right side, in the problem's own sense.

Duals are easy to get subtly wrong and aren't unique at degenerate optima; step 1 has none of that risk and answers
"what is binding".

## 7. Rust: any number type, a `sum()` function, borrowed keys, typed reads

From the repository's own code:

- Only `f64` and `i32` convert into expressions and bounds, so `usize` and `i64` need casts everywhere:
  `bounds(lo as i32, hi as i32)` (`tests/builder.rs:195`), `best[start] + length as f64` (`examples/job_shop.rs:41`).
- Every sum needs a turbofish: `(0..4).map(|i| weight[i] * take[i]).sum::<Expression>()` (18 places).
- Keys must be owned or exactly `&str`: `names.iter().map(|p| ship[(p, c)])` fails for `p: &&str`.
- Integer values are read through `f64`: `s[take[i]] == 1.0`, `s[c] as i64`.
- `Element` has no accessors, so the documented way to use a key is to format it (`src/api/builder.rs:373`).
- `between` takes only `f64` while `leq`/`geq`/`eq` take `impl Into<Expression>`.

```rust
pub fn sum<T: Into<Expression>>(terms: impl IntoIterator<Item = T>) -> Expression;   // no turbofish
impl From<i64> for Expression {}      // and u32, usize, f32 (right-hand sides, list items)
impl VarSpec { pub fn bounds(self, lower: impl Number, upper: impl Number) -> Self; }  // a sealed trait
impl<T: KeyPart> KeyPart for &T {}
impl Solution { pub fn int(&self, v: Variable) -> i64; pub fn is_one(&self, v: Variable) -> bool; }
impl Element { pub fn as_str(&self) -> Option<&str>; pub fn as_int(&self) -> Option<i64>; }
```

Scalars on the *left* of an operator (`2 * x`) should stay `i32` and `f64` only: a second integer impl would leave
the type of `2 * x + 1` undecided. That needs a doctest before anyone relies on it.

## 8. Expressions: index arithmetic, and inline `abs`/`min`/`max`

The expression language has `+ - * / ^` and eight real functions, all for nonlinear models. Index arithmetic has
none, and the examples work around it with data: `examples/sudoku.json` defines an 81-entry `box` table because
`(r-1) div 3` can't be written; `examples/tsp_tour.json` repeats the size of `STOPS` as a parameter and splits the
return leg off because `city[t mod n + 1]` can't be written; `examples/shift_rota.json` hard-codes the last day.

Stage 1 (small), all on constants, evaluated when the model is built:

| Function | Meaning |
|---|---|
| `mod(a, b)`, `div(a, b)` | remainder and floored quotient of integers |
| `floor(a)`, `ceil(a)`, `abs(a)`, `min(a, b, ...)`, `max(a, b, ...)` | on constants |
| `card(S)`, `first(S)`, `last(S)`, `at(S, k)` | a set's size, ends, and k-th element |
| `pos(i)` | the 1-based position of a bound index in its set |

Stage 2 (medium): `abs(e)`, `min(...)`, `max(...)` of *variable* affine expressions in `expr` and the objective,
lowered to the existing kinds with auxiliary variables whose bounds come from the argument's bounds.
`examples/production_smoothing.json` declares three auxiliary families with hand-picked bounds for what is
`sum(t) 2*abs(make[t] - demand[t])`; hand-picked bounds on auxiliaries are where generated models go wrong.

## 9. `!=` and strict comparisons in `expr`; literals as constraints in Rust

`"expr": "y[1] != y[2]"` fails; the idiom is a two-item `all_different`. `docs/problem-format.md:91` says a
symbolic variable may appear in `v != e` "as a whole `expr` constraint", but the reader rejects it (fix the
document now, whichever way this goes). In Rust there are four spellings near "equals" (`constraint!(x == 3)`,
`x.eq(3)`, `x.equals("mill")`, `x.is("mill")`) and none for "differs".

In files: `!=` on integer-valued sides (the pair encoding `all_different` uses), and `<`, `>` on integer-valued
sides (`<= rhs - 1`); on continuous expressions all three are errors that say why. In Rust: `Expression::ne`,
`impl From<Literal> for Constraint` so that `m.add(machine.is("mill"))` and `m.add(x.is_not(3))` work, and
`Variable::equals` deprecated.

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

## 11. Name the engines for what they do

`Engine::Nlp` and `--engine=nlp` mean "a local interior point solve, continuous variables only". The engine `auto`
uses for nonlinear problems, the global search, has no name and can't be asked for. The skill itself says `auto`
uses "NLP for problems with nonlinear terms" (`SKILL.md:170`), which in `--engine` terms is false, and
`--engine=nlp` on a linear knapsack answers "the problem is nonlinear" (`src/solve.rs:162`).

`Engine::{Auto, Milp, Cp, Global, Local}`; `--engine=global|local`, with `nlp` accepted as an alias of `local` the
way `mip` is of `milp`. `statistics.engine` (proposal 5) then reports one of four concrete names. A second engine
rename in one release (`Mip` became `Milp`), so better now than after 0.2.

## 12. Command line: optional RESULT, `--check`, built-in docs, `--nonzeros`

- `optimizam problem.json` is a usage error; default RESULT to stdout.
- `--check` parses and builds without solving and prints what was built (variables and constraint instances per
  name, the engine `auto` would pick, warnings): how one catches a `forall` that expanded to nothing or to far too
  much. In Rust, `Model::check()`.
- The help says the formats are documented "in the source repository"; after `cargo install` there is none. The
  two format documents are already compiled in for docs.rs: `optimizam --docs problem|result|cli` and
  `--schema problem|result` would print them.
- `--nonzeros` leaves zero entries out of indexed variables (a 50 by 50 assignment is 2,500 entries, nearly all 0)
  and marks the result `"omitted": "zeros"`. Off by default.

## 13. Finish symbolic variables and loaded models in the Rust API

A Rust user can create symbolic variables but use them only in `is`, `is_not`, `equals`, and `all_different`:
`table` takes integer tuples, and `table_of` and `element_keyed` are crate-private. `constraint_named` searches
only `expr` constraints (`src/api/builder.rs:425`), so a loaded `all_different` or `no_overlap` can't be removed by
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

## 14. Reading solutions without knowing the model

`Solution` offers `sol[v]`, `value`, `eval`, `element`, and the string-typed `get("x", &["a", "3"])`. There is no
iteration, so a generic caller must call `to_json()` and parse it back. Add `Solution::iter()` (name, key, value),
`Solution::family(&VarFamily)`, `get(name, impl Key)` (breaking: `("a", 3)` instead of `&["a", "3"]`), and
`Display` for `println!`.

## 15. Data apart from the model: `--data`, and a job for `Problem`

The format separates data (`sets`, `parameters`) from the model, but only inside one file, and in Rust
`Model::from_file` has already folded the parameters into coefficients. The open decision "keep `Problem` or
deprecate it" has no answer while `Problem` can only parse and solve. Let `Problem` be the symbolic form, where
data changes (`set_parameter`, `set_set`, `to_json`), and `Model` the expanded form, where structure changes; on
the command line, `--data week12.json` and `--set capacity[north]=50`. If the API should shrink instead, deprecate
`Problem` now.

## 16. Dense arrays as parameter values

`"values": [[3, 2, 2], [2, 1, 4], [4, 3, 1]]` for range-indexed tables, nested one level per index set, lengths
checked. Today's keyed layout is three times the characters and a place to drop a key; a list is the first thing a
writer tries, and it fails with the `untagged enum ParamDef` message. Results keep the keyed layout.

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

`timeout` means "no solution found and nothing proven", whatever the cause; with stop reasons in results, files
will read `"status": "timeout", "stop_reason": "node_limit"`. Cheap and additive: `Status::has_solution()` and
`is_proven()`, so callers stop matching on eight variants. I would leave the names alone.

## 20. Version and extensibility policy

The writer must pick among four `version` values by feature, and the reader ignores the choice (`"1.0"` with
`all_different` loads; so does `"1.9"`). Unknown fields are rejected everywhere, including a top-level
`"description"`, which generated files often carry. One paragraph in each format document ("minor versions add; a
reader accepts every version up to its own and rejects a newer minor"), tell writers to write the current version,
and allow `name` and `description` at the top level.

## 21. Named expressions, reported in results

A quantity used twice, or wanted in the answer, must be an auxiliary variable with bounds and a defining row
(`examples/load_balance.json`'s `load[m]`). An `"expressions"` section whose entries may be used wherever an
expression may (substituted: no column, no bound to get wrong), and are reported per solution beside `variables`.

## 22. Later

Tuple sets and sparse families (`"ARCS": [["a","b"], ...]`, `flow` indexed by `ARCS`: large, and the biggest
change to the format's data model here); `circuit`; `count`; full reification (`"iff": "b"`); reading MPS in Rust
(`tools/mps_to_json.py` is the specification); `Options::node_limit` with a documented default (today the global
search stops after 2,000 nodes only when there is *no* time limit, so adding `--time` can make a run longer, and
`docs/cli.md:31` says those statuses never occur without `--time`).

## On the 0.2 plan and its open decisions

- **Threads.** `Options::threads` and `--threads` exist now (CP portfolio, MILP branch and bound). Results with
  more than one thread vary from run to run; the docs say so. A `seed` option isn't worth adding until some part
  of a parallel run is reproducible.
- **Warm starts.** The plan offers `Options::start(&Solution)` "or `Model::hint(v, value)`". The variable already
  has `VarSpec::start` and the file has `start`; keep that one word: `Model::set_start(v, value)` next to
  `set_bounds`, plus `Options::start(&Solution)` for re-solves.
- **Symbolic typos.** Proposal 4: an error for literals, "never matches" for elements from bindings and tables.
- **`Problem`.** Keep it only if it gets a job (proposal 15); otherwise deprecate it in 0.2 while the API is small.
- **The CP bound.** `Progress::gap` presents the weak level-0 bound without qualification (a first gap of 19.5 on
  crew_schedule). If the bound search doesn't land, consider marking such bounds.
- **`Model: Clone` keeps the id**, so handles work across clones, but after a variable is added to each clone one
  handle means two different columns and the "another model" check can't see it. Document it, or give clones a new
  id.
- CLAUDE.md says `Element` is `#[non_exhaustive]`; it isn't (`src/api/family.rs:13`).

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

## Inconsistencies and papercuts

Documentation against behavior:

- `docs/problem-format.md:91` allows `v != e` as a whole `expr`; the reader and the grammar at `:242` don't.
- `docs/cli.md:31` ("those three statuses never occur" without `--time`) against `:81` and `src/solve.rs:119`.
- `SKILL.md:170` (`auto` uses "NLP"); `:9` and the description say "continuous nonlinear programs" while `:82` and
  `:180` allow integers; `docs/solver-design.md:6` says "continuous" too.
- `SKILL.md:16` tells the user to clone and `cargo install --path .`; the README has `cargo install optimizam`.
- `src/main.rs:93`: the `--help` text for `auto` omits nonlinear problems.
- `docs/cli.md:58` shows a progress line's keys in an order the command doesn't write (it writes them sorted).
- `examples/job_shop.result.json` and `plant_carriers.result.json` have `"objective": 12.0` and `800.0`; the
  command writes `12` and `800`. Some example results carry a `"$schema"` key the command never writes.

Messages:

- `--engine=nlp` on a linear problem with integers reports "the problem is nonlinear" (`src/solve.rs:162`).
- Arity errors read as range errors: `take[gold,1] is outside the variable's index sets`.
- A string element prints without quotes: `y['1']` gives `y[1] is outside ...`, although `y[1]` exists.

Rust API (`src/api/builder.rs` unless another file is named):

- Result first or last: `min_of(value, items)`, `abs_of(value, of)` against `element(index, items, value)`.
- Slices against iterators: `table(&[Variable], ..)`, `no_overlap(&[IntervalVar])` against
  `all_different(impl IntoIterator)`.
- `VarSpec::min`/`max` against `lower`/`upper` in the format and in `set_bounds`; `min` then means three things.
- `VarSpec::binary` says bounds "are ignored"; a file with bounds on a binary is an error.
- `add_family` ignores `spec.name` silently; `add_var` and `add_vars` use it.
- `Progress::bound()` against `SolveResult::objective_bound()`; `Progress::gap()` without `SolveResult::gap()`.
- Three `to_json` signatures: `Result<String, Error>`, `String`, `Option<String>`.
- `Variable` has `==` (handle identity) and `.eq(rhs)` (a `Constraint`).
- `Options::presolve`, `value_symmetry`, and `large_neighborhood_search` are documented as debugging switches;
  consider `#[doc(hidden)]` before 1.0.

Naming between the three surfaces (not all worth changing; listed so the choice is deliberate):

| Concept | JSON | Rust | CLI |
|---|---|---|---|
| Conditional constraint | `enforce_if` | `only_if` | |
| Bounds | `lower`, `upper` | `min`, `max`, `bounds`; `set_bounds` | |
| Allowed values | `domain` | `domain(values)`, `values(&set)` | |
| Aggregates | `min`, `max`, `abs` | `min_of`, `max_of`, `abs_of` | |
| Every solution | `"solutions": "all"` | `all_solutions(true)` | |
| Time limit | | `time_limit(Duration)`, `deadline(Instant)` | `--time=SECONDS` |
| Threads | | `threads(n)` | `--threads=N` |
| Local nonlinear solve | | `Engine::Nlp` | `--engine=nlp` |
| Global nonlinear search | | `Engine::Auto` only | `--engine=auto` only |
| Bound on the optimum | `objective_bound` | `objective_bound()`, `Progress::bound()` | `"bound"` in `--progress` |
