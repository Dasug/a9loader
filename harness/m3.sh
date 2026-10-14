#!/usr/bin/env bash
# Merge-overlay harness — the runtime overlay under wine. The fake-game host carries a
# fake CFile::Open at the real anchor RVA with the real expect bytes (plus a
# tail-jump body), so the loader's trampoline mechanics are exercised for
# real: expect verification, hook install, redirect to the loader-managed
# cache, and passthrough for unmatched paths.
#
# Content is verified byte-exactly: the cache must equal an `a9ml mkpack`-
# built pack with the same entries in the same order (the merge keeps the
# original entry order and appends adds; the encoder is deterministic —
# proved byte-parity).
#
# usage: harness/m3.sh
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"
source "$(dirname "$0")/lib.sh"

RUN="$REPO/harness/run/m3"
PREFIX="$REPO/harness/run/.prefix"
GAME_EXE="${ATRAIN9_EXE:-/run/media/frybarec/dasug/Games/steamlibrary/steamapps/common/A-Train9v5/ATrain9v5.exe}"

echo "== building the host + the Rust version.dll"
mkdir -p "$RUN"
x86_64-w64-mingw32-windres harness/host.rc -O coff -o "$RUN/hostres.o"
x86_64-w64-mingw32-gcc -O2 -o "$RUN/host.exe" harness/host.c "$RUN/hostres.o"
cargo build --release --target x86_64-pc-windows-gnu
cargo build --release --bin a9ml
DLL="$REPO/target/x86_64-pc-windows-gnu/release/version.dll"
A9ML="$REPO/target/release/a9ml"

mk() { # mk <name> <payload-bytes...>
    printf '%b' "$2" > "$RUN/$1"
}

mk_csv() { # mk_csv <outfile> <row1> <row2> ...  (cells separated by '|')
    # UTF-16 (BOM) CSV, CRLF, every field quoted — the game's own table style
    # and the canonical form a9-formats writes for changed/added rows.
    python3 - "$@" <<'EOF'
import csv, sys
out = sys.argv[1]
rows = [r.split('|') for r in sys.argv[2:]]
with open(out, 'w', encoding='utf-16', newline='') as f:
    w = csv.writer(f, quoting=csv.QUOTE_ALL, lineterminator='\r\n')
    w.writerows(rows)
EOF
}

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

echo "== building the synthetic packs (a9ml mkpack)"
mkdir -p "$RUN/data"
mk a.bin 'AAAA'
mk b.bin 'BBBBBBBB'
mk n1.bin 'N1N1'
mk n2.bin 'N2N2N2'
mk csv1 'C1,C1,C1\n'
mk csv2 'C2\nC2\n'
"$A9ML" mkpack --container images --out "$RUN/data/mtfake.bin" \
    "a.dds=64:64:$RUN/a.bin" "b.dds=128:128:$RUN/b.bin"
"$A9ML" mkpack --container csvs --out "$RUN/data/csvp.bin" \
    "t1.csv=$RUN/csv1"

# This is the MERGE harness — pin merge explicitly (inject is the default now)
DEFAULT_CFG='pack_serving = "merge"\n'
scen_fixtures() {
    cp "$RUN/data/mtfake.bin" "$RUN/data/csvp.bin" "$scen_dir/data/"
}

echo "== S1: passthrough (no mods — no hook installed)"
new_scen s1-passthrough
run_host overlaypack
check "opened the original pack" "path=data.mtfake.bin"      "$scen_dir/out.txt"
check "mods: none" "mods: none — pure proxy" "$scen_dir/mods/a9loader.log"
if grep -q "overlay: cfile_open hooked" "$scen_dir/mods/a9loader.log" 2>/dev/null; then
    echo "   [FAIL] hook installed with no overlay mods"
    FAILED=$((FAILED + 1))
else
    echo "   [ ok ] no hook without overlay mods"
fi

echo "== S2: [[pack]] entry add + cache reuse on second run"
new_scen s2-add
mod addone \
    'id = "addone"' \
    'name = "Add one"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[pack]]' \
    'pack = "mtfake"' \
    '' \
    '[[pack.entry]]' \
    'name = "n1.dds"' \
    'width = 16' \
    'height = 16' \
    'file = "entries/n1.bin"'
