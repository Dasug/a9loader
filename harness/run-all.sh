#!/usr/bin/env bash
# run-all.sh — the AGENTS.md harness gate under one command: run every
# harness/m*.sh in name order and summarize the verdicts. A script is green
# when its LAST line is "<NAME>: ALL SCENARIOS GREEN"; this wrapper captures
# each script's output to harness/run/all.log/<name>.log, prints the verdicts
# and the tail of any script that is not green, and exits non-zero when any
# of them failed. Extra arguments pass through to every script verbatim
# (e.g. the exe path: bash harness/run-all.sh "$GAME_EXE").
set -uo pipefail
cd "$(dirname "$0")/.."

LOGDIR="harness/run/all.log"
mkdir -p "$LOGDIR"

failed=()
ran=0
for h in harness/m*.sh; do
    ran=$((ran + 1))
    name="$(basename "$h" .sh)"
    log="$LOGDIR/$name.log"
    echo "===== $h"
    bash "$h" "$@" > "$log" 2>&1
    last="$(tail -1 "$log")"
    case "$last" in
        *"ALL SCENARIOS GREEN"*) echo "  ├ $h: GREEN" ;;
        *)
            echo "  ├ $h: NOT GREEN (last line: $last)"
            failed+=("$h")
            sed -n '/FAIL/p' "$log" | tail -5 | sed 's/^/  │   /'
            ;;
    esac
done

echo
if [ "${#failed[@]}" -eq 0 ]; then
    echo "run-all: $ran harnesses, all GREEN (logs in $LOGDIR)"
    exit 0
else
    echo "run-all: $ran harnesses, ${#failed[@]} NOT GREEN: ${failed[*]}"
    exit 1
fi
