#!/usr/bin/env bash
# M7-2 harness — the `[[train]]` section. The fake host maps the real anchor
# pages (filter/whitelist/FORM, cfile_open, the M7-0 clamp, the M7 tab/form
# fixes, the M7-1 spec-table row helpers and the two M7-2 anchors) BEFORE the
# proxy DLL loads, so the loader's REAL boot runs: it scans the scenario's
# mods, compiles the `[[train]]`, installs the composed keep-set (with the
# reserved ids), the overlay plan and the injection hook. The host then calls
# the `train_list_builder` site to fire the injection, reads the injected row
# through the game's own accessor, opens `data\csvp.bin` through the hooked
# CFile::Open (the overlay merges the derived rows) and dumps the merged tables.
#
# Scenarios:
#   T0 train-off    — a mod with no [[train]]: nothing installed, 421 rows
#   T1 train        — one [[train]]: allocation + naming, the four derived rows
#                     byte-exactly in the merged csvp (the pattern group
#                     appended positionally), the injected row, the reserved id
#                     admitted, the name-pointer clone fix exercised
#   T2 train-refuse — a bad model dir: refused, nothing injected, 421 rows
#   T3 train-range  — the id range exhausted: the overflowing train refused
#   T4 train-collide— a FORM name that collides with the shipped catalog: refused
#
# usage: harness/m7train.sh
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"
source "$(dirname "$0")/lib.sh"

RUN="$REPO/harness/run/m7train"
PREFIX="$REPO/harness/run/.prefix"
GAME_EXE="${ATRAIN9_EXE:-/run/media/frybarec/dasug/Games/steamlibrary/steamapps/common/A-Train9v5/ATrain9v5.exe}"

# the needles carry regex metacharacters — match them as FIXED strings
check() { # check <label> <needle> <file>
    if grep -qF -- "$2" "$3"; then
        echo "   [ ok ] $1"
    else
        echo "   [FAIL] $1 (missing: $2 in $3)"
        FAILED=$((FAILED + 1))
    fi
}
checkq() { # checkq <label> <needle> <file> — assert the needle is ABSENT
    if grep -qF -- "$2" "$3"; then
        echo "   [FAIL] $1 (unexpected: $2 in $3)"
        FAILED=$((FAILED + 1))
    else
        echo "   [ ok ] $1"
    fi
}

echo "== building the host + the Rust version.dll"
mkdir -p "$RUN"
x86_64-w64-mingw32-windres harness/host.rc -O coff -o "$RUN/hostres.o"
x86_64-w64-mingw32-gcc -O2 -Wl,--strip-debug -o "$RUN/host.exe" harness/host.c "$RUN/hostres.o"
cargo build --release --target x86_64-pc-windows-gnu
cargo build --release --bin a9ml
DLL="$REPO/target/x86_64-pc-windows-gnu/release/version.dll"
A9ML="$REPO/target/release/a9ml"

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

