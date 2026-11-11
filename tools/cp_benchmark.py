#!/usr/bin/env python3
"""Benchmark optimizam's CP engine on standard constraint programming problems.

Most instances are published benchmarks, downloaded on first use into
target/cp_benchmark/ and converted to the problem format (format 1.1):

  jobshop    JSPLIB (github.com/tamy0612/JSPLIB): ft06, la01, la05, la16,
             ft10 (Fisher-Thompson; Lawrence). Intervals, no_overlap per
             machine, makespan. Published optima.
  rcpsp      MiniZinc benchmarks (github.com/MiniZinc/minizinc-benchmarks,
             MIT license): PSPLIB j30 instances from four parameter groups.
             Intervals, cumulative over 4 resources, precedences.
  coloring   DIMACS graph coloring instances (mat.tepper.cmu.edu/COLOR):
             myciel4, myciel5, queen6_6, queen7_7. all_different per edge,
             fewest colors. Known chromatic numbers.
  golomb     Golomb rulers with 7 and 8 marks (CSPLib prob006, generated):
             all_different over pairwise differences. Known optima.
  sudoku     Arto Inkala's published "hardest" puzzle, all solutions.
  queens     N-queens with one integer per row: 100 queens (one solution)
             and 12 queens (all 14,200 solutions).
  tournament A double round robin of 12 teams in 2 stadiums (generated):
             10-minute games, 40 minutes of rest between a team's games,
             team 1 vs 2 and team 3 vs 4 opening at time 0. Intervals,
             no_overlap per team, cumulative over the stadiums, max for the
             last start. Optimum unknown; at least 1050 (each team plays 22
             games at least 50 minutes apart).

Usage (from the repository root, after `cargo build --release`):
    python3 tools/cp_benchmark.py [--time SECONDS] [--cpsat] [--bin PATH]
        [--dir DIR] [names...]

Prints, per instance, the wall time, status, objective (or number of
solutions), and the known answer where there is one, flagging results that
contradict it. With --cpsat, also runs the OR-Tools CP-SAT reference
(tools/cpsat.sh, which needs the project's .venv) and flags disagreements
between proven answers. Exits with status 1 on any contradiction. Uses only
the standard library.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.request

# One thread unless OPTIMIZAM_THREADS says otherwise (optimizam's own default
# is one per core): timings and search paths then repeat from run to run.
os.environ.setdefault("OPTIMIZAM_THREADS", "1")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
JSPLIB = "https://raw.githubusercontent.com/tamy0612/JSPLIB/master/instances/{}"
J30 = "https://raw.githubusercontent.com/MiniZinc/minizinc-benchmarks/master/rcpsp/data_psplib/j30/{}.dzn"
DIMACS = "https://mat.tepper.cmu.edu/COLOR/instances/{}.col"


def download(url, path):
    if not os.path.exists(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        req = urllib.request.Request(url, headers={"User-Agent": "optimizam-benchmark"})
        with urllib.request.urlopen(req, timeout=60) as r, open(path + ".part", "wb") as f:
            f.write(r.read())
        os.replace(path + ".part", path)
    with open(path) as f:
        return f.read()


# ------------------------------------------------------------------ models


def jobshop(text):
    """JSPLIB format: 'jobs machines', then per job (machine, duration) pairs."""
    rows = [l.split() for l in text.splitlines() if l.strip() and not l.startswith("#")]
    n_jobs, n_machines = int(rows[0][0]), int(rows[0][1])
    machine, length = {}, {}
    for j, r in enumerate(rows[1:1 + n_jobs], 1):
        nums = list(map(int, r))
        machine[str(j)] = {str(k + 1): nums[2 * k] + 1 for k in range(n_machines)}
        length[str(j)] = {str(k + 1): nums[2 * k + 1] for k in range(n_machines)}
    horizon = sum(sum(v.values()) for v in length.values())
    return {
        "version": "1.1",
        "sets": {"JOBS": {"from": 1, "to": n_jobs}, "STEPS": {"from": 1, "to": n_machines},
                 "MACHINES": {"from": 1, "to": n_machines}},
        "parameters": {"machine": {"index": ["JOBS", "STEPS"], "values": machine},
                       "length": {"index": ["JOBS", "STEPS"], "values": length}},
        "variables": [{"name": "begin", "type": "integer", "index": ["JOBS", "STEPS"], "lower": 0, "upper": horizon},
                      {"name": "makespan", "type": "integer", "lower": 0, "upper": horizon}],
        "intervals": [{"name": "op", "index": ["JOBS", "STEPS"], "start": "begin", "duration": "length"}],
        "constraints": [
            {"name": "order", "forall": "j in JOBS, s in STEPS: s > 1",
             "expr": "begin[j,s] >= begin[j,s-1] + length[j,s-1]"},
            {"name": "machines", "forall": "m in MACHINES",
             "no_overlap": "[op[j,s] | j in JOBS, s in STEPS: machine[j,s] == m]"},
            {"name": "finish", "forall": "j in JOBS, s in STEPS: s == %d" % n_machines,
             "expr": "makespan >= begin[j,s] + length[j,s]"}],
        "solve": {"method": "minimize", "objective": "makespan"},
    }


def rcpsp(text):
    """MiniZinc RCPSP data: n_res, rc, n_tasks, d, rr (resources x tasks), suc."""
    def ints(name):
        m = re.search(name + r"\s*=\s*\[(.*?)\];", text, re.S)
        return [int(v) for v in re.findall(r"-?\d+", m.group(1))]
    n_res = int(re.search(r"n_res\s*=\s*(\d+)", text).group(1))
    n_tasks = int(re.search(r"n_tasks\s*=\s*(\d+)", text).group(1))
    rc, d = ints("rc"), ints("d")
    rr = ints("rr")
    suc_text = re.search(r"suc\s*=\s*\[(.*?)\];", text, re.S).group(1)
    sucs = [[int(v) for v in re.findall(r"\d+", s)] for s in re.findall(r"\{(.*?)\}", suc_text)]
    assert len(rc) == n_res and len(d) == n_tasks and len(rr) == n_res * n_tasks and len(sucs) == n_tasks
    horizon = sum(d)
    tasks = range(1, n_tasks + 1)
    return {
        "version": "1.1",
        "sets": {"TASKS": {"from": 1, "to": n_tasks}, "RES": {"from": 1, "to": n_res}},
        "parameters": {
            "length": {"index": ["TASKS"], "values": {str(t): d[t - 1] for t in tasks}},
            "capacity": {"index": ["RES"], "values": {str(r + 1): rc[r] for r in range(n_res)}},
            "need": {"index": ["RES", "TASKS"],
                     "values": {str(r + 1): {str(t): rr[r * n_tasks + t - 1] for t in tasks} for r in range(n_res)}},
            "after": {"index": ["TASKS", "TASKS"], "default": 0,
                      "values": {str(i): {str(j): 1 for j in sucs[i - 1]} for i in tasks if sucs[i - 1]}}},
        "variables": [{"name": "start", "type": "integer", "index": ["TASKS"], "lower": 0, "upper": horizon},
                      {"name": "makespan", "type": "integer", "lower": 0, "upper": horizon}],
        "intervals": [{"name": "work", "index": ["TASKS"], "start": "start", "duration": "length"}],
        "constraints": [
            {"name": "precedence", "forall": "i in TASKS, j in TASKS: after[i,j] == 1",
             "expr": "start[j] >= start[i] + length[i]"},
            {"name": "resources", "forall": "r in RES",
             "cumulative": {"intervals": "[work[t] | t in TASKS]", "demands": "[need[r,t] | t in TASKS]",
                            "capacity": "capacity[r]"}},
            {"name": "finish", "forall": "t in TASKS", "expr": "makespan >= start[t] + length[t]"}],
        "solve": {"method": "minimize", "objective": "makespan"},
    }


def coloring(text, colors):
    """DIMACS .col: 'p edge n m' and 'e u v' lines; colors is an upper bound."""
    n = int(re.search(r"^p\s+\w+\s+(\d+)", text, re.M).group(1))
    edges = sorted({tuple(sorted(map(int, l.split()[1:3]))) for l in text.splitlines() if l.startswith("e ")})
    return {
        "version": "1.1",
        "sets": {"V": {"from": 1, "to": n}, "E": {"from": 1, "to": len(edges)}, "COLORS": {"from": 1, "to": colors}},
        "parameters": {"head": {"index": ["E"], "values": {str(k + 1): u for k, (u, _) in enumerate(edges)}},
                       "tail": {"index": ["E"], "values": {str(k + 1): v for k, (_, v) in enumerate(edges)}}},
        "variables": [{"name": "color", "type": "integer", "index": ["V"], "domain": "COLORS"},
                      {"name": "used", "type": "integer", "lower": 1, "upper": colors}],
        "constraints": [
            {"name": "edges", "forall": "e in E", "all_different": "[color[head[e]], color[tail[e]]]"},
            {"name": "count", "forall": "v in V", "expr": "used >= color[v]"},
            {"name": "symmetry", "expr": "color[1] == 1"}],
        "solve": {"method": "minimize", "objective": "used"},
    }


def golomb(marks, bound):
    return {
        "version": "1.1",
        "sets": {"M": {"from": 1, "to": marks}},
        "variables": [{"name": "mark", "type": "integer", "index": ["M"], "lower": 0, "upper": bound}],
        "constraints": [
            {"name": "origin", "expr": "mark[1] == 0"},
            {"name": "increasing", "forall": "i in M: i > 1", "expr": "mark[i] >= mark[i-1] + 1"},
            {"name": "distinct_differences", "all_different": "[mark[j] - mark[i] | i in M, j in M: i < j]"},
            {"name": "mirror", "expr": "mark[2] - mark[1] <= mark[%d] - mark[%d] - 1" % (marks, marks - 1)}],
        "solve": {"method": "minimize", "objective": "mark[%d]" % marks},
    }


def sudoku(puzzle):
    given = {str(r + 1): {str(c + 1): int(puzzle[9 * r + c]) for c in range(9) if puzzle[9 * r + c] != "0"}
             for r in range(9)}
    box = {str(r): {str(c): 3 * ((r - 1) // 3) + (c - 1) // 3 + 1 for c in range(1, 10)} for r in range(1, 10)}
    return {
        "version": "1.1",
        "sets": {"N": {"from": 1, "to": 9}},
        "parameters": {"given": {"index": ["N", "N"], "values": {r: v for r, v in given.items() if v}, "default": 0},
                       "box": {"index": ["N", "N"], "values": box}},
        "variables": [{"name": "cell", "type": "integer", "index": ["N", "N"], "domain": "N"}],
        "constraints": [
            {"name": "clues", "forall": "r in N, c in N: given[r,c] > 0", "expr": "cell[r,c] == given[r,c]"},
            {"name": "rows", "forall": "r in N", "all_different": "[cell[r,c] | c in N]"},
            {"name": "columns", "forall": "c in N", "all_different": "[cell[r,c] | r in N]"},
            {"name": "boxes", "forall": "b in N", "all_different": "[cell[r,c] | r in N, c in N: box[r,c] == b]"}],
        "solve": {"method": "satisfy", "solutions": "all"},
    }


def queens(n, solutions):
    return {
        "version": "1.1",
        "sets": {"N": {"from": 1, "to": n}},
        "variables": [{"name": "col", "type": "integer", "index": ["N"], "domain": "N"}],
        "constraints": [{"all_different": "[col[r] | r in N]"},
                        {"all_different": "[col[r] + r | r in N]"},
                        {"all_different": "[col[r] - r | r in N]"}],
        "solve": {"method": "satisfy", "solutions": solutions},
    }


def tournament(teams, stadiums, game, rest, meetings, horizon, opening):
    """Every pair of teams meets `meetings` times; minimize the last start.

    A game is an interval of length `game` (`stadiums` at a time) and also
    occupies both teams for `game + rest`. The pairs in `opening` play their
    first meeting at time 0 (stadiums are interchangeable, so which pair is
    in which stadium is a labeling).
    """
    names = [f"{a}-{b}" for a in range(1, teams + 1) for b in range(a + 1, teams + 1)]
    involves = {p: {str(t): int(str(t) in p.split("-")) for t in range(1, teams + 1)} for p in names}
    first = {f"{a}-{b}": 1 for a, b in opening}
    return {
        "version": "1.1",
        "sets": {"TEAMS": list(range(1, teams + 1)), "PAIRS": names, "REPS": list(range(1, meetings + 1))},
        "parameters": {"involves": {"index": ["PAIRS", "TEAMS"], "values": involves, "default": 0},
                       "opening": {"index": ["PAIRS"], "values": first, "default": 0}},
        "variables": [{"name": "start", "type": "integer", "index": ["PAIRS", "REPS"], "lower": 0, "upper": horizon},
                      {"name": "last_start", "type": "integer", "lower": 0, "upper": horizon}],
        "intervals": [{"name": "game", "index": ["PAIRS", "REPS"], "start": "start", "duration": game},
                      {"name": "busy", "index": ["PAIRS", "REPS"], "start": "start", "duration": game + rest}],
        "constraints": [
            {"name": "rest", "forall": "t in TEAMS",
             "no_overlap": "[busy[p,r] | p in PAIRS, r in REPS: involves[p,t] == 1]"},
            {"name": "stadiums", "cumulative": {"intervals": "[game[p,r] | p in PAIRS, r in REPS]",
                                                "demands": "[1 | p in PAIRS, r in REPS]",
                                                "capacity": str(stadiums)}},
            {"name": "meetings_in_order", "forall": "p in PAIRS, r in REPS: r > 1",
             "expr": "start[p,r] >= start[p,r-1] + 1"},
            {"name": "opening", "forall": "p in PAIRS: opening[p] == 1", "expr": "start[p,1] == 0"},
            {"name": "last", "expr": "last_start == max(p in PAIRS, r in REPS) start[p,r]"}],
        "solve": {"method": "minimize", "objective": "last_start"},
    }


INKALA = "800000000003600000070090200050007000000045700000100030001000068008500010090000400"

# name -> (builder taking the download directory, known answer). A known
# answer is ("objective", value), ("solutions", count), or None.
INSTANCES = {}


def add(name, build, known):
    INSTANCES[name] = (build, known)


for js, opt in (("ft06", 55), ("la01", 666), ("la05", 593), ("la16", 945), ("ft10", 930)):
    add(f"jobshop_{js}", lambda d, js=js: jobshop(download(JSPLIB.format(js), f"{d}/jsplib/{js}")), ("objective", opt))
for inst in ("J30_1_1", "J30_13_1", "J30_29_1", "J30_45_1"):
    add(f"rcpsp_{inst.lower()}", lambda d, i=inst: rcpsp(download(J30.format(i), f"{d}/j30/{i}.dzn")), None)
for g, chi, bound in (("myciel4", 5, 8), ("myciel5", 6, 8), ("queen6_6", 7, 9), ("queen7_7", 7, 9)):
    add(f"coloring_{g}", lambda d, g=g, b=bound: coloring(download(DIMACS.format(g), f"{d}/dimacs/{g}.col"), b),
        ("objective", chi))
add("golomb_7", lambda d: golomb(7, 40), ("objective", 25))
add("golomb_8", lambda d: golomb(8, 50), ("objective", 34))
add("sudoku_inkala", lambda d: sudoku(INKALA), ("solutions", 1))
add("queens_100", lambda d: queens(100, "one"), None)
add("queens_12_all", lambda d: queens(12, "all"), ("solutions", 14200))
add("tournament_12", lambda d: tournament(12, 2, 10, 40, 2, 1500, [(1, 2), (3, 4)]), None)


# ------------------------------------------------------------------ running


def run(command, path, time_limit):
    start = time.monotonic()
    r = subprocess.run([command, f"--time={time_limit}", path, "-"], capture_output=True, text=True)
    seconds = time.monotonic() - start
    if r.returncode != 0:
        return {"seconds": seconds, "status": "error", "message": (r.stderr.strip().splitlines() or ["?"])[-1]}
    result = json.loads(r.stdout)
    sols = result["solutions"]
    return {"seconds": seconds, "status": result["status"], "count": len(sols),
            "objective": sols[0].get("objective") if sols else None}


def answer(res, want_all):
    """The comparable answer: the objective, or the solution count with "all"."""
    if res["status"] == "error":
        return None
    return res["count"] if want_all else res["objective"]


def fmt(v):
    return "" if v is None else (f"{v:g}" if isinstance(v, float) else str(v))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("names", nargs="*", help="instances (default: all); prefixes such as jobshop select a class")
    ap.add_argument("--time", type=float, default=60, help="time limit per solve in seconds (default 60)")
    ap.add_argument("--cpsat", action="store_true", help="also run the OR-Tools CP-SAT reference")
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"), help="optimizam binary")
    ap.add_argument("--dir", default=os.path.join(ROOT, "target", "cp_benchmark"), help="cache directory")
    args = ap.parse_args()

    names = [n for n in INSTANCES if not args.names or any(n == a or n.startswith(a + "_") for a in args.names)]
    if not names:
        sys.exit(f"no such instances; known: {', '.join(INSTANCES)}")
    if not os.path.exists(args.bin):
        sys.exit(f"{args.bin} not found; run `cargo build --release` or pass --bin")

    header = f"{'instance':22} {'seconds':>8} {'status':19} {'answer':>8} {'known':>8}"
    if args.cpsat:
        header += f" | {'CP-SAT s':>8} {'CP-SAT status':19} {'answer':>8}"
    print(header)
    problems, total = 0, 0.0
    for name in names:
        build, known = INSTANCES[name]
        path = os.path.join(args.dir, "instances", f"{name}.json")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        model = build(args.dir)
        with open(path, "w") as f:
            json.dump(model, f)
        want_all = model["solve"].get("solutions") == "all"
        res = run(args.bin, path, args.time)
        total += res["seconds"]
        ours = answer(res, want_all)
        notes = []
        if res["status"] == "error":
            notes.append(f"error: {res['message']}")
        if known and res["status"] == "complete" and ours != known[1]:
            notes.append("CONTRADICTS KNOWN ANSWER")
        line = f"{name:22} {res['seconds']:8.2f} {res['status']:19} {fmt(ours):>8} {fmt(known and known[1]):>8}"
        if args.cpsat:
            ref = run(os.path.join(ROOT, "tools", "cpsat.sh"), path, args.time)
            theirs = answer(ref, want_all)
            line += f" | {ref['seconds']:8.2f} {ref['status']:19} {fmt(theirs):>8}"
            if res["status"] == ref["status"] == "complete" and ours != theirs:
                notes.append("DISAGREES WITH CP-SAT")
        problems += any("KNOWN" in n or "DISAGREES" in n for n in notes)
        print(line + ("  " + "; ".join(notes) if notes else ""), flush=True)
    print(f"total {total:.2f}s")
    if problems:
        print(f"{problems} instances with contradicted answers")
        sys.exit(1)


if __name__ == "__main__":
    main()
