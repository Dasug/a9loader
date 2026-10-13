#!/usr/bin/env bash
# M5 W1 harness — the i.dat family-A row model: [[asset]] payloads appended to
# a family-A host pack's decoded stream + the matching merged i.dat rows.
#
# The host fakes the game's CFile::Open (exactly like m3) and opens
# `data\i.dat` then `data\vp.bin`, so the loader's real overlay path runs:
# every host pack's cache is built FIRST (atomicity), then the merged index
# whose row offsets come from the shared `plan_family_a`. Content is verified
# byte-exactly against a9ml-built expectation files (the same pattern as m3).
#
# S1–S4 exercise the single-file `[[asset]]` form; S5 the folder form with
# `host = "auto"` (the host pack resolved from the embedded per-build dir
# index — a brand-new `train\<name>` dir rides the `train` prefix to vp); S6
# the refusals (a dir with no shipped row and no known prefix, and a real
# ambiguous dir whose vanilla rows span packs).
#
# usage: harness/m5.sh
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"

RUN="$REPO/harness/run/m5"
PREFIX="$REPO/harness/run/.prefix"

echo "== building the host + the Rust version.dll"
mkdir -p "$RUN"
x86_64-w64-mingw32-windres harness/host.rc -O coff -o "$RUN/hostres.o"
x86_64-w64-mingw32-gcc -O2 -o "$RUN/host.exe" harness/host.c "$RUN/hostres.o"
cargo build --release --target x86_64-pc-windows-gnu
cargo build --release --bin a9ml
DLL="$REPO/target/x86_64-pc-windows-gnu/release/version.dll"
A9ML="$REPO/target/release/a9ml"

py_mk() { # py_mk <file> <size> <seed> — a deterministic ramp payload
    python3 - "$1" "$2" "$3" <<'EOF'
import sys
path, size, seed = sys.argv[1], int(sys.argv[2], 0), int(sys.argv[3], 0)
open(path, "wb").write(bytes((seed + i) & 0xff for i in range(size)))
EOF
}

echo "== building the fixtures (a 64 B family-A vp + a one-row i.dat)"
mkdir -p "$RUN/data"
py_mk "$RUN/vbase.bin" 64 0x10    # the vanilla vp decoded stream V (64 bytes)
py_mk "$RUN/probe.x"   96 0x80    # asset payload P1 (96 bytes)
py_mk "$RUN/probe2.x"  96 0x40    # asset payload P2 (96 bytes)
py_mk "$RUN/lpay1.bin" 64 0x70    # M5-U: a preview asset payload
py_mk "$RUN/lpay2.bin" 64 0x90    # M5-U: a second preview asset payload
py_mk "$RUN/ovr.bin"   32 0x33    # M5-U: a vanilla-row override payload
py_mk "$RUN/treenew.bin" 40 0x55  # M5-U: a non-name-keyed group asset
printf 'FORM_DUMMY\x00' > "$RUN/form_list.bin"   # the host only needs a file here
"$A9ML" mkpack --container a    --out "$RUN/data/vp.bin" "$RUN/vbase.bin"
"$A9ML" mkpack --container idat --out "$RUN/data/i.dat" '_t_van_a.x=train\van:37:0'

FAILED=0
scen_dir=""

new_scen() {
    scen_dir="$RUN/$1"
    rm -rf "$scen_dir"
    mkdir -p "$scen_dir/data"
    cp "$RUN/host.exe" "$scen_dir/"
    cp "$DLL" "$scen_dir/version.dll"
    cp "$RUN/form_list.bin" "$scen_dir/"
    cp "$RUN/data/vp.bin" "$RUN/data/i.dat" "$scen_dir/data/"
    # the S1–S6 scenarios are the MERGE regression: pin both mechanisms so the
    # loader's default (tailgate) does not change what they assert
    printf 'pack_serving = "merge"\nasset_serving = "merge"\n' > "$scen_dir/a9loader.toml"
}

run_host() {
    ( cd "$scen_dir" && WINEPREFIX="$PREFIX" WINEDEBUG=-all timeout 60 wine host.exe form_list.bin "$1" ) > "$scen_dir/out.txt" 2>&1 || true
}

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

echo "== S1: [[asset]] appends to vp + writes the i.dat row (default inject -> still merge)"
new_scen s1-asset
# pack_serving=inject with asset_serving=merge: [[asset]] must still merge, and
# the loader must say so (inject is a family-B-only seam)
printf 'pack_serving = "inject"\nasset_serving = "merge"\n' > "$scen_dir/a9loader.toml"
mod newasset \
    'id = "newasset"' \
    'name = "New asset"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'name = "_t_w1probe_a.x"' \
    'dir = "train\\w1probe"' \
    'host = "vp"' \
    'file = "assets/probe.x"'
pay newasset "assets/probe.x" probe.x
run_host overlaydat
check "i.dat redirected to the cache"  "path=mods.cache.i.dat"   "$scen_dir/out.txt"
check "vp redirected to the cache"     "path=mods.cache.vp.bin"  "$scen_dir/out.txt"
check "i.dat opened + read"            "overlay i.dat: "         "$scen_dir/out.txt"
check "host append journaled"          "overlay vp: 1 asset appended (1 row;" "$scen_dir/mods/a9loader.log"
check "index merge journaled"          "overlay i.dat: 1 row merged (1 new, 0 replaced; 1 host pack;" "$scen_dir/mods/a9loader.log"
check "inject noted (assets force merge)" "force merge serving"  "$scen_dir/mods/a9loader.log"
# byte-exact: merged vp == V ++ P1
"$A9ML" mkpack --container a --out "$scen_dir/expected-vp.bin" "$RUN/vbase.bin" "$RUN/probe.x"
checkf "merged vp == V ++ probe.x"  cmp -s "$scen_dir/mods/cache/vp.bin" "$scen_dir/expected-vp.bin"
# byte-exact: merged i.dat == shipped row + the new row (len 96, off |V|=64)
"$A9ML" mkpack --container idat --out "$scen_dir/expected-i.dat" \
    '_t_van_a.x=train\van:37:0' '_t_w1probe_a.x=train\w1probe:96:64'
