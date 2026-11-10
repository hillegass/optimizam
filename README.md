# optimizam

optimizam is an optimization and constraint solver in pure Rust, usable as a
library and as a command. You describe a problem (variables, constraints,
and an objective) in Rust, or in a JSON file with index sets and data, and it
finds the best solution, every solution, or proves there is none.

It handles:

- **Linear programs** and **mixed-integer programs**: production planning,
  facility location, lot sizing, knapsacks (dual simplex with presolve,
  cutting planes, and heuristics).
- **Constraint programming**: scheduling, timetabling, assignment, puzzles
  (a lazy clause generation engine, with `all_different`, `element`, `table`,
  `min`/`max`/`abs`, conditional constraints, and intervals with
  `no_overlap` and `cumulative`).
- **Nonlinear programs**, with continuous and integer variables:
  engineering design, portfolios, curve fitting, chemical equilibrium,
  sizing decisions (spatial branch and bound for global optima, with an
  interior point method after IPOPT for local solves and exact derivatives of
  expressions built from `+ - * /`, powers, `ln`, `exp`, `sqrt`, and
  trigonometric functions). It proves global optima when it can, and says
  when it couldn't.
- **One optimal solution, or all of them**, and feasibility problems.

It has no C or C++ dependencies, and needs nothing beyond `serde`,
`serde_json`, and (for the command) `clap` and `ctrlc`.

