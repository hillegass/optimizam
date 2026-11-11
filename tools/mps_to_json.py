#!/usr/bin/env python3
"""Convert an MPS file (fixed or free format, optionally gzipped) to an
optimizam problem file (docs/problem-format.md).

Usage (from the repository root):
    python3 tools/mps_to_json.py INPUT.mps[.gz] OUTPUT.json [--names NAMES.json] [--pretty]

OUTPUT can be "-" for stdout. With --names, also writes the MPS name of every
variable element and constraint, to read results back in MPS terms. Output is
compact JSON unless --pretty is given.

Free format is tried first, with strict field counts; if that fails, the file
is read as fixed format (fields at columns 2-3, 5-12, 15-22, 25-36, 40-47,
50-61), where names may contain spaces, as in Netlib's FORPLAN.

Supported MPS: NAME, OBJSENSE (MIN/MAX, on the same or the next line), ROWS
(N, E, L, G; the first N row is the objective, later ones are dropped),
COLUMNS (with 'MARKER' INTORG/INTEND), RHS (a value on the objective row is
minus the objective constant), RANGES, BOUNDS (UP, LO, FX, FR, MI, PL, BV, LI,
UI), ENDATA. An UP bound below zero on a column with no explicit lower bound
makes the lower bound -infinity, as in the original MPS convention, with a
warning on stderr; readers differ here (HiGHS 1.15 keeps the lower bound at 0,
which makes the column's bounds inconsistent). Integer
columns from markers default to [0, +infinity). Semicontinuous (SC) bounds
and quadratic sections are rejected.

The problem format has no string literals and no way to write an infinite
parameter, so columns become integer-indexed variables grouped by type and by
which bounds are finite: e.g. `cont_lu[17]` is the 17th continuous column with
finite lower and upper bounds, given by parameters `cont_lu_lo` and
`cont_lu_up`. Ranged rows become two constraints; MPS row names go into the
constraints' `description`. Coefficients are written with Python's `repr`, so
they round-trip exactly.

Uses only the standard library; `convert` can be imported by other tools.
"""

import gzip
import json
import math
import sys

INF = math.inf
# Bounds and right-hand sides of this magnitude or more are infinite (as in HiGHS).
INFINITE_BOUND = 1e20


class MpsError(Exception):
    pass