pay addone "entries/n1.bin" n1.bin
run_host overlaypack
check "redirected to the cache" "path=mods.cache.mtfake.bin" "$scen_dir/out.txt"
check "merge journaled"        "1 entry merged (1 added"      "$scen_dir/mods/a9loader.log"
check "hook installed"         "overlay: cfile_open hooked"   "$scen_dir/mods/a9loader.log"
checkf "cache exists"          test -f                         "$scen_dir/mods/cache/mtfake.bin"
checkf "cache key exists"      test -f                         "$scen_dir/mods/cache/mtfake.key"
# byte-exact content: the cache must equal a mkpack-built pack with the same
# entries in the same order
"$A9ML" mkpack --container images --out "$scen_dir/expected.bin" \
    "a.dds=64:64:$RUN/a.bin" "b.dds=128:128:$RUN/b.bin" "n1.dds=16:16:$RUN/n1.bin"
checkf "cache == expected merged pack" cmp -s "$scen_dir/mods/cache/mtfake.bin" "$scen_dir/expected.bin"
# second run: unchanged mod set -> cache reused
run_host overlaypack
check "cache reused on 2nd run" "cache fresh — reusing"  "$scen_dir/mods/a9loader.log"
checkf "still the merged cache"  cmp -s "$scen_dir/mods/cache/mtfake.bin" "$scen_dir/expected.bin"

echo "== S3: [[pack]] entry replace"
new_scen s3-replace
mod repl1 \
    'id = "repl1"' \
    'name = "Replace a"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[pack]]' \
    'pack = "mtfake"' \
    '' \
    '[[pack.entry]]' \
    'name = "a.dds"' \
    'width = 32' \
    'height = 32' \
    'file = "entries/a2.bin"'
pay repl1 "entries/a2.bin" n2.bin
run_host overlaypack
check "redirected"        "path=mods.cache.mtfake.bin" "$scen_dir/out.txt"
check "replace journaled" "1 entry merged (0 added, 1 replaced" "$scen_dir/mods/a9loader.log"
"$A9ML" mkpack --container images --out "$scen_dir/expected.bin" \
    "a.dds=32:32:$RUN/n2.bin" "b.dds=128:128:$RUN/b.bin"
checkf "cache == expected replaced pack" cmp -s "$scen_dir/mods/cache/mtfake.bin" "$scen_dir/expected.bin"

echo "== S4: [[csv]] table replace+add"
new_scen s4-csv
mod csvmod \
    'id = "csvmod"' \
    'name = "Csv tables"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[csv]]' \
    'name = "t1.csv"' \
    'file = "csv/t1.csv"' \
    '' \
    '[[csv]]' \
    'name = "t2.csv"' \
    'file = "csv/t2.csv"'
pay csvmod "csv/t1.csv" csv2
pay csvmod "csv/t2.csv" csv1
run_host overlaycsv
check "redirected to csvp cache" "path=mods.cache.csvp.bin" "$scen_dir/out.txt"
check "csv merge journaled"       "2 table(s) merged (1 added, 1 replaced" "$scen_dir/mods/a9loader.log"
"$A9ML" mkpack --container csvs --out "$scen_dir/expected.bin" \
    "t1.csv=$RUN/csv2" "t2.csv=$RUN/csv1"
checkf "cache == expected csvp" cmp -s "$scen_dir/mods/cache/csvp.bin" "$scen_dir/expected.bin"

echo "== S5: overlap — later mod's entry refused"
new_scen s5-overlap
mod firstm \
    'id = "firstm"' \
    'name = "First"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    '' \
    '[[pack]]' \
    'pack = "mtfake"' \
    '' \
    '[[pack.entry]]' \
    'name = "n1.dds"' \
    'width = 16' \
    'height = 16' \
    'file = "entries/n1.bin"'
pay firstm "entries/n1.bin" n1.bin
mod secondm \
    'id = "secondm"' \
    'name = "Second"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    '' \
    '[[pack]]' \
    'pack = "mtfake"' \
    '' \
    '[[pack.entry]]' \
    'name = "n1.dds"' \
    'width = 99' \
    'height = 99' \
    'file = "entries/n1x.bin"'
