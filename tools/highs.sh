#!/bin/sh
# Run the HiGHS reference solver with optimizam's command-line interface.
dir=$(cd "$(dirname "$0")" && pwd)
exec "$dir/../.venv/bin/python" "$dir/highs_reference.py" "$@"