checkf "merged i.dat == shipped + new row" cmp -s "$scen_dir/mods/cache/i.dat" "$scen_dir/expected-i.dat"
# the row's off IS the byte position of P in the decoded merged pack
"$A9ML" inspect --all "$scen_dir/mods/cache/i.dat" > "$scen_dir/inspect-i.txt" 2>&1
check "new row carries off=|V|" "off=64" "$scen_dir/inspect-i.txt"
check "new row carries len=|P|" "len=96"  "$scen_dir/inspect-i.txt"
"$A9ML" inspect "$scen_dir/mods/cache/vp.bin" > "$scen_dir/inspect-v.txt" 2>&1
check "merged vp decoded 160 B" "decoded 160 B" "$scen_dir/inspect-v.txt"
# second run: unchanged mod set -> both caches reused
run_host overlaydat
check "index cache reused"  "overlay i.dat: cache fresh — reusing" "$scen_dir/mods/a9loader.log"
check "host cache reused"   "overlay vp: cache fresh — reusing"    "$scen_dir/mods/a9loader.log"

echo "== S2: vanilla-name collision without override_vanilla -> loud refusal"
new_scen s2-vanilla-refuse
mod clash \
    'id = "clash"' \
    'name = "Clash"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'name = "_t_van_a.x"' \
    'dir = "train\\van"' \
    'host = "vp"' \
    'file = "assets/probe.x"'
pay clash "assets/probe.x" probe.x
run_host overlaydat
check "collision refused loudly"  "collides with a SHIPPED i.dat row" "$scen_dir/mods/a9loader.log"
check "i.dat serves vanilla"      "path=data.i.dat"  "$scen_dir/out.txt"
checkf "no merged index written"  test ! -f          "$scen_dir/mods/cache/i.dat"

echo "== S3: the same collision WITH override_vanilla = true -> the row is replaced"
new_scen s3-override
mod clashok \
    'id = "clashok"' \
    'name = "Clash ok"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'name = "_t_van_a.x"' \
    'dir = "train\\van"' \
    'host = "vp"' \
    'file = "assets/probe2.x"' \
    'override_vanilla = true'
pay clashok "assets/probe2.x" probe2.x
run_host overlaydat
check "override merge journaled" "overlay i.dat: 1 row merged (0 new, 1 replaced;" "$scen_dir/mods/a9loader.log"
"$A9ML" mkpack --container a --out "$scen_dir/expected-vp.bin" "$RUN/vbase.bin" "$RUN/probe2.x"
checkf "merged vp == V ++ probe2.x" cmp -s "$scen_dir/mods/cache/vp.bin" "$scen_dir/expected-vp.bin"
"$A9ML" mkpack --container idat --out "$scen_dir/expected-i.dat" '_t_van_a.x=train\van:96:64'
checkf "row replaced in place (same count)" cmp -s "$scen_dir/mods/cache/i.dat" "$scen_dir/expected-i.dat"

echo "== S4: two mods, same new asset name -> higher priority wins, loser logged"
new_scen s4-modmod
mod dupa \
    'id = "dupa"' \
    'name = "Dup A"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 10' \
    '' \
    '[[asset]]' \
    'name = "_t_dup_a.x"' \
    'dir = "train\\dup"' \
    'host = "vp"' \
    'file = "assets/probe.x"'
pay dupa "assets/probe.x" probe.x
mod dupb \
    'id = "dupb"' \
    'name = "Dup B"' \
    'version = "1.0.0"' \
    'author = "test"' \
    'priority = 20' \
    '' \
    '[[asset]]' \
    'name = "_t_dup_a.x"' \
    'dir = "train\\dup"' \
    'host = "vp"' \
    'file = "assets/probe2.x"'
pay dupb "assets/probe2.x" probe2.x
run_host overlaydat
check "overlap reported"           "overlap on asset:_t_dup_a.x" "$scen_dir/mods/a9loader.log"
check "loser refused (not fatal)"  "refused (overlap loser"      "$scen_dir/mods/a9loader.log"
check "exactly one asset merged"   "overlay vp: 1 asset appended" "$scen_dir/mods/a9loader.log"
"$A9ML" mkpack --container a --out "$scen_dir/expected-vp.bin" "$RUN/vbase.bin" "$RUN/probe.x"
checkf "merged vp == V ++ winner's payload" cmp -s "$scen_dir/mods/cache/vp.bin" "$scen_dir/expected-vp.bin"
"$A9ML" mkpack --container idat --out "$scen_dir/expected-i.dat" \
    '_t_van_a.x=train\van:37:0' '_t_dup_a.x=train\dup:96:64'
checkf "merged i.dat == shipped + winner row" cmp -s "$scen_dir/mods/cache/i.dat" "$scen_dir/expected-i.dat"

echo "== S5: folder form with host = \"auto\" (resolved from the embedded dir index)"
new_scen s5-autofolder
mod autofolder \
    'id = "autofolder"' \
    'name = "Auto folder"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'folder = "assets"' \
    'override_vanilla = false'
# two files in subfolders -> two rows, dirs from the subfolders, host auto
pay autofolder "assets/train/w1auto/_t_a.x" probe.x
pay autofolder "assets/train/w1auto/lod/_t_b.x" probe2.x
run_host overlaydat
check "auto host resolved via the train prefix"  "asset _t_a.x: host auto -> vp" "$scen_dir/mods/a9loader.log"
check "both rows resolved"                       "asset _t_b.x: host auto -> vp" "$scen_dir/mods/a9loader.log"
check "two assets appended to vp"                "2 assets"                      "$scen_dir/mods/a9loader.log"
check "i.dat redirected to the cache"            "path=mods.cache.i.dat"         "$scen_dir/out.txt"
check "vp redirected to the cache"               "path=mods.cache.vp.bin"        "$scen_dir/out.txt"
# byte-exact: merged vp == V ++ P(_t_a) ++ P(_t_b) (expansion sorts by relpath)
"$A9ML" mkpack --container a --out "$scen_dir/expected-vp.bin" "$RUN/vbase.bin" "$RUN/probe.x" "$RUN/probe2.x"
checkf "merged vp == V ++ both payloads" cmp -s "$scen_dir/mods/cache/vp.bin" "$scen_dir/expected-vp.bin"
# byte-exact: dir is the subfolder path relative to the folder (backslahes)
"$A9ML" mkpack --container idat --out "$scen_dir/expected-i.dat" \
    '_t_van_a.x=train\van:37:0' '_t_a.x=train\w1auto:96:64' '_t_b.x=train\w1auto\lod:96:160'
