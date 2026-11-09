# Benchmark results: optimizam (LTO, PGO) vs. reference solvers

Measured 2026-11-08 on an Apple M4 Max (14 cores, 36 GB), macOS, rustc 1.101.0-nightly, commit 9440561 plus the
release-profile change. Every run was sequential; nothing else ran during the timings.

The CP engine was then reworked, and the CP benchmark rerun after each step against the same CP-SAT reference: see
[CP engine work](#cp-engine-work) below. Those changes are in the CP engine (which runs `satisfy` and `"all"`
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
