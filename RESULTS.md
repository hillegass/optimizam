# Benchmark results: optimizam vs. reference solvers

Measured 2026-11-08 on an Apple M4 Max (14 cores, 36 GB), macOS, rustc 1.101.0-nightly, commit 9440561 plus the
release-profile change. Every run was sequential; nothing else ran during the timings.

The CP engine was then reworked, and the CP benchmark rerun after each step against the same CP-SAT reference: see
[CP engine work](#cp-engine-work) below. After that the LP engine was: see [LP engine work](#lp-engine-work), whose
numbers replace the LTO column's for the MILP, LP, NLP, and MINLP benchmarks. The MILP engine's cuts and search were
strengthened last: see [MILP proof strength](#milp-proof-strength), whose numbers replace the MILP and MIPLIB rows of
[Parallel tree search](#parallel-tree-search). Those changes are in the CP engine (which runs `satisfy` and `"all"`
problems and those with format 1.1 constraint kinds) and in the `all_different` encoding; none of the other
benchmarks use either, so their numbers here still stand.

Three configurations:

- **Reference**: the established open-source solver for each problem class. HiGHS 1.15.1 (MILP, LP), OR-Tools CP-SAT
  9.15 (CP), IPOPT via CasADi 3.8.2 (local NLP), and SCIP 10.0 via PySCIPOpt 6.3 (global MINLP).
- **LTO**: `cargo build --release` with the new `[profile.release]` (`lto = "fat"`, `codegen-units = 1`,
  `panic = "abort"`).
- **PGO**: the same profile plus profile-guided optimization, built with `cargo-pgo` (trained on the examples and a
  subset of each benchmark at 5 s per solve).

## Summary

Shifted geometric mean of wall time per instance (shift 1 s, so short instances don't dominate), in seconds. Lower is
better. "PGO/LTO" is the ratio of the two optimizam columns.

| Benchmark | Instances | Reference | LTO | PGO | PGO/LTO |
|---|---:|---:|---:|---:|---:|
| MILP (`milp_benchmark.py`) | 7 | HiGHS 3.642 | 3.715 | 3.660 | 0.985 |
| LP (Netlib, `netlib_lp_benchmark.py`) | 91 | HiGHS 0.079 | 0.140 | 0.136 | 0.971 |
| CP (`cp_benchmark.py`) | 19 | CP-SAT 1.540 | 2.955 | 2.883 | 0.976 |
| CP, after the CP engine work | 19 | CP-SAT 1.540 | **1.115** | | |
| NLP (Hock–Schittkowski, `nlp_benchmark.py`) | 103 | IPOPT 0.135 | 0.362 | 0.355 | 0.979 |
| MINLP (MINLPLib ≤ 30 vars, `minlp_benchmark.py`) | 342 | SCIP 0.533 | 0.411 | 0.408 | 0.995 |

Total wall time, in seconds:

| Benchmark | Reference | LTO | PGO |
|---|---:|---:|---:|
| MILP | 39.1 | 41.9 | 41.2 |
| LP (Netlib) | 11.0 | 28.1 | 27.4 |
| CP | 92.2 | 238.4 | 234.3 |
| CP, after the CP engine work | 92.2 | 118.6 | |
| NLP | 14.1 | 90.7 | 88.1 |
| MINLP | 436.7 | 381.0 | 379.5 |

Takeaways:

- **PGO is a small, consistent gain: 1.5–3% on four benchmarks**, and within noise on MINLP. It changed no answer
  except one instance that sits at its time limit (pointpack06, below). For scale, the LTO binary ran twice (once
  alone, once alongside the reference solver), and its two geometric means differed by 0.0–1.0% per benchmark. So the
  LP, CP, and NLP gains (2–3%) are about 3x the noise, MILP's (1.5%) is borderline, and MINLP's (0.5%) is noise. Where
  PGO helps most is the larger solves: jobshop_la16 −9%, hs059 −10%, pilot87, fit2p, d2q06c, and most CP instances
  −3 to −5%. Instances that run to the time limit can't get faster, which is why the totals move less.
- **Against the references:** optimizam beats SCIP on MINLPLib in both time (0.41 vs 0.53) and total (381 s vs 437 s),
  though SCIP proves a few more instances (310 vs 303–304). On MILP it is roughly even with HiGHS: it wins on mknap,
  setcover, gap, lotsize, and windows, and loses on lotsize_big (15.9 s vs 2.3 s) and cfl_large (5.4 s vs 2.9 s). On
  Netlib LPs it takes about 1.8x HiGHS's time. On CP it trailed CP-SAT at first (2.96 vs 1.54), most of all on ft10,
  golomb_8, myciel5, and 12-queens enumeration; after the CP engine work it is ahead (1.12), with CP-SAT's parallel
  portfolio still well ahead on the hardest scheduling proofs (below). Next to IPOPT the NLP numbers look worse than they are: optimizam runs a global search and
  proves 81 of the 103 problems globally optimal, while IPOPT only finds local optima; optimizam also lands on the
  published optimum more often (82 vs 76).
- **No wrong answers** in any configuration: every proven result agrees with the published or reference optimum.

## Method

Each benchmark ran three times, back to back: first with its reference flag (`--highs`, `--cpsat`, `--ipopt`,
`--scip`; Netlib always runs HiGHS) alongside the LTO binary, then the LTO binary alone, then the PGO binary alone.
The LTO and PGO columns above come from the two solo passes. Time limits are the benchmarks' defaults: 60 s per
instance for MILP, LP, and CP, and 10 s for NLP and MINLP. `minlp_benchmark.py` ran with `--max-vars 30`, the
selection CLAUDE.md's figures use. The NLP benchmark doesn't record IPOPT's times, so I timed `tools/ipopt.sh` on the
same translated problems separately, with a 10 s limit.

How the reference times were measured differs, and that favors some solvers over others:

- **HiGHS (Netlib)** is timed inside Python around `readModel` + `run`: solve time only, no process startup.
- **HiGHS (MILP), CP-SAT, IPOPT, and SCIP** are timed as whole runs of their wrapper scripts (`tools/*.sh`). That
  includes starting Python, importing the solver, and parsing our JSON format. This is a fixed cost per run: about
  0.4 s for CP-SAT (no CP-SAT instance finished faster than 0.40 s), 0.12 s for IPOPT (its fastest problem took 0.121 s,
  so most of IPOPT's per-problem times are this overhead), and about 0.1 s for HiGHS and SCIP.
- **optimizam** is timed as a whole process run, too, but it starts in a few milliseconds.

With a 1 s shift, the geometric means are fairly robust to these offsets. Still, CP-SAT and IPOPT are faster than the
tables show on small instances.

## Results by benchmark

### MILP (7 generated instances)

All three solvers prove the same optimum on every instance.

| Instance | HiGHS | LTO | PGO |
|---|---:|---:|---:|
| cfl_large | 2.86 | 5.44 | 5.24 |
| gap | 3.24 | 1.78 | 1.77 |
| lotsize | 1.65 | 0.66 | 0.64 |
| lotsize_big | 2.34 | 15.90 | 15.76 |
| mknap | 17.51 | 9.63 | 9.45 |
| setcover | 11.11 | 8.42 | 8.33 |
| windows | 0.43 | 0.03 | 0.03 |

### LP (91 Netlib instances)

All 91 instances match HiGHS within 1e-6 relative error in all configurations. The slowest instances:

| Instance | HiGHS | LTO | PGO |
|---|---:|---:|---:|
| dfl001 | 3.54 | 10.37 | 10.30 |
| pilot87 | 2.75 | 6.06 | 5.79 |
| maros-r7 | 0.45 | 3.45 | 3.40 |
| fit2p | 0.66 | 2.72 | 2.60 |
| d2q06c | 0.50 | 1.01 | 0.97 |

### CP (19 instances)

With the LTO build (before the CP engine work), optimizam proved 17 instances: not **coloring_myciel5** within 60 s
(CP-SAT: 3.1 s), and neither solver proves **tournament_12** (optimizam 1200, CP-SAT 1120). The PGO column was within
noise of LTO. For the per-instance times now, see the next section. CP-SAT's 9.6 s on jobshop_ft06 is an outlier; it
proves the next instances in 0.4 s.

### NLP (103 Hock–Schittkowski problems; 23 more are skipped by the translator)

| | optimizam (LTO and PGO) | IPOPT |
|---|---:|---:|
| At the published optimum | 82 | 76 |
| At another local optimum | 5 | 12 |
| Solved, no published optimum | 15 | 15 |
| Failed | 1 (hs013, not proven in 10 s) | 0 |
| Proven globally optimal or infeasible | 81 | — |

optimizam's long times come from the global search trying to close the gap. hs070, hs105, and hs108 run to the 10 s
limit, and hs101/102/103/119 take 7 s. IPOPT, a local solver, stops at its first local optimum: 0.12–0.33 s on all
but one problem, mostly startup overhead. With PGO, hs059 drops from 6.2 s to 5.6 s and hs101–103 by about 4%.

### MINLP (342 MINLPLib instances with at most 30 variables)

| | LTO | PGO | SCIP |
|---|---:|---:|---:|
| Proven | 303 | 304 | 310 |
| Found (not proven) | 22 | 21 | 17 |
| Local | 16 | 16 | 13 |
| Failed | 1 | 1 | 2 |
| WRONG | 0 | 0 | 0 |

The one difference between LTO and PGO is **pointpack06**. optimizam proves it in 9.76–9.99 s against the 10 s
limit, so it lands on either side of the limit from run to run. The LTO binary also proved it in its reference pass.

- **optimizam proves and SCIP doesn't (in 10 s):** alkylation, chem, csched1a, du-opt, ex8_1_7, kall_circles_c7a,
  kall_diffcircles_8, nvs05, st_e36.
- **SCIP proves and optimizam doesn't:** autocorr_bern20-05, ball_mk2_30, cvxnonsep_pcon30, du-opt5, ex14_1_7,
  ex4_1_5, ex5_4_4, ex9_2_3, fac1, gear4 (a known limitation; see CLAUDE.md), hadamard_5, himmel16,
  kall_congruentcircles_c72, pointpack06 (borderline, above), supplychain, tln4. Several of these take SCIP 0.1–0.2 s
  (fac1, gear4, tln4), which points to presolve or bound-tightening steps that optimizam lacks.

## CP engine work

Five rounds of changes to the CP engine, each followed by a full run of `cp_benchmark.py` (60 s per instance) and
compared with the CP-SAT column above (OR-Tools 9.15, default parameters: a parallel portfolio of workers):

1. **Incremental linear propagation and a native `all_different`.** Rows keep their minimum activity up to date along
   the trail, instead of recomputing it on every wake-up. `all_different` instances with an item over several columns
   with 32 or more values (golomb's differences) are propagated by a Hall-interval propagator instead of being
   value-encoded.
2. **Value symmetry.** Columns with interchangeable values (graph coloring's colors) get value precedence rows.
3. **Scheduling proofs** (option 1). No-op pushes no longer build explanations, clause watches store their literal's
   value, conflict analysis doesn't look entries up again, and `no_overlap` has detectable precedences.
4. **Scheduling search** (option 2). A faster edge finding (O(n^2 log n), same bounds and search).
5. **Enumeration** (option 3). No restarts while enumerating, blocker literals on clause watches, and replacement
   searches that resume where they stopped.

Shifted geometric mean (shift 1 s) and total over the 19 instances, in seconds:

| | CP-SAT | Before | 1 | 2 | 3 | 4 | 5 |
|---|---:|---:|---:|---:|---:|---:|---:|
| Shifted geometric mean | 1.540 | 2.955 | 2.336 | 1.482 | 1.263 | 1.260 | **1.115** |
| Total | 92.2 | 238.4 | 217.3 | 151.7 | 122.5 | 123.2 | 118.6 |

Per instance, in seconds ("limit": not proven in 60 s):

| Instance | CP-SAT | Before | After | |
|---|---:|---:|---:|---|
| jobshop_ft06 | 9.64 | 0.02 | 0.07 | |
| jobshop_la01 | 0.41 | 0.09 | 0.06 | |
| jobshop_la05 | 0.40 | 0.02 | 0.02 | |
| jobshop_la16 | 0.55 | 1.36 | 0.82 | |
| jobshop_ft10 | 2.15 | 47.25 | 42.61 | 29–30 s after rounds 3 and 4; the search path is chaotic (below) |
| rcpsp_j30_1_1 | 0.41 | 0.01 | 0.01 | |
| rcpsp_j30_13_1 | 2.78 | 14.16 | 5.25 | |
| rcpsp_j30_29_1 | 0.53 | 0.59 | 0.24 | |
| rcpsp_j30_45_1 | 0.75 | 0.25 | 0.25 | |
| coloring_myciel4 | 0.42 | 0.01 | 0.01 | |
| coloring_myciel5 | 3.13 | 60.04 (limit) | 0.41 | value symmetry (round 2) |
| coloring_queen6_6 | 0.52 | 5.06 | 0.11 | |
| coloring_queen7_7 | 0.44 | 0.55 | 0.39 | |
| golomb_7 | 0.40 | 1.21 | 0.02 | native `all_different` (round 1) |
| golomb_8 | 0.42 | 10.87 | 0.17 | native `all_different` (round 1) |
| sudoku_inkala | 0.42 | 0.04 | 0.03 | |
| queens_100 | 0.74 | 0.57 | 0.14 | |
| queens_12_all (14,200 solutions) | 7.65 | 36.26 | 7.98 | enumeration (round 5) |
| tournament_12 | 60.44 (limit) | 60.02 (limit) | 60.05 (limit) | best 1180 (was 1200; CP-SAT 1120) |

All answers agree with the known optima and with CP-SAT; no answer got worse, and myciel5 is now proven.

Single scheduling instances were too noisy to judge changes on (any change to propagation order changes the search
path), so rounds 3 to 5 were also measured on a development set of 27 more instances with known optima: 15 JSPLIB job
shops (la02–04, la17–20, abz5–6, orb01–05, ft10) and 12 PSPLIB j30 RCPSP instances, at 20 s each. Shifted geometric
mean: CP-SAT 1.172 s (27 proven); before round 3 3.218 s (20 proven); after round 5 2.190 s (23 proven).

Where CP-SAT is still ahead, it is mostly its parallel portfolio, which optimizam doesn't have: a single CP-SAT worker
(`num_workers: 1`) takes 16 s on jobshop_ft10 (8 s on rcpsp_j30_13_1), and ends tournament_12 at 1240, worse than
optimizam's 1180. The experiments that didn't help (learned-clause minimization, not-last, restart and VSIDS
settings, and others) are listed in CLAUDE.md.

## Parallel tree search

Measured 2026-11-09, after the CP portfolio and the LP engine work below. With `--threads=N`, the MILP engine's
branch and bound and the global nonlinear search each share one tree among N workers (`src/milp.rs`,
`src/nlp/global.rs`). Ten threads on the M4 Max against one thread of the same binary, and against the references
with their default settings. One thread's search statistics are identical to before on every example and benchmark
instance (apart from the number of heuristic dives, which depends on the clock). Shifted geometric mean and total,
in seconds:

| Benchmark | Reference | 1 thread | 10 threads |
|---|---:|---:|---:|
| MILP (7 generated instances) | HiGHS 3.642; 39.1 | 2.643; 28.9 | **0.779**; 6.2 |
| MIPLIB (58 instances, 30 s limit; `miplib_benchmark.py`) | HiGHS 9.603; 985.5 | 12.531; 1242.3 | **8.138**; 916.0 |
| MIPLIB: proven | 33 | 22 | 34 |
| NLP (103 Hock–Schittkowski problems, 10 s limit) | | 0.301; 66.3 | **0.119**; 24.1 |
| NLP: proven | | 81 | 81 |
| MINLP (342 MINLPLib instances, 10 s limit) | SCIP 0.533; 436.7 | 0.376; 337.9 | **0.211**; 185.2 |
| MINLP: proven / found / local / failed | | 307 / 21 / 12 / 2 | 312 / 19 / 10 / 1 |

| Instance | 1 thread | 10 threads |
|---|---:|---:|
| lotsize_big (MILP benchmark) | 10.71 | 1.62 |
| mknap | 8.30 | 1.41 |
| setcover | 5.48 | 1.60 |
| cfl_large | 2.52 | 0.94 |
| ran13x13 (MIPLIB) | 27.40 | 4.44 |
| air05 | 25.77 | 5.54 |
| misc07 | 16.17 | 1.45 |
| swath1 | not proven in 30 s | 3.4 |
| pointpack06 (MINLPLib) | 6.34 | 0.61 |
| kall_congruentcircles_c72 | 5.96 | 0.98 |
| nvs24 | 5.68 | 0.55 |
| ex8_4_1 | 3.53 | 0.60 |
| kall_diffcircles_10 | no feasible point in 10 s | 5.32 |
| hs119 (Hock–Schittkowski) | 3.72 | 0.78 |

- **No wrong answers**: every proven result agrees with the known optimum, and with threads the differential
  tests report no disagreement (`compare_random.py`, 600 MILPs against HiGHS with 8 threads; `compare_global.py`,
  100 nonlinear problems against SCIP with 10). No MINLPLib or MIPLIB instance has a worse verdict with ten threads
  than with one. One unproven answer differs: hs047 (unbounded variables, stopped by stall detection either way)
  ends at -0.0267 instead of the published local optimum 0.
- **MIPLIB**: with ten threads, twelve more instances are proven within 30 s (aflow30a, air04, binkar10_1,
  cap6000, eil33-2, mas76, mod011, pk1, qiu, ran12x21, rout, swath1), one more than HiGHS in all. HiGHS proves six
  that optimizam doesn't (beasleyC3, enlight_hard, h50x2450, mik-250-20-75-4, neos-911970, p200x1188c: mostly
  fixed-charge network flows, which need stronger cuts), and optimizam seven that HiGHS doesn't (eil33-2,
  markshare_4_0, mas76, mod011, pk1, qiu, ran12x21). With one thread optimizam proves 22 against HiGHS's 33: where
  both finish it is often the faster, and what it lacks is proof strength (no cuts below the root, conflict
  analysis, or probing), which threads make up for only by brute force.
- **What mattered in the global search.** Workers taking boxes side by side gave only 2x on the circle packings
  (kall_congruentcircles_c72: 5.7 s to 2.8 s), whose trees are narrow and whose time goes to local solves at
  shallow boxes: the workers were waiting 82% of the time. Leaving each box's local solve and integer search for
  an idle worker, while the box's own worker goes on to split it, gave 1.0 s. The root's bound tightening was then
  a third to a half of each run (nvs24: 0.37 s of 0.9 s). Giving each thread a share of its columns made the root
  six times faster but the boxes looser, since each thread saw only its own cuts (nvs24: 1500 boxes instead of
  600); with the cuts pooled the trees are as before (470 to 700) and the root takes 0.09 to 0.24 s.
- **What mattered in branch and bound.** The root's strong-branching LPs, solved in batches on the idle threads
  while the first worker is alone in the search (cfl_large spends most of its time there), and one LP solver per
  worker, which keeps its factorization when it takes the child of the node it has just finished.
- Runs with more than one thread differ from one to the next; these are single runs.

## MILP proof strength

Measured 2026-11-10, after the parallel tree search. The MIPLIB runs above showed where the MILP engine lost to
HiGHS: on the models HiGHS proved and one thread didn't, the root bound was far weaker (beasleyC3: 444 against
HiGHS's 736, with the optimum at 754). Three causes were in the cut loop itself: it stopped after one round without
progress, at 20 rounds at most, and it dropped any cut with more nonzeros than a fifth of the columns plus ten. What
changed, in `src/cuts/` and `src/milp.rs`:

- **The cut loop** runs until five rounds in a row make no progress (at most 50), where progress is a gain of
  0.1% of what the cuts have gained so far. The rounds after the last one that made progress are undone, and at
  the end every cut that doesn't help define the LP's solution (its slack is basic) is removed, so the tree's LPs
  stay small. Cuts may have 100 more nonzeros than before.
- **Lifted cover inequalities** (a lifted knapsack cover for pure binary rows, a lifted mixed-binary cover, which
  is the flow cover form, for rows with continuous columns) compete with c-MIR on each aggregated row; the more
  efficacious cut is kept.
- **Reduced-cost fixing** at every node: an integer column at a bound with reduced cost d can move at most
  (cutoff - bound) / d from it. The changes are propagated.
- **Root restarts**: when the root, after its heuristics and fixing, has fixed a fifth of the integer columns that
  were free, the search starts over with new cut rounds from the tighter bounds, keeping the incumbent and the
  pseudocosts.
- **Plunging**: in best-bound search, a worker goes on with the preferred child of the node it has just finished
  while that child's bound is within a quarter of the gap between the best bound and the incumbent.

Shifted geometric mean and total, in seconds; MIPLIB with a 30 s limit:

| | Before, 1 thread | After, 1 thread | Before, 10 threads | After, 10 threads | HiGHS |
|---|---:|---:|---:|---:|---:|
| MIPLIB (58): proven | 22 | **28** | 34 | **38** | 33 |
| MIPLIB: mean; total | 12.531; 1242.3 | **11.066**; 1129.9 | 8.138; 916.0 | **6.584**; 789.5 | 9.603; 985.5 |
| MILP (7 generated): mean; total | 2.643; 28.9 | 2.699; 38.5 | 0.779; 6.2 | 0.918; 8.5 | 3.642; 39.1 |

HiGHS ran at its default relative gap of 1e-4, a looser "optimal" than optimizam's 1e-6; `miplib_benchmark.py` now
sets 1e-6 for it, and its column will be rerun.

| Root bound | Before | After | HiGHS | Optimum |
|---|---:|---:|---:|---:|
| beasleyC3 | 443.6 | 503.7 | 736 | 754 |
| neos-911970 | 23.26 | 52.09 | 53.6 | 54.76 |
| mik-250-20-75-4 | -61389 | -54115 | -53333 | -52301 |
| p200x1188c | 8301 | 9746 | 11640 | 15078 |
| h50x2450 | 24134 | 25308 | 32845 | 32907 |

| Instance | Before, 1 thread | After, 1 thread | Before, 10 threads | After, 10 threads |
|---|---:|---:|---:|---:|
| nw04 (MIPLIB) | not proven | 2.61 | not proven | 2.70 |
| cap6000 | not proven | 3.57 | 22.98 | 0.72 |
| swath1 | not proven | 10.74 | 3.35 | 4.98 |
| pg5_34 | not proven | 16.77 | not proven | 4.11 |
| aflow30a | not proven | 20.20 | 19.70 | 5.54 |
| eil33-2 | not proven | 25.29 | 7.93 | 7.62 |
| mik-250-20-75-4 | not proven | not proven | not proven | 5.78 |
| p200x1188c | not proven | not proven | not proven | 28.25 |
| gap (MILP benchmark) | 1.54 | 0.63 | 0.48 | 0.28 |
| mknap | 8.30 | 5.34 | 1.41 | 0.80 |
| lotsize | 0.31 | 0.73 | 0.16 | 0.38 |
| lotsize_big | 10.71 | 24.24 | 1.62 | 4.23 |

- **No wrong answers** on MIPLIB with one thread or ten, and `compare_random.py` (600 MILPs against HiGHS) agrees
  with one thread and with eight. The nonlinear benchmarks, whose outer approximation uses the MILP engine, are
  unchanged: MINLPLib 307 proven, 21 found, 12 local, 2 failed, in 342 s (338 s before); Hock–Schittkowski 81
  proven. One MINLPLib instance is slower: clay0203m takes 8.0 s instead of 1.4 s. Its outer approximation gives up
  after five MILP rounds without a better bound, and the MILP now returns another of several equal optima.
- **What each part gave**, on MIPLIB with one thread (proven; mean): the committed build 22; 12.53. The cut loop's
  new limits with cover cuts, fixing, and restarts 25; 11.58. With the 0.1% rule 25; 11.49. With plunging 26;
  11.44. With basic cuts removed at the end 28; 11.11. With restarts at a fifth instead of a twentieth of the
  columns 28; 11.07. (The last four ran two at a time; the first two alone.) In a trial of six configurations run
  side by side, reduced-cost fixing was what proved cap6000 and swath1, and restarts nw04, which fixes 77,664 of
  its 87,482 columns at the root.
- **Lot sizing got slower**, and this is the step's cost. lotsize_big needs 38,766 nodes where it needed 13,091.
  Its root bound is the same to four digits (14,659 against 14,666), so the loss is in the tree. About half came
  with the new cut loop and half with removing the basic cuts: tight cuts that didn't define the root's solution
  were helping below it. Its tree size was noisy before (9,000 to 26,000 nodes between LP kernels that differ only
  in rounding), but this is outside that range. The remedy is to keep removed cuts in a pool and add them back at
  nodes where they are violated, which is not done. The small lotsize model is slower because it does more cut
  rounds (0.73 s against 0.31 s).
- **Dropped, measured**: a 1% progress rule (stops too early: lotsize_big's root bound fell from 14,666 to 14,574,
  p200x1188c's from 9,859 to 8,571); looser limits without undoing the last rounds (the useless cuts stayed in the
  LP: markshare_4_0 did 412,000 nodes in 10 s instead of 601,000); keeping tight basic cuts at the end (aflow30a's
  LP grew from 730 rows to 2,482 and it did 13 nodes in 10 s instead of 1,300; without them it is proven in 20 s);
  100 rounds instead of 50 (no better); restarts at a twentieth of the columns (no more proofs than at a fifth,
  and each restart repeats the root's heuristics: air04 restarted without finding one cut).
- **Still behind HiGHS's root bounds** on fixed-charge network flows (beasleyC3, h50x2450, p200x1188c). SCIP with
  only its c-MIR separator reaches 683 on beasleyC3, so the aggregation is the weaker part: HiGHS also runs tableau
  rows through its c-MIR and cover generator, gets more variable bounds from probing, and presolves again when it
  restarts. Not done from the plan either: conflict analysis, probing, and cuts below the root.
- Solutions at the limit moved both ways on the models that aren't proven. tr12-30 ended 7% to 13% above the
  optimum in the last two runs, where it had been at 1.9%; neos-911970 ended at 0.4% where it had been at 30%.

## CP portfolio

Measured 2026-11-09, after the LP engine work below. With `--threads=N` the CP engine runs N searches side by side
that share the best solution and their short learned clauses (`src/cp/portfolio.rs`). Ten threads on the M4 Max
(its performance cores), against one thread of the same binary and against the CP-SAT reference of 2026-11-08
(OR-Tools 9.15 with its default parallel portfolio). One thread's search statistics are identical to before.

| | CP-SAT | 1 thread | 10 threads |
|---|---:|---:|---:|
| CP benchmark (19 instances, 60 s): shifted geometric mean | 1.540 | 1.102 | **0.811** |
| CP benchmark: total | 92.2 | 117.8 | 81.1 |
| Development set (15 JSPLIB job shops, 12 PSPLIB j30; 20 s): shifted geometric mean | 1.172 | 2.151 | **1.268** |
| Development set: proven | 27 | 23 | 26 |

| Instance | CP-SAT | 1 thread | 10 threads |
|---|---:|---:|---:|
| jobshop_ft10 | 2.15 | 41.98 | 9.51 (five runs: 6.7 to 13.2, median 9.3) |
| rcpsp_j30_13_1 | 2.78 | 5.05 | 1.56 |
| jobshop_la16 | 0.55 | 0.83 | 0.65 |
| coloring_myciel5 | 3.13 | 0.41 | 0.25 |
| queens_12_all (enumeration: one thread) | 7.65 | 7.94 | 8.03 |
| tournament_12 (60 s; best objective) | 1120 | 1180 | 1180 |

Every answer agrees with the known optima. Runs with more than one thread differ from one to the next. The
development set's unproven instance is orb01 (its optimum, 1059, is found). tournament_12 doesn't get better with
threads: the LNS neighborhoods stall at 1180 to 1200 whoever runs them. CP-SAT is still about four times faster on
ft10's proof.

## LP engine work

Measured 2026-11-09 on the same machine, at commit 0e8b9da ("Before") and after each of five steps on the simplex
method and LP presolve, each followed by a full sequential run of the five benchmarks. The reference columns are the
runs of 2026-11-08 above (the Netlib benchmark reruns nothing: it keeps HiGHS's times). The CP benchmark doesn't use
the LP engine and stayed at 1.10 throughout.

1. **Factorization.** Forrest–Tomlin updates instead of product-form etas; L and U each kept by row and by column,
   so every solve is a scatter loop that skips zeros; the factorization built by a triangular pass over singletons and
   a Markowitz kernel with count-linked lists (HiGHS's `HFactor`).
2. **Sparse vectors.** Work vectors that list their nonzeros; hyper-sparse triangular solves; PRICE and the primal
   and weight updates over the nonzeros only; the leaving row chosen from a list of infeasible rows.
3. **A persistent solver.** One `Simplex` per search keeps its basis, factorization, and work space: a child node
   solved after its parent, a dive step, and a strong-branching probe (on a copy) factorize nothing. An updated
   factorization is kept at a rebuild when the recomputed values satisfy their equations.
4. **Cleanup.** Primal simplex iterations remove the dual infeasibilities left when cost perturbation is taken off;
   infeasibility is only declared from a new factorization.
5. **LP presolve.** Columns substituted out through equality rows (doubleton equations, implied-free columns), with a
   postsolve stack; no implied bounds in LPs.

Shifted geometric mean (shift 1 s), in seconds, and for MINLPLib the instances proven:

| Benchmark | Reference | Before | 1 | 2 | 3 | 4 | 5 | 5 / Before |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| MILP (7) | HiGHS 3.642 | 3.757 | 3.558 | 2.971 | 2.662 | 2.641 | **2.650** | 0.71 |
| LP, Netlib (91) | HiGHS 0.079 | 0.140 | 0.107 | 0.101 | 0.101 | 0.099 | **0.080** | 0.57 |
| NLP, Hock–Schittkowski (103) | IPOPT 0.135 (local) | 0.356 | 0.333 | 0.315 | 0.302 | 0.300 | **0.300** | 0.84 |
| MINLP, MINLPLib (342) | SCIP 0.533 | 0.409 | 0.406 | 0.386 | 0.376 | 0.371 | **0.371** | 0.91 |
| MINLPLib proven | SCIP 310 | 304 | 304 | 304 | 307 | 308 | 307 | |

Total wall time, in seconds:

| Benchmark | Reference | Before | 1 | 2 | 3 | 4 | 5 |
|---|---:|---:|---:|---:|---:|---:|---:|
| MILP | 39.1 | 42.4 | 54.3 | 32.6 | 29.2 | 28.9 | 28.9 |
| LP, Netlib | 11.0 | 27.7 | 17.7 | 16.5 | 16.5 | 16.2 | 11.5 |
| NLP | 14.1 | 88.1 | 77.5 | 69.6 | 66.1 | 66.0 | 65.9 |
| MINLP | 436.7 | 375.0 | 480.6 | 348.0 | 339.5 | 332.7 | 332.7 |

Notes:

- **No wrong answers at any step**: all 91 Netlib optima match HiGHS, the MILP optima are unchanged, and no MINLPLib
  verdict contradicts a known optimum. After step 5, `tools/compare_random.py` (600 MILPs against HiGHS) and
  `tools/compare_global.py` (100 nonconvex problems against SCIP) report no disagreement.
- **Step 1's totals include two artifacts.** MINLPLib's `least` ran for 128 s: the changed LP rounding sent a local
  solve into the interior point method's line search at a feasible point, where its minimum step is zero and it
  halved the step forever, ignoring the time limit. That is fixed (a smallest step), in its own commit. And
  lotsize_big took 34 s instead of 16 s because its tree was 26,500 nodes instead of 10,900: the root cuts depend on
  which optimal vertex the LP returns. Across the six binaries its tree ranged from 9,000 to 26,500 nodes, so its
  time says little about the kernel; mknap, whose tree is the same 163,000 nodes in every version, went from 7.6 s
  to 6.5 s of node LP time.
- **Netlib is at parity with HiGHS** (0.080 against 0.079), counting optimizam's process start, JSON parsing, and
  presolve, and HiGHS's `run()` alone. The largest instances:

  | Instance | HiGHS | Before | After |
  |---|---:|---:|---:|
  | dfl001 | 3.54 | 10.11 | 3.30 |
  | pilot87 | 2.75 | 5.92 | 3.58 |
  | maros-r7 | 0.45 | 3.54 | 0.58 |
  | fit2p | 0.66 | 2.61 | 0.70 |
  | d2q06c | 0.50 | 0.97 | 0.44 |
  | pilot | 0.82 | 0.77 | 0.55 |
  | greenbeb | 0.31 | 0.51 | 0.18 |

- **MILP** is ahead of HiGHS on six of the seven instances; lotsize_big (10.7 s against 2.3 s) needs a stronger root
  bound, not a faster LP:

  | Instance | HiGHS | Before | After |
  |---|---:|---:|---:|
  | cfl_large | 2.86 | 5.74 | 2.64 |
  | gap | 3.24 | 1.79 | 1.52 |
  | lotsize | 1.65 | 0.66 | 0.31 |
  | lotsize_big | 2.34 | 16.11 | 10.67 |
  | mknap | 17.51 | 9.51 | 8.23 |
  | setcover | 11.11 | 8.53 | 5.48 |
  | windows | 0.43 | 0.03 | 0.03 |

- **MINLPLib**: proofs gained and lost along the way are instances at the edge of the 10 s limit or of a tolerance
  (ex14_1_1, himmel16, jit1, kall_congruentcircles_c72, kall_diffcircles_9, cvxnonsep_psig20); the count went from
  304 to 307 or 308. kall_diffcircles_10 no longer finds a feasible point in 10 s (a different search path; the
  root box is the same).
- What was tried and dropped is in CLAUDE.md: other update limits, the primal simplex for bound tightening, one
  scaled LP per relaxation, slack substitution, substitutions in MILPs.

## Caveats

- **One pass per configuration.** Single-instance timings vary 5–10% between runs (CLAUDE.md), so differences on
  individual instances smaller than that are not meaningful. The benchmark-level ratios are more reliable, and the
  noise estimate above (0–1%) applies to those ratios.
- **No measurement before LTO.** These runs don't show what LTO itself bought over the old default release profile.
  Measuring that would need a build without `[profile.release]`.
- **PGO was dropped after this run.** The 1–3% gain didn't justify a second build pipeline and profiles that go
  stale, so the build script was removed from the repository (see CLAUDE.md).
- The raw logs are not committed. To reproduce the reference and LTO columns, run each benchmark with its reference flag
  and `--bin target/release/optimizam`.
