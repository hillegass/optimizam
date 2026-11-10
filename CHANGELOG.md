# Changelog

All notable changes to optimizam are recorded here. The project follows
[semantic versioning](https://semver.org); before 1.0, minor versions may
change the API.

## Unreleased

- Functions in expressions (problem format 1.3). Of data, sets, and indexes, worked out when the model is built:
  `mod(a, b)` and `div(a, b)` (rounded down), `floor`, `ceil`, `card(S)`, `first(S)`, `last(S)`, `at(S, k)`,
  `pos(i)` (an index's place in its set), and `pos(e, S)`. So `x[mod(t, card(T)) + 1]` wraps around, and a Sudoku
  cell's box needs no table. And `abs(e)`, `min(a, b, ...)`, `max(a, b, ...)`, `min(i in I) e`, and `max(i in I) e`
  of numbers or of expressions with variables: `sum(t in T) abs(make[t] - demand[t])` as an objective,
  `max(j in JOBS) end[j]` as a makespan. The solver adds the variable (results don't show it); where a term is
  minimized or bounded above it is plain rows, so least-absolute-deviation fits are linear programs and nothing
  needs bounds. In Rust: `Expression::abs`, `Variable::abs`, and the functions `max` and `min`.
  `examples/line_fit.json` is new; `sudoku.json`, `sudoku_unique.json`, `tsp_tour.json`, and `shift_rota.json` use
  the functions in place of tables and repeated constants. The error for ordering named elements
  (`d < 'tue'`) now shows what to write: `pos(d) < pos('tue', DAYS)`.
- Fewer casts and annotations in Rust: any integer or floating-point type is a coefficient, a right-hand side, or
  a bound (the sealed `Number` trait: `bounds(0, n)` with a `usize`, `x * weight` with an `i64`); `sum(terms)`
  adds up terms without `::<Expression>`; a borrowed key is a key (`make[p]` for `p: &&str`);
  `Solution::int` and `is_one` read integer and binary values; `Element::as_str` and `as_int`. A number on the
  left of an operator is still an `i32` or an `f64` (`2 * x`).
- One run reports every error in a problem file: what can't be read, what doesn't fit together, and each
  constraint's first error, without repeating what only follows from an error already given.
  `Error::errors()` iterates over them, `Error::place()` and `name()` say where each is, and the command prints a
  line for each (JSON events with `--progress`). Misspelled names, sets, and elements are answered with the one
  that was probably meant.
- Constraints in results, on request (`Options::constraints`, `--constraints`): each solution's slack in every
  comparison constraint (`Solution::slack`), and for a linear program the constraints' dual values and the
  variables' reduced costs (`SolveResult::dual`, `reduced_cost`), in the problem's own sense.
- Results say more (result format 1.2): `sense`; `objective_bound` whenever the search stopped with one, also with
  `timeout`; `gap`; `stop_reason`; and `statistics` with the engine that ran, its threads, the time, and the size
  of the search. In Rust: `SolveResult::gap`, `engine`, `elapsed`, `nodes`, and `StopReason::as_str`. `--progress`
  ends with a `done` line that carries the final status.
- Breaking: every result file says version "1.2" (it was "1.0" or "1.1" by status), and the result schema no
  longer forbids unknown fields at the top level: later versions add fields, and readers should ignore the ones
  they don't know.
- Silent traps in problem files are now errors or warnings. Errors: a key of a parameter table that isn't an
  element of its index set (it used to be ignored, and the element it was meant for took the default); a string
  literal compared with a symbolic variable that isn't one of its values (`carrier == 'raill'`; in Rust,
  `v.is("dril")`, `is_not`, and `equals`), where an element that arrives through an index still just never
  matches; and `<`, `<=`, `>`, `>=` between two named elements, which compared them alphabetically. Warnings
  (`Problem::warnings`, and `warning:` lines on stderr from the command): a `forall` that matches nothing, an
  index that hides another name, two constraints with one name, unused parameters and variables, and a problem
  that is infeasible on its face.
- `Problem` converts its file once and keeps the model, so solving it again doesn't read the expressions again.
- Error messages say where and what to do. A structural error in a problem file names the place as a path with
  the item's name (`variables[1].type (carrier): expected "continuous", "integer", or "binary", found "int"`)
  instead of a line and column; misspelled fields are suggested and numbers in quotes pointed out. The usual
  expression slips come with a hint: a sum's index used after its term, a set element without quotes, `=` for
  `==`, double quotes, `max(...)` as a function, a computed subscript one past the end of its set. The wrong
  number of subscripts is no longer reported as a range error, and string elements are shown in quotes.
  `constraint!(x != y)` and `constraint!(x < y)` are compile errors that say what to write.
- Breaking: a constraint without a name is called `constraints[k]` everywhere (its place in the problem file's
  list, or its id in a builder model, counting from 0), where messages said `#k` or `(unnamed)`.
- The library no longer depends on `serde` directly, only on `serde_json`.
- A limit on the number of solutions: `"solutions": N` in problem files (format 1.3) and
  `Model::solution_limit(n)`. The result has at most N solutions (optimal ones, with an objective): all there are
  with status `complete`, or N with `incomplete` and `StopReason::SolutionLimit`. Two is enough to tell whether a
  solution is the only one (`examples/sudoku_unique.json`).
- Infeasible and unbounded results say why. An infeasible result has a `conflict` (`SolveResult::conflict`,
  result format 1.2) when the cause can be read off the model: variables whose bounds or domain leave them no
  value, and constraints that can't hold within their variables' bounds, `0 >= 1` among them. With
  `Options::explain`, `--explain`, or `Model::conflict`, a search follows for a small set of constraints that
  can't hold together, each of them needed (it costs about a solve per constraint, within the time limit). An
  unbounded result lists the variables with an objective coefficient and no bound in the improving direction
  (`SolveResult::unbounded_variables`, `unbounded` in result files).
- Faster LPs, MILPs, and nonlinear searches (the LP engine underlies all three): the simplex method updates its
  factorization by Forrest–Tomlin, works with sparse vectors, and keeps its factorization from one node's LP to
  the next; LPs are presolved by substituting columns out through equality rows. On the Netlib LPs the time is
  0.57 of 0.1's (shifted geometric mean), on a par with HiGHS; `RESULTS.md` has the tables.
- Stronger MILP proofs: root cut rounds go on while the bound still gains, with lifted cover inequalities beside
  the mixed-integer rounding cuts; integer columns are fixed by reduced cost at every node; the search restarts
  from the root when that has fixed a fifth of them; and best-bound search plunges. Of 58 MIPLIB instances, 28 are
  proven within 30 s instead of 22 (38 instead of 34 with ten threads). Lot sizing got slower (the generated
  `lotsize_big` takes 24 s instead of 11 s); `RESULTS.md` has the tables.
- `Options::threads` and `--threads`: with more than one thread, the searches run in parallel. The CP engine runs
  a portfolio of searches that share solutions and learned clauses when it optimizes or looks for one solution (on
  27 job shop and project scheduling instances, 26 proven within 20 s instead of 23, in 0.59 of the time). The MILP
  engine and the global nonlinear search share their search tree among workers: with ten threads, 0.30 of one
  thread's time on the MILP benchmark, 34 of 58 MIPLIB instances proven within 30 s instead of 22, and 0.56 of the
  time on MINLPLib, where 312 of 342 instances are proven within 10 s instead of 307. By default a search uses one
  thread per core, at most 8 (`threads(0)` and `--threads=0` say the same); with one thread a run repeats exactly.
- Fixed: a local nonlinear solve could loop forever at a feasible point, ignoring the time limit and cancellation.
- `CancelToken` and `Options::cancel_on`: stop a solve from another thread.
  It returns normally, with the best solution found so far, as if its time
  limit had been reached.
- `Options::on_progress`: a callback with each better solution and, about
  twice a second, the bound or a heartbeat (`Progress`: objective, bound,
  gap, the best solution, and a result file for it). Returning
  `Control::Stop` stops the solve, for example at a gap.
- The CP engine reports a bound when stopped early (in results and progress
  events), from propagation at decision level 0: valid, but often weak.
- Index sets and variable families: `Set` (named, of integers or strings),
  `Model::add_family` and `add_family_with` (per-element specs), and
  `VarFamily`, indexed by keys such as `x[("north", 3)]`; results report
  families as tables keyed by element.
- Symbolic variables: `VarSpec::values` with a set of strings,
  `Variable::is("mill")` literals, `Solution::element`.
- Lookups and editing: `Model::family`, `var_named`, `constraint_named`,
  `set_bounds`, and `remove`.
- Breaking: `Variable::is` and `is_not` take any element (`impl
  Into<Element>`), and `Literal` is no longer `Copy`.
- `Model::to_json` writes integer-keyed families over their sets, and now
  rejects models that solving would reject.
- Breaking: the MILP engine is `Engine::Milp` and `--engine=milp` (it was
  `Mip`/`mip`, which suggests mixed-integer nonlinear problems too). The
  command still accepts `--engine=mip`.
- Problem format 1.3: string literals in expressions, in single quotes
  (`x['north', 3]`, `"enforce_if": "machine[t] == 'mill'"`; `'o''hare'`
  for a quote). `Model::to_json` uses them for string keys and comparisons
  with strings, writing version 1.3 only then.
- `Variable::equals`: fix a variable to a number or (symbolic) an element.
- Tables accept symbolic variables, and a tuple naming an element outside a
  variable's values never matches, as in problem files (it was an error).
- Builder literals on an element outside a symbolic variable's set follow
  the format: `is` never holds and `is_not` always does (they were errors).
- `Model::from_json`, `Model::from_file`, and `Problem::to_model`: read a
  problem file into a model to change before solving (its variables are
  families, its constraints named by instance, such as `capacity[north]`).
  Problem files now go through the same path as builder models.
- The nonlinear engine gets only the expression nodes its rows and
  objective use: on MINLPLib, five time-limited instances now end with
  better solutions, and large sums load several times faster.
- Variable names may be function names (`exp`, `ln`), as in problem files.
- `SolveResult::stop_reason`: why a search stopped early (`TimeLimit`,
  `Canceled`, `Callback`, `NodeLimit`, `NoProgress`).
- The command's `--incumbents=FILE` keeps `FILE` up to date with the best
  solution so far (written whole, so a killed run leaves a valid result),
  and `--progress` writes progress to stderr as JSON lines.
- The command handles Ctrl-C, SIGTERM, and SIGHUP: the solve stops and the
  result is written with the best solution found (exit code 0). A second
  Ctrl-C quits at once (exit code 130). New dependency of the command (not
  of the library): `ctrlc`.

## 0.1.0 (2026-11-09)

The first release on crates.io, as a library and a command.

- The library API: `Model` (variables from `var()`, expressions with the
  arithmetic operators and `ln`, `exp`, `sqrt`, the trigonometric functions,
  and powers; constraints from `constraint!` or `leq`/`geq`/`eq`/`between`;
  `all_different`, `table`, `element`, `min_of`, `max_of`, `abs_of`,
  intervals with `no_overlap` and `cumulative`, and `Constraint::only_if`),
  `Problem` for JSON problem files, `Options`, and `SolveResult`/`Solution`.
- `Model::to_json` writes a model as a problem file (format 1.2).
- The `optimizam` command, behind the default `cli` feature. Its behavior is
  unchanged.
- The solver: LP/MILP (dual simplex, presolve, cuts, heuristics, branch and
  bound), CP (lazy clause generation with native scheduling and
  `all_different` propagators), and global nonlinear optimization (spatial
  branch and bound with an interior point method).
