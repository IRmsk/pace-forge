#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$script_dir"
printf '%s\n' 'Open http://127.0.0.1:9999 in your browser.'
printf '%s\n' 'Keep this terminal open while using pace-forge. Press Ctrl+C to stop.'
exec ./pace-forge web 127.0.0.1:9999