checkf "merged i.dat == shipped + both auto rows" cmp -s "$scen_dir/mods/cache/i.dat" "$scen_dir/expected-i.dat"
"$A9ML" inspect --all "$scen_dir/mods/cache/i.dat" > "$scen_dir/inspect-i.txt" 2>&1
checkf "row dir = subfolder path" grep -Fq 'train\w1auto' "$scen_dir/inspect-i.txt"
checkf "row dir keeps the nested subfolder" grep -Fq 'train\w1auto\lod' "$scen_dir/inspect-i.txt"

echo "== S6: host = \"auto\" refusals (unknown dir, ambiguous dir)"
new_scen s6a-auto-unknown
mod unknownmod \
    'id = "unknownmod"' \
    'name = "Unknown dir"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'folder = "assets"'
# a dir with no shipped row and no known prefix
pay unknownmod "assets/zzz/nowhere/_t_c.x" probe.x
run_host overlaydat
check "unknown dir refused"      "host auto refused for dir"  "$scen_dir/mods/a9loader.log"
check "refusal says unknown"     "is unknown"                 "$scen_dir/mods/a9loader.log"
check "i.dat serves vanilla"     "path=data.i.dat"            "$scen_dir/out.txt"
checkf "no merged index written" test ! -f                   "$scen_dir/mods/cache/i.dat"

new_scen s6b-auto-ambiguous
mod ambmod \
    'id = "ambmod"' \
    'name = "Ambiguous dir"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'folder = "museum"'
# `museum` is a real dir whose shipped rows span packs (ep + vp in the
# committed dirindex.toml) -> auto refuses rather than guessing a window
pay ambmod "museum/_t_m.x" probe.x
run_host overlaydat
check "ambiguous dir refused"     "host auto refused for dir"  "$scen_dir/mods/a9loader.log"
check "refusal says AMBIGUOUS"    "is AMBIGUOUS"               "$scen_dir/mods/a9loader.log"
check "i.dat serves vanilla"      "path=data.i.dat"            "$scen_dir/out.txt"
checkf "no merged index written"  test ! -f                   "$scen_dir/mods/cache/i.dat"

# ---- M5 tailgate: [[asset]] served in place at the family-A decode -------------
# The host fakes pack_lzss_decode at its real RVA, the vp/i.dat call sites with
# their address-book expect bytes, and the relay cave; it calls the (spliced)
# sites with the loader's stack shape and dumps the post-decode workspace.