echo "== building the fake csvp (4 train tables) + the icon payloads"
mkdir -p "$RUN"
mk_csv() { # mk_csv <outfile> <row1> <row2> ...  (cells separated by '|')
    python3 - "$@" <<'EOF'
import csv, sys
out = sys.argv[1]
rows = [r.split('|') for r in sys.argv[2:]]
with open(out, 'w', encoding='utf-16', newline='') as f:
    w = csv.writer(f, quoting=csv.QUOTE_ALL, lineterminator='\r\n')
    w.writerows(rows)
EOF
}
# 29 / 18 / 11 / 19 columns
H29=$(python3 -c "print('|'.join(['h']*29))")
H18=$(python3 -c "print('|'.join(['h']*18))")
H11=$(python3 -c "print('|'.join(['h']*11))")
H19=$(python3 -c "print('|'.join(['h']*19))")
DON29=$(python3 -c "print('|'.join(['FORM_AR4']+['d']*28))")
DON11=$(python3 -c "print('|'.join(['FORM_AR4']+['d']*10))")
DON19=$(python3 -c "print('|'.join(['FORM_AR4']+['d']*18))")
# a shipped pattern group for the donor: head + 9 keyless continuation + separator
DON18=$(python3 -c "print('|'.join(['FORM_AR4']+['d']*17))")
CONT=$(python3 -c "print('|'.join(['']*18))")
mk_csv "$RUN/tf.csv"  "$H29" "$DON29"
mk_csv "$RUN/tp.csv"  "$H18" "$DON18" "$CONT" "$CONT" "$CONT" "$CONT" "$CONT" "$CONT" "$CONT" "$CONT" "$CONT" "$CONT"
mk_csv "$RUN/tc.csv"  "$H11" "$DON11"
mk_csv "$RUN/tu.csv"  "$H19" "$DON19"
"$A9ML" mkpack --container csvs --out "$RUN/csvp.bin" \
    "train_form.csv=$RUN/tf.csv" "train_pattern.csv=$RUN/tp.csv" \
    "train_comment.csv=$RUN/tc.csv" "train_custom.csv=$RUN/tu.csv"
# three tiny icon payloads
python3 - "$RUN" <<'EOF'
import sys, os
d = sys.argv[1]
for n in ("form_trainmod_sample_f.dds", "form_trainmod_sample_s.dds", "form_trainmod_sample_s_1.dds"):
    open(os.path.join(d, n), "wb").write(bytes(range(16)))
EOF

# M3/M5 both work through the merge overlay here; pin it so the derived rows are
# verified in a cache file the harness can extract.
DEFAULT_CFG='pack_serving = "merge"\nasset_serving = "merge"\n'
scen_fixtures() {
    cp "$RUN/csvp.bin" "$scen_dir/data/csvp.bin"
}

run_train() {
    run_host train
}
run_train_quiet() {
    run_host train-nodump
}

# the four train tables out of the merged cache, for the byte-exact row checks
dump_cache() { # dump_cache <scen_dir> <table>
    local d="$1" t="$2"
    mkdir -p "$d/merged/data"
    cp "$d/mods/cache/csvp.bin" "$d/merged/data/csvp.bin"
    "$A9ML" extract-csv --game "$d/merged" --table "$t" --utf8 2>/dev/null || true
}

echo "== T0: train-off — a mod with no [[train]] installs nothing"
new_scen t0-off
mod plain \
    'id = "plain"' \
    'name = "Plain"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[csv]]' \
    'name = "train_comment.csv"' \
    'file = "csv/row.csv"' \
    'mode = "rows"'
pay plain csv/row.csv tc.csv
run_train_quiet
OUT="$scen_dir/out.txt"; LOG="$scen_dir/mods/a9loader.log"
check "host ran"            "train: done"                          "$OUT"
check "vector stays 421"    "train: vector rows after injection = 421" "$OUT"
checkq "no allocation line" "allocated FORM name"                  "$LOG"
checkq "no injection line"  "train injection"                      "$LOG"

echo "== T1: train — allocation, the four derived rows, the injection"
new_scen t1-train
mod trainmod \
    'id = "trainmod"' \
    'name = "Train mod"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[train]]' \
    'key = "sample"' \
    'name = "サンプル気動車"' \
    'name_en = "Sample Railcar"' \
    'price = 42000000' \
    'capacity = 88' \
    'running_cost = 24000' \
    'cruise_speed = 70' \
    'fare = 250'
for n in f s s_1; do
    pay trainmod "assets/form_trainmod_sample_$n.dds" "form_trainmod_sample_$n.dds"
