#!/usr/bin/env bash
# M2 harness — exercises the full loader engine under wine against a
# fake-game host whose exe carries the international Steam build's
# VERSIONINFO (5,0,0,5634) and whose image has the real RVAs mapped
# (filter fn, whitelist table, FORM list) with the real original bytes.
# Each scenario assembles a fresh run dir (mods/, lists) and asserts both
# the end effects and the loader log.
#
# usage: harness/m2.sh [ATrain9v5.exe]
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"

GAME_EXE="${1:-${ATRAIN9_EXE:-/run/media/frybarec/dasug/Games/steamlibrary/steamapps/common/A-Train9v5/ATrain9v5.exe}}"
GAME_DIR="$(dirname "$GAME_EXE")"
RUN="$REPO/harness/run/m2"
PREFIX="$REPO/harness/run/.prefix"

echo "== building hosts (right + wrong build) and the Rust version.dll"
mkdir -p "$RUN"
x86_64-w64-mingw32-windres harness/host.rc -O coff -o "$RUN/hostres.o"
x86_64-w64-mingw32-windres harness/host_wrong.rc -O coff -o "$RUN/hostres_wrong.o"
x86_64-w64-mingw32-gcc -O2 -o "$RUN/host.exe" harness/host.c "$RUN/hostres.o"
x86_64-w64-mingw32-gcc -O2 -o "$RUN/host-wrong.exe" harness/host.c "$RUN/hostres_wrong.o"
cargo build --release --target x86_64-pc-windows-gnu
DLL="$REPO/target/x86_64-pc-windows-gnu/release/version.dll"

echo "== extracting FORM list from $GAME_EXE"
python3 - "$GAME_EXE" "$RUN/form_list.bin" <<'EOF'
import sys
data = open(sys.argv[1], 'rb').read()
off = 0x6FB830
units = []
i = off
while i + 1 < len(data) and len(units) < 16384:
    u = data[i] | (data[i + 1] << 8)
    if u == 0:
        break
    units.append(u)
    i += 2
open(sys.argv[2], 'wb').write(b''.join(u.to_bytes(2, 'little') for u in units) + b'\x00\x00')
print(f"   {len(units)} units -> {sys.argv[2]}")
EOF

FAILED=0

# assemble a fresh scenario dir: run(<name>, host-exe, host-check)
scen_dir=""
new_scen() {
    scen_dir="$RUN/$1"
    rm -rf "$scen_dir"
    mkdir -p "$scen_dir"
    cp "$RUN/$2" "$scen_dir/host.exe"
    cp "$DLL" "$scen_dir/version.dll"
    cp "$RUN/form_list.bin" "$scen_dir/"
    SCEN_CHECK="$3"
    SCEN_NAME="$1"
}

run_host() {
    # timeout: a crashed host must FAIL the scenario fast, never hang wine's
    # interactive debugger attach
    ( cd "$scen_dir" && WINEPREFIX="$PREFIX" WINEDEBUG=-all timeout 60 wine host.exe form_list.bin "$SCEN_CHECK" ) > "$scen_dir/out.txt" 2>&1 || true
}

check() { # check <label> <needle> <file>
    if grep -q "$2" "$3"; then
        echo "   [ ok ] $1"
    else
        echo "   [FAIL] $1 (missing: $2 in $3)"
        FAILED=$((FAILED + 1))
    fi
}

checkf() { # checkf <label> <test-command...>
    local label="$1"; shift
    if "$@" >/dev/null 2>&1; then
        echo "   [ ok ] $label"
    else
        echo "   [FAIL] $label"
        FAILED=$((FAILED + 1))
    fi
}

mod() { # mod <dir> <manifest-body...>
    mkdir -p "$scen_dir/mods/$1"
    printf '%s\n' "${@:2}" > "$scen_dir/mods/$1/mod.toml"
}

# NOTE: legacy atrain9_proxy.ini support (incl. auto-migration) was dropped
# before anything shipped — mods are created as mods/<id>/mod.toml directly.

echo "== S2: manifest mod (train_list allow, model list)"
new_scen s2-manifest host.exe kept
mod trainnames \
    'id = "trainnames"' \
    'name = "Train names"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'mode = "allow"' \
    'list = "atrain9_filter_model.txt"'