tg_fixtures() { # the canned vanilla decoded streams + the expected merged images
    cp "$RUN/vbase.bin" "$scen_dir/vanilla_vp.dec"
    cat "$RUN/vbase.bin" "$RUN/probe.x" > "$scen_dir/expected_vp.bin"
    python3 - "$scen_dir" <<'EOF'
import sys, os
d = sys.argv[1]
MAGIC = bytes.fromhex("610064007a00700069006400780030003000")  # adzpidx00 UTF-16
PAD   = bytes.fromhex("300030003000")                          # PAD_STEAM
def rec(name, dirn, ln, flag, off):
    nb = name.encode("utf-16-le") + b"\x00\x00"
    db = dirn.encode("utf-16-le") + b"\x00\x00"
    tail = ln.to_bytes(4, "little") + bytes([flag]) + off.to_bytes(4, "little")
    size = 4 + len(nb) + 2 + len(db) + 9
    return (size.to_bytes(2, "little") + (len(nb)//2).to_bytes(2, "little") + nb
            + (len(db)//2).to_bytes(2, "little") + db + tail)
def idat(rows):
    out = MAGIC + PAD + len(rows).to_bytes(4, "little")
    for r in rows:
        out += rec(*r)
    return out
vanilla = idat([("_t_van_a.x", "train\\van", 37, 0, 0)])
open(os.path.join(d, "vanilla_idat.dec"), "wb").write(vanilla)
merged = idat([("_t_van_a.x", "train\\van", 37, 0, 0),
               ("_t_w1probe_a.x", "train\\w1probe", 96, 0, 64)])
open(os.path.join(d, "expected_idat.bin"), "wb").write(merged)
print("   tg fixtures: vanilla idat %d B, expected merged %d B" % (len(vanilla), len(merged)))
EOF
}

echo "== T1: tailgate serves [[asset]] in place (vp append + i.dat row, no cache file)"
new_scen t1-tailgate
printf 'asset_serving = "tailgate"\npack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
tg_fixtures
mod tgasset \
    'id = "tgasset"' \
    'name = "Tailgate asset"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'name = "_t_w1probe_a.x"' \
    'dir = "train\\w1probe"' \
    'host = "vp"' \
    'file = "assets/probe.x"'
pay tgasset "assets/probe.x" probe.x
run_host tailgate
LOG="$scen_dir/mods/a9loader.log"
check "chosen path is the tailgate" "asset serving: tailgate" "$LOG"
check "vp splice installed"  "asset tailgate: vp splice installed rva=0x3f1434 expect=verified" "$LOG"
check "i.dat splice installed" "asset tailgate: i.dat splice installed rva=0x31516f expect=verified" "$LOG"
check "vp append journaled"  "asset tailgate: vp appended 1 payload (96 B) at +64" "$LOG"
check "i.dat append journaled" "asset tailgate: i.dat appended 1 payload" "$LOG"
# the decoder produced MORE than the declared size (the live exhaustion shape):
# the append still lands at the planned declared base and the delta is logged
check "vp exhaustion delta logged" "decoded 77 B — 13 B past the declared size 64" "$LOG"
check "i.dat exhaustion delta logged" "decoded 106 B — 21 B past the declared size 85" "$LOG"
check "host ran the vp decode" "tg host: vp site ret=1 cap=77" "$scen_dir/out.txt"
check "host ran the i.dat decode" "tg host: i.dat site ret=1 cap=106" "$scen_dir/out.txt"
# byte-exact: the workspace past |V| carries the payload (vp) / the row records (i.dat)
checkf "vp tail == V ++ probe.x" cmp -s -n 160 "$scen_dir/ws_vp.bin" "$scen_dir/expected_vp.bin"
NAPP=$(stat -c%s "$scen_dir/expected_idat.bin")
checkf "i.dat tail == vanilla ++ row" cmp -s -n "$NAPP" "$scen_dir/ws_idat.bin" "$scen_dir/expected_idat.bin"
# the appended i.dat is a real table: wrap exactly the merged image and inspect
mkdir -p "$scen_dir/merged"
head -c "$NAPP" "$scen_dir/ws_idat.bin" > "$scen_dir/idat_merged.raw"
"$A9ML" mkpack --container a --out "$scen_dir/merged/i.dat" "$scen_dir/idat_merged.raw"
"$A9ML" inspect --all "$scen_dir/merged/i.dat" > "$scen_dir/inspect-i.txt" 2>&1 || true
check "new row carries off=|V|" "off=64" "$scen_dir/inspect-i.txt"
check "new row carries len=|P|" "len=96"  "$scen_dir/inspect-i.txt"
# no re-encode, no cache: nothing on disk for vp or i.dat
checkf "no vp cache built"    test ! -f "$scen_dir/mods/cache/vp.bin"
checkf "no i.dat cache built" test ! -f "$scen_dir/mods/cache/i.dat"

echo "== T2: a SHORT decode (tail would overrun) -> loud + fail-open (no append)"
new_scen t2-tailgate-mismatch
printf 'asset_serving = "tailgate"\npack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
tg_fixtures
mod tgbad \
    'id = "tgbad"' \
    'name = "Tailgate mismatch"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'name = "_t_w1probe_a.x"' \
    'dir = "train\\w1probe"' \
    'host = "vp"' \
    'file = "assets/probe.x"'
pay tgbad "assets/probe.x" probe.x
run_host tailgate-mismatch
LOG="$scen_dir/mods/a9loader.log"
check "vp splice installed"     "asset tailgate: vp splice installed" "$LOG"
check "short decode logged loudly" "produced 56 bytes but the plan appends at the vanilla declared size 64" "$LOG"
check "hard failure names it"   "vp FAILED at the decode stub" "$LOG"
checkq "no vp append (fail-open)" "asset tailgate: vp appended" "$LOG"
checkf "vp tail stays zero" python3 -c "import sys; d=open('$scen_dir/ws_vp.bin','rb').read(); sys.exit(0 if all(b==0 for b in d[64:]) else 1)"

echo "== T3: budget overrun -> per-host merge fallback (no vp splice)"
new_scen t3-tailgate-budget
printf 'asset_serving = "tailgate"\npack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
# a vanilla vp.bin whose declared size is 3.3 GB: the pack is not decodable, but
# the cheap header peek the tailgate uses reads the giant size -> no room
python3 - "$scen_dir/data/vp.bin" <<'EOF'
import sys, struct
ADD4 = [0x3b, 0x16, 0x6f, 0xd8]              # the pack ADD table's first four bytes
declared = 0xC8000000                          # 3.3 GB > the 200 MB out-cap
hdr = b"\x55" * 16                             # arbitrary header (the reader checks no MD5)
plain = struct.pack("<I", declared)
enc = bytearray(64)
for i in range(4):
    enc[i] = ((plain[i] ^ hdr[i]) - ADD4[i]) & 0xff
open(sys.argv[1], "wb").write(hdr + bytes(enc))
print("   tg fixture: data/vp.bin declared=%d (over the 200 MB cap)" % declared)
EOF
mod tgbudget \
    'id = "tgbudget"' \
    'name = "Tailgate budget"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'name = "_t_w1probe_a.x"' \
    'dir = "train\\w1probe"' \
    'host = "vp"' \
    'file = "assets/probe.x"'
pay tgbudget "assets/probe.x" probe.x
run_host overlaydat
LOG="$scen_dir/mods/a9loader.log"
check "budget fallback logged"  "does not fit the decode workspace" "$LOG"
check "merge fallback named"    "merge fallback for this host" "$LOG"
check "i.dat joins the merge"   "i.dat joins the merge path" "$LOG"
checkq "no vp splice installed" "asset tailgate: vp splice installed" "$LOG"

echo "== T4: asset_serving = merge keeps today's cache overlay (regression)"
new_scen t4-asset-merge
printf 'pack_serving = "merge"\nasset_serving = "merge"\n' > "$scen_dir/a9loader.toml"
mod t4asset \
    'id = "t4asset"' \
    'name = "Merge asset"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'name = "_t_w1probe_a.x"' \
    'dir = "train\\w1probe"' \
    'host = "vp"' \
    'file = "assets/probe.x"'
pay t4asset "assets/probe.x" probe.x
run_host overlaydat
LOG="$scen_dir/mods/a9loader.log"
check "merge redirect taken" "path=mods.cache.vp.bin" "$scen_dir/out.txt"
check "asset merge logged"   'asset_serving = "merge"' "$LOG"
check "vp merged (W1 path)"  "overlay vp: 1 asset appended" "$LOG"
checkq "no tailgate machinery" "asset tailgate:" "$LOG"
checkf "merge cache built"   test -f "$scen_dir/mods/cache/vp.bin"

# ===========================================================================
# M5-U: auto-merged `.dir` group listings + overrides on the tailgate path
# ===========================================================================
# A `.dir` row is a flat run of NUL-terminated UTF-16LE names (no count, no
# stride; the row's `len` is the only bound). The loader derives each mod's
# contribution from its OWN [[asset]] rows (`dir = D` => append the row name to
# `D.dir`), unions them, and rebuilds the listing: the shipped listing bytes
# VERBATIM followed by the new names (deduped, NUL-terminated UTF-16LE), served
# through whichever path serves that host. The fixtures below put the shipped
# `previewtexture.dir` listing in tp (`previewtexture` resolves to tp in the
# embedded dir index).

m5u_tp_fixtures() { # the preview fixture: a real previewtexture.dir in tp + a
                    # vanilla preview asset, in a decoded tp stream
    python3 - "$scen_dir" <<'EOF'
import sys, os
d = sys.argv[1]
MAGIC = bytes.fromhex("610064007a00700069006400780030003000")
PAD   = bytes.fromhex("300030003000")
def rec(name, dirn, ln, flag, off):
    nb = name.encode("utf-16-le") + b"\x00\x00"
    db = dirn.encode("utf-16-le") + b"\x00\x00"
    tail = ln.to_bytes(4, "little") + bytes([flag]) + off.to_bytes(4, "little")
    size = 4 + len(nb) + 2 + len(db) + 9
    return (size.to_bytes(2, "little") + (len(nb)//2).to_bytes(2, "little") + nb
            + (len(db)//2).to_bytes(2, "little") + db + tail)
def idat(rows):
    out = MAGIC + PAD + len(rows).to_bytes(4, "little")
    for r in rows: out += rec(*r)
    return out
def listing(names):
    out = b""
    for n in names: out += n.encode("utf-16-le") + b"\x00\x00"
    return out
VAN = ["form_van_a_f.dds", "form_van_a_s.dds", "form_van_a_s_1.dds"]
van_listing = listing(VAN)
off_list = 0x40
van_asset = bytes((0xA0 + i) & 0xff for i in range(0x20))
off_van = off_list + len(van_listing)
tp = bytes([0x11]) * off_list + van_listing + van_asset + bytes([0x22]) * 0x40
open(os.path.join(d, "tp.dec"), "wb").write(tp)
open(os.path.join(d, "vanilla_tp.dec"), "wb").write(tp)
open(os.path.join(d, "van_listing.bin"), "wb").write(van_listing)
open(os.path.join(d, "van_asset.bin"), "wb").write(van_asset)
vd = idat([("previewtexture.dir", "previewtexture.dir", len(van_listing), 0, off_list),
           ("form_van_a_f.dds", "previewtexture", 0x20, 0, off_van)])
open(os.path.join(d, "vanilla_idat.dec"), "wb").write(vd)
open(os.path.join(d, "m5u.env"), "w").write(
    "OFF_LIST=%d\nLEN_LIST=%d\nOFF_VAN=%d\nTP_DECL=%d\nVAN_IDAT_LEN=%d\nVAN_COUNT=2\n"
    % (off_list, len(van_listing), off_van, len(tp), len(vd)))
print("   m5u/tp fixture: tp decoded %d B, listing off %d len %d, van asset off %d, idat %d B"
      % (len(tp), off_list, len(van_listing), off_van, len(vd)))
EOF
    . "$scen_dir/m5u.env"
    "$A9ML" mkpack --container a --out "$scen_dir/data/tp.bin" "$scen_dir/tp.dec"
    "$A9ML" mkpack --container idat --out "$scen_dir/data/i.dat" \
        "previewtexture.dir=previewtexture.dir:$LEN_LIST:$OFF_LIST" \
        "form_van_a_f.dds=previewtexture:32:$OFF_VAN"
}

# the tailgate expectations: the extended listing bytes + the appended i.dat
# records (assets first, then the listing row) — parsed back byte-exactly
m5u_tail_expect() { # <listing-names...> ; the payload files are p1/p2 in the scen
    python3 - "$scen_dir" "$@" <<'EOF'
import sys, os
d = sys.argv[1]
names = sys.argv[2:]
env = dict(l.strip().split("=", 1) for l in open(os.path.join(d, "m5u.env")) if "=" in l)
n = {k: int(v) for k, v in env.items()}
van = open(os.path.join(d, "van_listing.bin"), "rb").read()
present = set(); cur = bytearray()
for i in range(0, len(van), 2):
    u = van[i] | (van[i+1] << 8)
    if u == 0: present.add(cur.decode("utf-16-le")); cur = bytearray()
    else: cur += bytes([van[i], van[i+1]])
ext = bytearray(van)
appended = []
for nm in names:
    if nm in present or nm in appended: continue
    appended.append(nm); ext += nm.encode("utf-16-le") + b"\x00\x00"
ext = bytes(ext)
def rec(name, dirn, ln, off):
    nb = name.encode("utf-16-le") + b"\x00\x00"; db = dirn.encode("utf-16-le") + b"\x00\x00"
    tail = ln.to_bytes(4, "little") + b"\x00" + off.to_bytes(4, "little")
    size = 4 + len(nb) + 2 + len(db) + 9
    return (size.to_bytes(2, "little") + (len(nb)//2).to_bytes(2, "little") + nb
            + (len(db)//2).to_bytes(2, "little") + db + tail)
tpdecl = n["TP_DECL"]; off = tpdecl; blob = b""; tail = b""
for i, nm in enumerate(names):
    pl = open(os.path.join(d, "p%d" % (i + 1)), "rb").read()
    blob += rec(nm, "previewtexture", len(pl), off); off += len(pl); tail += pl
if appended:
    blob += rec("previewtexture.dir", "previewtexture.dir", len(ext), off)
    tail += ext
open(os.path.join(d, "expected_idat_appended.bin"), "wb").write(blob)
open(os.path.join(d, "expected_tp_tail.bin"), "wb").write(tail)
open(os.path.join(d, "expected_listing.bin"), "wb").write(ext)
print("   m5u: ext %d B (%d appended), appended idat %d B" % (len(ext), len(appended), len(blob)))
EOF
}

echo "== U1: auto-extended previewtexture.dir served IN PLACE (tailgate, no .dir file)"
new_scen u1-listing-tailgate
printf 'asset_serving = "tailgate"\npack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
m5u_tp_fixtures
mod u1mod \
    'id = "u1mod"' 'name = "U1"' 'version = "1.0.0"' 'author = "test"' '' \
    '[[asset]]' 'name = "form_new_1.dds"' 'dir = "previewtexture"' \
    'host = "auto"' 'file = "assets/p1"'
pay u1mod "assets/p1" lpay1.bin
cp "$RUN/lpay1.bin" "$scen_dir/p1"
P1_LEN=$(stat -c%s "$RUN/lpay1.bin")
run_host tailgate-tp
LOG="$scen_dir/mods/a9loader.log"
check "listing extended (1 name -> 4 entries, host tp)" \
      "overlay listing: previewtexture.dir extended by 1 name(s) -> 4 entries (host tp" "$LOG"
check "tp splice installed"    "asset tailgate: tp splice installed rva=0x3f0f4d expect=verified" "$LOG"
check "i.dat splice installed" "asset tailgate: i.dat splice installed rva=0x31" "$LOG"
m5u_tail_expect form_new_1.dds
checkf "tp tail == the new asset, then vanilla listing ++ name" python3 -c "
import sys
ws=open('$scen_dir/ws_tp.bin','rb').read(); exp=open('$scen_dir/expected_tp_tail.bin','rb').read()
o=$TP_DECL
sys.exit(0 if ws[o:o+len(exp)]==exp else 1)"
checkf "appended i.dat == the two records" python3 -c "
import sys
ws=open('$scen_dir/ws_idat.bin','rb').read(); exp=open('$scen_dir/expected_idat_appended.bin','rb').read()
o=$VAN_IDAT_LEN
sys.exit(0 if ws[o:o+len(exp)]==exp else 1)"
checkf "entry count bumped by 2" python3 -c "
import sys
ws=open('$scen_dir/ws_idat.bin','rb').read()
sys.exit(0 if int.from_bytes(ws[0x18:0x1c],'little')==$VAN_COUNT+2 else 1)"
# the (A) consumer check: replay the game's last-wins map insert over the
# appended image and read the listing the `previewtexture.dir` query resolves to
checkf "consumer read of previewtexture.dir == the extended listing" python3 -c "
import sys
img=open('$scen_dir/ws_idat.bin','rb').read(); ws=open('$scen_dir/ws_tp.bin','rb').read()
n=int.from_bytes(img[0x18:0x1c],'little'); pos=0x1c; m={}
for _ in range(n):
    size=int.from_bytes(img[pos:pos+2],'little'); nl=int.from_bytes(img[pos+2:pos+4],'little')
    name=img[pos+4:pos+4+nl*2].decode('utf-16-le').split(chr(0))[0]; np=pos+4+nl*2
    dl=int.from_bytes(img[np:np+2],'little'); dp=np+2+dl*2
    ln=int.from_bytes(img[dp:dp+4],'little'); off=int.from_bytes(img[dp+5:dp+9],'little')
    m[name]=(off,ln); pos=dp+9
off,ln=m['previewtexture.dir']; ext=open('$scen_dir/expected_listing.bin','rb').read()
sys.exit(0 if (off==$TP_DECL+$P1_LEN and ln==len(ext) and ws[off:off+ln]==ext) else 1)"
checkf "no cache files (tailgate)" test ! -f "$scen_dir/mods/cache/tp.bin"
checkf "no i.dat cache (tailgate)"  test ! -f "$scen_dir/mods/cache/i.dat"

echo "== U2: the SAME auto-extended listing in MERGE mode (whole new image)"
new_scen u2-listing-merge
printf 'asset_serving = "merge"\npack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
m5u_tp_fixtures
mod u2mod \
    'id = "u2mod"' 'name = "U2"' 'version = "1.0.0"' 'author = "test"' '' \
    '[[asset]]' 'name = "form_new_1.dds"' 'dir = "previewtexture"' \
    'host = "auto"' 'file = "assets/p1"'
pay u2mod "assets/p1" lpay1.bin
run_host overlaytp
LOG="$scen_dir/mods/a9loader.log"
check "listing extended (merge)" "overlay listing: previewtexture.dir extended by 1 name(s) -> 4 entries (host tp" "$LOG"
check "tp redirected to the cache" "path=mods.cache.tp.bin" "$scen_dir/out.txt"
# merged tp == V ++ payload ++ ext  (built with the same a9-formats encoder)
python3 - "$scen_dir" <<'EOF'
import sys, os
d = sys.argv[1]
env = dict(l.strip().split("=",1) for l in open(os.path.join(d,"m5u.env")) if "=" in l)
n = {k:int(v) for k,v in env.items()}
van = open(os.path.join(d,"van_listing.bin"),"rb").read()
ext = van + "form_new_1.dds".encode("utf-16-le") + b"\x00\x00"
open(os.path.join(d,"p1"),"wb").write(open(os.path.join(d,"mods/u2mod/assets/p1"),"rb").read())
open(os.path.join(d,"ext.bin"),"wb").write(ext)
print("   u2: tp_decl %d, ext %d B" % (n["TP_DECL"], len(ext)))
EOF
P1=$(stat -c%s "$scen_dir/p1")
EXT=$(stat -c%s "$scen_dir/ext.bin")
"$A9ML" mkpack --container a --out "$scen_dir/expected-tp.bin" \
    "$scen_dir/tp.dec" "$scen_dir/p1" "$scen_dir/ext.bin"
checkf "merged tp == V ++ payload ++ extended listing" cmp -s "$scen_dir/mods/cache/tp.bin" "$scen_dir/expected-tp.bin"
"$A9ML" mkpack --container idat --out "$scen_dir/expected-i.dat" \
    "previewtexture.dir=previewtexture.dir:$EXT:$((TP_DECL + P1))" \
    "form_van_a_f.dds=previewtexture:32:$OFF_VAN" \
    "form_new_1.dds=previewtexture:$P1:$TP_DECL"
checkf "merged i.dat == listing row rewritten + the new row" cmp -s "$scen_dir/mods/cache/i.dat" "$scen_dir/expected-i.dat"

echo "== U3: TWO preview mods in one boot -> both name sets end up listed"
new_scen u3-listing-two-mods
printf 'asset_serving = "tailgate"\npack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
m5u_tp_fixtures
mod u3a \
    'id = "u3a"' 'name = "U3a"' 'version = "1.0.0"' 'author = "test"' 'priority = 10' '' \
    '[[asset]]' 'name = "form_new_1.dds"' 'dir = "previewtexture"' \
    'host = "auto"' 'file = "assets/p1"'
pay u3a "assets/p1" lpay1.bin
mod u3b \
    'id = "u3b"' 'name = "U3b"' 'version = "1.0.0"' 'author = "test"' 'priority = 20' '' \
    '[[asset]]' 'name = "form_new_2.dds"' 'dir = "previewtexture"' \
    'host = "auto"' 'file = "assets/p2"'
pay u3b "assets/p2" lpay2.bin
cp "$RUN/lpay1.bin" "$scen_dir/p1"; cp "$RUN/lpay2.bin" "$scen_dir/p2"
run_host tailgate-tp
LOG="$scen_dir/mods/a9loader.log"
check "listing extended by both names" "previewtexture.dir extended by 2 name(s) -> 5 entries (host tp" "$LOG"
# resolution order: priority 10 (u3a) before 20 (u3b) -> p1 then p2
m5u_tail_expect form_new_1.dds form_new_2.dds
checkf "tp tail carries BOTH name sets in resolve order" python3 -c "
import sys
ws=open('$scen_dir/ws_tp.bin','rb').read(); exp=open('$scen_dir/expected_tp_tail.bin','rb').read()
o=$TP_DECL
sys.exit(0 if ws[o:o+len(exp)]==exp else 1)"
checkf "both new names present in the live listing" python3 -c "
import sys
ws=open('$scen_dir/ws_tp.bin','rb').read()
tail=ws[$TP_DECL:]
n1=b'form_new_1.dds'.decode(); n2=b'form_new_2.dds'.decode()
sys.exit(0 if (n1.encode('utf-16-le')+b'\x00\x00' in tail and n2.encode('utf-16-le')+b'\x00\x00' in tail) else 1)"
checkf "appended i.dat carries both rows + the listing row" python3 -c "
import sys
ws=open('$scen_dir/ws_idat.bin','rb').read(); exp=open('$scen_dir/expected_idat_appended.bin','rb').read()
o=$VAN_IDAT_LEN
sys.exit(0 if ws[o:o+len(exp)]==exp else 1)"

echo "== U4: an override of an EXISTING asset on the TAILGATE path changes consumer bytes"
new_scen u4-override-tailgate
printf 'asset_serving = "tailgate"\npack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
m5u_tp_fixtures
mod u4mod \
    'id = "u4mod"' 'name = "U4"' 'version = "1.0.0"' 'author = "test"' '' \
    '[[asset]]' 'name = "form_van_a_f.dds"' 'dir = "previewtexture"' \
    'host = "auto"' 'file = "assets/ovr"' 'override_vanilla = true'
pay u4mod "assets/ovr" ovr.bin
run_host tailgate-tp
LOG="$scen_dir/mods/a9loader.log"
check "override allowed on the tailgate" "asset serving: tailgate" "$LOG"
checkq "the listing is NOT needlessly extended (dedup)" "previewtexture.dir extended by" "$LOG"
# the appended duplicate row for form_van_a_f.dds: off = |V_tp|, len = |ovr|
checkf "tp tail == the override payload" python3 -c "
import sys
ws=open('$scen_dir/ws_tp.bin','rb').read(); pl=open('$scen_dir/mods/u4mod/assets/ovr','rb').read()
o=$TP_DECL
sys.exit(0 if ws[o:o+len(pl)]==pl else 1)"
# the (B) regression: replay the game's map insert-or-find (LAST duplicate wins,
# disassembly-proven) over the appended image and read the bytes the query names
checkf "last-wins map resolves the name to the OVERRIDE bytes" python3 -c "
import sys
img=open('$scen_dir/ws_idat.bin','rb').read(); ws=open('$scen_dir/ws_tp.bin','rb').read()
n=int.from_bytes(img[0x18:0x1c],'little'); pos=0x1c; m={}
for _ in range(n):
    size=int.from_bytes(img[pos:pos+2],'little'); nl=int.from_bytes(img[pos+2:pos+4],'little')
    name=img[pos+4:pos+4+nl*2].decode('utf-16-le').split(chr(0))[0]; np=pos+4+nl*2
    dl=int.from_bytes(img[np:np+2],'little'); dp=np+2+dl*2
    ln=int.from_bytes(img[dp:dp+4],'little'); off=int.from_bytes(img[dp+5:dp+9],'little')
    m[name]=(off,ln); pos=dp+9
off,ln=m['form_van_a_f.dds']
ovr=open('$scen_dir/mods/u4mod/assets/ovr','rb').read()
van=open('$scen_dir/van_asset.bin','rb').read()
ok = off==$TP_DECL and ln==len(ovr) and ws[off:off+ln]==ovr and ws[off:off+ln]!=van
sys.exit(0 if ok else 1)"

echo "== U5: a mod adding into a NON-name-keyed listing logs it + still works (explicit override)"
new_scen u5-not-automerged
printf 'asset_serving = "merge"\npack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
# an mcp-resident `tree.dir` listing (`tree` resolves to mcp) — NOT on the
# auto-merge allow-list (its consumer may be positional)
python3 - "$scen_dir" <<'EOF'
import sys, os
d = sys.argv[1]
MAGIC = bytes.fromhex("610064007a00700069006400780030003000")
PAD   = bytes.fromhex("300030003000")
def rec(name, dirn, ln, flag, off):
    nb=name.encode("utf-16-le")+b"\x00\x00"; db=dirn.encode("utf-16-le")+b"\x00\x00"
    tail=ln.to_bytes(4,"little")+bytes([flag])+off.to_bytes(4,"little")
    size=4+len(nb)+2+len(db)+9
    return (size.to_bytes(2,"little")+(len(nb)//2).to_bytes(2,"little")+nb
            +(len(db)//2).to_bytes(2,"little")+db+tail)
def idat(rows):
    out=MAGIC+PAD+len(rows).to_bytes(4,"little")
    for r in rows: out+=rec(*r)
    return out
van_listing = "tree_van.dds".encode("utf-16-le")+b"\x00\x00"
off_list=0x40; van_asset=bytes((0xB0+i)&0xff for i in range(0x20)); off_van=off_list+len(van_listing)
mcp=bytes([0x33])*off_list+van_listing+van_asset+bytes([0x44])*0x40
open(os.path.join(d,"mcp.dec"),"wb").write(mcp)
open(os.path.join(d,"van_listing.bin"),"wb").write(van_listing)
open(os.path.join(d,"van_asset.bin"),"wb").write(van_asset)
open(os.path.join(d,"m5u.env"),"w").write("OFF_LIST=%d\nLEN_LIST=%d\nOFF_VAN=%d\nMCP_DECL=%d\n"
    % (off_list, len(van_listing), off_van, len(mcp)))
# the explicit whole-row override the mod ships: the listing + a new name
treelist = van_listing + "tree_new.dds".encode("utf-16-le")+b"\x00\x00"
open(os.path.join(d,"treelist.bin"),"wb").write(treelist)
print("   u5: mcp decoded %d B, tree.dir off %d len %d, override listing %d B"
      % (len(mcp), off_list, len(van_listing), len(treelist)))
EOF
. "$scen_dir/m5u.env"
"$A9ML" mkpack --container a --out "$scen_dir/data/mcp.bin" "$scen_dir/mcp.dec"
"$A9ML" mkpack --container idat --out "$scen_dir/data/i.dat" \
    "tree.dir=tree.dir:$LEN_LIST:$OFF_LIST" \
    "tree_van.dds=tree:32:$OFF_VAN"
mod u5mod \
    'id = "u5mod"' 'name = "U5"' 'version = "1.0.0"' 'author = "test"' '' \
    '[[asset]]' 'name = "tree_new.x"' 'dir = "tree"' 'host = "auto"' 'file = "assets/tn"' '' \
    '[[asset]]' 'name = "tree.dir"' 'dir = "tree"' 'host = "auto"' \
    'file = "assets/treelist"' 'override_vanilla = true'
pay u5mod "assets/tn" treenew.bin
cp "$scen_dir/treelist.bin" "$scen_dir/mods/u5mod/assets/treelist"
run_host overlaydat
LOG="$scen_dir/mods/a9loader.log"
check "not-auto-merged line names tree.dir" "overlay listing: tree.dir NOT auto-merged" "$LOG"
check "the line names the contributing mod" "mod(s) u5mod" "$LOG"
check "the line says how to extend it"      "override_vanilla = true to extend it" "$LOG"
# the explicit override still works: mcp == V ++ tree_new.x ++ the override listing
TN=$(stat -c%s "$RUN/treenew.bin"); TL=$(stat -c%s "$scen_dir/treelist.bin")
"$A9ML" mkpack --container a --out "$scen_dir/expected-mcp.bin" \
    "$scen_dir/mcp.dec" "$RUN/treenew.bin" "$scen_dir/treelist.bin"
checkf "merged mcp == V ++ new asset ++ explicit override listing" cmp -s "$scen_dir/mods/cache/mcp.bin" "$scen_dir/expected-mcp.bin"
"$A9ML" mkpack --container idat --out "$scen_dir/expected-i.dat" \
    "tree.dir=tree.dir:$TL:$((MCP_DECL + TN))" \
    "tree_van.dds=tree:32:$OFF_VAN" \
    "tree_new.x=tree:$TN:$MCP_DECL"
checkf "merged i.dat == tree.dir row rewritten + the new row" cmp -s "$scen_dir/mods/cache/i.dat" "$scen_dir/expected-i.dat"

echo "== T5: M5-T workspace swap — the game's 200 MB buffer is replaced (cap raised, old freed)"
new_scen t5-workspace
printf 'asset_serving = "tailgate"\nworkspace_out_mb = 256\n' > "$scen_dir/a9loader.toml"
run_host workspace
LOG="$scen_dir/mods/a9loader.log"
check "swap armed at the alloc tail" "M5-T workspace: swap armed at rva=0x31981a" "$LOG"
check "the OUT buffer grew"          "M5-T workspace: OUT buffer 200 MiB -> 256 MiB" "$LOG"
check "the shim reached the pinned callee (RAX flowed back)" \
      "callee rax=0xdeadbeef (PASS)" "$scen_dir/out.txt"
check "allocated through the game's own allocator" \
      "malloc calls=1 requested=256 MiB (PASS)" "$scen_dir/out.txt"
check "the global was repointed"     "(SWAPPED)" "$scen_dir/out.txt"
check "the cap was raised with it"   "cap=268435456 (PASS)" "$scen_dir/out.txt"
check "the game's buffer was freed"  "free calls=1" "$scen_dir/out.txt"

echo "== T6: allocator failure -> loud + fail-open (nothing changes, no free)"
new_scen t6-workspace-fail
printf 'asset_serving = "tailgate"\nworkspace_out_mb = 256\n' > "$scen_dir/a9loader.toml"
run_host workspace-fail
LOG="$scen_dir/mods/a9loader.log"
check "swap armed"                  "M5-T workspace: swap armed" "$LOG"
check "the alloc failure is loud" \
      "_aligned_malloc(256 MiB) failed — keeping the game's 200 MiB buffer" "$LOG"
check "nothing was swapped"         "(unchanged)" "$scen_dir/out.txt"
check "the shipped cap is untouched" "cap=209715200" "$scen_dir/out.txt"
check "nothing was freed"           "free calls=0" "$scen_dir/out.txt"
check "the callee still ran"        "callee rax=0xdeadbeef (PASS)" "$scen_dir/out.txt"

echo "== T7: M5-T + tailgate together — the grow re-decides and the tailgate stands (regression)"
new_scen t7-workspace-tailgate
printf 'asset_serving = "tailgate"\npack_serving = "merge"\nworkspace_out_mb = 256\n' > "$scen_dir/a9loader.toml"
tg_fixtures
mod tgasset \
    'id = "tgasset"' \
    'name = "Tailgate asset"' \
    'version = "1.0.0"' \
    'author = "test"' \
    '' \
    '[[asset]]' \
    'name = "_t_w1probe_a.x"' \
    'dir = "train\\w1probe"' \
    'host = "vp"' \
    'file = "assets/probe.x"'
pay tgasset "assets/probe.x" probe.x
run_host tailgate-ws
LOG="$scen_dir/mods/a9loader.log"
check "the workspace swap armed"  "M5-T workspace: swap armed at rva=0x31981a" "$LOG"
check "the tailgate decision stands" "asset serving: tailgate" "$LOG"
checkq "the grow did NOT knock the tailgate into merge" "no host pack could be tailgated" "$LOG"
checkq "no second cave install refusal" "relay cave bytes differ" "$LOG"
check "vp splice installed"  "asset tailgate: vp splice installed rva=0x3f1434 expect=verified" "$LOG"
check "the append still happened" "asset tailgate: vp appended 1 payload (96 B) at +64" "$LOG"

echo
if [ "$FAILED" = 0 ]; then
    echo "M5 HARNESS: ALL SCENARIOS GREEN"
else
    echo "M5 HARNESS: $FAILED FAILURE(S)"
fi
exit "$FAILED"