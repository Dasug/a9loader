#!/usr/bin/env bash
# Shared helpers for the a9loader wine harnesses (m2, m3, m4probe, m4inj, m5,
# m6). Each harness sources this file after it has cd'd to the repo root and
# set REPO / RUN / PREFIX / DLL; this file owns the mutable scenario state
# (FAILED, scen_dir) and the helpers every harness used to redefine.
#
# Per-script hooks (all optional):
#   * DEFAULT_CFG    — when set (non-empty), new_scen writes it (printf %b) as
#                      the scenario's default a9loader.toml.
#   * scen_fixtures  — when the sourcing script defines it, new_scen calls it
#                      after the common copies so the script can drop its own
#                      fixtures into the scenario dir.
#   * run_host [mode] — mode defaults to $SCEN_CHECK (m2's per-scenario check);
#                      m4probe passes "probe" explicitly.
# A harness that needs a different `check` (m4probe/m4inj: fixed-string) simply
# redefines it after sourcing this file.

FAILED=0
scen_dir=""

check() { # check <label> <needle> <file>
    if grep -q "$2" "$3"; then
        echo "   [ ok ] $1"
    else
        echo "   [FAIL] $1 (missing: $2 in $3)"
        FAILED=$((FAILED + 1))
    fi
}

checkf() { # checkf <label> <cmd...>
    local label="$1"; shift
    if "$@" >/dev/null 2>&1; then
        echo "   [ ok ] $label"
    else
        echo "   [FAIL] $label"
        FAILED=$((FAILED + 1))
    fi
}

checkq() { # checkq <label> <needle> <file> — assert the needle is ABSENT
    if grep -q "$2" "$3"; then
        echo "   [FAIL] $1 (unexpected: $2 in $3)"
        FAILED=$((FAILED + 1))
    else
        echo "   [ ok ] $1"
    fi
}

mod() { # mod <dir> <manifest-body...>
    mkdir -p "$scen_dir/mods/$1"
    printf '%s\n' "${@:2}" > "$scen_dir/mods/$1/mod.toml"
}

pay() { # pay <mod> <relpath> <runfile>
    mkdir -p "$(dirname "$scen_dir/mods/$1/$2")"
    cp "$RUN/$3" "$scen_dir/mods/$1/$2"
}

py_mk() { # py_mk <file> <size> <seed> — a deterministic ramp payload
    python3 - "$1" "$2" "$3" <<'EOF'
import sys
path, size, seed = sys.argv[1], int(sys.argv[2], 0), int(sys.argv[3], 0)
open(path, "wb").write(bytes((seed + i) & 0xff for i in range(size)))
EOF
}

run_host() { # run_host [mode] — mode defaults to $SCEN_CHECK (m2's variant)
    # timeout: a crashed host must FAIL the scenario fast, never hang wine's
    # interactive debugger attach
    local mode="${1:-${SCEN_CHECK:-}}"
    ( cd "$scen_dir" && WINEPREFIX="$PREFIX" WINEDEBUG=-all timeout 60 wine host.exe form_list.bin "$mode" ) > "$scen_dir/out.txt" 2>&1 || true
}

new_scen() { # new_scen <name> [host-exe] [scen-check]
    scen_dir="$RUN/$1"
    rm -rf "$scen_dir"
    mkdir -p "$scen_dir/data"
    cp "$RUN/${2:-host.exe}" "$scen_dir/host.exe"
    cp "$DLL" "$scen_dir/version.dll"
    cp "$RUN/form_list.bin" "$scen_dir/"
    SCEN_CHECK="${3:-}"
    SCEN_NAME="$1"
    if [ -n "${DEFAULT_CFG:-}" ]; then
        printf '%b' "$DEFAULT_CFG" > "$scen_dir/a9loader.toml"
    fi
    if declare -F scen_fixtures >/dev/null; then
        scen_fixtures
    fi
}

harness_done() { # harness_done "<HARNESS LABEL>" — the shared summary + exit
    echo
    if [ "$FAILED" = 0 ]; then
        echo "$1: ALL SCENARIOS GREEN"
    else
        echo "$1: $FAILED FAILURE(S)"
    fi
    exit "$FAILED"
}
