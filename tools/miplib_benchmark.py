#!/usr/bin/env python3
"""Benchmark optimizam's MILP solver on instances from MIPLIB 2017.

A fixed selection of the smaller instances of the MIPLIB 2017 collection
(miplib.zib.de) with a known optimal value: classics such as air05, misc07,
and mas76 beside newer ones, most of them solved by a good solver in seconds
to a minute. They are real models, unlike the generated instances of
tools/milp_benchmark.py, so they guard against tuning the solver to those.

The instances (gzipped MPS) and MIPLIB's list of optimal values are
downloaded on demand into target/miplib/ and converted with
tools/mps_to_json.py; nothing is committed. Each result is checked against
the known optimal value.

Usage (from the repository root, after `cargo build --release`):
    python3 tools/miplib_benchmark.py [--time SECONDS] [--threads N] [--highs]
        [--refresh] [--bin PATH] [--dir DIR] [names...]

Verdicts:
  proven   complete, at the known optimum
  found    stopped by the time limit with the known optimum as its solution
  feasible stopped with a solution that isn't optimal (the gap is printed)
  failed   stopped without a solution
  error    optimizam failed, or the MPS converter rejected the file
  WRONG    complete with a different objective, or infeasible: a bug
Exits with status 1 on any WRONG.

With --highs, HiGHS solves each MPS file too, with the same time limit and
the relative gap optimizam proves to (1e-6; HiGHS's own default is 1e-4, a
looser "optimal"), otherwise with its default settings. Its results are
cached in target/miplib/highs.json and need the project's .venv (see
CLAUDE.md). A cached result from another gap is recomputed, so both solvers
always prove to the same gap; --refresh recomputes every cached result. The tool itself uses only the
standard library.

The last line gives the shifted geometric mean of the times (shift 1 s) over
all instances run, unsolved ones counting as the time limit.
"""

import argparse
import gzip
import json
import math
import os
import subprocess
import sys
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
import mps_to_json  # noqa: E402

INSTANCE_URL = "https://miplib.zib.de/WebData/instances/{}.mps.gz"
SOLU_URL = "https://miplib.zib.de/downloads/miplib2017-v26.solu"
VENV_PYTHON = os.path.join(ROOT, ".venv", "bin", "python")
# MIPLIB's optimal values are given to about this accuracy.
REL_TOL = 1e-4
# The relative gap HiGHS is told to prove to: optimizam's MILP search's
# (OPTIMALITY_TOLERANCE in src/milp.rs), so that "proven" means the same for
# both. Each cached result records the gap it was computed with.
HIGHS_GAP = 1e-6
# The gap of results cached before they recorded it: HiGHS's default.
HIGHS_DEFAULT_GAP = 1e-4

INSTANCES = """
22433 23588 aflow30a air03 air04 air05 assign1-5-8 beasleyC3 binkar10_1 blend2 bppc4-08 cap6000
control30-3-2-3 dcmulti eil33-2 enlight_hard exp-1-500-5-5 fhnw-binpack4-48 fiber flugpl gen gen-ip002
gen-ip054 gmu-35-40 gt2 h50x2450 khb05250 markshare_4_0 mas74 mas76 mcsched mik-250-20-75-4 misc07
mod010 mod011 neos-1171737 neos-1445765 neos-3046615-murg neos-911970 neos5 neos8 nexp-150-20-8-5
nu25-pr12 nw04 p0201 p200x1188c pg pg5_34 pk1 prod1 qiu qnet1 ran12x21 ran13x13 rout swath1 timtab1
tr12-30
""".split()

# Run by the project's .venv python: solve an MPS file with HiGHS, print JSON.
HIGHS_SCRIPT = r"""
import json, sys, time, highspy
h = highspy.Highs()
h.silent()
h.setOptionValue("time_limit", float(sys.argv[2]))
h.setOptionValue("mip_rel_gap", float(sys.argv[3]))
start = time.monotonic()
h.readModel(sys.argv[1])
h.run()
info = h.getInfo()
print(json.dumps({
    "status": h.modelStatusToString(h.getModelStatus()),
    "objective": info.objective_function_value,
    "seconds": time.monotonic() - start,
}))
"""


def download(url, path):
    if os.path.exists(path):
        return
    os.makedirs(os.path.dirname(path), exist_ok=True)
    req = urllib.request.Request(url, headers={"User-Agent": "optimizam-benchmark"})
    with urllib.request.urlopen(req, timeout=120) as r, open(path + ".part", "wb") as f:
        f.write(r.read())
    os.replace(path + ".part", path)


def known_optima(path):
    """{name: optimal value} from MIPLIB's .solu file (lines `=opt= name value`)."""
    values = {}
    with open(path) as f:
        for line in f:
            parts = line.split()
            if len(parts) == 3 and parts[0] == "=opt=":
                values[parts[1]] = float(parts[2])
    return values


def run_optimizam(binary, path, time_limit, threads):
    command = [binary, f"--time={time_limit}", f"--threads={threads}", path, "-"]
    start = time.monotonic()
    r = subprocess.run(command, capture_output=True, text=True)
    seconds = time.monotonic() - start
    if r.returncode != 0:
        message = (r.stderr.strip().splitlines() or ["(no message)"])[0]
        return {"seconds": seconds, "status": "error", "objective": None, "message": message}
    result = json.loads(r.stdout)
    objective = result["solutions"][0].get("objective") if result["solutions"] else None
    return {"seconds": seconds, "status": result["status"], "objective": objective}


