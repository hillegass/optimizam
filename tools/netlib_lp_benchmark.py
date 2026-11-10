#!/usr/bin/env python3
"""Benchmark optimizam's LP solver on the Netlib LP test set.

The 91 problems of the main Netlib LP set (netlib.org/lp/data) that the
COIN-OR Data-Netlib mirror provides are downloaded from it as gzipped MPS, converted with
tools/mps_to_json.py, and solved. Each result is checked against:
  - HiGHS reading the original MPS file (independent of the converter), and
  - the optimal value published in Netlib's README (from MINOS, to about 11
    digits; a looser check, skipped where the README marks the value
    approximate).
Netlib's problems are small but notoriously degenerate and badly scaled, so
this is the classic robustness test for a simplex code. Not included (not in
the mirror): PILOT.JA, PILOT.WE, and the problems Netlib itself ships only as
generators or separately (QAP8, QAP12, QAP15, STOCFOR3, TRUSS).

Usage (from the repository root, after `cargo build --release`):
    python3 tools/netlib_lp_benchmark.py [--time SECONDS] [--check-converter]
        [--refresh] [--bin PATH] [--dir DIR] [names...]

Files are cached in target/netlib_lp/ (mps/, json/, the README, and HiGHS's
results in highs.json; --refresh recomputes those). With --check-converter,
HiGHS also solves the converted JSON (through tools/highs.sh) and must agree
with itself on the MPS file, which separates converter bugs from solver bugs.

Verdicts: ok (optimal and within 1e-6 relative of HiGHS), wrong (a different
status or objective: a bug), limit (time ran out), error (optimizam failed,
e.g. "LP solver failed"), unconverted (the MPS converter rejected the file).
Exits with status 1 if any verdict is wrong or unconverted, or the converter
check fails.

The "vs Netlib" column is informational: Netlib's published values come from
MINOS, which ignores an objective constant given on the objective row in the
RHS section (E226's published value differs by exactly that constant), and
the README lists codes that find slightly different values.
HiGHS needs the project's .venv (see CLAUDE.md); the tool itself uses only
the standard library.
"""

import argparse
import gzip
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
sys.path.insert(0, os.path.join(ROOT, "tools"))
import mps_to_json  # noqa: E402

MIRROR = "https://raw.githubusercontent.com/coin-or-tools/Data-Netlib/master/{}.mps.gz"
README = "https://netlib.org/lp/data/readme"
VENV_PYTHON = os.path.join(ROOT, ".venv", "bin", "python")
REL_TOL = 1e-6
NETLIB_TOL = 1e-5

INSTANCES = """
25fv47 80bau3b adlittle afiro agg agg2 agg3 bandm beaconfd blend bnl1 bnl2 boeing1 boeing2 bore3d brandy
capri cycle czprob d2q06c d6cube degen2 degen3 dfl001 e226 etamacro fffff800 finnis fit1d fit1p fit2d
fit2p forplan ganges gfrd-pnc greenbea greenbeb grow15 grow22 grow7 israel kb2 lotfi maros maros-r7
modszk1 nesm perold pilot pilot4 pilot87 pilotnov recipe sc105 sc205 sc50a sc50b scagr25 scagr7 scfxm1
scfxm2 scfxm3 scorpion scrs8 scsd1 scsd6 scsd8 sctap1 sctap2 sctap3 seba share1b share2b shell ship04l
ship04s ship08l ship08s ship12l ship12s sierra stair standata standgub standmps stocfor1 stocfor2 tuff
vtpbase wood1p woodw
""".split()

# Run by the project's .venv python: solve an MPS file with HiGHS, print JSON.
HIGHS_SCRIPT = r"""
import json, sys, time, highspy
h = highspy.Highs()
h.silent()
h.setOptionValue("time_limit", float(sys.argv[2]))
start = time.monotonic()
h.readModel(sys.argv[1])
h.run()
lp = h.getLp()
info = h.getInfo()
print(json.dumps({
    "status": h.modelStatusToString(h.getModelStatus()),
    "objective": info.objective_function_value,
    "seconds": time.monotonic() - start,
    "rows": lp.num_row_, "cols": lp.num_col_, "nonzeros": len(lp.a_matrix_.value_),
}))
"""


def download(url, path):
    if os.path.exists(path):
        return
    os.makedirs(os.path.dirname(path), exist_ok=True)
    req = urllib.request.Request(url, headers={"User-Agent": "optimizam-benchmark"})
    with urllib.request.urlopen(req, timeout=60) as r, open(path + ".part", "wb") as f:
        f.write(r.read())
    os.replace(path + ".part", path)


def published_values(readme_path):
    """{name: optimal value} from the README's main table, without approximate (**) entries."""
    values = {}
    with open(readme_path) as f:
        text = f.read()
    start = text.index("Name       Rows   Cols")
    for line in text[start:].splitlines()[1:]:
        if not line.strip():
            if values:
                break
            continue
        m = re.match(r"^(\S+)\s+\d+\s+\d+\s+\d+.*?(-?\d\.\d+E[+-]\d+)\s*(\*\*)?\s*$", line)
        if m and not m.group(3):
            values[m.group(1).lower().replace(".", "")] = float(m.group(2))
    return values


def run_optimizam(binary, path, time_limit):
    start = time.monotonic()
    r = subprocess.run([binary, f"--time={time_limit}", path, "-"], capture_output=True, text=True)
    seconds = time.monotonic() - start
    if r.returncode != 0:
        message = (r.stderr.strip().splitlines() or ["(no message)"])[0]
        return {"seconds": seconds, "status": "error", "objective": None, "message": message}
    result = json.loads(r.stdout)
    objective = result["solutions"][0].get("objective") if result["solutions"] else None
    return {"seconds": seconds, "status": result["status"], "objective": objective}


