#!/bin/sh
# Run the OR-Tools CP-SAT reference with optimizam's command-line interface.
dir=$(cd "$(dirname "$0")" && pwd)
exec "$dir/../.venv/bin/python" "$dir/cpsat_reference.py" "$@"
