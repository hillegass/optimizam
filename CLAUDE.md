# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

LP/MILP, CP, and continuous NLP are implemented and pass the regression tests. Modules in `src/`:

- `problem.rs` reads the JSON, by hand from a `serde_json::Value` so that a structural error says where (`variables[1].type (carrier): expected ...`, a JSON path counting from 0 with the item's name; misspelled fields are suggested, numbers in quotes pointed out); `expr.rs` parses expression strings; `api/convert.rs` evaluates sets, parameters, sums, and `forall` into a builder `Model` (one family per variable, one labeled constraint per instance), which `api/lower.rs` turns into the solver's `LinearModel` (`model.rs`) as for any builder model: one front end. `model::build` is convert, then lower. `Assembly::finish` compacts the nonlinear graph to the nodes the rows and objective reach, in a canonical order (`Graph::compact`), so equal expressions give equal graphs however they were built.
- `lp.rs` converts it to a HiGHS-style `Lp` (minimization, row/column bounds, CSC + CSR).
- `simplex/` is the bounded dual simplex (`mod.rs`, after HiGHS `HEkkDual`). `Simplex` is a solver object for one LP that keeps its basis, factorization, and work space between solves: a `Basis` carries the mark of the solver state it was read from, and a solve from the state the solver is still in (a child node after its parent, a dive step) allocates and factorizes nothing; strong-branching probes run on clones. `simplex::solve` is the one-shot form (it also leaves free rows out). `factor.rs` is the LU factorization after `HFactor`: a triangular pass over singletons, then a Markowitz kernel with count-linked lists; Forrest–Tomlin updates; L and U each by row and by column, so every solve is a scatter loop, and a sparse right-hand side visits only the pivots it reaches (Gilbert–Peierls, gated by a running density estimate per kind of solve). `vector.rs` has the work vectors that list their nonzeros (`HVector`), which PRICE and the updates read by list or by scan as density suggests. `primal.rs` has primal simplex iterations for one job: removing the dual infeasibilities left when cost perturbation is taken off at a primal feasible basis. A rebuild that only wants fresh values keeps an updated factorization if the recomputed basic values and duals satisfy their equations componentwise to 1e-9 (`accurate`); infeasibility is only declared from a new factorization.
- `milp.rs` is branch and bound (DFS until an incumbent, then best-bound, where a worker plunges: it goes on with the preferred child of the node it has just finished while that child's bound is within a quarter of the gap; reliability branching, i.e. pseudocosts seeded by strong branching; reduced-cost fixing of integer columns at every node; `want_all` splits integral nodes until every variable is fixed). When the root, after its heuristics and fixing, has fixed a fifth of the free integer columns, `branch_and_bound` starts the search over with new cut rounds from the tighter bounds, keeping the incumbent, the pseudocosts, and the node count (`Step::Restart`). `Search` holds what the workers share (the open nodes, the incumbent, the pseudocosts, each behind a lock), and a `Worker` is one thread's part (its own `Simplex`, heuristics, and trail). With `Options::threads` above 1, the calling thread's worker processes the root alone, then one worker per thread takes nodes from the shared tree, as one worker would; a worker alone in the search (as at the root) solves its strong-branching LPs in batches on the idle threads (`Prober`). Sub-MILPs and `want_all` searches use one worker.
- `cuts/` adds root cutting planes: `cmir.rs` (on row aggregations with variable-bound substitution, c-MIR and lifted cover inequalities, pure and mixed-binary, the more efficacious of the two per aggregation), `gmi.rs` (Gomory mixed-integer cuts from the tableau), and `mod.rs` (rounds until five in a row gain less than 0.1% of the gain so far, at most 50; numerical cleanup; selection; removing cuts that are slack between rounds; at the end, undoing the rounds after the last one that made progress and removing every cut whose slack is basic).
- `presolve.rs` reduces the problem before the search (bound and coefficient tightening, row and column removal, dual fixing with `"one"`; in an LP, no implied bounds, and columns substituted out through equality rows: doubleton equations and implied-free columns); its postsolve re-inserts fixed columns and works the substitutions back, last first.
- `propagation.rs` is the domain store shared by the engines: bound propagation over linear rows with a trail of bound changes and their reasons (for conflict analysis). The MILP search propagates at every node and in strong-branching probes before solving the LP.
- `symmetry.rs` finds value symmetry (columns linked by `all_different` with interchangeable values, as in graph coloring) before encoding, and `encode.rs` adds value precedence rows for it (not with `"all"`).
- `heuristics.rs` finds solutions early: rounding, dives, and RENS/RINS sub-MILPs (which call presolve and branch and bound recursively, with `Settings::sub_milp`).
- `cp/` is the CP engine (lazy clause generation, after OR-Tools CP-SAT) for all-integer problems with finite bounds: `domains.rs` (integer trail of bound literals with reasons), `linear.rs` (exact integer bound propagation with lazy explanations), `clauses.rs` (two-watched-literal clauses), `solver.rs` (CDCL: 1-UIP, VSIDS, Luby restarts, clause deletion; earliest-start task branching until the first solution when there are scheduling constraints), `linear.rs` keeps each row's minimum activity incrementally along the trail, `scheduling.rs` (native `no_overlap` and `cumulative` propagators: timetabling, pairwise and detectable precedences, edge finding, with stored explanations), `alldiff.rs` (a native Hall-interval `all_different` propagator, for instances with a large multi-column item, which aren't encoded for CP), `lns.rs` (large neighborhood search: relaxed tasks re-solved with the others kept in incumbent order, or random columns fixed), `optimize.rs` (the optimizing search: the CDCL search with `objective <= best - 1`, alternating with LNS phases on the best solution), `portfolio.rs` (with `Options::threads` above 1: one `Optimizer` per thread, each with its own restart schedule, activity decay, starting order and values, and share of LNS; they share the best solution, the clauses they learn with LBD at most 3 or at most 3 literals, bounds learned at level 0, and the end, since the first to exhaust its search has proven the best solution optimal; the calling thread's search reports progress; `satisfy` for one solution races plain searches the same way; enumeration stays on one thread), and `mod.rs` (integer scaling and the satisfy/all/optimize phases).
- `nlp/` is the nonlinear engine, for continuous problems: `graph.rs` is the expression graph that `model.rs` builds for the nonlinear part of rows and the objective (`LinearModel::nonlinear`; hash-consed nodes, constant folding), with an `Evaluator` for values, gradients (reverse mode), and the Hessian of the Lagrangian (forward-over-reverse, with the pattern's columns colored to share directions); `ldl.rs` is a dense Bunch–Kaufman LDLᵀ that reports the inertia; `sparse_ldl.rs` is a sparse LDLᵀ with 1x1 pivots in a fixed minimum-degree order (QDLDL-style: elimination tree, up-looking numeric factorization, inertia from D), for KKT systems of size 150 or more; `interval.rs` (outward-rounded interval arithmetic for every op), `fbbt.rs` (bound propagation, forward and backward through inverses, to a fixed point), `convexity.rs` (each node's curvature over a box: composition rules with interval signs of f' and f'', products of powers, entropy terms), and `relax.rs` (a linear relaxation of fixed shape: McCormick, tangents and secants by curvature, tangent planes of convex quadratic sums, outer-approximation cuts on convex composite nodes, hint blocks for warm starts; LPs scaled, warm-started from the parent's basis, returning reduced costs; an infeasible verdict is confirmed with an elastic LP and ignored on badly scaled LPs) bound the problem over a box; `intsearch.rs` is integer local search (±1 moves on one integer, then on pairs, repairing violation before improving the objective; continuous variables solved for from equality rows, or by local solves), run at the root and from new incumbents; `ecp.rs` is outer approximation on the MILP engine for convex mixed-integer problems (cutting planes, then MILP rounds with Duran–Grossmann cuts at local solutions), tried before branching; `global.rs` is spatial branch and bound over boxes (best bound first, propagation with an incumbent cutoff, OBBT with cutting-plane rounds at the root and, with the cutoff, again at the root and at nodes down to depth 6 while it narrows boxes, branching on fractional integers or the most violated nonlinear term, local solves as heuristics, stall detection; a `Search` shared by one `Worker` per thread, as in `milp.rs`: with `Options::threads` above 1 the workers take boxes side by side and leave each box's local solve and integer search for any idle worker, and the root's bound tightening gives each thread a share of the columns, with the cuts pooled), which `--engine=auto` (and `global`) uses for every nonlinear problem, integer variables included, while `--engine=local` is the local solve alone; `ipm.rs` is the interior point method after IPOPT (Wächter–Biegler 2006: filter line search, second-order corrections, inertia correction, monotone barrier updates, bound relaxation, and a restoration phase that runs the same method on an l1 feasibility problem), over a small `Problem` trait with sparse derivatives (fixed Jacobian and Hessian patterns; quasi-definite sparse factorization with a small dc, iterative refinement against the true system, and the dense factorization as the fallback); `mod.rs` builds the standard form (fixed columns removed, slacks for inequality rows, gradient-based scaling) and maps the result (local optimum, local infeasibility, limits).
- `solve.rs` runs presolve, picks the engine (`--engine=auto|milp|cp|global|local`; auto uses the global search for problems with nonlinear terms, CP for `satisfy` and `"all"` problems it can take; `Engine::Local` is `solve_local`, the interior point solve alone), and maps the outcome to a result status; `result.rs` writes the result file.
- The crate is a library (`lib.rs`, whose docs are the docs.rs front page) and the `optimizam` command (`main.rs`, behind the default `cli` feature, the only user of clap and ctrlc; Ctrl-C, SIGTERM, and SIGHUP cancel the solve through a `CancelToken`, so the result is written as at a time limit, and a second interrupt exits with 130). The public API is in `api/`: `builder.rs` (`Model`, `var()`/`VarSpec`, the CP kinds, intervals, families, lookups, editing), `family.rs` (`Element`, `Set`, `Key`, `VarFamily`), `expression.rs` (`Variable`, `Expression` as an affine part plus nonlinear `Tree` terms, the operators, `Constraint`, `Literal`, `constraint!`), `lower.rs` (a `Model` to the same `model::Assembly` that `model::build` makes from a problem file; `Assembly::finish` does symmetry detection and the encodings for both), `export.rs` (`Model::to_json`, flat format 1.2, auxiliary `_`-named variables for what the format can't say directly), `solution.rs` (`SolveResult`, `Solution`), `explain.rs` (why a model is infeasible or unbounded, worked out on the builder `Model` so that it holds for every engine: `obvious` reads variables with no value left and constraints that can't hold within their bounds off the model after any infeasible solve; `conflict`, for `Options::explain`, `--explain`, and `Model::conflict`, is a deletion filter over the constraints, names alike but for a subscript together first, with solves that share the deadline; `unbounded_variables`), `progress.rs` (`Progress`, `Event`, `Control`, `StopReason`, and the observer that turns engine reports into events), `options.rs`, and `error.rs`; `mod.rs` has `Problem` (problem files) and the shared solve path. Only `api/` and `Status`/`Engine` are public; the engines stay private so they can change. Before 1.0, keep public structs' fields private, options as chained methods, and enums `#[non_exhaustive]`, so features can be added without breaking users.
- `config.rs` holds the settings of the solve running on the thread (presolve, value symmetry, LNS, the statistics logger, the cancel flag; also the stop reason and the progress observer), installed by the public entry points with `config::scoped`; the engines read them with `config::debug()` and friends and log with `config::debug_log!` instead of `eprintln!`. `config::threads()` is how many threads a search may use, and `config::team(threads, others, first)` is the one way to start worker threads: it gives each of them this thread's settings and halt flags (a new thread has neither). A parallel search runs its team under `config::halting(flag)`, which makes `expired` true for all of them once one sets the flag, without recording a stop reason: only the calling thread records why a search stopped, and only it has the progress observer. Halt flags nest, so a search inside a worker (a sub-MILP, a batch of probes) still stops with the outer one. With nothing installed (unit tests calling engines directly), the environment variables below decide.

Set `OPTIMIZAM_DEBUG=1` (the command, or unit tests; library users pass `Options::log`) to print presolve, cut, heuristic, and branch-and-bound statistics (including when each incumbent was found, and by what), and the nonlinear engine's iterations, to stderr. Set `OPTIMIZAM_NO_PRESOLVE=1` to skip presolve, for comparisons or to isolate a presolve bug. Set `OPTIMIZAM_NO_LNS=1` to turn off the CP engine's large neighborhood search, and `OPTIMIZAM_NO_SYMMETRY=1` to turn off value precedence.

Known limitations: no probing, conflict analysis, or cuts below the root, so some MILPs with weak root bounds still need large trees (lot sizing with many periods is the slowest class tried, and the cut loop of 2026-11-10 made lotsize_big slower: cuts removed at the end of the root aren't kept in a pool for the nodes). Root bounds on fixed-charge network flows are still well below HiGHS's (beasleyC3 504 against 736). Before branch and bound, `Lp::tighten_integer_rows` rounds all-integer rows to multiples of their coefficients' gcd, and presolve's congruence check handles equality rows with decimal coefficients and small-range columns; together they prove cases like `2x - 4y = 7` or `2x - 4y + 8.25z = 9.5` (z binary) infeasible. But an infeasible MILP with unbounded integer variables whose infeasibility needs several rows combined can still run forever without `--time`. The nonlinear engine factors KKT systems of size 150 and up sparsely (`tools/nlp_scaling.py`: 400 variables and 400 sparse nonlinear rows in 0.8 s, from 25 s dense; IPOPT 0.2 s); on large problems the root relaxation's LP now costs more than the local solve (chain-1000: 6.7 s vs 0.5 s), and the sparse factorization's fixed order can lose accuracy on badly scaled systems (it then falls back to the dense one, which is slow at that size). The global search struggles with nonlinear terms of unbounded variables (it branches, with a smaller budget, but may never close the gap), and objectives that measure a row's slack against a large constant (gear4) can't be proven: the rows' feasibility tolerance (1e-6 relative) exceeds the gap.

Tried and dropped (measured, so don't retry without a new idea):

- Global search: pseudocost branching on continuous variables (milestone 7: no gain) and on integers (nvs19 took 945 nodes instead of 2865, but nvs23/24 got worse). Difference-of-convex (eigenvalue) estimators of indefinite quadratic sums: valid, but over wide boxes they barely move the bound (nvs23's root bound -3734 vs -3740), and MINLPLib took 490 s instead of 472 s; OBBT with cutting rounds is what proves nvs23/24. More cutting-plane rounds per node (6 or 10 instead of 3): fewer nodes, more time (nvs24 not proven in 30 s with 10).
- OBBT tuning: fewer cutting rounds at nodes (0 to 2 instead of 5) leaves far bigger trees (nvs24 unproven after 4768 nodes with 0); node OBBT on every problem, without the stop when it stops narrowing boxes, costs more than it saves on circle packings (kall_*) and hs119 (4.5 s to 10.8 s); capping the cuts at 2x the relaxation's rows everywhere slowed nvs24, 4x slowed the kall problems (hence 3x at the root, 1x at nodes); loosening OBBT's results by only 1e-6 let LP error round an integer bound from 0 to 1 (a wrong answer on nvs16).
- Point boxes: offering only the local solve's exact point loses proofs whose last gap step is a tolerance-sized one (ex14_1_1); offering every point at the tolerance proves wrong optima (gear4); hence the 1e-4 limit on how much the box's own points may beat the local solve's.
- Integer search: with local solves at the full budget before outer approximation, m3 went from 0.14 s to 7.5 s (solves on infeasible fixings ran 3000 iterations): hence 60 iterations per solve, an iteration budget per search, and only a short search before outer approximation.
- Performance work on the LP engine (measured with `measure.py`, the shifted geometric mean over all five benchmarks; one full run takes about 12 minutes once the instances are downloaded, and single-instance timings vary by 5 to 10% between runs, so compare instruction counts under `valgrind --tool=callgrind` and only decide on back-to-back full runs). Kept: Markowitz pivots searched from count buckets (Factor::build sorted every active column per kernel pivot: 20% of d2q06c), free rows left out of simplex solves (relaxations are mostly free placeholder rows: hs110 500 of 1081), the kernel in flat arenas (allocation was 25% of hs081's instructions), relaxation LPs scaled in place, and the free list cleared in dual phase 1 (hs111lnp's OBBT LPs cycled for 112,000 iterations each); together 0.85 of the baseline score (two back-to-back pairs: 0.870 before the free-list fix, 0.852 with it), with no instance's outcome worse: hs111lnp answers in 2 s instead of timing out, and hs101/102/103/110 stall before their 10 s limits. Profiles: netlib and MILP time is ftran/btran (the U solve, then L, then the product-form etas) plus the factorization; the global search's time is LP overhead on tiny LPs (two factorizations per solve, Lp construction, allocation), with 1 to 4 LP solves per box plus up to 12 per nonlinear variable in node OBBT; cfl_large spends 82% in strong-branching probes of 150 to 450 iterations. Dropped: row-wise L and column-wise U copies for skip-sparse solves (no instruction-count gain, vectors are dense on netlib and cfl_large; changes FP summation order so search paths differ), refactorizing every 25 or 50 updates instead of 100 (slower: pilot87 9 s to 18 s), skipping the refactorization that confirms optimality after at most 20 updates (no gain, and nvs24's tree tripled), and computing LP scaling's nearest power of 2 from the exponent bits (rounds the sqrt(2) boundary differently from log2, which changed OBBT's root box on nvs24 and its tree from 435 to 6700 nodes). Beware: MINLP search paths are chaotic in the LP's rounding, so any change to LP numerics must be checked on nvs24 and the full MINLPLib run; nlp:hs085 is "wrong" (complete at -1.9051554 vs the published -1.90513375, outside 1e-6) in the baseline too.
- LP engine, second round (2026-11-09; `RESULTS.md` has the per-step table; shifted geometric means against the 0.1 kernel: Netlib 0.140 to 0.082 with HiGHS at 0.079, MILP 3.76 to 2.64, Hock–Schittkowski 0.356 to 0.300, MINLPLib 0.409 to 0.371 with 308 proven instead of 304). Kept, in order: Forrest–Tomlin updates with scatter solves and the HFactor-style build (product-form etas and row-wise dot-product solves cost 1.5 to 4 times HiGHS's time per iteration; the old build re-sorted a count bucket per pivot); sparse work vectors with hyper-sparse solves (80bau3b 2x; neutral on dense LPs once dense vectors are scanned, not read through their lists); the persistent `Simplex` with the verified rebuild (mknap, whose tree is identical in every version, 8.3 s to 6.6 s of node LP time); primal cleanup (pilot87 no longer ends with dual infeasibilities); LP presolve by substitution (maros-r7 2.3 s to 0.4 s) and without implied bounds (dfl001 15% fewer iterations). Dropped, measured: a larger or smaller update limit than 100 (50: 16.9 s, 100: 13.9 s, 200: 14.3 s, 400: 15.5 s on the nine slowest Netlib LPs; an adaptive rule would have had 2 to 5% to gain); the primal simplex for bound tightening's LPs, which start at the last optimum with a new objective (more pivots than the dual simplex with its bound-flipping ratio test: ex8_4_1 261,000 iterations against 184,000); one scaled LP and one hot solver per relaxation for bound tightening (no gain: those LPs take 70 to 190 iterations each, so setup isn't what costs); substituting slack columns whose bounds aren't implied, leaving range rows (dfl001 50% more iterations), and more fill in substitutions; substitutions on the continuous columns of MILPs (lot sizing needed two to three times the nodes: the cuts read those rows). Beware: an infeasibility verdict from an updated factorization is wrong often enough to matter (the ratio test's pivot tolerance grows with the update count; jit1's outer approximation lost its proof), and lotsize_big's time is tree-size noise (9,000 to 26,000 nodes across kernels that differ only in rounding), so judge kernels on mknap's node LP seconds and on Netlib. The MILP path still has no LP scaling.
- MILP cuts and search (2026-11-10; `RESULTS.md`, "MILP proof strength": MIPLIB at 30 s from 22 proven to 28 with one thread and from 34 to 38 with ten, none wrong; the generated benchmark's mean went from 2.64 s to 2.70 s with one thread and from 0.78 s to 0.92 s with ten, with gap and mknap faster and lot sizing slower). Kept: a cut loop that goes on through flat rounds (the old one stopped after one, at 20 rounds, and dropped cuts with more than n/5 + 10 nonzeros: beasleyC3's root bound was 444, now 504; neos-911970's 23.3, now 52.1 with the optimum at 54.8), lifted cover cuts beside c-MIR, reduced-cost fixing (proves cap6000 and swath1), root restarts (nw04), plunging (eil33-2), and removing every basic cut at the end of the root (aflow30a, nu25-pr12). Dropped, measured: looser round limits without undoing the rounds after the last progress (useless cuts stay in the LP: markshare_4_0 601,000 nodes in 10 s to 412,000); a 1% progress rule instead of 0.1% (stops too early: p200x1188c's root bound 9,859 to 8,571); keeping tight basic cuts at the end (aflow30a's LP 2,482 rows instead of 617, and 13 nodes in 10 s instead of 1,900), although lot sizing wants them (lotsize_big 26,000 nodes with them, 39,000 without, 13,000 before any of this): the fix is a cut pool separated at nodes, not a rule about which to keep; 100 rounds instead of 50 (no better); restarts at 5% of the columns instead of 20% (no more proofs, and each restart repeats the root heuristics); ordering the dual ratio test's bound flips with a bit set instead of a sort (no gain: the sort that showed in the profile was `cuts::finish` sorting dense Gomory cuts, since fixed by merging terms through a dense scratch vector; identical generic functions are folded, so a profile's symbol name can point at the wrong caller). Beware: the gap at the time limit of an unproven model is noise (tr12-30 ended 7% and 13% above the optimum in two runs of the same search path under different load), so judge by proofs and the mean time; the outer approximation's stall test (`ecp.rs`) compares with `bound + 1e-6 * (1 + |bound|)`, which is NaN while the bound is -inf, so its first MILP round always counts as stalled and it gives up after four more (clay0203m: 1.4 s to 8 s when the MILP began returning other optima; not fixed, as it changes every convex MINLP's path and needs a MINLPLib run of its own). Not done from this package: conflict analysis, probing, cuts below the root (a cut pool), tableau-row aggregations through the c-MIR and cover generator (what HiGHS adds on fixed-charge flows; SCIP's c-MIR alone reaches 683 on beasleyC3), presolve at restarts.
- Profile-guided optimization (`cargo-pgo`, trained on the examples and a subset of every benchmark), on top of the release profile in `Cargo.toml` (fat LTO, one codegen unit, `panic = "abort"`): a negligible improvement of 1 to 3% (shifted geometric means: Netlib 0.971, CP 0.976, NLP 0.979, MILP 0.985, MINLPLib 0.995, where two runs of the same binary differed by up to 1%; `RESULTS.md`), not worth a second build pipeline and profiles that go stale. `-C target-cpu=native` wasn't tried: it can change floating-point rounding through fused multiply-adds, and with it the MINLP search paths.
- CP engine (measured on the CP benchmark and on 27 more JSPLIB and PSPLIB j30 instances with known optima, single-instance timings being chaotic in the search path): learned-clause minimization (local, with the implying literal earlier on the trail): worse on job shop (shifted geometric mean 3.88 s vs 3.55 s); the not-last/not-first rule for `no_overlap` (O(n^3 log n)): much worse (4.59 s vs 3.35 s, two fewer proven), its cost outweighs the pruning; restart unit 50 or 300 and VSIDS decay 0.9 or 0.99 instead of 100 and 0.95: no better; running edge finding only once the cheaper `no_overlap` rules stop changing bounds: no faster, and ft10 lost its proof in 60 s; a native Hall-interval `all_different` on top of the value encoding: slower on queens and sudoku, nothing gained. The CP engine's bound (results stopped early, progress events) is the objective row's minimum over the level-0 bounds (`Solver::root_min`), min'd with the incumbent: free and valid, but weak on scheduling (ft10: 655 at 1 s and 10 s against the optimum 930, the busiest machine's load; la16 717 vs 945). An LP bound over the presolved rows wouldn't help there: the MILP engine's root LP on ft10, with the scheduling constraints encoded, gives 681 (760 after 5 s), and the CP engine's rows leave those constraints out; on tournament_12 the root LP didn't finish in 5 s. A stronger bound needs a bound search (CP-SAT's objective lower-bound workers: prove `objective <= L` infeasible under a conflict limit, raising L), which costs main-search time and would need the CP benchmark to decide. CP-SAT's remaining lead on scheduling (jobshop_ft10 2 s vs 30 s, tournament_12 1120 vs 1180) is its parallel portfolio: one CP-SAT worker takes 16 s on ft10 and ends tournament_12 at 1240.
- Sparse LDLᵀ for small KKT systems (size 30 and up): 1x1 pivots in a fixed order lose accuracy on barrier-scaled systems (jit1, size 71: backward error 1 by the third iteration, 4167 dense fallbacks, and the proof lost), and the dense factorization costs under a millisecond there: hence size 150. Accepting sparse solves by a normwise backward error (relative to |b| + max|K| |x|) let inaccurate directions through (jit1's differed from the dense ones by 1e-3 from the first iteration): hence the componentwise (Oettli–Prager) error. A SIMD or blocked dense kernel wasn't tried: the dense factorization already ran at about 1.9 GFLOP/s and was 99% of the time; sparsity, not the kernel, was the lever.

- Rust edition: **2024**; the minimum Rust is 1.88 (`rust-version`, checked in CI), for `let` chains
- A library and a binary target. Dependencies: `serde_json`, and `clap` and `ctrlc` (optional, for the binary); `serde` itself went when `problem.rs` stopped deriving. Dual-licensed MIT or Apache-2.0.

## Users

`README.md` is for users: the library (`cargo add optimizam`), installing the binary (`cargo install optimizam` or `--path .`), and the Claude Code skill in `skills/optimizam/` (`SKILL.md`, plus `references/` with symlinks to `docs/problem-format.md`, `docs/result-format.md`, `docs/cli.md`, and `examples/`). Keep `SKILL.md`'s summary of the format and statuses, and `--help` in `src/main.rs`, in sync with the docs when the format or CLI changes. The format docs are also the docs.rs pages `optimizam::format::problem` and `format::result` (through `include_str!`), so their links are absolute GitHub URLs. Record user-visible changes in `CHANGELOG.md`. Publishing: `cargo package --list` must not include `tools/`, `skills/`, or benchmark data, and `cargo publish --dry-run` must pass.

## Plan: 0.2 API (in progress: steps 1 to 5 done)

Goals: indexed variable families, loading a problem file into a `Model` to edit it, progress callbacks, and
cancellation that returns the best solution found so far. Everything here is additive except where marked
**breaking** (allowed before 1.0; record it in `CHANGELOG.md`). Search behavior must not change: after each step,
compare `OPTIMIZAM_DEBUG=1` statistics on all examples against the previous release binary (only timings may differ),
as was done for the 0.1 library split.

### 1. Index sets and variable families

Done (`api/family.rs`, `tests/families.rs`), as sketched below, with these details: `Family` keeps a `Shape` (scalar,
`add_vars`'s 0..n, or `Sets`) and one `VarSpec` or one per element (`check_family`: same kind, domain, and
symbolic set); `lower::col_bounds` is the one place a column's bounds are decided (spec, then `set_bounds`:
replaced, intersected with 0..=1 for binaries, clipping a domain's values), used by lowering and export alike;
`Key` positions are computed without allocating (each `Set` has integer and string lookup tables); symbolic
variables are integers 1..k, as for problem files, rejected in arithmetic and tables, allowed in `is`/`is_not` and
in `all_different` over one set; `Literal` lost `Copy` (it can hold a string element). Export writes integer-keyed
families over their sets, per-element bounds and starts as parameters (`_x_upper`), `set_bounds` changes as
constraints, and runs `lower` first so that it rejects what solving rejects; string keys and comparisons with
strings are an `Unsupported` error until format 1.3. Not done: `Solution::family`, symbolic variables in `table`
and `element` (needed for `to_model`, step 5).

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
  is `config::expired(deadline)` (11 sites; the deadlines themselves are unchanged). Sub-MILPs, LNS, and the nonlinear
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
- Plumbing: `config::Report` (engine columns) is an explicit `report` field in `milp::Settings` and `cp::Settings` and
  a parameter of `nlp::global::solve`, set only by `solve.rs` for the top-level search: `Restored` maps presolved
  columns back with `post.restore` and converts the bound (`sign * (b + post.offset) + obj_offset`); `Direct` passes
  the global search's model-column points through. Both feed the `config::Observer` that `api::solve_with`
  installs after building the model. Sub-MILPs (RENS/RINS), outer approximation's MILPs, the zero-objective
  feasibility search, and LNS rounds report nothing: their columns are their own, and the parent reports a solution
  when it adopts it. Report sites: `milp.rs`'s `improve`, `cp/mod.rs`'s `improved` and the first satisfy solution,
  and `nlp/global.rs`'s `offer`. `--engine=local`'s single solve reports nothing (the result has it).
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

- CP bound search (not started; done so far: the free level-0 bound, see the CP engine notes above): between main
  search slices, as LNS is now, try to prove `objective <= L` infeasible with a fresh solver and a conflict limit,
  raising the bound to L + 1 on success (bisecting between the bound and the incumbent, so a probe that finds a
  solution improves the incumbent instead). It would make `gap()` and `relative_gap` useful on scheduling problems,
  where the level-0 bound is weak (ft10: 655 vs 930). Keep it only if `tools/cp_benchmark.py` shows no loss on
  ft10 and tournament_12 (time to the best solution, and the final objective at 60 s).
- `Options::relative_gap(f64)` and `absolute_gap(f64)`: stop when proven within the gap (status `NotProvenOptimal`
  with the bound, stop reason `Gap`), and CLI `--gap`. The engines' fixed gaps become the defaults.
- Warm starts: `Options::start(&Solution)` (or `Model::hint(v, value)`), offered to the MILP engine as an incumbent
  (checked for feasibility), to the CP engine as a phase-saving hint, and to the NLP as the start point.
- `SolveResult::elapsed()` and `nodes()`.

### 6. Format changes (problem format 1.3, result format 1.2)

- String literals in expressions, single-quoted so they don't need escaping inside JSON strings: `x['north', 3]`,
  `machine[t] == 'mill'`, `['a', 'b']` in lists. Needed to export families and symbolic variables, and useful to
  write single constraints on named elements. Update the grammar, the schema's version enum, `problem.rs`/`expr.rs`,
  `tools/highs_reference.py` (the shared parser), and `SKILL.md`.
- `start` applies to every variable type and every engine (a MILP start, a CP hint), not only to local nonlinear
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
- Done (2026-11-09): the CP portfolio (`cp/portfolio.rs`, `Options::threads`, `--threads`, `OPTIMIZAM_THREADS` for the benchmark scripts). With 10 threads on the M4 Max, the CP benchmark's shifted geometric mean goes from 1.10 s to 0.81 s (ft10: 42 s to a median of 9 s over five runs, range 6.7 to 13.2; rcpsp_j30_13_1: 5.1 s to 1.5 s), and the 27-instance development set from 2.15 s with 23 proven to 1.27 s with 26 proven (CP-SAT's portfolio: 1.17 s, 27). One thread's statistics are identical to before (`Optimizer` is the old loop). What mattered, measured on ft10: sharing the incumbent alone, with LNS-only workers, gave 24 to 30 s (LNS finds 930 in 0.3 s; the proof is what takes time, and every search repeated it); letting the improving workers go back to searching when LNS stops paying, plus sharing units and binary clauses, gave about 10 s; sharing more clauses (LBD up to 2, 3, 4, or 6) left ft10 within noise (9 to 11 s) and helped rcpsp_j30_13_1 (2.9 s with binary clauses only, 2.0 s at LBD 2, 1.5 s at LBD 3 or 6): hence LBD 3. tournament_12 doesn't improve (1180 to 1200 at 30 or 60 s with 1 or 10 threads; CP-SAT 1120): its LNS neighborhoods stall, and more workers running the same three kinds don't help. Next for threads then: an objective lower-bound worker, then the MILP and global searches' trees (done, below).
- Done (2026-11-09): parallel branch and bound and the parallel global search (`milp.rs`, `nlp/global.rs`: a `Search` shared by one `Worker` per thread; `RESULTS.md` has the tables). Ten threads against one, shifted geometric means: MILP benchmark 2.64 s to 0.78 s (lotsize_big 10.7 to 1.6, mknap 8.3 to 1.4), MIPLIB at 30 s 12.5 to 8.1 with 34 proven instead of 22 (HiGHS: 9.6, 33), Hock–Schittkowski 0.301 to 0.119, MINLPLib 0.376 to 0.211 with 312 proven instead of 307; no wrong answers, and `compare_random.py` and `compare_global.py` agree with `OPTIMIZAM_THREADS` set. One thread's statistics are identical to before (check with the previous binary, as for any refactoring; only the heuristics' dive counts, which depend on the clock, differ). What mattered in the global search: boxes side by side gave only 2x on circle packings, whose trees are narrow and whose time is local solves at shallow boxes (kall_congruentcircles_c72 5.7 s to 2.8 s, workers waiting 82% of the time), so each box's local solve and integer search is left for an idle worker (1.0 s); the root's bound tightening was then a third to a half of a run, and a share of its columns per thread without sharing cuts gave looser boxes (nvs24: 1500 boxes instead of 600), hence the cut pool. Not done: node bound tightening on idle threads (an estimated 10% on ex8_4_1), and pending incumbent searches are dropped when a node limit or stall ends the search. Next for threads: an objective lower-bound worker for CP.
- `Options::threads(n)`: the default (and 0) is one thread per core, at most 8 (`config::thread_count`:
  `available_parallelism`, halved when cores have two hardware threads, which Linux reports and which is assumed
  on x86 elsewhere; efficiency cores count as cores). The rule is provisional (2026-11-10): the owner asked for a
  default that performs well, and for repeatability to be a developer's tool, not a promise to users. Still to do:
  tune it (4, 7, 10, and 14 threads on the 14-CPU development machine; whether efficiency cores help). On the
  examples no problem is slower with the default than with one thread. One thread stays deterministic, and the
  developer's side depends on it: engine unit tests run with one (with nothing installed, `OPTIMIZAM_THREADS` or
  1), the benchmark and differential scripts set `OPTIMIZAM_THREADS=1` unless it is set already, and statistics
  comparisons need `--threads=1`. A test whose assertion depends on timing or on a run repeating pins
  `.threads(1)`: cargo runs a file's tests side by side, each solve with its own team of threads (found so:
  `cancel.rs`'s expired-deadline comparison, `progress.rs`'s 200 ms knapsack and millisecond job shops).
- `config.rs`'s settings are per thread: a worker thread started without `config::team` silently runs with the
  environment's defaults (no logger, no cancel flag, no halt flags).
- Progress callbacks are serialized (a `Mutex` around the call) and fire only for improvements on the shared
  incumbent, so users never see concurrent or out-of-order calls.

### 8. A barrier LP engine (later; measure first)

The engine with the most measured upside, if one is added. The dual simplex is the inner loop of the MILP search, of
every global-search bound, and of the nonlinear relaxations, and at scale it dominates: the root relaxation's LP
costs more than the local solve (chain-1000: 6.7 s vs 0.5 s), and Netlib and MILP profiles are ftran/btran and
refactorization. An interior-point method takes a few dozen iterations whatever the size.

- Mehrotra predictor-corrector on the LP's standard form (bounds as in `Lp`), solving the normal equations or the
  augmented system with `nlp/sparse_ldl.rs` (and the dense LDLᵀ for small systems); much of `nlp/ipm.rs` carries
  over, but an LP needs no line-search filter or restoration phase.
- Crossover to an optimal basis (push the interior point to a vertex, then a few primal/dual simplex pivots), so
  that branch and bound can warm-start children from it: the barrier for root LPs and large standalone LPs, the dual
  simplex for node reoptimization, where it is unbeatable.
- Choosing it: an `Engine` value or an `Options` switch to force it for benchmarks; `auto` uses it for LPs (and root
  relaxations) above a size threshold set by measurement.
- First step, before writing any of it: profile where LP time goes on `tools/nlp_scaling.py` and the larger
  Netlib instances, and how much of it a root solve is, to confirm the payoff.
- Measure with `tools/netlib_lp_benchmark.py` (against HiGHS and Netlib's optima), `tools/nlp_scaling.py`, and
  `tools/milp_benchmark.py` (root solves); check MINLPLib and `compare_random.py` for unchanged answers, since LP
  numerics change search paths.
- Ranked after it: Feasibility Jump (a weighted local search for first solutions to integer programs, as in CP-SAT
  and HiGHS; about 500 lines, helps incumbents, not proofs), then a convex QP/conic engine (proofs for convex
  quadratic problems without spatial branching, and relaxations for mixed-integer QPs). Neither is a substitute for
  the bigger documented gaps, which aren't engines: the CP portfolio (section 7) and conflict analysis, probing,
  and cuts below the root in the MILP search.

### Notes for the next session (2026-11-09)

Where things stand: steps 1 to 5 are done and pushed (0.1.0 is the published release; everything since is
unreleased, in `CHANGELOG.md`). Next is step 6: gaps (`Options::relative_gap`, `--gap`), warm starts, the docs
pass, then releasing 0.2.

`PROPOSALS.md` lists API and format changes suggested by a usability review, with the evidence for each. Its
proposals 1 to 6 were implemented on 2026-11-10 and removed from it; 7 to 27 remain, with their numbers (25 to 27
came out of implementing the first six), and the user chooses.

Open decisions for the user before 0.2:
- `Problem` is now a thin wrapper (parse, then `to_model` or solve). Keep it, or deprecate it in favor of
  `Model::from_json`/`from_file`? Decide before 1.0; keeping it costs nothing now.
- Decided 2026-11-10 (proposal 4): a literal outside a symbolic variable's set is an error, in files
  (`carrier == 'raill'`, checked in `convert.rs` where the literal's token is still known) and in the builder
  (`is`, `is_not`, checked in `lower.rs`); an element that arrives through an index or a table still
  never matches, and `convert.rs` applies that itself (the constraint is skipped, the literal dropped, or the
  row made `0 == 1`), so no model holds an out-of-set element outside a table.
- The CP bound search (section 5) is the way to make `gap()` useful on scheduling; it needs the CP benchmark.

Constraints in results (2026-11-10, proposal 6): with `Options::constraints` (`--constraints`), `api/constraints.rs` builds a `Table` of the builder model's `expr` items and each solution's slacks (expressions evaluated at the solution; None when an `only_if` literal is false there), and `api::solve_with` first tries `solve::linear_program`: a continuous linear model with an objective is solved by `Simplex` directly, without presolve (whose substitutions would leave rows without a dual), for the row duals `y = B^-T c_B` (`LpSolution::row_dual`, beside the reduced costs) and the reduced costs, turned to the problem's sense (negated for a maximization). `LinearModel::row_items` says which builder item each of the first rows is. `tests/constraints.rs` checks the duals against finite differences on random LPs.

Result metadata (2026-11-10, proposal 5): every result file says version "1.2" (`result::VERSION`) and carries `sense`, the bound whenever a search stopped with one (also with `timeout`), `gap`, `stop_reason`, and `statistics` (engine, threads, time, nodes). `solve.rs` records the engine with `config::ran(Engine::Cp)` where it hands the problem over (none: "presolve" in the file, `None` from `SolveResult::engine()`), and each top-level search adds its count with `config::count_nodes` when it ends on the solve's thread (branch-and-bound nodes, not those of sub-MILPs; the CP searches' conflicts, the portfolio workers' through `Shared::conflicts`; the global search's boxes): thread-locals reset by `config::scoped`, like the stop reason. Tests that compare result files across runs strip `statistics` and `stop_reason`. serde_json reads floats a last digit off (no `float_roundtrip`), so tests compare numbers from JSON with a tolerance.

Warnings (2026-11-10): `Problem::warnings()` and the command's `warning:` lines (JSON events with `--progress`) come from `convert::convert`, which returns them with the model: a `forall` that matched nothing (counted while expanding), `lint.rs` (read off the parsed expressions without expanding them: indexes that hide another name, constraints sharing a name, parameters and variables nothing refers to), and `explain::obvious` on the finished model. `Problem` keeps the converted model and warnings in a `OnceLock`, so `warnings`, `solve`, `conflict`, and `to_model` convert once. A parameter table's keys are checked against its index sets in `problem.rs` (`check_keys`), by walking the table's keys (sorted, as `serde_json`'s map keeps them) beside the set's sorted keys: the first version looked each key up in the set's list and made loading the set-covering benchmark's 600,000-entry table take 2.5 s instead of 0.4 s, which showed only as `setcover` going from 5.4 s to 7.5 s in `tools/milp_benchmark.py`. After touching `problem.rs`, `convert.rs`, or `lint.rs`, time a load of the largest files (`--time=0.001` on `target/milp_benchmark/setcover.json`, `target/miplib/json/nw04.json`, and `target/minlp_benchmark/instances/autocorr_bern25-25.json`: 0.40, 0.39, and 0.60 s); the statistics comparison on the examples doesn't see load time. `lint.rs` is handed the expressions `convert.rs` has parsed, so nothing is parsed twice.

Every error in one run (2026-11-10, the old proposal 26): `problem.rs` reads on past what is wrong. Its readers return `Option` and note each error in a `Sink` with its place and the item's name (an `Issue`), and a placeholder stands for the value: a bound that doesn't read is left out, a variable without a type is continuous, a set, parameter, interval, or constraint that doesn't read is left out whole. `Problem::read` then validates item by item, takes out what fails (`take_out`), and validates again until what is left is sound, since `convert.rs` relies on a sound problem and panics on less. Items that didn't read or failed are `broken`: later errors whose text mentions a broken name are echoes and are dropped (`Reading::echoes`). If anything was wrong, `api::Problem::from_json` converts what is left for the expressions' errors too, one per constraint (`convert` returns `Issues`), and returns them all as one `Error` (`Error::errors`, at most 20). The unit test `mutated_problems_never_panic` runs this path on thousands of mutated files.

Command line (2026-11-10, the old proposal 12): `RESULT` is optional; `--docs` and `--schema` print the documents and schemas `main.rs` includes with `include_str!` (so `docs/` and `schema/` must stay in the package); `--quiet` drops warnings, which are now `api/warning.rs`'s `Warning` (kind, place, message; made by `lint.rs`, `convert.rs`, and `explain::obvious_warnings`). `--check` is `api/check.rs`: `Model::check` and `Problem::check` lower the model and run `solve::solve` with `Config::plan` set, under which `config::ran` returns false and each hand-over to an engine returns at once (`solve::planned`), so the engine reported is the one a solve would use, presolve included, without a second copy of the choice. `--nonzeros` and `--constraints=binding` (`Options::nonzeros`, `binding_only`) are flags of the `Layout`, read only by the result writer (`Details::omit_zeros`, `omit_slack`, and the result's `omitted` list); results in memory are whole. After changing the command's flags, update `docs/cli.md`, `--help` in `main.rs`, `SKILL.md`, and `tests/command.rs`.

Bounds as expressions (2026-11-10, the old proposal 10): a variable's or an interval's `index` entry may name its index (`"t in TASKS"`; `problem.rs` keeps the sets in `index` and the names in `index_names`), and `Bound` is a number or an expression's text (`Bound::Expr`), which `convert.rs` parses once per item and works out per element with `Ctx::field` (the named indexes in an `Env` from `Ctx::scope`, places included). A bare name is the old form and still the fast path: a scalar parameter, or a table (for interval fields, a variable) over the item's own sets, read at the element's key. A parameter table's value may be `null` (`Ctx::param` returns `Option`): by itself as a bound it means none, and in arithmetic it is an error. `problem.rs` no longer checks what a bound names; `convert.rs` reports it with the place (`variables[3].upper`), one error and no more, since the model can't be built past a variable without bounds. `tools/mps_to_json.py` still splits columns into families by their finite bounds; with `null` it could keep an MPS file's column order, which would change every converted benchmark's search path, so it wasn't touched.

Comparisons (2026-11-10, the old proposal 9): `expr.rs`'s `Rel` has `Ne`, `Lt`, and `Gt`, and a `Constraint` has a `Mode`: `Within` (lower <= expr <= upper), `Strict` (the same limits, already moved by one: `a < b` is `a - b <= -1`), and `Differs` (expr != lower). `lower.rs` refuses the last two unless the sides are integer-valued (integer columns, whole coefficients and constant). A `Differs` row without literals becomes a two-item `Global::AllDifferent`, the terms with positive coefficients against the rest (so `x != y` shares the value binaries and value symmetry of `all_different`); with literals it is two `Global::Enforced` rows on either side of the value, chosen by a new binary column that further enforced rows hold at 0 while a literal fails (so that enumeration finds each solution once). A literal is a constraint (`From<Literal> for Constraint`: `element` with `Within` or `Differs`), which replaced `Variable::equals`, and `Model::add` takes `impl Into<Constraint>`. Two symbolic variables of one set may be compared (`as_difference`), by position, in `==` and `!=` only. `constraint!` takes `!=`; a strict `<` still can't come through the macro (`lt`, `gt`).

Functions in expressions (2026-11-10, the old proposal 8): `expr.rs` parses `Expr::Apply(Builtin, args)` and `Expr::Over(Min|Max, bindings, term)`; `convert.rs` works out the ones of data, sets, and indexes (`mod`, `div`, `floor`, `ceil`, `card`, `first`, `last`, `at`, `pos`; its `Env` carries each index's place in its set, so `pos(i)` costs nothing), and turns `abs`, `min`, and `max` of expressions with variables into `Tree::Abs`, `Tree::Min`, and `Tree::Max` terms (`Expression::abs`, `max`, `min` in the builder). Those are piecewise linear, not nonlinear: `lower.rs` (`Lowering::piece`) gives each a new column after the model's own, with bounds from its items' ranges, and holds it by rows alone where only one side can matter (`Pull`: an `abs` or `max` in an objective to minimize or under a row's upper limit, a `min` the other way; the pull passes through a `max`'s items, so `minimize max(i) abs(r[i])` is a linear program), and otherwise by a `Global::Abs` or `Global::MinMax` instance, which needs finite ranges. Columns held by rows alone are `LinearModel::loose`; `LinearModel::settled` puts them on their terms' values before a solution's objective is computed (a solution that isn't optimal may leave one above its term), which progress events don't do. With every solution wanted, all terms are exact. The piece rows come after the model's rows, so `row_items` still covers the first rows. Not done: one column for equal terms (each occurrence gets its own), and `abs` of a nonlinear argument (an error).

Numbers in the Rust API (2026-11-10, the old proposal 7): `Expression: From<T: Number>` is one blanket impl over a sealed trait for every primitive number type, and bounds take `impl Number`; an untyped literal still falls back to `i32`. Scalars on the left of an operator have impls for `i32` and `f64` only, on purpose: with a second integer type there, `2 * x + 1` has no known type (the output of `{integer} * Variable` is needed before the fallback happens). `KeyPart` is implemented for `&T` (and for `str`), so `&&str` and `&String` are keys.

Messages (2026-11-10, `tests/messages.rs` pins them): an unnamed constraint is called by its place, `constraints[3]` (in a file's list, or for a builder model its id), and `Error::new` drops the "constraint" before it; `convert.rs` adds hints to the usual slips (a name bound by a sum and used after its term, a set element without quotes, a subscript one past the end of its set, the wrong number of subscripts), `expr.rs` to `=`, double quotes, and `min`/`max`/`abs`/`log` as functions; `constraint!` has `compile_error!` arms for `!=` and for no comparison (a strict `<` can't be told from the `<` of `sum::<Expression>()`, so it is reported as no comparison).

Not done, listed so they aren't forgotten: `Progress::nodes()`, `Solution::family`, a public keyed `element` and
element tables (`table_of`, `element_keyed` are crate-private), tests that reach `StopReason::NodeLimit` and
`NoProgress`, and `Model::set_name` for non-`expr` constraints (labels are crate-private).

Things learned the hard way today:
- The equivalence check that made step 5 safe (both paths' `LinearModel`s compared through `Debug`, on the examples,
  the benchmark instances, and the fuzz test's mutated problems) was removed with the old path. If the front end
  changes again, keep a copy of the old binary and compare search statistics and results on every example
  (`OPTIMIZAM_DEBUG=1`, timings stripped), as was done for every step today.
- `Graph`'s hash-consing index is a `HashMap`, so its `Debug` prints only the nodes. Nonlinear coefficients can
  differ in the last bit between two ways of building the same expression: `(a + b) * c` against `a*c + b*c` when
  shared terms merge. Linear rows and CP instances are exact.
- Before compaction, partial sums left dead nodes in the graph that the global search scanned for OBBT
  candidates. Any new code that iterates over every graph node should iterate over the rows' and objective's
  reachable nodes instead (compaction now guarantees that the graph is exactly those).
- Run the nonlinear benchmarks one at a time and don't compile meanwhile: they use 10 s limits, and contention
  changes which instances finish.
- In shell chains, put `&&` between `git add` and `git commit`: a failed `git add` followed by `; git commit`
  pushed a commit holding only a file rename (d5400b3), which broke `main` until the next commit.
- The reference solvers' `.venv` may be missing in a checkout; `python3 -m venv` plus `pip install highspy ortools
  jsonschema` (in a scratch directory) is enough for the CP-SAT and HiGHS checks and the schema check. The wrapper
  scripts (`tools/cpsat.sh`, `tools/highs.sh`) hard-code `.venv`.
- zsh doesn't split unquoted variables into words: use Python (or arrays) for loops over file lists.

### Notes (2026-11-10)

The owner's directions today, in order: the MILP proof-strength step (committed); the automatic thread default
(above); then `PROPOSALS.md` 1 to 6, one commit each (done, 2026-11-10), stopping after 6 for permission to go on to 7 to
13; the thread rule's tuning after the proposals. The owner has no users and wants no backward compatibility:
clean breaks, no aliases or deprecations, one version per format. Waiting on the owner's word: rerunning HiGHS on MIPLIB at the 1e-6 gap
(`tools/miplib_benchmark.py --time 30 --highs --refresh`, half an hour), whose 1e-4 numbers the docs still
quote. Not asked for, so not started: root heuristics beside the cut loop on idle threads (nw04 takes 2.6 s with
one thread and with ten), effort scaled to the time limit, racing engines. "Easy to use well" is the owner's
test for defaults: argue from what a user gains, not from what makes testing easier.

### Order of work

1. Done: `CancelToken`, `config::expired` (statistics identical on every example).
2. Done: progress callbacks, stop reasons, `--incumbents`, `--progress` (`tests/progress.rs`).
3. Done: `Set`, `Element`, `VarFamily`, symbolic variables, lookups, editing methods; export of integer families.
4. Done: format 1.3 string literals (`expr.rs`'s `Tok::Str`/`Expr::Str`, evaluated to `Val::Str`;
   `tools/highs_reference.py` reads them as constants); export of every family, at version 1.3 only when a literal
   is written; `examples/plant_carriers.json`. The builder now follows the format for an element outside a
   symbolic variable's set (`is` never holds, `is_not` always does) instead of rejecting it.
5. Done: problem files go through `api/convert.rs` to a `Model` (`Problem::to_model`, `Model::from_json`,
   `Model::from_file`). The builder first gained what the format can say (domains as ranges, tables of elements,
   keyed `element` over symbolic or integer indexes, `Variable::equals`, named intervals, constraint labels). Before
   switching, both paths were compared: 498 problem files (examples, MINLPLib and Hock–Schittkowski translations,
   CP and MILP benchmark instances) gave identical solver models except for last-bit rounding in 2 nonlinear graphs
   (coefficients of shared nonlinear terms combine in another order), and 12,524 mutated examples gave identical
   errors or models. Graph compaction (dead partial-sum nodes, which `nlp/global.rs` scanned for OBBT candidates)
   was measured alone: MINLPLib 305 proven / 23 found / 13 local (from 305 / 21 / 15), the only changes 5
   time-limited `autocorr_bern` instances, all better; Hock–Schittkowski unchanged; loading `autocorr_bern25-25`
   3.1 s to 0.4 s. A loaded model's `to_json` writes the expanded problem.
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
cargo test --release --test families             # sets, variable families, symbolic variables, editing
cargo test --release --test threads              # Options::threads: the CP portfolio's answers, progress, and stopping
cargo build --no-default-features                # the library without the command (and clap, ctrlc)
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps   # API docs; doctests run with cargo test
cargo test --test regression knapsack            # one example
cargo clippy --all-targets       # lint
cargo fmt                        # format with rustfmt.toml (120 columns); cargo fmt --check must pass
```

## Solver

Phase 1 is an LP/MILP solver (dual simplex + branch and bound, HiGHS-style). Scope, error cases, numerics, status mapping, and the dependency policy (no crates beyond serde/serde_json/clap/ctrlc without asking) are in `docs/solver-design.md`.

## Regression tests and reference solver

`tests/regression.rs` runs the binary on each `examples/<name>.json` and compares against `examples/<name>.result.json`: status exactly; objectives within 1e-6 relative tolerance; the full solution set for `"all"`; only the variable layout for `"one"`, since optima need not be unique. A new example must be added to the `regression_tests!` list, and `every_example_is_tested` enforces that.

HiGHS (`highspy` 1.15.1), OR-Tools (`ortools` 9.15, for CP-SAT), SymPy (1.14, for derivatives), CasADi (3.8.2, whose wheel bundles IPOPT, for local nonlinear solves), and PySCIPOpt (6.3, SCIP, the global reference for nonlinear and mixed-integer nonlinear problems) are installed in the project's `.venv` (gitignored) as development-only references. Optimizam itself must not depend on them. To recreate: `python3 -m venv .venv && .venv/bin/pip install highspy ortools jsonschema sympy casadi pyscipopt`.

- `tools/scip_reference.py` (wrapped by `tools/scip.sh`, same arguments as optimizam) is the global search's reference: SCIP on our format (continuous, integer, and binary variables; `expr` constraints; no `atan`), reusing `highs_reference.py`'s parser with its own SCIP expressions. Verify the global search against it (and brute force), not against IPOPT, which only finds local optima.
- `tools/minlp_benchmark.py` is the global benchmark: MINLPLib instances with a proven optimum, at most `--max-vars` variables (default 12), and only operations we have, downloaded on demand (GAMS scalar format) into `target/minlp_benchmark/` and translated. Verdicts: proven, found (not proven), local, failed, and WRONG (a `complete` or `infeasible` that contradicts the proven optimum); it exits with status 1 on any WRONG. `cargo build --release && python3 tools/minlp_benchmark.py` (`--scip` adds SCIP; `--time`, default 10 s; naming instances runs only those). Run it after changing the global search, relaxations, propagation, or the local solve. As of graph compaction (step 5 of the 0.2 plan; all 342 selected instances with at most 30 variables, `--time 10`): 305 proven, 23 found but not proven, 13 local, 1 failed, none wrong, 337 s. As of the OBBT with cutting rounds: 305 proven, 22 found but not proven, 14 local, 1 failed, none wrong, 370 s (integer local search: 297, 26, 18, 1, 465 s; milestone 7: 297, 20, 22, 3; milestone 6: 262 proven and one wrong answer, nvs09).
- `tools/compare_global.py` is the global search's differential test: random small nonconvex problems (some with an integer variable, some infeasible) from fixed seeds, solved by the release binary and by SCIP; disagreements (contradicting proofs, or a proven optimum beaten by the other's point) are saved to `compare_global_failures/` (gitignored), with exit status 1. `cargo build --release && python3 tools/compare_global.py` (`--count`, `--seed`, `--time`).

- `tools/ipopt_reference.py` (wrapped by `tools/ipopt.sh`, same arguments as optimizam) is the nonlinear engine's reference: it reuses `highs_reference.py`'s parser, sets, parameters, and variables, but evaluates expressions into CasADi expressions and solves with IPOPT, writing our result format with the nonlinear statuses. Continuous variables and `expr` constraints only. Produce a nonlinear example's expected result with `tools/ipopt.sh examples/<name>.json examples/<name>.result.json`, and check them with `OPTIMIZAM_BIN=tools/ipopt.sh cargo test --test regression -- curve_fit compound_growth supply_chain_lp` (it reads linear continuous problems too). The other nonlinear examples' expected results come from optimizam itself, checked independently: `hs071`, `portfolio_risk`, and (since the convexity proofs) `chemical_equilibrium` are `complete` (proven global, at IPOPT's local optima), `disk_infeasible` is `infeasible` (IPOPT can only report `locally_infeasible`), and `batch_sizing` (integers, which IPOPT doesn't take) agrees with enumeration over the integer.
- `tools/nlp_benchmark.py` is the nonlinear benchmark: the Hock–Schittkowski problems in Vanderbei's AMPL versions, downloaded on demand into `target/nlp_benchmark/` and translated by a small interpreter for the AMPL subset they use (some models are skipped, with the reason printed), each checked against the published optimum from its `display obj - VALUE` line. `cargo build --release && python3 tools/nlp_benchmark.py` (`--ipopt` adds IPOPT's answers and flags differences; `--time`, default 10 s; naming problems or prefixes runs only those). Local solvers can find another local optimum of a nonconvex problem; those are counted apart from failures. As of milestone 5 (global search, `--time 60`): 81 of the 103 translated problems at the published optimum (IPOPT: 76), none failed, 55 proven globally optimal; every proven optimum agrees with the published one (hs059's published value is from the original paper's coefficients: its point evaluates to our optimum in this model).
- `tools/nlp_scaling.py` is the local solve's scaling benchmark: generated problems with 50 to 1000 variables and as many nonlinear rows, touching 3 variables each (`chain-N`) or 20 (`wide-N`), timed with `--engine=local`; `--ipopt` adds IPOPT's times and flags differing objectives. `cargo build --release && python3 tools/nlp_scaling.py --ipopt`. Run it after changing the interior point method or its linear algebra. As of the sparse LDLᵀ: chain-400 0.8 s (dense: 25 s), chain-1000 4.8 s (dense: timed out at 120 s), wide-400 3.6 s (dense: 74 s), all at IPOPT's objectives.
- `tools/sympy_derivatives.py` writes `tests/data/sympy_derivatives.json`: random format 1.2 expressions over x1..x3 with SymPy's values, gradients, and Hessians at random points, which `nlp::tests::derivatives_match_sympy` checks through the model builder. Regenerate it with `.venv/bin/python tools/sympy_derivatives.py tests/data/sympy_derivatives.json` after changing the generator.

- `tools/highs_reference.py` is an independent Python implementation of the problem format (parser + linearization) that solves with HiGHS and writes our result format. `tools/highs.sh` wraps it with optimizam's CLI. It encodes integers one-hot for gapped domains and for "all" (enumerated with no-good cuts), so those need finite bounds.
- To produce an expected result: `tools/highs.sh examples/<name>.json examples/<name>.result.json` for a format 1.0 example, `tools/cpsat.sh ...` (the same arguments) for one using format 1.1. The small examples' results were brute-forced instead, and agree with HiGHS.
- To check the expected files and the test itself: `OPTIMIZAM_BIN=tools/highs.sh cargo test --test regression -- --skip n_queens_alldiff --skip room_assignment --skip sudoku --skip machine_assignment --skip shift_rota --skip tsp_tour --skip load_balance --skip production_smoothing --skip job_shop --skip crew_schedule --skip hs071 --skip portfolio_risk --skip curve_fit --skip chemical_equilibrium --skip disk_infeasible --skip batch_sizing --skip plant_carriers --skip sudoku_unique --skip line_fit` (HiGHS reads only format 1.0 and linear 1.2; `line_fit`'s optimum was checked with HiGHS on the same fit written with a variable above each distance), and `OPTIMIZAM_BIN=tools/cpsat.sh cargo test --test regression -- n_queens_alldiff room_assignment sudoku machine_assignment shift_rota tsp_tour load_balance production_smoothing job_shop crew_schedule plant_carriers sudoku_unique four_queens knapsack domain_gaps` (the all-integer examples); both must pass.
- `tools/cpsat_reference.py` (wrapped by `tools/cpsat.sh`) is the CP engine's reference: it reuses `highs_reference.py`'s parser and evaluator (extended for 1.1 lists and set domains; `highspy` is imported lazily there, since OR-Tools bundles its own HiGHS and the two clash) and builds a native CP-SAT model (`AddAllDifferent`, `AddAllowedAssignments`/`AddForbiddenAssignments`, `AddElement`, `AddMin/Max/AbsEquality`, interval variables with `AddNoOverlap`/`AddCumulative`, `OnlyEnforceIf`, set domains), so it shares nothing with `encode.rs`. Integer variables with finite bounds only.
- The large examples come from `tools/generate_examples.py` (fixed seeds). If you regenerate them, regenerate their result files too.
- `tools/milp_benchmark.py` is the MILP performance benchmark: seven generated instances (facility location, generalized assignment, two lot sizing, multidimensional knapsack, set covering, time-window scheduling) from fixed seeds, each stressing a different part of the solver. `cargo build --release && python3 tools/milp_benchmark.py` prints time, status, objective and bound per instance (about 50s); `--highs` adds HiGHS's times and flags disagreeing optima, and naming instances runs only those. Run it before and after performance work.
- `tools/miplib_benchmark.py` is the MILP benchmark on real models: 58 of the smaller MIPLIB 2017 instances with a known optimum, downloaded on demand into `target/miplib/` and converted with `mps_to_json.py`, each checked against MIPLIB's optimal value (verdicts: proven, found, feasible, failed, error, and WRONG, with exit status 1). `cargo build --release && python3 tools/miplib_benchmark.py` (`--time`, default 60 s; `--threads`; `--highs` adds HiGHS on the MPS files, cached in `target/miplib/highs.json` until `--refresh`; naming instances runs only those). It guards against tuning the solver to the seven generated instances. HiGHS runs at a relative gap of 1e-6, optimizam's own (its default is 1e-4); cached results from another gap are shown with a note. As of the cut loop, fixing, and restarts of 2026-11-10 (`--time 30`): 28 proven with one thread, 38 with ten, none wrong (22 and 34 before); HiGHS 33, still from its 1e-4 run. HiGHS proves beasleyC3, enlight_hard, h50x2450, and neos-911970, which optimizam doesn't even with ten threads: the targets for a cut pool, tableau aggregations, conflict analysis, and probing.
- `tools/mps_to_json.py` converts MPS (fixed or free format, optionally gzipped) to our problem format: `python3 tools/mps_to_json.py in.mps.gz out.json [--names names.json] [--pretty]`. Columns become integer-indexed variables grouped by type and finite bounds (the format has no string literals or infinite parameters); ranged rows become two constraints. One convention differs between readers: a negative UP bound with no lower bound sets the lower bound to -infinity here (with a warning), while HiGHS 1.15 keeps 0.
- `tools/netlib_lp_benchmark.py` is the LP benchmark: it downloads the 91 main Netlib LPs that the mirror has (gzipped MPS from the COIN-OR Data-Netlib mirror) into `target/netlib_lp/`, converts them, solves them, and checks each against HiGHS reading the original MPS and against Netlib's published optimum. `cargo build --release && python3 tools/netlib_lp_benchmark.py` (add `--check-converter` to also solve the converted JSON with HiGHS, which separates converter bugs from solver bugs). The Netlib files are downloaded on demand and never committed.
- `tools/cp_benchmark.py` is the CP benchmark: published instances downloaded on demand into `target/cp_benchmark/` (job shop from JSPLIB, RCPSP j30 from PSPLIB via the MiniZinc benchmarks, DIMACS graph coloring) plus generated Golomb rulers, Sudoku, N-queens, and a sports tournament schedule, each checked against its known optimum. `cargo build --release && python3 tools/cp_benchmark.py` (`--time`, default 60 s per instance; `--cpsat` adds OR-Tools CP-SAT's times and answers; naming instances or prefixes runs only those).
- `tools/compare_random.py` is a differential test: it solves random MILPs from fixed seeds (facility-location-shaped, unstructured, and medium instances of classic classes that exercise presolve and the heuristics) with both the release binary and HiGHS, and reports any status or objective mismatch. Disagreeing problems are saved to `compare_random_failures/` (gitignored), and it exits with status 1. Run it after changing the simplex, branching, cuts, presolve, or heuristics: `cargo build --release && python3 tools/compare_random.py` (about a minute; `--count`, `--seed`, and `--kind` adjust it).

## Problem file format

Problems are JSON files; solving one produces a JSON result file (`schema/result.schema.json`, `docs/result-format.md`). `schema/problem.schema.json` (JSON Schema 2020-12) defines the structure. `docs/problem-format.md` defines what the fields mean and the expression grammar. Constraint and objective expressions are strings (e.g. `sum(i in I) w[i]*x[i] <= cap`), so the JSON Schema can't validate them; they need a parser in the Rust code. Keep the schema, the doc, and `examples/` in sync when changing the format.

Format 1.1 adds set-valued variables (`domain` naming a set; symbolic when the set has strings), an `intervals` section, `enforce_if` on `expr` constraints, and the constraint kinds `all_different`, `element`, `table`, `min`, `max`, `abs`, `no_overlap`, and `cumulative`, which take list arguments (`"[x[i] | i in I]"`). Implemented so far: lists, set-valued variables (symbolic ones in lists, `table`, `element` indexes, and `v == e` / `v != e` comparisons), `all_different`, `table`, `element`, `enforce_if`, `min`, `max`, `abs`, intervals, `no_overlap`, and `cumulative`: all of format 1.1. `model.rs` evaluates each instance to an `encode::Global`, and `encode.rs` encodes it as linear rows on extra columns (shared value binaries `[x = v]`, selectors, big-M) so both engines solve it; auto picks CP for problems using 1.1 kinds. The exceptions are `no_overlap` and `cumulative`: the model keeps them in `LinearModel::scheduling`, the CP engine propagates them natively (`cp/scheduling.rs`), and `solve.rs` encodes them only when the MILP engine is chosen. Other kinds could get native propagators the same way if their encodings prove slow. OR-Tools CP-SAT is the CP engine's reference, as HiGHS is for MILP; verify the CP engine with brute-force tests and OR-Tools, not by comparing it against HiGHS.

Format 1.3 also takes a number for `solve.solutions` (at most N solutions, optimal ones with an objective: `complete` if those are all, `incomplete` with `StopReason::SolutionLimit` if the search stopped at the Nth; `Model::solution_limit`; the MILP search stops once N solutions tie an incumbent that no open node can beat, the CP engine's enumeration at the Nth).

Format 1.3 also has functions of data, sets, and indexes (`mod`, `div`, `floor`, `ceil`, `card`, `first`, `last`, `at`, `pos`) and `abs`, `min`, and `max` of numbers or of affine expressions with variables (`max(j in J) end[j]` like a sum); `tools/highs_reference.py`'s shared parser and evaluator have the functions (so every reference reads them), and `abs`, `min`, and `max` of variables only where the model sets `on_piece` (`tools/cpsat_reference.py`, with CP-SAT's own equalities).

Format 1.3 adds string literals in expressions: single-quoted set elements (`'north'`, a quote doubled inside), allowed wherever an element is (subscripts, conditions, `enforce_if` comparisons, a whole `expr` comparing a symbolic variable, lists) and not in arithmetic; the string `'3'` isn't the integer 3. The reader takes any 1.x version, so the version only documents what a file uses.

Format 1.2 adds the functions `ln`, `exp`, `sqrt`, `sin`, `cos`, `tan`, `atan`, `tanh` (`expr::Func`), powers with any exponent (`Expr::PowE`; integer constant exponents stay `Expr::Pow`), and `start` on variables; result format 1.1 adds the statuses `locally_optimal` and `locally_infeasible` (written with result version "1.1"). All of it is implemented: functions and powers of constants are evaluated when the model is built; nonlinear terms in the variables, which `model.rs` keeps in an expression graph (`nlp/graph.rs`), go to the nonlinear engine, which needs every variable to be continuous (mixed-integer nonlinear problems are rejected). IPOPT (through CasADi) is the nonlinear engine's reference; nonlinear examples' expected results come from `tools/ipopt.sh`. To check the examples against the schema:

```sh
python3 -c "import json,glob,jsonschema; s=json.load(open('schema/problem.schema.json')); [jsonschema.validate(json.load(open(f)), s) for f in glob.glob('examples/*.json') if not f.endswith('.result.json')]; r=json.load(open('schema/result.schema.json')); [jsonschema.validate(json.load(open(f)), r) for f in glob.glob('examples/*.result.json')]"
```

`Model::to_json` must write files that pass the same schema. `tests/builder.rs` writes every model it exports to `$OPTIMIZAM_EXPORT_DIR` when that is set; check them with:

```sh
mkdir -p /tmp/exports && OPTIMIZAM_EXPORT_DIR=/tmp/exports cargo test --release --test builder
python3 -c "import json,glob,jsonschema; s=json.load(open('schema/problem.schema.json')); [jsonschema.validate(json.load(open(f)), s) for f in glob.glob('/tmp/exports/*.json')]"
```
