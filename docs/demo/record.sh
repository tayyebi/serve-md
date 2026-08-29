#!/bin/sh
# Records the README demo. Requires asciinema; see README.md in this directory.
set -eu
command -v asciinema >/dev/null 2>&1 || { echo "asciinema is required" >&2; exit 1; }
cd "$(dirname "$0")"
rm -f demo.cast
asciinema rec demo.cast \
  --cols 80 --rows 24 \
  --title "serve-md — a folder of Markdown, ready for an AI agent" \
  --idle-time-limit 1.5
echo "recorded: $(pwd)/demo.cast"
echo "now: agg demo.cast demo.gif --font-size 16 --theme asciinema"