def run_highs_mps(path, time_limit):
    r = subprocess.run([VENV_PYTHON, "-c", HIGHS_SCRIPT, path, str(time_limit)], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"HiGHS failed on {path}: {r.stderr.strip()[-500:]}")
    return json.loads(r.stdout)


def run_highs_json(path, time_limit):
    r = subprocess.run([os.path.join(ROOT, "tools", "highs.sh"), f"--time={time_limit}", path, "-"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return None
    result = json.loads(r.stdout)
    return result["solutions"][0]["objective"] if result["solutions"] else None


def rel(a, b):
    return abs(a - b) / max(1.0, abs(b))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("names", nargs="*", help="instances to run (default: all 91)")
    ap.add_argument("--time", type=float, default=60, help="time limit per solve in seconds (default 60)")
    ap.add_argument("--check-converter", action="store_true", help="also solve the converted JSON with HiGHS")
    ap.add_argument("--refresh", action="store_true", help="recompute cached HiGHS results")
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", "optimizam"), help="optimizam binary")
    ap.add_argument("--dir", default=os.path.join(ROOT, "target", "netlib_lp"), help="cache directory")
    args = ap.parse_args()

    unknown = [n for n in args.names if n not in INSTANCES]
    if unknown:
        sys.exit(f"unknown instances: {', '.join(unknown)}")
    if not os.path.exists(args.bin):
        sys.exit(f"{args.bin} not found; run `cargo build --release` or pass --bin")
    if not os.path.exists(VENV_PYTHON):
        sys.exit(f"{VENV_PYTHON} not found; create the .venv (see CLAUDE.md)")

    readme = os.path.join(args.dir, "readme")
    download(README, readme)
    published = published_values(readme)
    cache_path = os.path.join(args.dir, "highs.json")
    cache = {}
    if os.path.exists(cache_path) and not args.refresh:
        with open(cache_path) as f:
            cache = json.load(f)

    converter_mtime = os.path.getmtime(os.path.join(ROOT, "tools", "mps_to_json.py"))
    print(f"{'instance':10} {'rows':>6} {'cols':>6} {'nnz':>7} {'seconds':>8} {'status':19} {'objective':>18}"
          f" {'vs HiGHS':>9} {'vs Netlib':>9} {'HiGHS s':>8}  verdict")
    verdicts = {}
    totals = {"optimizam": 0.0, "highs": 0.0}
    for name in args.names or INSTANCES:
        mps = os.path.join(args.dir, "mps", f"{name}.mps.gz")
        download(MIRROR.format(name), mps)
        js = os.path.join(args.dir, "json", f"{name}.json")
        if not os.path.exists(js) or os.path.getmtime(js) < converter_mtime:
            os.makedirs(os.path.dirname(js), exist_ok=True)
            try:
                with gzip.open(mps, "rt") as f:
                    problem, _ = mps_to_json.convert(f.read())
            except mps_to_json.MpsError as e:
                print(f"{name:10} unconverted: {e}", flush=True)
                verdicts.setdefault("unconverted", []).append(name)
                continue
            with open(js, "w") as f:
                json.dump(problem, f, separators=(",", ":"))
        if name not in cache:
            cache[name] = run_highs_mps(mps, args.time)
            with open(cache_path, "w") as f:
                json.dump(cache, f, indent=1)
        ref = cache[name]
        ours = run_optimizam(args.bin, js, args.time)

        verdict = "ok"
        err = rel(ours["objective"], ref["objective"]) if ours["objective"] is not None else None
        if ours["status"] == "error":
            verdict = "error"
        elif ours["status"] in ("timeout", "not_proven_optimal"):
            verdict = "limit"
        elif ref["status"] != "Optimal":
            verdict = "ok" if ours["status"] != "complete" else "wrong"  # HiGHS: infeasible/unbounded/limit
        elif ours["status"] != "complete" or err > REL_TOL:
            verdict = "wrong"
        netlib = rel(ours["objective"], published[name]) if ours["objective"] is not None and name in published else None
        if args.check_converter:
            via_json = run_highs_json(js, args.time)
            if via_json is None or rel(via_json, ref["objective"]) > REL_TOL:
                verdict += " (CONVERTER MISMATCH: HiGHS on JSON gives {})".format(via_json)
                verdicts.setdefault("converter mismatch", []).append(name)
        verdicts.setdefault(verdict.split()[0], []).append(name)
        totals["optimizam"] += ours["seconds"]
        totals["highs"] += ref["seconds"]

        def f(v, spec):
            return "" if v is None else format(v, spec)
        line = (f"{name:10} {ref['rows']:6} {ref['cols']:6} {ref['nonzeros']:7} {ours['seconds']:8.2f} {ours['status']:19}"
                f" {f(ours['objective'], '18.10g'):>18} {f(err, '9.1e'):>9} {f(netlib, '9.1e'):>9} {ref['seconds']:8.2f}  {verdict}")
        if ours["status"] == "error":
            line += f": {ours['message']}"
        print(line, flush=True)

    print()
    for verdict in ("ok", "wrong", "limit", "error", "unconverted", "converter mismatch"):
        names = verdicts.get(verdict, [])
        print(f"{verdict:18} {len(names):3}" + (f"  {' '.join(names)}" if names and verdict != "ok" else ""))
    print(f"total seconds: optimizam {totals['optimizam']:.1f}, HiGHS {totals['highs']:.1f}")
    sys.exit(1 if verdicts.get("wrong") or verdicts.get("unconverted") or verdicts.get("converter mismatch") else 0)


if __name__ == "__main__":
    main()