def read_text(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt") as f:
        return f.read()


def number(token, line_no):
    try:
        return float(token)
    except ValueError:
        raise MpsError(f"line {line_no}: {token!r} is not a number")


def bound(token, line_no):
    """A number, with magnitudes of INFINITE_BOUND or more made infinite."""
    v = number(token, line_no)
    return math.copysign(INF, v) if abs(v) >= INFINITE_BOUND else v


# Fixed-format field positions (0-based slices of columns 2-3, 5-12, 15-22, 25-36, 40-47, 50-61).
FIXED_FIELDS = [(1, 3), (4, 12), (14, 22), (24, 36), (39, 47), (49, 61)]
# Allowed field counts in free format, per section, to catch names with spaces.
FREE_COUNTS = {"ROWS": {2}, "COLUMNS": {3, 5}, "RHS": {2, 3, 4, 5}, "RANGES": {2, 3, 4, 5}, "BOUNDS": {2, 3, 4}}


def split_fixed(raw, section):
    f = [raw[a:b].strip() for a, b in FIXED_FIELDS]
    fields = {"ROWS": f[:2], "BOUNDS": f[:4]}.get(section, f[1:])
    while fields and not fields[-1]:
        fields.pop()
    return fields


def parse(text):
    """Parses MPS text, in free format or else fixed format."""
    try:
        return parse_as(text, fixed=False)
    except MpsError as free_error:
        try:
            return parse_as(text, fixed=True)
        except MpsError:
            raise free_error


def parse_as(text, fixed):
    """Parses MPS text into a dict of rows, columns, bounds, and the objective."""
    sense = "minimize"
    objective = None
    row_type = {}  # name -> "E" | "L" | "G"
    row_order = []
    dropped = set()  # extra N rows
    columns = {}  # name -> {"coefs": {row: value}, "integer": bool}
    col_order = []
    rhs, ranges = {}, {}
    lower, upper, explicit_lower, binary = {}, {}, set(), set()
    negative_up = []
    obj_constant = 0.0
    section, integer_block, expect_sense = None, False, False

    for line_no, raw in enumerate(text.splitlines(), 1):
        if not raw.strip() or raw.startswith("*"):
            continue
        if raw[0].isspace() and fixed:
            fields = split_fixed(raw, section)
        else:
            fields = raw.split()
            is_marker = section == "COLUMNS" and "'MARKER'" in fields
            if raw[0].isspace() and not is_marker and section in FREE_COUNTS and len(fields) not in FREE_COUNTS[section]:
                raise MpsError(f"line {line_no}: unexpected number of fields for free-format {section}")
        if not raw[0].isspace():
            head = fields[0].upper()
            if expect_sense and head in ("MAX", "MAXIMIZE", "MIN", "MINIMIZE"):
                sense = "maximize" if head.startswith("MAX") else "minimize"
                expect_sense = False
                continue
            section = head
            expect_sense = False
            if head == "OBJSENSE":
                if len(fields) > 1:
                    sense = "maximize" if fields[1].upper().startswith("MAX") else "minimize"
                else:
                    expect_sense = True
            elif head == "ENDATA":
                break
            elif head not in ("NAME", "ROWS", "COLUMNS", "RHS", "RANGES", "BOUNDS", "OBJSENSE"):
                raise MpsError(f"line {line_no}: unsupported section {head}")
            continue
        if expect_sense:
            head = fields[0].upper()
            sense = "maximize" if head.startswith("MAX") else "minimize"
            expect_sense = False
            continue

        if section == "ROWS":
            kind, name = fields[0].upper(), fields[1]
            if name in row_type or name == objective or name in dropped:
                raise MpsError(f"line {line_no}: duplicate row {name}")
            if kind == "N":
                if objective is None:
                    objective = name
                else:
                    dropped.add(name)
            elif kind in ("E", "L", "G"):
                row_type[name] = kind
                row_order.append(name)
            else:
                raise MpsError(f"line {line_no}: unknown row type {kind}")
        elif section == "COLUMNS":
            words = [w.strip("'").upper() for w in fields]
            if "MARKER" in words[1:]:
                if "INTORG" in words:
                    integer_block = True
                elif "INTEND" in words:
                    integer_block = False
                continue
            name = fields[0]
            if name not in columns:
                columns[name] = {"coefs": {}, "integer": integer_block}
                col_order.append(name)
            elif col_order[-1] != name:
                raise MpsError(f"line {line_no}: column {name} is not contiguous")
            pairs = fields[1:]
            if len(pairs) % 2:
                raise MpsError(f"line {line_no}: expected row/value pairs")
            for row, value in zip(pairs[::2], pairs[1::2]):
                if row in dropped:
                    continue
                if row != objective and row not in row_type:
                    raise MpsError(f"line {line_no}: unknown row {row}")
                coefs = columns[name]["coefs"]
                coefs[row] = coefs.get(row, 0.0) + number(value, line_no)
        elif section in ("RHS", "RANGES"):
            pairs = fields[1:] if len(fields) % 2 else fields  # optional set name
            target = rhs if section == "RHS" else ranges
            for row, value in zip(pairs[::2], pairs[1::2]):
                v = number(value, line_no) if row == objective else bound(value, line_no)
                if row == objective and section == "RHS":
                    obj_constant = -v
                elif row in row_type:
                    target[row] = v
                elif row not in dropped:
                    raise MpsError(f"line {line_no}: unknown row {row}")
        elif section == "BOUNDS":
            kind = fields[0].upper()
            takes_value = kind in ("UP", "LO", "FX", "LI", "UI", "SC")
            if kind == "SC":
                raise MpsError(f"line {line_no}: semicontinuous bounds are not supported")
            if takes_value:
                # "KIND SET COL VALUE", or "KIND COL VALUE" without a bound set name.
                col, value = (fields[2], fields[3]) if len(fields) >= 4 else (fields[1], fields[2])
                v = bound(value, line_no)
            else:
                # "KIND SET COL", "KIND COL", or (some writers) "BV SET COL 1" / "BV COL 1".
                col = fields[2] if len(fields) >= 3 else fields[1]
                if col not in columns and fields[1] in columns:
                    col = fields[1]
            if col not in columns:
                raise MpsError(f"line {line_no}: unknown column {col}")
            if kind == "UP":
                upper[col] = v
                if v < 0 and col not in explicit_lower and lower.get(col, 0.0) == 0.0:
                    lower[col] = -INF
                    negative_up.append(col)
            elif kind == "LO":
                lower[col] = v
                explicit_lower.add(col)
            elif kind == "FX":
                lower[col] = upper[col] = v
                explicit_lower.add(col)
            elif kind == "FR":
                lower[col], upper[col] = -INF, INF
                explicit_lower.add(col)
            elif kind == "MI":
                lower[col] = -INF
                explicit_lower.add(col)
            elif kind == "PL":
                upper[col] = INF
            elif kind == "BV":
                binary.add(col)
                lower[col], upper[col] = 0.0, 1.0
            elif kind == "LI":
                lower[col] = v
                explicit_lower.add(col)
                columns[col]["integer"] = True
            elif kind == "UI":
                upper[col] = v
                columns[col]["integer"] = True
            else:
                raise MpsError(f"line {line_no}: unknown bound type {kind}")
        elif section in ("NAME", None):
            continue
        else:
            raise MpsError(f"line {line_no}: data in unsupported section {section}")

    if objective is None and not row_order and not columns:
        raise MpsError("no ROWS or COLUMNS found")
    # A later LO bound overrides the convention.
    negative_up = [c for c in negative_up if c not in explicit_lower]
    if negative_up:
        shown = ", ".join(negative_up[:5]) + (", ..." if len(negative_up) > 5 else "")
        print(f"warning: negative UP bound with no lower bound on {len(negative_up)} column(s) ({shown}): "
              "lower bound set to -infinity", file=sys.stderr)
    return {
        "sense": sense, "objective": objective, "obj_constant": obj_constant,
        "row_type": row_type, "row_order": row_order, "columns": columns, "col_order": col_order,
        "rhs": rhs, "ranges": ranges, "lower": lower, "upper": upper, "binary": binary,
    }


def row_bounds(kind, b, r):
    """(lower, upper) of a row of type kind with rhs b and range r (None if absent)."""
    if r is None:
        return {"E": (b, b), "L": (-INF, b), "G": (b, INF)}[kind]
    if kind == "L":
        return b - abs(r), b
    if kind == "G":
        return b, b + abs(r)
    return (b, b + r) if r > 0 else (b + r, b)


def term(coef, ref):
    return f"{coef!r} * {ref}"


def convert(text):
    """Converts MPS text; returns (problem, names) as JSON-ready dicts."""
    m = parse(text)
    # Group columns by type and finite bounds.
    # group name -> (type, has finite lower, has finite upper, MPS column names)
    groups = {}
    ref = {}  # MPS column name -> "group[k]"
    lower = lambda n: m["lower"].get(n, 0.0)
    upper = lambda n: m["upper"].get(n, INF)
    for name in m["col_order"]:
        lo, up = lower(name), upper(name)
        integer = m["columns"][name]["integer"] or name in m["binary"]
        if integer and lo == 0.0 and up == 1.0:
            key = ("bin", "binary", False, False)
        else:
            has_lo, has_up = lo > -INF, up < INF
            suffix = {(True, True): "lu", (True, False): "l", (False, True): "u", (False, False): "free"}[(has_lo, has_up)]
            kind = "integer" if integer else "continuous"
            key = (f"{'int' if integer else 'cont'}_{suffix}", kind, has_lo, has_up)
        group = groups.setdefault(key[0], (key[1], key[2], key[3], []))
        group[3].append(name)
        ref[name] = f"{key[0]}[{len(group[3])}]"

    sets, parameters, variables = {}, {}, []
    for gname, (kind, has_lo, has_up, names) in groups.items():
        set_name = f"S_{gname}"
        sets[set_name] = {"from": 1, "to": len(names)}
        var = {"name": gname, "type": kind, "index": [set_name]}
        for present, field, values in ((has_lo, "lower", lower), (has_up, "upper", upper)):
            if not present:
                continue
            distinct = {values(n) for n in names}
            if len(distinct) == 1:
                var[field] = distinct.pop()  # the same bound for every element
            else:
                pname = f"{gname}_{'lo' if field == 'lower' else 'up'}"
                parameters[pname] = {"index": [set_name], "values": {str(k): values(n) for k, n in enumerate(names, 1)}}
                var[field] = pname
        variables.append(var)

    # Row expressions.
    row_terms = {r: [] for r in m["row_order"]}
    obj_terms = []
    for name in m["col_order"]:
        for row, coef in m["columns"][name]["coefs"].items():
            if coef == 0.0:
                continue
            (obj_terms if row == m["objective"] else row_terms[row]).append(term(coef, ref[name]))
    constraints = []
    for row in m["row_order"]:
        lhs = " + ".join(row_terms[row]) or "0"
        lo, up = row_bounds(m["row_type"][row], m["rhs"].get(row, 0.0), m["ranges"].get(row))
        if lo == up:
            constraints.append({"description": row, "expr": f"{lhs} == {lo!r}"})
            continue
        if lo > -INF:
            constraints.append({"description": row, "expr": f"{lhs} >= {lo!r}"})
        if up < INF:
            constraints.append({"description": row, "expr": f"{lhs} <= {up!r}"})
    objective = " + ".join(obj_terms) or "0"
    if m["obj_constant"]:
        objective += f" + {m['obj_constant']!r}"
    if not variables:
        raise MpsError("the problem has no columns")

    problem = {"version": "1.3", "sets": sets, "parameters": parameters, "variables": variables,
               "constraints": constraints, "solve": {"method": m["sense"], "objective": objective, "solutions": "one"}}
    if not parameters:
        del problem["parameters"]
    names = {"columns": {ref[n]: n for n in m["col_order"]}, "rows": m["row_order"], "objective": m["objective"]}
    return problem, names


def main():
    args = sys.argv[1:]
    names_path = None
    pretty = "--pretty" in args
    args = [a for a in args if a != "--pretty"]
    if "--names" in args:
        i = args.index("--names")
        names_path = args[i + 1]
        del args[i:i + 2]
    if len(args) != 2:
        sys.exit(__doc__.split("\n\n")[1])
    try:
        problem, names = convert(read_text(args[0]))
    except MpsError as e:
        sys.exit(f"error: {args[0]}: {e}")
    text = json.dumps(problem, indent=1) if pretty else json.dumps(problem, separators=(",", ":"))
    if args[1] == "-":
        sys.stdout.write(text + "\n")
    else:
        with open(args[1], "w") as f:
            f.write(text + "\n")
    if names_path:
        with open(names_path, "w") as f:
            json.dump(names, f, indent=1)


if __name__ == "__main__":
    main()