pay secondm "entries/n1x.bin" n2.bin
run_host overlaypack
check "overlap reported"        "overlap on pack:mtfake:n1.dds" "$scen_dir/mods/a9loader.log"
check "loser refused"           "refused (overlap loser)"         "$scen_dir/mods/a9loader.log"
# only the first mod's entry applies
"$A9ML" mkpack --container images --out "$scen_dir/expected.bin" \
    "a.dds=64:64:$RUN/a.bin" "b.dds=128:128:$RUN/b.bin" "n1.dds=16:16:$RUN/n1.bin"
checkf "cache == first-mod result" cmp -s "$scen_dir/mods/cache/mtfake.bin" "$scen_dir/expected.bin"

echo "== S6: stale cache rebuilt when the mod set changes"
new_scen s6-stale
mod addone \
    'id = "addone"' \
    'name = "Add one"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[pack]]' \
    'pack = "mtfake"' \
    '' \
    '[[pack.entry]]' \
    'name = "n1.dds"' \
    'width = 16' \
    'height = 16' \
    'file = "entries/n1.bin"'
pay addone "entries/n1.bin" n1.bin
run_host overlaypack
check "first merge" "1 entry merged (1 added" "$scen_dir/mods/a9loader.log"
# a second mod joins — the fingerprint changes, the cache must rebuild
mod addtwo \
    'id = "addtwo"' \
    'name = "Add two"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[pack]]' \
    'pack = "mtfake"' \
    '' \
    '[[pack.entry]]' \
    'name = "n2.dds"' \
    'width = 24' \
    'height = 24' \
    'file = "entries/n2.bin"'
pay addtwo "entries/n2.bin" n2.bin
run_host overlaypack
check "rebuild after mod set change" "2 entries merged (2 added" "$scen_dir/mods/a9loader.log"
"$A9ML" mkpack --container images --out "$scen_dir/expected.bin" \
    "a.dds=64:64:$RUN/a.bin" "b.dds=128:128:$RUN/b.bin" "n1.dds=16:16:$RUN/n1.bin" "n2.dds=24:24:$RUN/n2.bin"
checkf "cache == both-mods result" cmp -s "$scen_dir/mods/cache/mtfake.bin" "$scen_dir/expected.bin"

echo "== S7: missing payload — fail open (original served, loud log)"
new_scen s7-missing
mod brokenmod \
    'id = "brokenmod"' \
    'name = "Broken"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[pack]]' \
    'pack = "mtfake"' \
    '' \
    '[[pack.entry]]' \
    'name = "x.dds"' \
    'width = 1' \
    'height = 1' \
    'file = "entries/nosuchfile.bin"'
run_host overlaypack
check "original served"   "path=data.mtfake.bin"             "$scen_dir/out.txt"
check "loud failure"       "serving the original this boot"      "$scen_dir/mods/a9loader.log"

echo "== S8: [[csv]] row patches — two mods on one table (replace a cell + add a row)"
new_scen s8-csv-rows
mk_csv "$RUN/base_t1.csv" "KEY|VAL" "R1|a" "R2|b" "R3|c"
mk_csv "$RUN/a_patch.csv" "R2|B2"
mk_csv "$RUN/b_patch.csv" "R4|d"
mk_csv "$RUN/exp_t1.csv"  "KEY|VAL" "R1|a" "R2|B2" "R3|c" "R4|d"
# the scenario's base csvp carries a real UTF-16 table (the shared one is bytes)
"$A9ML" mkpack --container csvs --out "$scen_dir/data/csvp.bin" "t1.csv=$RUN/base_t1.csv"
mod rowedit \
    'id = "rowedit"' \
    'name = "Row edit"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    '' \
    '[[csv]]' \
    'name = "t1.csv"' \
    'file = "csv/a.patch.csv"' \
    'mode = "rows"'
pay rowedit "csv/a.patch.csv" a_patch.csv
mod rowadd \
    'id = "rowadd"' \
    'name = "Row add"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    '' \
    '[[csv]]' \
    'name = "t1.csv"' \
    'file = "csv/b.patch.csv"' \
    'mode = "rows"'