It also comes with a skill for [Claude Code](https://claude.com/claude-code),
so you can describe a problem in plain language and have Claude formulate it,
solve it with optimizam, and explain the answer.

## Use it from Rust

```sh
cargo add optimizam
```

(The `cli` feature, on by default, builds the command. A library can leave it,
clap, and ctrlc out with `cargo add optimizam --no-default-features`.)

```rust
use optimizam::{Model, Options, constraint, var};

let mut m = Model::new();
let chairs = m.add_var(var().integer().bounds(0, 40).name("chairs"));
let tables = m.add_var(var().integer().bounds(0, 30).name("tables"));
m.add(constraint!(3 * chairs + 5 * tables <= 150));
m.add(constraint!(chairs + 2 * tables <= 50));
m.maximize(45 * chairs + 80 * tables);

let result = m.solve(&Options::default())?;
let best = result.best().unwrap();
println!("{}: {} chairs, {} tables", result.status(), best[chairs], best[tables]);
```

Nonlinear expressions (`x.ln()`, `(x * y).sqrt()`, `x.powf(1.5)`) and the
constraint programming kinds (`m.all_different(...)`, `m.no_overlap(...)`,
`constraint!(...).only_if(b)`) work the same way. A `Model` can be saved as a
JSON problem file with `m.to_json()`, and problem files can be solved from
Rust with `Problem::from_file`, or read into a `Model` to change before
solving with `Model::from_file`. The API documentation is on
[docs.rs](https://docs.rs/optimizam), and
[`examples/`](examples/) has runnable programs:

```sh
cargo run --example knapsack
cargo run --example job_shop
cargo run --example curve_fit
cargo run --example solve_file -- examples/sudoku.json
```

## Use it from the command line

You need Rust 1.88 or newer. If you don't have it, install it with
[rustup](https://rustup.rs):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then install the command:

```sh
cargo install optimizam
```

or build it from the source:

```sh
git clone https://github.com/hillegass/optimizam.git
cd optimizam
cargo install --path .
```

Either way puts an optimized binary in `~/.cargo/bin`, which rustup adds to
your `PATH`. Check it:

```sh
optimizam --version
optimizam --help
```

Then:

```sh
optimizam problem.json result.json
optimizam --time=30 problem.json -          # print the result, stop after 30 seconds
cat problem.json | optimizam - -            # read stdin, write stdout
optimizam --engine=milp problem.json -       # choose the engine (auto, milp, cp, nlp)
optimizam --incumbents=best.json --progress problem.json result.json
                                            # long runs: best so far in best.json, progress on stderr
```

A small example (a knapsack):

```json
{
  "version": "1.0",
  "sets": { "ITEMS": ["gold", "silver", "gem"] },
  "parameters": {
    "weight": { "index": ["ITEMS"], "values": { "gold": 4, "silver": 3, "gem": 2 } },
    "value": { "index": ["ITEMS"], "values": { "gold": 10, "silver": 7, "gem": 5 } }
  },
  "variables": [{ "name": "take", "type": "binary", "index": ["ITEMS"] }],
  "constraints": [{ "name": "fits", "expr": "sum(i in ITEMS) weight[i] * take[i] <= 7" }],
  "solve": { "method": "maximize", "objective": "sum(i in ITEMS) value[i] * take[i]" }
}
```

```json
{
  "version": "1.0",
  "status": "complete",
  "solutions": [
    { "objective": 17, "variables": { "take": { "gold": 1, "silver": 1, "gem": 0 } } }
  ]
}
```

The `examples/` directory has worked problems of every kind, each with its
expected result.

## Use it from Claude Code

Install the command first (above). The skill lives in `skills/optimizam/` in
the source repository. Install it for all your projects by linking it into
your Claude Code skills directory:

```sh
mkdir -p ~/.claude/skills
ln -s "$(pwd)/skills/optimizam" ~/.claude/skills/optimizam
```

The link keeps the skill and its reference documents up to date when you pull
new versions of this repository. (To copy it instead, use
`cp -RL skills/optimizam ~/.claude/skills/`; the `-L` copies the documents the
skill links to.) To install it for a single project instead, link or copy it
into that project's `.claude/skills/` directory.

Then start Claude Code and describe your problem. For example:

- "I have 8 tasks and 3 machines; each task takes a different time on each
  machine. Assign them so everything finishes as early as possible."
- "Plan next week's shifts: 5 people, an early and a late shift every day,
  nobody works a late shift followed by an early one."
- "Which of these projects should we fund to maximize value within a
  $2M budget?"
- "Solve this Sudoku and tell me whether the solution is unique."

Claude picks up the skill automatically for problems like these. You can also
invoke it directly with `/optimizam`. It writes a problem file, runs
`optimizam`, checks the status, and explains the result. The problem file is
yours to keep, edit, and rerun.

## Performance

Shifted geometric mean of the time per instance (shift 1 s), in seconds,
against the established open-source solver for each class (details and
per-instance results in [`RESULTS.md`](RESULTS.md)):

| Benchmark | Instances | Reference | optimizam | with 10 threads |
|---|---:|---:|---:|---:|
| MILP (generated, seven classes) | 7 | HiGHS 3.64 | 2.64 | 0.78 |
| LP (Netlib) | 91 | HiGHS 0.079 | 0.080 | |
| CP (job shop, RCPSP, coloring, puzzles) | 19 | OR-Tools CP-SAT 1.54 | 1.10 | 0.81 |
| NLP (Hock–Schittkowski) | 103 | IPOPT 0.135 | 0.301 | 0.119 |
| Global MINLP (MINLPLib, ≤ 30 variables) | 342 | SCIP 0.533 | 0.376 | 0.211 |

The references ran with their default settings (CP-SAT with its parallel
portfolio); optimizam uses one thread unless `--threads` says otherwise.

## Limitations

- It uses one thread unless asked for more (`--threads`, `Options::threads`),
  and an LP's simplex method always does.
- Some MILPs with weak root bounds need large search trees: there is no
  probing, conflict analysis, or cutting below the root. Lot sizing with many
  periods is the slowest class tried.
- On hard scheduling instances, OR-Tools CP-SAT's parallel portfolio is
  faster.
- The global nonlinear search struggles with nonlinear terms of unbounded
  variables. Give nonlinear variables bounds.
- Without a time limit, hard problems can take arbitrarily long.

## Documentation

- [docs.rs/optimizam](https://docs.rs/optimizam): the Rust API.
- [`docs/problem-format.md`](docs/problem-format.md): the problem format and
  expression grammar.
- [`docs/result-format.md`](docs/result-format.md): the result format and
  statuses.
- [`docs/cli.md`](docs/cli.md): the command line, engines, and time limits.
- [`docs/solver-design.md`](docs/solver-design.md): how the solver works.
- [`schema/`](schema/): JSON Schemas for problem and result files.
- [`CHANGELOG.md`](CHANGELOG.md): what changed in each release.

## Development

`cargo test` runs the unit, builder, and regression tests. Contributors should
read [`CLAUDE.md`](CLAUDE.md), which describes the code, the reference solvers
(HiGHS, OR-Tools, IPOPT, and SCIP, used only for testing), and the benchmarks.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in optimizam by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
