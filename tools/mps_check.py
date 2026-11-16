#!/usr/bin/env python3
"""Check `optimizam --write-mps` against HiGHS.

For each problem file (default: every example), the release binary writes the
model as MPS, HiGHS solves that file, and the answer is compared with
optimizam's own on the problem file:

- both infeasible, both unbounded, or both with a solution, and for a minimize
  or maximize problem the same objective (1e-6, relative);
- and HiGHS's solution is a solution of the problem file: its values for the
  integer and binary variables, read by the columns' names, are added to the
  problem as constraints, and optimizam must then find the same objective.
  (That catches a file that says less than the model; the objective alone
  catches one that says more. Continuous variables are left free, and so are
  symbolic ones, whose values are elements and not numbers.)

Nonlinear problems, which `--write-mps` refuses, are skipped. Disagreements
are listed, and the exit status is 1 if there is any.

    cargo build --release && .venv/bin/python tools/mps_check.py [--time T] [FILES...]
"""
import argparse
import glob
import itertools
import json
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("OPTIMIZAM_BIN", os.path.join(ROOT, "target", "release", "optimizam"))
ENV = dict(os.environ, OPTIMIZAM_THREADS=os.environ.get("OPTIMIZAM_THREADS", "1"))


def optimizam(args, time):
    return subprocess.run([BIN, f"--time={time}", "--quiet"] + args, capture_output=True, text=True, env=ENV)


def solve(path, time):
    run = optimizam([path, "-"], time)
    if run.returncode != 0:
        raise RuntimeError(run.stderr.strip())
    result = json.loads(run.stdout)
    solutions = result["solutions"]
    return result["status"], (solutions[0].get("objective") if solutions else None)


def highs(mps, time):
    import highspy

    h = highspy.Highs()
    h.setOptionValue("output_flag", False)
    h.setOptionValue("time_limit", float(time))
    h.setOptionValue("mip_rel_gap", 1e-9)
    h.setOptionValue("mip_abs_gap", 1e-9)
    if h.readModel(mps) != highspy.HighsStatus.kOk:
        return "unreadable", None, {}
    h.run()
    status = h.modelStatusToString(h.getModelStatus())
    if status != "Optimal":
        return status, None, {}
    lp = h.getLp()
    values = dict(zip(lp.col_names_, h.getSolution().col_value))
    return status, h.getInfo().objective_function_value, values


def elements(problem, entry):
    """The elements of the set an index entry ("I" or "i in I") names."""
    s = problem.get("sets", {})[entry.split(" in ")[-1].strip()]
    return list(range(s["from"], s["to"] + 1)) if isinstance(s, dict) else s


def fixed(problem, values):
    """The problem with its integer variables held at HiGHS's values."""
    fixed = json.loads(json.dumps(problem))
    count = 0
    for var in problem["variables"]:
        if var.get("type", "continuous") == "continuous":
            continue
        domain = var.get("domain")
        if isinstance(domain, str) and any(isinstance(e, str) for e in elements(problem, domain)):
            continue  # symbolic: the column holds an element's place, not a number
        for key in itertools.product(*(elements(problem, e) for e in var.get("index", []))):
            column = var["name"] + ("[" + ",".join(str(k) for k in key) + "]" if key else "")
            column = re.sub(r"\s", "_", column)
            if column not in values:
                raise RuntimeError(f"the MPS file has no column {column}")
            quoted = ["'" + k.replace("'", "''") + "'" if isinstance(k, str) else str(k) for k in key]
            name = var["name"] + ("[" + ", ".join(quoted) + "]" if key else "")
            fixed["constraints"].append({"expr": f"{name} == {round(values[column])}"})
            count += 1
    fixed["solve"]["solutions"] = "one"
    return fixed, count


def close(a, b):
    return abs(a - b) <= 1e-6 * max(1.0, abs(a), abs(b))


def check(path, time, tmp):
    mps = os.path.join(tmp, "model.mps")
    written = optimizam(["--write-mps", mps, path], time)
    if written.returncode != 0:
        message = written.stderr.strip()
        return ("skipped" if "nonlinear" in message else "FAILED"), message
    with open(path) as f:
        problem = json.load(f)
    status, objective = solve(path, time)
    theirs, their_objective, values = highs(mps, time)
    if status == "infeasible" or theirs == "Infeasible":
        same = status == "infeasible" and theirs == "Infeasible"
        return ("ok" if same else "DIFFERENT"), f"optimizam {status}, HiGHS {theirs}"
    if status == "unbounded" or "nbounded" in theirs:
        same = status == "unbounded" and "nbounded" in theirs
        return ("ok" if same else "DIFFERENT"), f"optimizam {status}, HiGHS {theirs}"
    if status in ("timeout", "not_proven_optimal") or theirs != "Optimal":
        return "undecided", f"optimizam {status}, HiGHS {theirs}"
    optimizing = problem["solve"]["method"] != "satisfy"
    if optimizing and not close(objective, their_objective):
        return "DIFFERENT", f"objective {objective}, HiGHS {their_objective}"
    again, count = fixed(problem, values)
    held = os.path.join(tmp, "fixed.json")
    with open(held, "w") as f:
        json.dump(again, f)
    try:
        status, objective = solve(held, time)
    except RuntimeError as e:
        return "FAILED", f"with HiGHS's values: {e}"
    if status == "infeasible":
        return "DIFFERENT", "HiGHS's solution isn't one of the problem's"
    if optimizing and (objective is None or not close(objective, their_objective)):
        return "DIFFERENT", f"HiGHS's solution has objective {objective} in the problem, {their_objective} in the file"
    what = f"objective {their_objective:.9g}" if optimizing else "a solution"
    return "ok", f"{what}; {count} values held"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("files", nargs="*")
    parser.add_argument("--time", type=float, default=30.0)
    args = parser.parse_args()
    files = args.files or sorted(
        f for f in glob.glob(os.path.join(ROOT, "examples", "*.json")) if not f.endswith(".result.json")
    )
    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        for path in files:
            try:
                verdict, note = check(path, args.time, tmp)
            except Exception as e:  # a tool failure is a finding too
                verdict, note = "FAILED", str(e)
            bad += verdict in ("DIFFERENT", "FAILED")
            print(f"{os.path.basename(path):34s} {verdict:10s} {note}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
