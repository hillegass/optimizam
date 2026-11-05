#!/bin/sh
# Run the IPOPT reference solver (through CasADi) with optimizam's command-line interface.
dir=$(cd "$(dirname "$0")" && pwd)
exec "$dir/../.venv/bin/python" "$dir/ipopt_reference.py" "$@"