done
run_train
OUT="$scen_dir/out.txt"; LOG="$scen_dir/mods/a9loader.log"
# allocation + naming
check "mod summary counts the train" "1 train)"                    "$LOG"
check "allocation + naming" "train 'sample': allocated FORM name FORM_TRAINMOD_SAMPLE (spec row id 421, donor FORM_AR4, 1 car(s))" "$LOG"
check "icon host auto -> tp" "asset form_trainmod_sample_f.dds: host auto -> tp" "$LOG"
# the injection
check "injection installed" "train injection: train_list_builder rva=0x19d9c0 expect=verified (1 reserved row(s) queued" "$LOG"
check "injection grew"      "train injection: grew the spec vector by 1 row(s)" "$LOG"
check "builder ran"         "train tlb_body: purchase-list builder ran" "$OUT"
check "clone fix exercised" "train: builder calls=1 clone_ref calls=4" "$OUT"
check "vector grown"        "train: vector rows after injection = 422" "$OUT"
check "annotator bound widened" "train: annotator bound now = 422 (was 417)" "$OUT"
check "bound widening logged"  "preview annotator bound widened to 422 rows (was 417)" "$LOG"
check "injected fields"     "train: injected row 421 bValid=1 special=0 cruise=70 fare=250 carCount=1" "$OUT"
check "injected jp name"    "train: injected names jp=30b5 30f3 30d7 30eb 6c17 52d5 8eca " "$OUT"
check "injected en name"    "en=0053 0061 006d 0070 006c 0065 0020 0052 0061 0069 006c 0063 0061 0072 " "$OUT"
check "injected form name"  "form=0066 006f 0072 006d 005f 0074 0072 0061 0069 006e 006d 006f 0064 005f 0073 0061 006d 0070 006c 0065 " "$OUT"
check "reserved id admitted" "train: reserved id 421 kept by the composed filter = 1" "$OUT"
# the derived rows in the merged csvp (byte-exact text via extract-csv)
check "csvp merged"         "4 row patch(es) (0 replaced, 3 added, 11 group-appended)" "$LOG"
TF="$(dump_cache "$scen_dir" train_form.csv)"
echo "$TF" > "$scen_dir/train_form.txt"
check "train_form row derived" '"FORM_TRAINMOD_SAMPLE","ON","旅客列車","ＪＲ四国","70","","低","250","低速車両","低速車両","低速車両_貨物警笛","低速車両","0","0","0","1","","ディーゼル車","ディーゼル車","サンプル気動車","","","","","","","","",""' "$scen_dir/train_form.txt"
TC="$(dump_cache "$scen_dir" train_comment.csv)"
echo "$TC" > "$scen_dir/train_comment.txt"
check "train_comment row derived" '"FORM_TRAINMOD_SAMPLE","サンプル気動車","Sample Railcar","","","","","","","",""' "$scen_dir/train_comment.txt"
TU="$(dump_cache "$scen_dir" train_custom.csv)"
echo "$TU" > "$scen_dir/train_custom.txt"
check "train_custom row derived" '"FORM_TRAINMOD_SAMPLE","","","1","0","0","","","","","","","","","","","","","サンプル気動車"' "$scen_dir/train_custom.txt"
TP="$(dump_cache "$scen_dir" train_pattern.csv)"
echo "$TP" > "$scen_dir/train_pattern.txt"
check "pattern head row derived" '"FORM_TRAINMOD_SAMPLE","kh32","_t_kh32_a","88","24000","42000000","1","","","","","","","","","","○","1"' "$scen_dir/train_pattern.txt"
check "pattern continuation keyless" '"","","","","","","","","","","","","","","","","","2"' "$scen_dir/train_pattern.txt"
check "pattern separator appended" '"","","","","","","","","","","","","","","","","",""' "$scen_dir/train_pattern.txt"

echo "== T2: train-refuse — a bad model dir is refused, nothing injected"
new_scen t2-refuse
mod trainmod \
    'id = "trainmod"' \
    'name = "Train mod"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[train]]' \
    'key = "sample"' \
    'name = "サンプル"' \
    'model = { dir = "../evil", mesh = "_t_x_a" }'
