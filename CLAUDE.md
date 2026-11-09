# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

LP/MILP, CP, and continuous NLP are implemented and pass the regression tests. Modules in `src/`:

- `problem.rs` reads the JSON; `expr.rs` parses expression strings; `model.rs` expands sets/sums into a flat `LinearModel` and rejects nonlinear terms.
- `lp.rs` converts it to a HiGHS-style `Lp` (minimization, row/column bounds, CSC + CSR).
- `simplex/` is the bounded dual simplex (`mod.rs`, after HiGHS `HEkkDual`) with its LU factorization (`factor.rs`).
- `mip.rs` is branch and bound (DFS until an incumbent, then best-bound; reliability branching, i.e. pseudocosts seeded by strong branching; `want_all` splits integral nodes until every variable is fixed).
- `cuts/` adds root cutting planes: `cmir.rs` (c-MIR on row aggregations, with variable-bound substitution), `gmi.rs` (Gomory mixed-integer cuts from the tableau), and `mod.rs` (rounds, numerical cleanup, selection, removing cuts that are no longer binding).
- `presolve.rs` reduces the problem before the search (bound and coefficient tightening, row and column removal, dual fixing with `"one"`); its postsolve only re-inserts fixed columns.
- `propagation.rs` is the domain store shared by the engines: bound propagation over linear rows with a trail of bound changes and their reasons (for conflict analysis). The MILP search propagates at every node and in strong-branching probes before solving the LP.
- `symmetry.rs` finds value symmetry (columns linked by `all_different` with interchangeable values, as in graph coloring) before encoding, and `encode.rs` adds value precedence rows for it (not with `"all"`).
- `heuristics.rs` finds solutions early: rounding, dives, and RENS/RINS sub-MIPs (which call presolve and branch and bound recursively, with `Settings::sub_mip`).
- `cp/` is the CP engine (lazy clause generation, after OR-Tools CP-SAT) for all-integer problems with finite bounds: `domains.rs` (integer trail of bound literals with reasons), `linear.rs` (exact integer bound propagation with lazy explanations), `clauses.rs` (two-watched-literal clauses), `solver.rs` (CDCL: 1-UIP, VSIDS, Luby restarts, clause deletion; earliest-start task branching until the first solution when there are scheduling constraints), `linear.rs` keeps each row's minimum activity incrementally along the trail, `scheduling.rs` (native `no_overlap` and `cumulative` propagators: timetabling, pairwise and detectable precedences, edge finding, with stored explanations), `alldiff.rs` (a native Hall-interval `all_different` propagator, for instances with a large multi-column item, which aren't encoded for CP), `lns.rs` (large neighborhood search between slices of the main search when optimizing: relaxed tasks re-solved with the others kept in incumbent order, or random columns fixed), and `mod.rs` (integer scaling and the satisfy/all/optimize modes).
- `nlp/` is the nonlinear engine, for continuous problems: `graph.rs` is the expression graph that `model.rs` builds for the nonlinear part of rows and the objective (`LinearModel::nonlinear`; hash-consed nodes, constant folding), with an `Evaluator` for values, gradients (reverse mode), and the Hessian of the Lagrangian (forward-over-reverse, with the pattern's columns colored to share directions); `ldl.rs` is a dense Bunch–Kaufman LDLᵀ that reports the inertia; `sparse_ldl.rs` is a sparse LDLᵀ with 1x1 pivots in a fixed minimum-degree order (QDLDL-style: elimination tree, up-looking numeric factorization, inertia from D), for KKT systems of size 150 or more; `interval.rs` (outward-rounded interval arithmetic for every op), `fbbt.rs` (bound propagation, forward and backward through inverses, to a fixed point), `convexity.rs` (each node's curvature over a box: composition rules with interval signs of f' and f'', products of powers, entropy terms), and `relax.rs` (a linear relaxation of fixed shape: McCormick, tangents and secants by curvature, tangent planes of convex quadratic sums, outer-approximation cuts on convex composite nodes, hint blocks for warm starts; LPs scaled, warm-started from the parent's basis, returning reduced costs; an infeasible verdict is confirmed with an elastic LP and ignored on badly scaled LPs) bound the problem over a box; `intsearch.rs` is integer local search (±1 moves on one integer, then on pairs, repairing violation before improving the objective; continuous variables solved for from equality rows, or by local solves), run at the root and from new incumbents; `ecp.rs` is outer approximation on the MILP engine for convex mixed-integer problems (cutting planes, then MILP rounds with Duran–Grossmann cuts at local solutions), tried before branching; `global.rs` is spatial branch and bound over boxes (best bound first, propagation with an incumbent cutoff, OBBT with cutting-plane rounds at the root and, with the cutoff, again at the root and at nodes down to depth 6 while it narrows boxes, branching on fractional integers or the most violated nonlinear term, local solves as heuristics, stall detection), which `--engine=auto` uses for every nonlinear problem, integer variables included, while `--engine=nlp` is the local solve alone; `ipm.rs` is the interior point method after IPOPT (Wächter–Biegler 2006: filter line search, second-order corrections, inertia correction, monotone barrier updates, bound relaxation, and a restoration phase that runs the same method on an l1 feasibility problem), over a small `Problem` trait with sparse derivatives (fixed Jacobian and Hessian patterns; quasi-definite sparse factorization with a small dc, iterative refinement against the true system, and the dense factorization as the fallback); `mod.rs` builds the standard form (fixed columns removed, slacks for inequality rows, gradient-based scaling) and maps the result (local optimum, local infeasibility, limits).
- `solve.rs` runs presolve, picks the engine (`--engine=auto|mip|cp|nlp`; auto uses NLP for problems with nonlinear terms, CP for `satisfy` and `"all"` problems it can take), and maps the outcome to a result status; `result.rs` writes the result file.
- The crate is a library (`lib.rs`, whose docs are the docs.rs front page) and the `optimizam` command (`main.rs`, behind the default `cli` feature, the only user of clap). The public API is in `api/`: `builder.rs` (`Model`, `var()`/`VarSpec`, the CP kinds, intervals), `expression.rs` (`Variable`, `Expression` as an affine part plus nonlinear `Tree` terms, the operators, `Constraint`, `Literal`, `constraint!`), `lower.rs` (a `Model` to the same `model::Assembly` that `model::build` makes from a problem file; `Assembly::finish` does symmetry detection and the encodings for both), `export.rs` (`Model::to_json`, flat format 1.2, auxiliary `_`-named variables for what the format can't say directly), `solution.rs` (`SolveResult`, `Solution`), `progress.rs` (`Progress`, `Event`, `Control`, `StopReason`, and the observer that turns engine reports into events), `options.rs`, and `error.rs`; `mod.rs` has `Problem` (problem files) and the shared solve path. Only `api/` and `Status`/`Engine` are public; the engines stay private so they can change. Before 1.0, keep public structs' fields private, options as chained methods, and enums `#[non_exhaustive]`, so features can be added without breaking users.
- `config.rs` holds the settings of the solve running on the thread (presolve, value symmetry, LNS, the statistics logger, the cancel flag; also the stop reason and the progress observer), installed by the public entry points with `config::scoped`; the engines read them with `config::debug()` and friends and log with `config::debug_log!` instead of `eprintln!`. With nothing installed (unit tests calling engines directly), the environment variables below decide.

Set `OPTIMIZAM_DEBUG=1` (the command, or unit tests; library users pass `Options::log`) to print presolve, cut, heuristic, and branch-and-bound statistics (including when each incumbent was found, and by what), and the nonlinear engine's iterations, to stderr. Set `OPTIMIZAM_NO_PRESOLVE=1` to skip presolve, for comparisons or to isolate a presolve bug. Set `OPTIMIZAM_NO_LNS=1` to turn off the CP engine's large neighborhood search, and `OPTIMIZAM_NO_SYMMETRY=1` to turn off value precedence.

Known limitations: no probing, conflict analysis, or cuts below the root, so some MILPs with weak root bounds still need large trees (lot sizing with many periods is the slowest class tried). Before branch and bound, `Lp::tighten_integer_rows` rounds all-integer rows to multiples of their coefficients' gcd, and presolve's congruence check handles equality rows with decimal coefficients and small-range columns; together they prove cases like `2x - 4y = 7` or `2x - 4y + 8.25z = 9.5` (z binary) infeasible. But an infeasible MILP with unbounded integer variables whose infeasibility needs several rows combined can still run forever without `--time`. The nonlinear engine factors KKT systems of size 150 and up sparsely (`tools/nlp_scaling.py`: 400 variables and 400 sparse nonlinear rows in 0.8 s, from 25 s dense; IPOPT 0.2 s); on large problems the root relaxation's LP now costs more than the local solve (chain-1000: 6.7 s vs 0.5 s), and the sparse factorization's fixed order can lose accuracy on badly scaled systems (it then falls back to the dense one, which is slow at that size). The global search struggles with nonlinear terms of unbounded variables (it branches, with a smaller budget, but may never close the gap), and objectives that measure a row's slack against a large constant (gear4) can't be proven: the rows' feasibility tolerance (1e-6 relative) exceeds the gap.

Tried and dropped (measured, so don't retry without a new idea):

- Global search: pseudocost branching on continuous variables (milestone 7: no gain) and on integers (nvs19 took 945 nodes instead of 2865, but nvs23/24 got worse). Difference-of-convex (eigenvalue) estimators of indefinite quadratic sums: valid, but over wide boxes they barely move the bound (nvs23's root bound -3734 vs -3740), and MINLPLib took 490 s instead of 472 s; OBBT with cutting rounds is what proves nvs23/24. More cutting-plane rounds per node (6 or 10 instead of 3): fewer nodes, more time (nvs24 not proven in 30 s with 10).
- OBBT tuning: fewer cutting rounds at nodes (0 to 2 instead of 5) leaves far bigger trees (nvs24 unproven after 4768 nodes with 0); node OBBT on every problem, without the stop when it stops narrowing boxes, costs more than it saves on circle packings (kall_*) and hs119 (4.5 s to 10.8 s); capping the cuts at 2x the relaxation's rows everywhere slowed nvs24, 4x slowed the kall problems (hence 3x at the root, 1x at nodes); loosening OBBT's results by only 1e-6 let LP error round an integer bound from 0 to 1 (a wrong answer on nvs16).
- Point boxes: offering only the local solve's exact point loses proofs whose last gap step is a tolerance-sized one (ex14_1_1); offering every point at the tolerance proves wrong optima (gear4); hence the 1e-4 limit on how much the box's own points may beat the local solve's.
- Integer search: with local solves at the full budget before outer approximation, m3 went from 0.14 s to 7.5 s (solves on infeasible fixings ran 3000 iterations): hence 60 iterations per solve, an iteration budget per search, and only a short search before outer approximation.
- Performance work on the LP engine (measured with `measure.py`, the shifted geometric mean over all five benchmarks; one full run takes about 12 minutes once the instances are downloaded, and single-instance timings vary by 5 to 10% between runs, so compare instruction counts under `valgrind --tool=callgrind` and only decide on back-to-back full runs). Kept: Markowitz pivots searched from count buckets (Factor::build sorted every active column per kernel pivot: 20% of d2q06c), free rows left out of simplex solves (relaxations are mostly free placeholder rows: hs110 500 of 1081), the kernel in flat arenas (allocation was 25% of hs081's instructions), relaxation LPs scaled in place, and the free list cleared in dual phase 1 (hs111lnp's OBBT LPs cycled for 112,000 iterations each); together 0.85 of the baseline score (two back-to-back pairs: 0.870 before the free-list fix, 0.852 with it), with no instance's outcome worse: hs111lnp answers in 2 s instead of timing out, and hs101/102/103/110 stall before their 10 s limits. Profiles: netlib and MILP time is ftran/btran (the U solve, then L, then the product-form etas) plus the factorization; the global search's time is LP overhead on tiny LPs (two factorizations per solve, Lp construction, allocation), with 1 to 4 LP solves per box plus up to 12 per nonlinear variable in node OBBT; cfl_large spends 82% in strong-branching probes of 150 to 450 iterations. Dropped: row-wise L and column-wise U copies for skip-sparse solves (no instruction-count gain, vectors are dense on netlib and cfl_large; changes FP summation order so search paths differ), refactorizing every 25 or 50 updates instead of 100 (slower: pilot87 9 s to 18 s), skipping the refactorization that confirms optimality after at most 20 updates (no gain, and nvs24's tree tripled), and computing LP scaling's nearest power of 2 from the exponent bits (rounds the sqrt(2) boundary differently from log2, which changed OBBT's root box on nvs24 and its tree from 435 to 6700 nodes). Beware: MINLP search paths are chaotic in the LP's rounding, so any change to LP numerics must be checked on nvs24 and the full MINLPLib run; nlp:hs085 is "wrong" (complete at -1.9051554 vs the published -1.90513375, outside 1e-6) in the baseline too.
- Profile-guided optimization (`cargo-pgo`, trained on the examples and a subset of every benchmark), on top of the release profile in `Cargo.toml` (fat LTO, one codegen unit, `panic = "abort"`): a negligible improvement of 1 to 3% (shifted geometric means: Netlib 0.971, CP 0.976, NLP 0.979, MILP 0.985, MINLPLib 0.995, where two runs of the same binary differed by up to 1%; `RESULTS.md`), not worth a second build pipeline and profiles that go stale. `-C target-cpu=native` wasn't tried: it can change floating-point rounding through fused multiply-adds, and with it the MINLP search paths.
- CP engine (measured on the CP benchmark and on 27 more JSPLIB and PSPLIB j30 instances with known optima, single-instance timings being chaotic in the search path): learned-clause minimization (local, with the implying literal earlier on the trail): worse on job shop (shifted geometric mean 3.88 s vs 3.55 s); the not-last/not-first rule for `no_overlap` (O(n^3 log n)): much worse (4.59 s vs 3.35 s, two fewer proven), its cost outweighs the pruning; restart unit 50 or 300 and VSIDS decay 0.9 or 0.99 instead of 100 and 0.95: no better; running edge finding only once the cheaper `no_overlap` rules stop changing bounds: no faster, and ft10 lost its proof in 60 s; a native Hall-interval `all_different` on top of the value encoding: slower on queens and sudoku, nothing gained. CP-SAT's remaining lead on scheduling (jobshop_ft10 2 s vs 30 s, tournament_12 1120 vs 1180) is its parallel portfolio: one CP-SAT worker takes 16 s on ft10 and ends tournament_12 at 1240.
- Sparse LDLᵀ for small KKT systems (size 30 and up): 1x1 pivots in a fixed order lose accuracy on barrier-scaled systems (jit1, size 71: backward error 1 by the third iteration, 4167 dense fallbacks, and the proof lost), and the dense factorization costs under a millisecond there: hence size 150. Accepting sparse solves by a normwise backward error (relative to |b| + max|K| |x|) let inaccurate directions through (jit1's differed from the dense ones by 1e-3 from the first iteration): hence the componentwise (Oettli–Prager) error. A SIMD or blocked dense kernel wasn't tried: the dense factorization already ran at about 1.9 GFLOP/s and was 99% of the time; sparsity, not the kernel, was the lever.

- Rust edition: **2024**; the minimum Rust is 1.88 (`rust-version`, checked in CI), for `let` chains
- A library and a binary target. Dependencies: `serde`, `serde_json`, and `clap` (optional, for the binary). Dual-licensed MIT or Apache-2.0.

## Users

`README.md` is for users: the library (`cargo add optimizam`), installing the binary (`cargo install optimizam` or `--path .`), and the Claude Code skill in `skills/optimizam/` (`SKILL.md`, plus `references/` with symlinks to `docs/problem-format.md`, `docs/result-format.md`, `docs/cli.md`, and `examples/`). Keep `SKILL.md`'s summary of the format and statuses, and `--help` in `src/main.rs`, in sync with the docs when the format or CLI changes. The format docs are also the docs.rs pages `optimizam::format::problem` and `format::result` (through `include_str!`), so their links are absolute GitHub URLs. Record user-visible changes in `CHANGELOG.md`. Publishing: `cargo package --list` must not include `tools/`, `skills/`, or benchmark data, and `cargo publish --dry-run` must pass.

## Plan: 0.2 API (in progress: steps 1 and 2 done)

Goals: indexed variable families, loading a problem file into a `Model` to edit it, progress callbacks, and
cancellation that returns the best solution found so far. Everything here is additive except where marked
**breaking** (allowed before 1.0; record it in `CHANGELOG.md`). Search behavior must not change: after each step,
compare `OPTIMIZAM_DEBUG=1` statistics on all examples against the previous release binary (only timings may differ),
as was done for the 0.1 library split.

### 1. Index sets and variable families

```rust
let plants = Set::new("PLANTS", ["north", "south"]);     // elements: strings or integers (`Element`)
let hours = Set::range("HOURS", 1..=24);
let x = m.add_family("open", &[&plants, &hours], var().binary());          // VarFamily
let cap = m.add_family_with("cap", &[&plants], |k| var().bounds(0, limit[k]));  // per-element specs
m.add(constraint!(x[("north", 3)] + x[("south", 3)] >= 1));
let total: Expression = x.iter().map(|(_key, v)| v).sum();
sol.get("open", &["north", "3"]); sol.family(&x)  // values by key
```

- Public types: `Element` (an enum of `i64` and `Arc<str>`, `From<i64>`, `From<&str>`, `Display`; `#[non_exhaustive]`),
  `Set` (named, ordered, distinct elements; `Clone`, cheap: an `Arc` inside), `VarFamily` (`Clone`, `Send + Sync`).
  `VarFamily` implements `Index<K: Key>` (panics on an unknown key, like a slice) and has `get(K) -> Option<Variable>`,
  `iter()`, `len()`, `name()`. `Key` is a sealed trait for `Element`-convertible scalars (one set), tuples of up to 4,
  and `&[Element]`.
- `add_vars(n, spec)` stays (a family over `0..n`, returning `Vec<Variable>`) for compatibility.
- Symbolic (string-valued) variables, the one part of format 1.2 the builder lacks:
  `var().values(&colors)` (a `Set` of strings or mixed), `v.is("red")`/`v.is_not("red")` literals, and
  `sol.element(v) -> Element`. Internally they are integer codes, as `model.rs` already does for set domains.
- Lookup by name, needed after loading a file: `m.family("open") -> Option<VarFamily>`,
  `m.var_named("x") -> Option<Variable>`, `m.constraint_named("cap[north]") -> Option<ConstraintId>`.
- Results already lay out indexed variables as nested tables (`VarInfo::elements`); families map onto that.
- Export: integer-element families can be written in format 1.2 (`x[3]`); string elements need string literals in
  expressions (format 1.3, below). `to_json` writes each family as an indexed variable over its sets, per-element
  bounds and starts as parameters, constraints still flat.

### 2. Loading a file into a Model (`to_model`)

Make the JSON front end produce a `Model` instead of its own `Assembly`: `model.rs` evaluates sets, parameters, sums,
and `forall` into builder items (families, `Expression`s, the CP kinds, intervals), and the one lowering path
(`api/lower.rs` to `Assembly::finish`) takes it from there. That deletes the checks duplicated between `model.rs` and
`lower.rs`, so both paths agree by construction. (Decompiling an `Assembly` back into a `Model` was considered: it
keeps two front ends and has to rebuild trees from the hash-consed graph.)

- Column order, row order, and constraint names (`cap[north]`) must come out exactly as today; the regression suite,
  the statistics comparison, and `cargo test --release --test builder` check it. Time loading the largest Netlib
  conversion (d2q06c, pilot87) before and after; `Expression`'s `BTreeMap` per row may need a `Vec` fast path.
- API: `Model::from_json(&str)`, `Model::from_file(path)`, and `Problem::to_model(&self) -> Model`. Then `Problem` is
  a thin wrapper; decide before 1.0 whether to deprecate it in favor of `Model::from_*`.
- Editing a loaded model: `m.set_bounds(v, lo, hi)`, `m.remove(ConstraintId)` (items become tombstones so ids stay
  valid), `m.set_objective`/`minimize`/`maximize` (already replace it). Variables can't be removed (columns are
  positional; fix one instead).
- `to_json` of a loaded model writes the expanded (flat) form, not the original sets and sums; document that.
- Errors keep the JSON path's messages and kinds (`Parse` vs `Invalid`).

### 3. Cancellation and the best solution so far

- Done: the cancel flag is part of `config::Config`, kept in its own thread-local, and every engine's deadline check
  is `config::expired(deadline)` (11 sites; the deadlines themselves are unchanged). Sub-MIPs, LNS, and the nonlinear
  sub-solves run on the same thread, so they stop with the parent. The flag check costs about 0.75 ns against 15 ns
  for the `Instant::now()` that each check already makes, so passing a `Limit` through every signature wasn't worth
  it. Any new loop that can run long must call `config::expired`.
- API: `CancelToken` (`Clone`, `Default`, `Send + Sync`; `cancel()`, `is_canceled()`) and
  `Options::cancel_on(&CancelToken)`. Solve on one thread, cancel from another; the solve returns normally.
- A canceled solve ends exactly like one that hit its time limit: `NotProvenOptimal`/`LocallyOptimal` with the
  incumbent and `objective_bound`, `Incomplete` for all-solutions, `Timeout` with none. Done:
  `SolveResult::stop_reason() -> Option<StopReason>` (`TimeLimit`, `Canceled`, `Callback`, `NodeLimit`, `NoProgress`
  for the global search's stall detection; `#[non_exhaustive]`), recorded by `config::expired` and
  `config::stopped_by`, the first reason winning, and `None` for proven outcomes; `Status` itself doesn't change.

### 4. Progress callbacks

```rust
let options = Options::default().on_progress(|p: &Progress| {
    if let Some(s) = p.solution() { save(s) }
    if p.gap().is_some_and(|g| g < 0.01) { Control::Stop } else { Control::Continue }
});
```

Done, as follows (`api/progress.rs`):

- `Progress` (accessors only): `event()` (`Event::Solution`, `Event::Bound`, `Event::Heartbeat`; non_exhaustive),
  `elapsed()`, `objective()`, `bound()`, `gap()` (`|objective - bound| / max(1, |objective|)`), `solution()` (the best
  so far, a full `Solution`), and `to_json()` (a `not_proven_optimal` result file; None for satisfy and
  all-solutions problems). Not done: `nodes()`. `Control::{Continue, Stop}`; `Stop` sets a per-solve flag (not the
  user's `CancelToken`, which other solves may share) that `config::expired` checks.
- Plumbing: `config::Report` (engine columns) is an explicit `report` field in `mip::Settings` and `cp::Settings` and
  a parameter of `nlp::global::solve`, set only by `solve.rs` for the top-level search: `Restored` maps presolved
  columns back with `post.restore` and converts the bound (`sign * (b + post.offset) + obj_offset`); `Direct` passes
  the global search's model-column points through. Both feed the `config::Observer` that `api::solve_with`
  installs after building the model. Sub-MIPs (RENS/RINS), outer approximation's MILPs, the zero-objective
  feasibility search, and LNS rounds report nothing: their columns are their own, and the parent reports a solution
  when it adopts it. Report sites: `mip.rs`'s `improve`, `cp/mod.rs`'s `improved` and the first satisfy solution,
  and `nlp/global.rs`'s `offer`. `--engine=nlp`'s single local solve reports nothing (the result has it).
- Bounds: the MILP root's LP bound, then (with an incumbent, so best bound first) each popped node's; the global
  search's popped box. The observer keeps the tightest and sends `Bound` events at most every 0.5 s;
  `config::expired` calls `Observer::tick`, which sends a `Heartbeat` after 0.5 s without any event (so a search
  that keeps improving sends none).
- All-solutions mode reports only improvements (the full set is in the result). The global search polishes its
  last incumbent with a local solve, so its result can be slightly better than the last reported solution.
- With no callback installed, nothing is reported or timed, and search statistics match the previous binary on
  every example; with `--incumbents` installed they match too.
- The `log` option stays separate (text statistics); progress is structured.

### 5. Other API improvements

- `Options::relative_gap(f64)` and `absolute_gap(f64)`: stop when proven within the gap (status `NotProvenOptimal`
  with the bound, stop reason `Gap`), and CLI `--gap`. The engines' fixed gaps become the defaults.
- Warm starts: `Options::start(&Solution)` (or `Model::hint(v, value)`), offered to the MIP engine as an incumbent
  (checked for feasibility), to the CP engine as a phase-saving hint, and to the NLP as the start point.
- `SolveResult::elapsed()` and `nodes()`.

### 6. Format changes (problem format 1.3, result format 1.2)

- String literals in expressions, single-quoted so they don't need escaping inside JSON strings: `x['north', 3]`,
  `machine[t] == 'mill'`, `['a', 'b']` in lists. Needed to export families and symbolic variables, and useful to
  write single constraints on named elements. Update the grammar, the schema's version enum, `problem.rs`/`expr.rs`,
  `tools/highs_reference.py` (the shared parser), and `SKILL.md`.
- `start` applies to every variable type and every engine (a MIP start, a CP hint), not only to local nonlinear
  solves. Semantics only; no new fields.
- Result: optional `stop_reason` (`"time_limit"`, `"canceled"`, `"node_limit"`, `"gap"`) and `statistics`
  (`{"time": s, "nodes": n}`), both ignorable by readers of 1.1.
- Solver settings (time, gap, engine) stay on the command line, out of the problem file.
- CLI: `--incumbents FILE` rewrites FILE (written to a temporary file and renamed) with a full result each time a
  better solution is found, so killing a run leaves the best so far without signal handling (catching Ctrl-C needs
  a crate such as `ctrlc`: ask first); `--progress` writes progress events as JSON lines to stderr.

### 7. Threads (later; design for it now)

- No async runtime: solves are CPU-bound, and async callers use `spawn_blocking` with a `CancelToken` (document
  that pattern in `lib.rs`; no tokio dependency). Parallelism uses `std::thread::scope`, atomics, and `Mutex`; ask
  before adding rayon (only fine-grained parallelism, such as strong-branching probes or parallel boxes, needs it).
- First target: a CP portfolio (workers with different settings sharing the incumbent, bounds, and the cancel
  flag, which they get by installing the parent's `Config`), then parallel LNS; that is CP-SAT's lead on scheduling (one CP-SAT worker takes 16 s on ft10).
- `Options::threads(n)`, default 1 until parallel results are trusted; one thread stays deterministic, for the
  regression tests and the statistics comparisons (parallel runs differ from run to run).
- `config.rs`'s settings are per thread: each worker must install a copy of the parent's `Config` with
  `config::scoped`, or it silently runs with the environment's defaults (and no logger).
- Progress callbacks are serialized (a `Mutex` around the call) and fire only for improvements on the shared
  incumbent, so users never see concurrent or out-of-order calls.

### Order of work

1. Done: `CancelToken`, `config::expired` (statistics identical on every example).
2. Done: progress callbacks, stop reasons, `--incumbents`, `--progress` (`tests/progress.rs`).
3. `Set`, `Element`, `VarFamily`, symbolic variables, lookups, editing methods; export of integer families.
4. Format 1.3 string literals; export of every family.
5. The JSON front end emitting a `Model`; `Model::from_json`, `Problem::to_model`.
6. Gaps, warm starts; docs (lib.rs, format docs, README, `SKILL.md`, `docs/cli.md`), CHANGELOG, release 0.2.

Tests: a callback that records every incumbent (objectives strictly improving, each feasible, the last equal to
the result's); cancel from another thread after the first incumbent on a hard instance (`milp_benchmark`'s lot sizing)
and check that the result holds it; `Control::Stop` at a 5% gap; families with string and integer keys round-tripped
through `to_json` and solved by the command; `Problem::to_model` on every example solving to the same results as
the file; editing a loaded model (tighten a bound, remove a constraint) against brute force.

## Commands

```sh
cargo build                      # build (debug)
cargo run -- --time=50 examples/knapsack.json -   # solve, print result (CLI: docs/cli.md)
cargo run --release -- problem.json result.json   # optimized build and run
cargo test                       # run all tests
cargo test <name_substring>      # run tests whose name matches
cargo test <name> -- --exact     # run one test by its full path, e.g. module::tests::foo
cargo test --release --test regression           # solve every example, compare to expected results
cargo test --release --test builder              # the builder API: examples in Rust, brute force, export round trips
cargo test --release --test cancel               # CancelToken: stops promptly, keeps the best solution
cargo test --release --test progress             # progress callbacks, stop reasons, --incumbents
cargo build --no-default-features                # the library without the command (and clap)
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps   # API docs; doctests run with cargo test
cargo test --test regression knapsack            # one example
cargo clippy --all-targets       # lint
cargo fmt                        # format with rustfmt.toml (120 columns); cargo fmt --check must pass
```

## Solver

Phase 1 is an LP/MILP solver (dual simplex + branch and bound, HiGHS-style). Scope, error cases, numerics, status mapping, and the dependency policy (no crates beyond serde/serde_json/clap without asking) are in `docs/solver-design.md`.

## Regression tests and reference solver

`tests/regression.rs` runs the binary on each `examples/<name>.json` and compares against `examples/<name>.result.json`: status exactly; objectives within 1e-6 relative tolerance; the full solution set for `"all"`; only the variable layout for `"one"`, since optima need not be unique. A new example must be added to the `regression_tests!` list, and `every_example_is_tested` enforces that.

HiGHS (`highspy` 1.15.1), OR-Tools (`ortools` 9.15, for CP-SAT), SymPy (1.14, for derivatives), CasADi (3.8.2, whose wheel bundles IPOPT, for local nonlinear solves), and PySCIPOpt (6.3, SCIP, the global reference for nonlinear and mixed-integer nonlinear problems) are installed in the project's `.venv` (gitignored) as development-only references. Optimizam itself must not depend on them. To recreate: `python3 -m venv .venv && .venv/bin/pip install highspy ortools jsonschema sympy casadi pyscipopt`.

- `tools/scip_reference.py` (wrapped by `tools/scip.sh`, same arguments as optimizam) is the global search's reference: SCIP on our format (continuous, integer, and binary variables; `expr` constraints; no `atan`), reusing `highs_reference.py`'s parser with its own SCIP expressions. Verify the global search against it (and brute force), not against IPOPT, which only finds local optima.
- `tools/minlp_benchmark.py` is the global benchmark: MINLPLib instances with a proven optimum, at most `--max-vars` variables (default 12), and only operations we have, downloaded on demand (GAMS scalar format) into `target/minlp_benchmark/` and translated. Verdicts: proven, found (not proven), local, failed, and WRONG (a `complete` or `infeasible` that contradicts the proven optimum); it exits with status 1 on any WRONG. `cargo build --release && python3 tools/minlp_benchmark.py` (`--scip` adds SCIP; `--time`, default 10 s; naming instances runs only those). Run it after changing the global search, relaxations, propagation, or the local solve. As of the OBBT with cutting rounds (all 342 selected instances with at most 30 variables, `--time 10`): 305 proven, 22 found but not proven, 14 local, 1 failed, none wrong, 370 s (integer local search: 297, 26, 18, 1, 465 s; milestone 7: 297, 20, 22, 3; milestone 6: 262 proven and one wrong answer, nvs09).
- `tools/compare_global.py` is the global search's differential test: random small nonconvex problems (some with an integer variable, some infeasible) from fixed seeds, solved by the release binary and by SCIP; disagreements (contradicting proofs, or a proven optimum beaten by the other's point) are saved to `compare_global_failures/` (gitignored), with exit status 1. `cargo build --release && python3 tools/compare_global.py` (`--count`, `--seed`, `--time`).

- `tools/ipopt_reference.py` (wrapped by `tools/ipopt.sh`, same arguments as optimizam) is the nonlinear engine's reference: it reuses `highs_reference.py`'s parser, sets, parameters, and variables, but evaluates expressions into CasADi expressions and solves with IPOPT, writing our result format with the nonlinear statuses. Continuous variables and `expr` constraints only. Produce a nonlinear example's expected result with `tools/ipopt.sh examples/<name>.json examples/<name>.result.json`, and check them with `OPTIMIZAM_BIN=tools/ipopt.sh cargo test --test regression -- curve_fit chemical_equilibrium compound_growth supply_chain_lp` (it reads linear continuous problems too). The other nonlinear examples' expected results come from optimizam itself, checked independently: `hs071` and `portfolio_risk` are `complete` (proven global, at IPOPT's local optima), `disk_infeasible` is `infeasible` (IPOPT can only report `locally_infeasible`), and `batch_sizing` (integers, which IPOPT doesn't take) agrees with enumeration over the integer.
- `tools/nlp_benchmark.py` is the nonlinear benchmark: the Hock–Schittkowski problems in Vanderbei's AMPL versions, downloaded on demand into `target/nlp_benchmark/` and translated by a small interpreter for the AMPL subset they use (some models are skipped, with the reason printed), each checked against the published optimum from its `display obj - VALUE` line. `cargo build --release && python3 tools/nlp_benchmark.py` (`--ipopt` adds IPOPT's answers and flags differences; `--time`, default 10 s; naming problems or prefixes runs only those). Local solvers can find another local optimum of a nonconvex problem; those are counted apart from failures. As of milestone 5 (global search, `--time 60`): 81 of the 103 translated problems at the published optimum (IPOPT: 76), none failed, 55 proven globally optimal; every proven optimum agrees with the published one (hs059's published value is from the original paper's coefficients: its point evaluates to our optimum in this model).
- `tools/nlp_scaling.py` is the local solve's scaling benchmark: generated problems with 50 to 1000 variables and as many nonlinear rows, touching 3 variables each (`chain-N`) or 20 (`wide-N`), timed with `--engine=nlp`; `--ipopt` adds IPOPT's times and flags differing objectives. `cargo build --release && python3 tools/nlp_scaling.py --ipopt`. Run it after changing the interior point method or its linear algebra. As of the sparse LDLᵀ: chain-400 0.8 s (dense: 25 s), chain-1000 4.8 s (dense: timed out at 120 s), wide-400 3.6 s (dense: 74 s), all at IPOPT's objectives.
- `tools/sympy_derivatives.py` writes `tests/data/sympy_derivatives.json`: random format 1.2 expressions over x1..x3 with SymPy's values, gradients, and Hessians at random points, which `nlp::tests::derivatives_match_sympy` checks through the model builder. Regenerate it with `.venv/bin/python tools/sympy_derivatives.py tests/data/sympy_derivatives.json` after changing the generator.

- `tools/highs_reference.py` is an independent Python implementation of the problem format (parser + linearization) that solves with HiGHS and writes our result format. `tools/highs.sh` wraps it with optimizam's CLI. It encodes integers one-hot for gapped domains and for "all" (enumerated with no-good cuts), so those need finite bounds.
- To produce an expected result: `tools/highs.sh examples/<name>.json examples/<name>.result.json` for a format 1.0 example, `tools/cpsat.sh ...` (the same arguments) for one using format 1.1. The small examples' results were brute-forced instead, and agree with HiGHS.
- To check the expected files and the test itself: `OPTIMIZAM_BIN=tools/highs.sh cargo test --test regression -- --skip n_queens_alldiff --skip room_assignment --skip sudoku --skip machine_assignment --skip shift_rota --skip tsp_tour --skip load_balance --skip production_smoothing --skip job_shop --skip crew_schedule --skip hs071 --skip portfolio_risk --skip curve_fit --skip chemical_equilibrium --skip disk_infeasible --skip batch_sizing` (HiGHS reads only format 1.0 and linear 1.2), and `OPTIMIZAM_BIN=tools/cpsat.sh cargo test --test regression -- n_queens_alldiff room_assignment sudoku machine_assignment shift_rota tsp_tour load_balance production_smoothing job_shop crew_schedule four_queens knapsack domain_gaps` (the all-integer examples); both must pass.
- `tools/cpsat_reference.py` (wrapped by `tools/cpsat.sh`) is the CP engine's reference: it reuses `highs_reference.py`'s parser and evaluator (extended for 1.1 lists and set domains; `highspy` is imported lazily there, since OR-Tools bundles its own HiGHS and the two clash) and builds a native CP-SAT model (`AddAllDifferent`, `AddAllowedAssignments`/`AddForbiddenAssignments`, `AddElement`, `AddMin/Max/AbsEquality`, interval variables with `AddNoOverlap`/`AddCumulative`, `OnlyEnforceIf`, set domains), so it shares nothing with `encode.rs`. Integer variables with finite bounds only.
- The large examples come from `tools/generate_examples.py` (fixed seeds). If you regenerate them, regenerate their result files too.
- `tools/milp_benchmark.py` is the MILP performance benchmark: seven generated instances (facility location, generalized assignment, two lot sizing, multidimensional knapsack, set covering, time-window scheduling) from fixed seeds, each stressing a different part of the solver. `cargo build --release && python3 tools/milp_benchmark.py` prints time, status, objective and bound per instance (about 50s); `--highs` adds HiGHS's times and flags disagreeing optima, and naming instances runs only those. Run it before and after performance work.
- `tools/mps_to_json.py` converts MPS (fixed or free format, optionally gzipped) to our problem format: `python3 tools/mps_to_json.py in.mps.gz out.json [--names names.json] [--pretty]`. Columns become integer-indexed variables grouped by type and finite bounds (the format has no string literals or infinite parameters); ranged rows become two constraints. One convention differs between readers: a negative UP bound with no lower bound sets the lower bound to -infinity here (with a warning), while HiGHS 1.15 keeps 0.
- `tools/netlib_lp_benchmark.py` is the LP benchmark: it downloads the 91 main Netlib LPs that the mirror has (gzipped MPS from the COIN-OR Data-Netlib mirror) into `target/netlib_lp/`, converts them, solves them, and checks each against HiGHS reading the original MPS and against Netlib's published optimum. `cargo build --release && python3 tools/netlib_lp_benchmark.py` (add `--check-converter` to also solve the converted JSON with HiGHS, which separates converter bugs from solver bugs). The Netlib files are downloaded on demand and never committed.
- `tools/cp_benchmark.py` is the CP benchmark: published instances downloaded on demand into `target/cp_benchmark/` (job shop from JSPLIB, RCPSP j30 from PSPLIB via the MiniZinc benchmarks, DIMACS graph coloring) plus generated Golomb rulers, Sudoku, N-queens, and a sports tournament schedule, each checked against its known optimum. `cargo build --release && python3 tools/cp_benchmark.py` (`--time`, default 60 s per instance; `--cpsat` adds OR-Tools CP-SAT's times and answers; naming instances or prefixes runs only those).
- `tools/compare_random.py` is a differential test: it solves random MILPs from fixed seeds (facility-location-shaped, unstructured, and medium instances of classic classes that exercise presolve and the heuristics) with both the release binary and HiGHS, and reports any status or objective mismatch. Disagreeing problems are saved to `compare_random_failures/` (gitignored), and it exits with status 1. Run it after changing the simplex, branching, cuts, presolve, or heuristics: `cargo build --release && python3 tools/compare_random.py` (about a minute; `--count`, `--seed`, and `--kind` adjust it).

## Problem file format

Problems are JSON files; solving one produces a JSON result file (`schema/result.schema.json`, `docs/result-format.md`). `schema/problem.schema.json` (JSON Schema 2020-12) defines the structure. `docs/problem-format.md` defines what the fields mean and the expression grammar. Constraint and objective expressions are strings (e.g. `sum(i in I) w[i]*x[i] <= cap`), so the JSON Schema can't validate them; they need a parser in the Rust code. Keep the schema, the doc, and `examples/` in sync when changing the format.

Format 1.1 adds set-valued variables (`domain` naming a set; symbolic when the set has strings), an `intervals` section, `enforce_if` on `expr` constraints, and the constraint kinds `all_different`, `element`, `table`, `min`, `max`, `abs`, `no_overlap`, and `cumulative`, which take list arguments (`"[x[i] | i in I]"`). Implemented so far: lists, set-valued variables (symbolic ones in lists, `table`, `element` indexes, and `v == e` / `v != e` comparisons), `all_different`, `table`, `element`, `enforce_if`, `min`, `max`, `abs`, intervals, `no_overlap`, and `cumulative`: all of format 1.1. `model.rs` evaluates each instance to an `encode::Global`, and `encode.rs` encodes it as linear rows on extra columns (shared value binaries `[x = v]`, selectors, big-M) so both engines solve it; auto picks CP for problems using 1.1 kinds. The exceptions are `no_overlap` and `cumulative`: the model keeps them in `LinearModel::scheduling`, the CP engine propagates them natively (`cp/scheduling.rs`), and `solve.rs` encodes them only when the MILP engine is chosen. Other kinds could get native propagators the same way if their encodings prove slow. OR-Tools CP-SAT is the CP engine's reference, as HiGHS is for MILP; verify the CP engine with brute-force tests and OR-Tools, not by comparing it against HiGHS.

Format 1.2 adds the functions `ln`, `exp`, `sqrt`, `sin`, `cos`, `tan`, `atan`, `tanh` (`expr::Func`), powers with any exponent (`Expr::PowE`; integer constant exponents stay `Expr::Pow`), and `start` on variables; result format 1.1 adds the statuses `locally_optimal` and `locally_infeasible` (written with result version "1.1"). All of it is implemented: functions and powers of constants are evaluated when the model is built; nonlinear terms in the variables, which `model.rs` keeps in an expression graph (`nlp/graph.rs`), go to the nonlinear engine, which needs every variable to be continuous (mixed-integer nonlinear problems are rejected). IPOPT (through CasADi) is the nonlinear engine's reference; nonlinear examples' expected results come from `tools/ipopt.sh`. To check the examples against the schema:

```sh
python3 -c "import json,glob,jsonschema; s=json.load(open('schema/problem.schema.json')); [jsonschema.validate(json.load(open(f)), s) for f in glob.glob('examples/*.json') if not f.endswith('.result.json')]; r=json.load(open('schema/result.schema.json')); [jsonschema.validate(json.load(open(f)), r) for f in glob.glob('examples/*.result.json')]"
```

`Model::to_json` must write files that pass the same schema. `tests/builder.rs` writes every model it exports to `$OPTIMIZAM_EXPORT_DIR` when that is set; check them with:

```sh
mkdir -p /tmp/exports && OPTIMIZAM_EXPORT_DIR=/tmp/exports cargo test --release --test builder
python3 -c "import json,glob,jsonschema; s=json.load(open('schema/problem.schema.json')); [jsonschema.validate(json.load(open(f)), s) for f in glob.glob('/tmp/exports/*.json')]"
```
