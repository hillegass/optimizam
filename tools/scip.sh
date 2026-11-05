#!/bin/sh
# Run the SCIP reference solver (through PySCIPOpt) with optimizam's command-line interface.
dir=$(cd "$(dirname "$0")" && pwd)
exec "$dir/../.venv/bin/python" "$dir/scip_reference.py" "$@"
