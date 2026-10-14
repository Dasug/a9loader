#!/usr/bin/env bash
# M6 harness — [[loose_asset]]: game files the game opens BY PATH (not content
# inside a pack) served from a loader-managed cache copy, matched by a
# case-insensitive glob over the game's own open path.
#
# The host fakes the game's CFile::Open (exactly like m3/m5) and opens three
# paths through it: an ABSOLUTE path shaped like the per-boot random temp dir
# (`<cwd>\tmpdir.adk\fe01.bin`), an exact relative path (`data\bmf\fy01.bin`),
# and a non-matching path (`data\mtfake.bin`). Evidence: the path each open
# actually USED (the redirect, from the fake CFile::Open body) + the bytes it
# read, byte-compared offline against the mod payload / the vanilla file.
#
# S1 the motivating glob on an absolute path; S2 an exact relative path; S3 a
# pattern that matches nothing (all untouched); S4 an unreadable payload
# (loud + fail-open, the original open proceeds); S5 case-insensitivity;
# S6 two mods on one pattern (overlap, earlier wins, loser logged).
#
# usage: harness/m6.sh
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"
source "$(dirname "$0")/lib.sh"

RUN="$REPO/harness/run/m6"
PREFIX="$REPO/harness/run/.prefix"

echo "== building the host + the Rust version.dll"
mkdir -p "$RUN"
x86_64-w64-mingw32-windres harness/host.rc -O coff -o "$RUN/hostres.o"
x86_64-w64-mingw32-gcc -O2 -o "$RUN/host.exe" harness/host.c "$RUN/hostres.o"
cargo build --release --target x86_64-pc-windows-gnu
cargo build --release --bin a9ml
DLL="$REPO/target/x86_64-pc-windows-gnu/release/version.dll"
A9ML="$REPO/target/release/a9ml"

echo "== building the fixtures"
mkdir -p "$RUN/data"
py_mk "$RUN/globby.bin"   48 0x11   # the *.adk\fe01.bin payload
py_mk "$RUN/exact.bin"    32 0x22   # the exact-relative-path payload
py_mk "$RUN/van_fy01.bin" 24 0xA0   # the VANILLA data\bmf\fy01.bin (different)
py_mk "$RUN/data/mtfake.bin" 40 0x55 # the non-matching path's vanilla bytes
printf 'FORM_DUMMY\x00' > "$RUN/form_list.bin"  # the host only needs a file here

# debug so the per-match line is emitted (the boot line is info-level)
DEFAULT_CFG='log_level = "debug"\n'
scen_fixtures() {
    mkdir -p "$scen_dir/data/bmf"
    cp "$RUN/data/mtfake.bin" "$scen_dir/data/"
    cp "$RUN/van_fy01.bin" "$scen_dir/data/bmf/fy01.bin"
}

echo "== S1: a GLOB over an absolute per-boot temp path (the motivating case)"
new_scen s1-glob
mod globmod \
    'id = "globmod"' \
    'name = "Glob"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[loose_asset]]' \
    'match = "*.adk\\fe01.bin"' \
    'file = "bmf/fe01.bin"'
pay globmod "bmf/fe01.bin" globby.bin
run_host loose
LOG="$scen_dir/mods/a9loader.log"
# the boot line: mod, pattern, cache path, byte count
check "boot line names the mod"       'loose: mod globmod pattern'        "$LOG"
check "boot line names the pattern"   '\*.adk.fe01.bin'                   "$LOG"
check "boot line names the cache"     'mods.cache.loose'                  "$LOG"
check "boot line carries the size"    '(48 B)'                            "$LOG"
check "hook counts the loose asset"   '1 loose asset'                     "$LOG"
# debug: one line per match, naming the redirect
check "debug match line"              'loose match:.*fe01.bin'            "$LOG"
check "debug match names the mod"     'mod globmod'                       "$LOG"
# the absolute open was redirected to the cache and read the mod's bytes
check "absolute open used the cache"  'loose abs: used=mods.cache.loose'  "$scen_dir/out.txt"
check "CFile::Open body saw the cache" 'path=mods.cache.loose'            "$scen_dir/out.txt"
checkf "absolute read == the mod payload" cmp -s "$scen_dir/read_abs.bin" "$RUN/globby.bin"
# the non-matching paths were untouched
check "relative path untouched"       'loose rel: used=data.bmf.fy01.bin' "$scen_dir/out.txt"
checkf "relative read == vanilla"     cmp -s "$scen_dir/read_rel.bin" "$RUN/van_fy01.bin"
check "non-matching path untouched"   'loose plain: used=data.mtfake.bin' "$scen_dir/out.txt"
checkf "non-matching read == vanilla" cmp -s "$scen_dir/read_plain.bin" "$RUN/data/mtfake.bin"

echo "== S2: an exact relative game path (no glob)"
new_scen s2-exact
mod exactmod \
    'id = "exactmod"' \
    'name = "Exact"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[loose_asset]]' \
    'match = "data\\bmf\\fy01.bin"' \
    'file = "bmf/fy01.bin"'