def run_highs(path, time_limit):
    command = [VENV_PYTHON, "-c", HIGHS_SCRIPT, path, str(time_limit), str(HIGHS_GAP)]
    r = subprocess.run(command, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"HiGHS failed on {path}: {r.stderr.strip()[-500:]}")
    return {**json.loads(r.stdout), "gap": HIGHS_GAP}


def rel(a, b):
    return abs(a - b) / max(1.0, abs(b))


def shifted_geometric_mean(times):
    return math.exp(sum(math.log(t + 1.0) for t in times) / len(times)) - 1.0 if times else float("nan")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("names", nargs="*", help=f"instances to run (default: all {len(INSTANCES)})")
    ap.add_argument("--time", type=float, default=60, help="time limit per solve in seconds (default 60)")
    ap.add_argument("--threads", type=int, default=1, help="optimizam's --threads (default 1; 0 for every core)")
    ap.add_argument("--highs", action="store_true", help="also run HiGHS on the MPS files")
    ap.add_argument("--refresh", action="store_true", help="recompute cached HiGHS results")
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"), help="optimizam binary")
    ap.add_argument("--dir", default=os.path.join(ROOT, "target", "miplib"), help="cache directory")
    args = ap.parse_args()

    unknown = [n for n in args.names if n not in INSTANCES]
    if unknown:
        sys.exit(f"unknown instances: {', '.join(unknown)}")
    if not os.path.exists(args.bin):
        sys.exit(f"{args.bin} not found; run `cargo build --release` or pass --bin")
    if args.highs and not os.path.exists(VENV_PYTHON):
        sys.exit(f"{VENV_PYTHON} not found; create the .venv (see CLAUDE.md)")

    solu = os.path.join(args.dir, "miplib2017.solu")
    download(SOLU_URL, solu)
    optima = known_optima(solu)
    cache_path = os.path.join(args.dir, "highs.json")
    cache = {}
    if os.path.exists(cache_path) and not args.refresh:
        with open(cache_path) as f:
            cache = json.load(f)
    highs_key = f"{args.time:g}"

    converter_mtime = os.path.getmtime(os.path.join(ROOT, "tools", "mps_to_json.py"))
    header = f"{'instance':20} {'seconds':>8} {'status':19} {'objective':>16} {'optimum':>16}"
    print(header + (f" {'HiGHS s':>8} {'HiGHS':10}" if args.highs else "") + "  verdict", flush=True)
    verdicts = {}
    times, highs_times = [], []
    for name in args.names or INSTANCES:
        if name not in optima:
            sys.exit(f"{name} has no optimal value in MIPLIB's list")
        optimum = optima[name]
        mps = os.path.join(args.dir, "mps", f"{name}.mps.gz")
        download(INSTANCE_URL.format(name), mps)
        js = os.path.join(args.dir, "json", f"{name}.json")
        if not os.path.exists(js) or os.path.getmtime(js) < converter_mtime:
            os.makedirs(os.path.dirname(js), exist_ok=True)
            try:
                with gzip.open(mps, "rt") as f:
                    problem, _ = mps_to_json.convert(f.read())
            except mps_to_json.MpsError as e:
                print(f"{name:20} unconverted: {e}", flush=True)
                verdicts.setdefault("error", []).append(name)
                continue
            with open(js, "w") as f:
                json.dump(problem, f, separators=(",", ":"))

        ours = run_optimizam(args.bin, js, args.time, args.threads)
        objective = ours["objective"]
        note = ""
        if ours["status"] == "error":
            verdict, note = "error", " " + ours["message"][:60]
        elif ours["status"] == "complete":
            verdict = "proven" if objective is not None and rel(objective, optimum) <= REL_TOL else "WRONG"
        elif ours["status"] in ("infeasible", "unbounded"):
            verdict = "WRONG"
        elif objective is None:
            verdict = "failed"
        elif rel(objective, optimum) <= REL_TOL:
            verdict = "found"
        else:
            verdict, note = "feasible", f" (gap {rel(objective, optimum):.2%})"
        verdicts.setdefault(verdict, []).append(name)
        times.append(ours["seconds"] if verdict == "proven" else max(ours["seconds"], args.time))

        shown = "" if objective is None else f"{objective:.10g}"
        line = f"{name:20} {ours['seconds']:8.2f} {ours['status']:19} {shown:>16} {optimum:>16.10g}"
        if args.highs:
            cached = cache.get(name, {}).get(highs_key)
            if cached is None or cached.get("gap", HIGHS_DEFAULT_GAP) != HIGHS_GAP:
                cache.setdefault(name, {})[highs_key] = run_highs(mps, args.time)
                with open(cache_path, "w") as f:
                    json.dump(cache, f, indent=1)
            ref = cache[name][highs_key]
            solved = ref["status"] == "Optimal" and rel(ref["objective"], optimum) <= REL_TOL
            highs_times.append(ref["seconds"] if solved else max(ref["seconds"], args.time))
            line += f" {ref['seconds']:8.2f} {ref['status'][:10]:10}"
        print(f"{line}  {verdict}{note}", flush=True)

    print()
    for verdict in ("proven", "found", "feasible", "failed", "error", "WRONG"):
        names = verdicts.get(verdict, [])
        print(f"{verdict:9} {len(names):3}  {' '.join(names) if verdict != 'proven' else ''}".rstrip())
    summary = f"optimizam ({args.threads} thread{'s' if args.threads != 1 else ''}): shifted geometric mean"
    summary += f" {shifted_geometric_mean(times):.3f} s, total {sum(times):.1f} s"
    if args.highs:
        summary += f"; HiGHS: {shifted_geometric_mean(highs_times):.3f} s, total {sum(highs_times):.1f} s"
    print(summary)
    sys.exit(1 if verdicts.get("WRONG") else 0)


if __name__ == "__main__":
    main()
