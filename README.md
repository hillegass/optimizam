# optimizam

optimizam is a command-line solver for optimization and constraint problems.
You describe a problem in a JSON file (index sets, data, variables,
constraints, and an objective), and it writes the solution as JSON.

It handles:

- **Linear programs** and **mixed-integer programs**: production planning,
  facility location, lot sizing, knapsacks (dual simplex with presolve,
  cutting planes, and heuristics).
- **Constraint programming**: scheduling, timetabling, assignment, puzzles
  (a lazy clause generation engine, with `all_different`, `element`, `table`,
  `min`/`max`/`abs`, conditional constraints, and intervals with
  `no_overlap` and `cumulative`).
- **Nonlinear programs** with continuous variables: engineering design,
  portfolios, curve fitting, chemical equilibrium (an interior point method
  after IPOPT, with exact derivatives of expressions built from `+ - * /`,
  powers, `ln`, `exp`, `sqrt`, and trigonometric functions). It finds local
  optima, and says so.
- **One optimal solution, or all of them**, and feasibility problems.

It comes with a skill for [Claude Code](https://claude.com/claude-code), so
you can describe a problem in plain language and have Claude formulate it,
solve it with optimizam, and explain the answer.

## Install

### 1. The `optimizam` command

You need Rust 1.85 or newer. If you don't have it, install it with
[rustup](https://rustup.rs):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then get the source and build it:

```sh
git clone https://github.com/hillegass/optimizam.git
cd optimizam
cargo install --path .
```

This builds an optimized binary and puts it in `~/.cargo/bin`, which rustup
adds to your `PATH`. Check it:

```sh
optimizam --version
optimizam --help
```

### 2. The Claude Code skill

The skill lives in `skills/optimizam/`. Install it for all your projects by
linking it into your Claude Code skills directory:

```sh
mkdir -p ~/.claude/skills
ln -s "$(pwd)/skills/optimizam" ~/.claude/skills/optimizam
```

The link keeps the skill and its reference documents up to date when you pull
new versions of this repository. (To copy it instead, use
`cp -RL skills/optimizam ~/.claude/skills/`; the `-L` copies the documents the
skill links to.)

To install it for a single project instead, link or copy it into that
project's `.claude/skills/` directory.

### Updating

```sh
git pull
cargo install --path . --force
```

## Use it from Claude Code

Start Claude Code and describe your problem. For example:

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

## Use it directly

```sh
optimizam problem.json result.json
optimizam --time=30 problem.json -          # print the result, stop after 30 seconds
cat problem.json | optimizam - -            # read stdin, write stdout
optimizam --engine=mip problem.json -       # choose the engine (auto, mip, cp)
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

## Documentation

- [`docs/problem-format.md`](docs/problem-format.md): the problem format and
  expression grammar.
- [`docs/result-format.md`](docs/result-format.md): the result format and
  statuses.
- [`docs/cli.md`](docs/cli.md): the command line, engines, and time limits.
- [`docs/solver-design.md`](docs/solver-design.md): how the solver works.
- [`schema/`](schema/): JSON Schemas for problem and result files.

## Development

`cargo test` runs the unit and regression tests. Contributors should read
[`CLAUDE.md`](CLAUDE.md), which describes the code, the reference solvers
(HiGHS and OR-Tools, used only for testing), and the benchmarks.