pay exactmod "bmf/fy01.bin" exact.bin
run_host loose
LOG="$scen_dir/mods/a9loader.log"
check "boot line names the exact pattern" 'pattern "data.bmf.fy01.bin"'  "$LOG"
check "exact relative redirected"         'loose rel: used=mods.cache.loose' "$scen_dir/out.txt"
checkf "relative read == the mod payload" cmp -s "$scen_dir/read_rel.bin" "$RUN/exact.bin"
# a path the pattern does NOT name stays untouched
check "absolute untouched (no match)"     'loose abs: used=.*tmpdir.adk'  "$scen_dir/out.txt"
check "non-matching path untouched"       'loose plain: used=data.mtfake.bin' "$scen_dir/out.txt"
checkf "non-matching read == vanilla"     cmp -s "$scen_dir/read_plain.bin" "$RUN/data/mtfake.bin"

echo "== S3: a pattern that matches nothing the game opens (all untouched)"
new_scen s3-nomatch
mod nomod \
    'id = "nomod"' \
    'name = "No match"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[loose_asset]]' \
    'match = "*.adk\\zz99.bin"' \
    'file = "bmf/fe01.bin"'
pay nomod "bmf/fe01.bin" globby.bin
run_host loose
LOG="$scen_dir/mods/a9loader.log"
check "boot line still emitted"    'loose: mod nomod pattern'           "$LOG"
check "absolute untouched"         'loose abs: used=.*tmpdir.adk'       "$scen_dir/out.txt"
check "relative untouched"         'loose rel: used=data.bmf.fy01.bin'  "$scen_dir/out.txt"
check "non-matching untouched"     'loose plain: used=data.mtfake.bin'  "$scen_dir/out.txt"
checkf "absolute open failed (no redirect)" \
       python3 -c "import os,sys; sys.exit(1 if os.path.exists('$scen_dir/read_abs.bin') else 0)"
checkq "no match logged"           'loose match:'                       "$LOG"

echo "== S4: an unreadable payload -> loud + fail-open (the original open proceeds)"
new_scen s4-unreadable
mod badmod \
    'id = "badmod"' \
    'name = "Bad payload"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[loose_asset]]' \
    'match = "data\\bmf\\fy01.bin"' \
    'file = "bmf/gone.bin"'
# NOTE: the payload is deliberately NOT created
run_host loose
LOG="$scen_dir/mods/a9loader.log"
check "loud line names the mod"       'loose: mod badmod pattern'       "$LOG"
check "loud line says unreadable"     'unreadable'                      "$LOG"
check "loud line says fail-open"      'fail-open'                       "$LOG"
checkq "no successful redirect line"  'loose: mod badmod pattern .* -> ' "$LOG"
check "relative path untouched (original open)" 'loose rel: used=data.bmf.fy01.bin' "$scen_dir/out.txt"
checkf "relative read == vanilla (untouched)"   cmp -s "$scen_dir/read_rel.bin" "$RUN/van_fy01.bin"

echo "== S5: the pattern is case-insensitive (uppercase manifest, lowercase path)"
new_scen s5-case
mod casemod \
    'id = "casemod"' \
    'name = "Case"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[loose_asset]]' \
    'match = "DATA\\BMF\\FY01.BIN"' \
    'file = "bmf/fy01.bin"'
pay casemod "bmf/fy01.bin" exact.bin
run_host loose
LOG="$scen_dir/mods/a9loader.log"
check "uppercase pattern normalized in the boot line" 'pattern "data.bmf.fy01.bin"' "$LOG"
check "relative redirected despite case" 'loose rel: used=mods.cache.loose' "$scen_dir/out.txt"
checkf "relative read == the mod payload" cmp -s "$scen_dir/read_rel.bin" "$RUN/exact.bin"

echo "== S6: two mods on the same pattern -> overlap, earlier wins, loser logged"
new_scen s6-overlap
mod lo_a \
    'id = "lo_a"' \
    'name = "Loose A"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    '' \
    '[[loose_asset]]' \
    'match = "*.adk\\fe01.bin"' \
    'file = "bmf/a.bin"'
pay lo_a "bmf/a.bin" globby.bin
mod lo_b \
    'id = "lo_b"' \
    'name = "Loose B"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    '' \
    '[[loose_asset]]' \
    'match = "*.ADK\\FE01.BIN"' \
    'file = "bmf/b.bin"'
pay lo_b "bmf/b.bin" exact.bin
run_host loose
LOG="$scen_dir/mods/a9loader.log"
check "overlap reported"            'overlap on loose:'               "$LOG"
check "loser refused (not fatal)"   'refused (overlap loser'          "$LOG"
check "exactly one loose asset served" '1 loose asset'                "$LOG"
check "winner's redirect taken"     'loose abs: used=mods.cache.loose' "$scen_dir/out.txt"
checkf "absolute read == the WINNER's payload" cmp -s "$scen_dir/read_abs.bin" "$RUN/globby.bin"

harness_done "M6 HARNESS"