pay rowadd "csv/b.patch.csv" b_patch.csv
run_host overlaycsv
check "redirected to csvp cache" "path=mods.cache.csvp.bin" "$scen_dir/out.txt"
check "one table, two row patches" \
    "1 table(s) merged (0 added, 1 replaced, 2 row patch(es) (1 replaced, 1 added" \
    "$scen_dir/mods/a9loader.log"
# byte-exact: an independently built csvp with the expected table
"$A9ML" mkpack --container csvs --out "$scen_dir/expected.bin" "t1.csv=$RUN/exp_t1.csv"
checkf "cache == expected row-merged csvp" cmp -s "$scen_dir/mods/cache/csvp.bin" "$scen_dir/expected.bin"

echo "== S9: [[csv]] row overlap — the same row key, the earlier mod keeps it"
new_scen s9-csv-row-overlap
mk_csv "$RUN/base_t1o.csv" "KEY|VAL" "R1|a" "R2|b" "R3|c"
mk_csv "$RUN/o1.csv" "R2|from-first"
mk_csv "$RUN/o2.csv" "R2|from-second"
mk_csv "$RUN/exp_o.csv" "KEY|VAL" "R1|a" "R2|from-first" "R3|c"
"$A9ML" mkpack --container csvs --out "$scen_dir/data/csvp.bin" "t1.csv=$RUN/base_t1o.csv"
mod firsto \
    'id = "firsto"' \
    'name = "First row"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    '' \
    '[[csv]]' \
    'name = "t1.csv"' \
    'file = "csv/o1.csv"' \
    'mode = "rows"'
pay firsto "csv/o1.csv" o1.csv
mod secondo \
    'id = "secondo"' \
    'name = "Second row"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    '' \
    '[[csv]]' \
    'name = "t1.csv"' \
    'file = "csv/o2.csv"' \
    'mode = "rows"'
pay secondo "csv/o2.csv" o2.csv
run_host overlaycsv
check "row overlap reported" "overlap on csv:t1.csv:R2"          "$scen_dir/mods/a9loader.log"
check "loser row refused"     "1 row patch(es) refused (overlap loser)" "$scen_dir/mods/a9loader.log"
"$A9ML" mkpack --container csvs --out "$scen_dir/expected.bin" "t1.csv=$RUN/exp_o.csv"
checkf "cache == first mod's row result" cmp -s "$scen_dir/mods/cache/csvp.bin" "$scen_dir/expected.bin"

echo "== S10: [[csv]] row patch from a plain UTF-8 file (cross-mod authoring)"
new_scen s10-csv-utf8
mk_csv "$RUN/base_u1.csv" "KEY|VAL" "R1|a" "R2|b"
# a normal editor's UTF-8 patch file (no BOM); the loader re-encodes the row
printf '"R2","B2"\r\n"R3","c"\r\n' > "$RUN/u_patch.csv"
mk_csv "$RUN/exp_u1.csv" "KEY|VAL" "R1|a" "R2|B2" "R3|c"
"$A9ML" mkpack --container csvs --out "$scen_dir/data/csvp.bin" "t1.csv=$RUN/base_u1.csv"
mod utf8rows \
    'id = "utf8rows"' \
    'name = "UTF-8 rows"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[csv]]' \
    'name = "t1.csv"' \
    'file = "csv/u.patch.csv"' \
    'mode = "rows"'
pay utf8rows "csv/u.patch.csv" u_patch.csv
run_host overlaycsv
check "redirected to csvp cache" "path=mods.cache.csvp.bin" "$scen_dir/out.txt"
check "one replaced, one added"  "2 row patch(es) (1 replaced, 1 added" "$scen_dir/mods/a9loader.log"
"$A9ML" mkpack --container csvs --out "$scen_dir/expected.bin" "t1.csv=$RUN/exp_u1.csv"
checkf "cache == expected utf8-row-merged csvp" cmp -s "$scen_dir/mods/cache/csvp.bin" "$scen_dir/expected.bin"

harness_done "M3 HARNESS"