run_train_quiet
OUT="$scen_dir/out.txt"; LOG="$scen_dir/mods/a9loader.log"
check "refusal logged"   "train 'sample' (#1): invalid: model dir/mesh must be plain names" "$LOG"
check "no injection"     "train: vector rows after injection = 421" "$OUT"
checkq "no injection line" "train injection" "$LOG"

echo "== T3: train-range — the id range is exhausted, the overflow refused"
new_scen t3-range
mkdir -p "$scen_dir/mods/bigmod"
{
    printf 'id = "bigmod"\nname = "Big"\nversion = "1.0.0"\nauthor = "test"\n'
    for i in $(seq 0 603); do
        printf '\n[[train]]\nkey = "t%d"\nname = "T%d"\n' "$i" "$i"
    done
} > "$scen_dir/mods/bigmod/mod.toml"
run_train_quiet
LOG="$scen_dir/mods/a9loader.log"
check "range refusal logged" "allocated id 1024 >= the composed keep-set width 1024" "$LOG"
check "the last train refused" "train 't603' (#604): invalid: allocated id 1024" "$LOG"

echo "== T4: train-collide — an allocated FORM name that is a shipped catalog name"
new_scen t4-collide
mod dummy \
    'id = "dummy"' \
    'name = "Dummy"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[train]]' \
    'key = "02"' \
    'name = "Alias"'
run_train_quiet
OUT="$scen_dir/out.txt"; LOG="$scen_dir/mods/a9loader.log"
check "collision refused" "train '02': the allocated FORM name FORM_DUMMY_02 is a SHIPPED catalog name" "$LOG"
check "nothing injected"  "train: vector rows after injection = 421" "$OUT"

echo "== T5: train-retry — the builder's first call happens BEFORE the rows are filled"
# The live shape that the harness missed: train_list_builder is called more than
# once per boot, and the first call predates the CSV pipeline's row fill, so the
# donor is still bValid==0. The loader must defer (warn once) and retry at the
# next call instead of giving up.
new_scen t5-retry
mod trainmod \
    'id = "trainmod"' \
    'name = "Train mod"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[train]]' \
    'key = "sample"' \
    'name = "サンプル気動車"' \
    'name_en = "Sample Railcar"' \
    'price = 42000000' \
    'capacity = 88' \
    'running_cost = 24000' \
    'cruise_speed = 70' \
    'fare = 250'
for n in f s s_1; do
    pay trainmod "assets/form_trainmod_sample_$n.dds" "form_trainmod_sample_$n.dds"
done
run_host train_retry
OUT="$scen_dir/out.txt"; LOG="$scen_dir/mods/a9loader.log"
check  "phase 1 fired unpopulated" "train: retry phase 1 fired with the donor unpopulated" "$OUT"
check  "phase 2 fired populated"   "train: retry phase 2 fired with the donor populated" "$OUT"
check  "deferred, not refused"     "train injection deferred: train 'FORM_TRAINMOD_SAMPLE': donor row " "$LOG"
check  "deferral is a warn"        "WARN: train injection deferred" "$LOG"
checkq "no refusal ERR"            "train injection refused" "$LOG"
check  "retried and injected"      "train injection: grew the spec vector by 1 row(s)" "$LOG"
check  "injected at attempt 2"     "(attempt 2 of the builder hook)" "$LOG"
check  "builder called 3x"         "train: builder calls=3 clone_ref calls=4" "$OUT"
check  "third call is probe-only" "train: third call (post-injection, probe only)" "$OUT"
check  "injected lanes are ours"  "train probe lanes: injected(421) [n0=サンプル気動車 n1=Sample Railcar" "$LOG"
check  "vector grown exactly once" "train: vector rows after injection = 422" "$OUT"
check  "annotator bound widened"    "train: annotator bound now = 422 (was 417)" "$OUT"
check  "injected fields"           "train: injected row 421 bValid=1 special=0 cruise=70 fare=250 carCount=1" "$OUT"
check  "reserved id admitted"      "train: reserved id 421 kept by the composed filter = 1" "$OUT"

harness_done "M7-2 TRAIN HARNESS"