cp "$GAME_DIR/atrain9_filter_model.txt" "$scen_dir/mods/trainnames/"
run_host
check "kept 341 forms"    "kept 341 / 1024"    "$scen_dir/out.txt"
check "mod scanned"       "mod: trainnames"   "$scen_dir/mods/a9loader.log"
check "effect journal"    "hook train_list@"  "$scen_dir/mods/a9loader.log"

echo "== S3: train_list deny (drop listed)"
new_scen s3-deny host.exe kept
mod deny2 \
    'id = "deny2"' \
    'name = "Deny two"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'mode = "deny"' \
    'list = "deny.txt"'
# the list lives INSIDE the mod dir (list paths are mod-relative)
printf 'FORM_113\n' > "$scen_dir/mods/deny2/deny.txt"
run_host
# FORM_114 does not exist in the exe list — deny only a real form: 417-1
check "kept 416 forms (417 - 1)" "kept 416 / 1024"  "$scen_dir/out.txt"
check "deny journaled"           "mode=deny"        "$scen_dir/mods/a9loader.log"

echo "== S4: wrong build -> vanilla boot (no effects)"
new_scen s4-wrongbuild host-wrong.exe none
mod trainnames \
    'id = "trainnames"' \
    'name = "Train names"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'list = "atrain9_filter_model.txt"'
cp "$GAME_DIR/atrain9_filter_model.txt" "$scen_dir/mods/trainnames/"
run_host
check "filter bytes untouched" "unchanged (PASS)"            "$scen_dir/out.txt"
check "unknown build refused"   "unknown build 5.00.9999"     "$scen_dir/mods/a9loader.log"
check "no memory effects"       "no memory effects"           "$scen_dir/mods/a9loader.log"

echo "== S5: train_list add — offer a form ON TOP of the shipped whitelist"
new_scen s5-add host.exe kept
# the harness host's whitelist table carries the two real entries the address
# book fingerprints (95 = FORM_AR3, 187 = FORM_AR9X); an `add` contribution
# seeds from that shipped table, so the result is baseline ∪ list.
mod addone \
    'id = "addone"' \
    'name = "Add one"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'mode = "add"' \
    'list = "add.txt"'
printf 'FORM_0\n' > "$scen_dir/mods/addone/add.txt"
run_host
check "shipped whitelist + FORM_0 (kept 4)" "kept 4 / 1024"           "$scen_dir/out.txt"
check "add composition journaled"          "add baseline = the shipped whitelist: 3 forms" "$scen_dir/mods/a9loader.log"
check "contribution journaled"             "train_list contribution 'addone': mode=add" "$scen_dir/mods/a9loader.log"

echo "== S5b: two train_list mods compose (allow ∪ allow, no overlap)"
new_scen s5b-two-allow host.exe kept
mod allowa \
    'id = "allowa"' \
    'name = "Allow A"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'mode = "allow"' \
    'list = "a.txt"'
printf 'FORM_0\n' > "$scen_dir/mods/allowa/a.txt"
mod allowb \
    'id = "allowb"' \
    'name = "Allow B"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'mode = "allow"' \
    'list = "b.txt"'
printf 'FORM_113\n' > "$scen_dir/mods/allowb/b.txt"
run_host
check "union of both allow lists (kept 2)" "kept 2 / 1024"  "$scen_dir/out.txt"
check "two contributions composed"         "composed 2 contribution(s)" "$scen_dir/mods/a9loader.log"
checkf "no false overlap" sh -c "! grep -q 'overlap on hook:train_list_filter' '$scen_dir/mods/a9loader.log'"

echo "== S5c: an add composes with an allow (allow owns the set, add unions in)"
new_scen s5c-add-allow host.exe kept
mod allowc \
    'id = "allowc"' \
    'name = "Allow C"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'mode = "allow"' \
    'list = "c.txt"'
printf 'FORM_113\n' > "$scen_dir/mods/allowc/c.txt"
mod addc \
    'id = "addc"' \
    'name = "Add C"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'mode = "add"' \
    'list = "d.txt"'
printf 'FORM_0\n' > "$scen_dir/mods/addc/d.txt"
run_host
check "allow ∪ add (kept 2, whitelist superseded)" "kept 2 / 1024" "$scen_dir/out.txt"

echo "== S5d: a train_list and a nop still conflict on the seam (one writer)"
new_scen s5d-seam-conflict host.exe kept
mod tled \
    'id = "tled"' \
    'name = "TL"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'mode = "allow"' \
    'list = "e.txt"'
