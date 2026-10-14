#!/usr/bin/env bash
# M7-1 harness — direct spec-row injection (the mechanism behind "add trains
# beyond the 417 catalog"). The fake host maps the spec-table pages at the real
# RVAs (the resize/realloc/ctor anchors carry their real prologues; the accessor
# runs as-is) and a fake manager whose rows vector is a 421-row table, then
# drives the loader's `a9loader_m7_rows` export.
#
# Scenarios:
#   R0 rows-off         — no export call: the resize site stays vanilla, 421 rows
#   R1 rows             — install the resize hook, call the resize site: the hook
#                         injects AFTER the original re-pin (421 -> 422) and the
#                         game's own accessor + a row walk see the injected row
#   R2 rows-refuse      — the name ctor returns NULL: the injection refuses, the
#                         vector is untouched (no realloc)
#   R3 rows-refuse-tmpl — the template row's pFormName is NULL: refuses
#   R4 rows-order       — inject BEFORE the resize: the resize re-pins to 421 and
#                         the pre-resize growth is silently undone
#
# usage: harness/m7rows.sh
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"
source "$(dirname "$0")/lib.sh"

RUN="$REPO/harness/run/m7rows"
PREFIX="$REPO/harness/run/.prefix"
GAME_EXE="${ATRAIN9_EXE:-/run/media/frybarec/dasug/Games/steamlibrary/steamapps/common/A-Train9v5/ATrain9v5.exe}"

echo "== building the host + the Rust version.dll"
mkdir -p "$RUN"
x86_64-w64-mingw32-windres harness/host.rc -O coff -o "$RUN/hostres.o"
# --strip-debug keeps SizeOfImage at 0x30000 so the 0x1aa000 / 0x190000 /
# 0xe14000 fake pages are mappable (the mingw .debug_* sections inflate it).
x86_64-w64-mingw32-gcc -O2 -Wl,--strip-debug -o "$RUN/host.exe" harness/host.c "$RUN/hostres.o"
cargo build --release --target x86_64-pc-windows-gnu
DLL="$REPO/target/x86_64-pc-windows-gnu/release/version.dll"

echo "== extracting the FORM list for the host's form page"
python3 - "$GAME_EXE" "$RUN/form_list.bin" <<'EOF'
import sys
data = open(sys.argv[1], 'rb').read()
units = []
i = 0x6FB830
while i + 1 < len(data) and len(units) < 16384:
    u = data[i] | (data[i + 1] << 8)
    if u == 0:
        break
    units.append(u)
    i += 2
open(sys.argv[2], 'wb').write(b''.join(u.to_bytes(2, 'little') for u in units) + b'\x00\x00')
print(f"   {len(units)} units -> {sys.argv[2]}")
EOF

# the needles carry regex metacharacters — match them as FIXED strings
check() { # check <label> <needle> <file>
    if grep -qF -- "$2" "$3"; then
        echo "   [ ok ] $1"
    else
        echo "   [FAIL] $1 (missing: $2 in $3)"
        FAILED=$((FAILED + 1))
    fi
}

echo "== R0: rows-off — no export call, the resize site stays vanilla"
new_scen r0-off
run_host rows-off
LOG="$scen_dir/mods/a9loader.log"
OUT="$scen_dir/out.txt"
check "host ran"              "rows: done"                                "$OUT"
check "site unmodified"       "rows: site[0]=0x48 (unmodified)"           "$OUT"
check "count stays 421"       "rows: count after the vanilla resize = 421" "$OUT"
if grep -q "rowinject" "$LOG" 2>/dev/null; then
    echo "   [FAIL] rowinject lines present with no export call"
    FAILED=$((FAILED + 1))
else
    echo "   [ ok ] log clean of rowinject lines"
fi

echo "== R1: rows — the resize hook injects after the re-pin; a consumer sees the row"
new_scen r1-rows
run_host rows
LOG="$scen_dir/mods/a9loader.log"
OUT="$scen_dir/out.txt"
check "hook installed"        "rowinject: resize hook installed rva=0x19f150 expect=verified (inject after spec_table_resize)" "$LOG"
check "hook is live"          "rows: install rc=0 site[0]=0xff"           "$OUT"
check "resize re-pinned"      "rows resize_body: re-pinned the vector to 421 rows" "$OUT"
check "grown through realloc" "rows realloc_body: 421 -> 422 rows (game-allocator stand-in" "$OUT"
check "engine name ctor used" "rows ctor_body: fmt=form_id_%d id=421 -> form_id_421" "$OUT"
check "lanes ctor used"       "rows ctor_body: fmt=modrow_%d id=421 -> modrow_421" "$OUT"
check "inject journaled"      "rowinject: grew the spec vector to 422 rows through the game's realloc (game allocator); injected reserved row 421 cloned from template 0 (pFormName \"form_id_421\")" "$LOG"
check "count 422"             "rows: count after the hooked resize = 422" "$OUT"
check "clone marker present"  "rows: injected row bValid=1 carCount=1 marker=0xa9c0de01" "$OUT"
check "names patched"         "rows: injected names jp=modrow_421 en=modrow_421 form=form_id_421" "$OUT"
check "realloc + ctor counts" "rows: realloc calls=1 new_count=422 ctor calls=3" "$OUT"
check "consumer row walk"     "rows: walk rows=422 valid=2 reserved-id-present=1" "$OUT"
check "under FORMS_MAX"       "rows: reserved id 421 < FORMS_MAX 1024 = 1" "$OUT"

echo "== R2: rows-refuse — a NULL name from the ctor is refused, nothing mutated"
new_scen r2-refuse
run_host rows-refuse
LOG="$scen_dir/mods/a9loader.log"
OUT="$scen_dir/out.txt"
check "refusal logged"        "rowinject refused: the name constructor produced a NULL JP display lane for reserved row 421" "$LOG"
check "nothing injected"      "rows: inject with a NULL name returned 0, count=421, realloc calls=0" "$OUT"
checkf "host ran"             grep -q "rows: done"                        "$OUT"

echo "== R3: rows-refuse-tmpl — a zero-filled template (NULL pFormName) is refused"
new_scen r3-refuse-tmpl
run_host rows-refuse-tmpl
LOG="$scen_dir/mods/a9loader.log"
OUT="$scen_dir/out.txt"
check "refusal logged"        "rowinject refused: template row 0 has a NULL pFormName" "$LOG"
check "nothing injected"      "rows: inject from a NULL-named template returned 0, count=421, realloc calls=0" "$OUT"
checkf "host ran"             grep -q "rows: done"                        "$OUT"

echo "== R4: rows-order — a growth BEFORE spec_table_resize is silently undone"
new_scen r4-order
run_host rows-order
LOG="$scen_dir/mods/a9loader.log"
OUT="$scen_dir/out.txt"
check "pre-resize growth"     "rows: after a PRE-resize inject count=422" "$OUT"
check "growth undone"         "rows: after the resize count=421 (the pre-resize growth is undone)" "$OUT"
check "grow journaled"        "rowinject: grew the spec vector to 422 rows" "$LOG"

harness_done "M7-1 ROWS HARNESS"