printf 'FORM_0\n' > "$scen_dir/mods/tled/e.txt"
mod noper \
    'id = "noper"' \
    'name = "Nop"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "nop"'
run_host
check "first (train_list) keeps the seam" "kept 1 / 1024"                    "$scen_dir/out.txt"
check "overlap reported"                  "overlap on hook:train_list_filter" "$scen_dir/mods/a9loader.log"
check "nop refused"                       "refused (overlap loser)"          "$scen_dir/mods/a9loader.log"

echo "== S6: requires / conflicts resolution"
new_scen s6-deps host.exe nop
mod libcap \
    'id = "libcap"' \
    'name = "Lib"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    'supplies = ["cap-x"]' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "nop"'
mod needcap \
    'id = "needcap"' \
    'name = "Needs"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    'requires = ["cap-x"]' \
    '' \
    '[[patch]]' \
    'target = "train_list_whitelist"' \
    'offset = 24' \
    'data = "88 00 00 00"' \
    'expect = "bb 00 00 00"'
mod brokenmod \
    'id = "brokenmod"' \
    'name = "Broken"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 30' \
    'requires = ["missing-thing"]' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "nop"'
mod firstc \
    'id = "firstc"' \
    'name = "FC"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 40' \
    '' \
    '[[hook]]' \
    'target = "form_list"' \
    'handler = "nop"'
mod secondc \
    'id = "secondc"' \
    'name = "SC"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 50' \
    'conflicts = ["firstc"]' \
    '' \
    '[[patch]]' \
    'target = "train_list_whitelist"' \
    'offset = 28' \
    'data = "88 00 00 00"'
run_host
check "nop identity applied"      "identity (PASS)"                     "$scen_dir/out.txt"
check "requirement satisfied"     "mod 'needcap'"                      "$scen_dir/mods/a9loader.log"
check "missing requirement"       "requires \"missing-thing\""          "$scen_dir/mods/a9loader.log"
check "conflict loser skipped"    "conflicts with 'firstc'"            "$scen_dir/mods/a9loader.log"
check "patch journaled"            "write=ok"                            "$scen_dir/mods/a9loader.log"

echo "== S7: [[patch]] whitelist entry 6 (table113 reborn)"
new_scen s7-patch host.exe table113
mod diag-table113 \
    'id = "diag-table113"' \
    'name = "Diagnostic table113"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[patch]]' \
    'target = "train_list_whitelist"' \
    'offset = 24' \
    'data = "88 00 00 00"' \
    'expect = "bb 00 00 00"'
run_host
check "tbl[6] = 136"          "tbl\[6\] = 136 (PASS)"     "$scen_dir/out.txt"
check "expect verified"       "expect=verified write=ok"  "$scen_dir/mods/a9loader.log"

echo "== S8: game_builds gate"
new_scen s8-gated host.exe none
mod gatedmod \
    'id = "gatedmod"' \
    'name = "Gated"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'game_builds = ["9.99.9999"]' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "nop"'
run_host
check "no patch applied"    "unchanged (PASS)"                        "$scen_dir/out.txt"
check "gate refusal logged" "game_builds"                             "$scen_dir/mods/a9loader.log"
check "gate reason"         "excludes running build 5.00.5634"        "$scen_dir/mods/a9loader.log"

echo "== S9: broken list files refuse loudly (empty + missing, one mod each)"
new_scen s9-badlist host.exe none
mod emptylist \
    'id = "emptylist"' \
    'name = "Empty list"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'list = "empty.txt"'
# an empty list inside the mod dir (present but empty — must refuse)
: > "$scen_dir/mods/emptylist/empty.txt"
run_host
check "no patch applied"      "unchanged (PASS)"   "$scen_dir/out.txt"
check "empty list refused"    "contains no names"   "$scen_dir/mods/a9loader.log"

new_scen s10-missinglist host.exe none
mod missinglist \
    'id = "missinglist"' \
    'name = "Missing list"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[hook]]' \
    'target = "train_list_filter"' \
    'handler = "train_list"' \
    'list = "nosuchfile.txt"'
run_host
check "no patch applied"      "unchanged (PASS)"   "$scen_dir/out.txt"
check "missing list refused"   "nosuchfile.txt"     "$scen_dir/mods/a9loader.log"

echo
if [ "$FAILED" = 0 ]; then
    echo "M2 HARNESS: ALL SCENARIOS GREEN"
else
    echo "M2 HARNESS: $FAILED FAILURE(S)"
fi
exit "$FAILED"
