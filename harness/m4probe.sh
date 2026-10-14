#!/usr/bin/env bash
# M4 fetch-probe harness — the probe_fetch diagnostics under wine. The fake
# host maps all four fetch anchors (CPackFileBuffer::fetch, the texture
# fetch-miss path, CCommonShader::fetch, the global fetch-cache get) at the
# real RVAs with the real expect bytes, and the "probe" host mode calls all
# four entries once. With the probe on, the loader's log must carry the four
# install lines plus the four per-call probe lines; with it off, no probe
# lines and no hooks.
#
# usage: harness/m4probe.sh
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"
source "$(dirname "$0")/lib.sh"

RUN="$REPO/harness/run/m4probe"
PREFIX="$REPO/harness/run/.prefix"
GAME_EXE="${ATRAIN9_EXE:-/run/media/frybarec/dasug/Games/steamlibrary/steamapps/common/A-Train9v5/ATrain9v5.exe}"

echo "== building the host + the Rust version.dll"
mkdir -p "$RUN"
x86_64-w64-mingw32-windres harness/host.rc -O coff -o "$RUN/hostres.o"
x86_64-w64-mingw32-gcc -O2 -o "$RUN/host.exe" harness/host.c "$RUN/hostres.o"
cargo build --release --target x86_64-pc-windows-gnu
DLL="$REPO/target/x86_64-pc-windows-gnu/release/version.dll"
cargo build --release --bin a9ml
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

# m4probe's needles carry regex metacharacters — match them as FIXED strings
check() { # check <label> <needle> <file>
    if grep -qF -- "$2" "$3"; then
        echo "   [ ok ] $1"
    else
        echo "   [FAIL] $1 (missing: $2 in $3)"
        FAILED=$((FAILED + 1))
    fi
}

# the probe only installs on boots that get past the mod resolution, so the
# probe-on scenarios carry a quiet no-effect mod (valid per the manifest
# schema; zero effects = pure diagnostic configuration)
quiet_mod() {
    mkdir -p "$scen_dir/mods/probequiet"
    cat > "$scen_dir/mods/probequiet/mod.toml" <<'EOF'
id = "probequiet"
name = "Probe quiet"
version = "1.0.0"
author = "test"
EOF
}

echo "== P1: probe off (default) — no probe lines, no hooks"
new_scen p1-off
run_host probe
checkf "boot log exists"    test -f "$scen_dir/mods/a9loader.log"
check "boot built identity" "exe build id 5.00.5634"                "$scen_dir/mods/a9loader.log"
if grep -q "probe: " "$scen_dir/mods/a9loader.log" 2>/dev/null; then
    echo "   [FAIL] probe lines present with probe_fetch unset"
    FAILED=$((FAILED + 1))
else
    echo "   [ ok ] log clean of probe lines"
fi

echo "== P2: probe on (probe_fetch = true) — 4 hook installs + 4 call lines"
new_scen p2-on
quiet_mod
printf 'probe_fetch = true\n' > "$scen_dir/a9loader.toml"
run_host probe
LOG="$scen_dir/mods/a9loader.log"
check "probe enabled line"        "probe: fetch hooks on (diagnostic)"    "$LOG"
check "pack hook installed"        "probe: pack_fetch hook installed rva=0x306e20 expect=verified" "$LOG"
check "texture hook installed"     "probe: texture_fetch hook installed rva=0x3010d0 expect=verified" "$LOG"
check "ccommon hook installed"     "probe: ccommon_fetch hook installed rva=0x2ec0e0 expect=verified" "$LOG"
check "cache hook installed"       "probe: fetch_cache_get hook installed rva=0x3089b0 expect=verified" "$LOG"
check "pack call logged"           "probe: pack_fetch obj=0x0 name=\"train\\\\113\\\\_d_113.dds\" dest=" "$LOG"
check "texture call logged"        'probe: texture_fetch ctx=0x0 src="train\\113\\_t_113_a.x" name="./_d_113.dds" mode=7' "$LOG"
check "ccommon call logged"        "probe: ccommon_fetch mgr=0x0 name=\"train\\\\113\\\\_d_113.dds\"" "$LOG"
check "cache call logged"          "probe: cache_get cache=0x0 name=\"train\\\\113\\\\_d_113.dds\"" "$LOG"
check "fakes still ran (via the trampolines)" "pack_fetch_body: called" "$scen_dir/out.txt"
checkf "all four probe bodies"     grep -c "called" "$scen_dir/out.txt"

echo "== P3: probe on, a9ml surfaces the diagnostic"
OUT=$("$A9ML" list --game "$scen_dir" 2>&1 || true)
if echo "$OUT" | grep -qF "probe: fetch hooks on (diagnostic)"; then
    echo "   [ ok ] a9ml list shows the probe flag"
else
    echo "   [FAIL] a9ml list missing the probe flag"
    echo "$OUT" | sed -n '1,12p'
    FAILED=$((FAILED + 1))
fi

# ---- M7: the purchase-window type-cycle fix + its opt-in probe -------------
# a train_list mod whose composed keep-set is exactly {4,16,42,329}: 4/16/42
# are in the shipped 97-entry whitelist, 329 (FORM_KIHA261_TOK) is not — the
# vanilla scan would drop it, the fix must keep it.
trainlist_mod() {
    mkdir -p "$scen_dir/mods/tablist"
    cat > "$scen_dir/mods/tablist/mod.toml" <<'EOF'
id = "tablist"
name = "Tab cycle fix probe"
version = "1.0.0"
author = "test"

[[hook]]
target = "train_list_filter"
handler = "train_list"
mode = "allow"
list = "forms.txt"
EOF
    printf 'FORM_0\nFORM_40\nFORM_205B\nFORM_KIHA261_TOK\n' > "$scen_dir/mods/tablist/forms.txt"
}

echo "== T1: the type-cycle fix rewrites the vanilla whitelist scan"
new_scen t1-tabfix
trainlist_mod
run_host tabfix
LOG="$scen_dir/mods/a9loader.log"
OUT="$scen_dir/out.txt"
check "fix installed"      "tab cycle fix: scan rva=0x19e7ee"        "$LOG"
check "cave recorded"      "cave rva=0x5b1e96"                      "$LOG"
checkf "host ran"          grep -q "tabfix: done"                   "$OUT"
check "site rewritten"     "site[0]=0x4d"                           "$OUT"
check "non-vanilla kept"   "329->1"                                 "$OUT"
check "vanilla kept"       " 4->1"                                  "$OUT"
check "unlisted dropped"   " 1->0"                                  "$OUT"
check "out-of-range dropped" "999->0"                               "$OUT"

echo "== F1: the custom-train-form fix rewrites the SECOND vanilla scan"
new_scen f1-formfix
trainlist_mod
run_host formfix
LOG="$scen_dir/mods/a9loader.log"
OUT="$scen_dir/out.txt"
check "fix installed"        "form readout fix: scan rva=0x19e9df"  "$LOG"
check "cave recorded"        "cave rva=0x5b2015"                   "$LOG"
checkf "host ran"            grep -q "formfix: done"               "$OUT"
check "site rewritten"       "formfix: site bytes: 89 d9 e8 2f 36 41 00 45 31 db 84 c0 75 65 90 90 90 90 90 90" "$OUT"
check "cave keeps non-vanilla" "329->1"                            "$OUT"
check "cave keeps vanilla"   " 4->1"                               "$OUT"
check "cave drops unlisted"  " 1->0"                               "$OUT"
check "cave drops out-of-range" "999->0"                           "$OUT"
check "scan branch: kept"    "scan branch: 329->1 4->1"            "$OUT"
check "scan branch: dropped" "1->2 999->2"                         "$OUT"

echo "== W1: window probe off (default) — no window probe lines"
new_scen w1-off
trainlist_mod
run_host window
checkf "host ran"          grep -q "window: done"                   "$scen_dir/out.txt"
if grep -q "probe: window\|probe: tab rebuild" "$scen_dir/mods/a9loader.log" 2>/dev/null; then
    echo "   [FAIL] window probe lines present with probe_window unset"
    FAILED=$((FAILED + 1))
else
    echo "   [ ok ] log clean of window probe lines"
fi

echo "== W2: window probe on (probe_window = true) — dumps + the evidence line"
new_scen w2-on
trainlist_mod
printf 'probe_window = true\n' > "$scen_dir/a9loader.toml"
run_host window
LOG="$scen_dir/mods/a9loader.log"
OUT="$scen_dir/out.txt"
check "probe enabled"       "probe: window hooks on (diagnostic)"      "$LOG"
check "tab hook installed"  "probe: tab rebuild (FUN_14019e760 — the type-cycle engine) hook installed rva=0x19e760 expect=verified" "$LOG"
check "window hook installed" "probe: window (train_list_big_list_window — the frame bracket) hook installed rva=0x2ca400 expect=verified" "$LOG"
check "window entry dump"   "probe: window #1 (entry) lists +0x30="   "$LOG"
check "window change dump"  "probe: window #2 (entry) lists +0x30="   "$LOG"
check "tab dump"            "probe: tab rebuild #1 lists +0x30="      "$LOG"
check "delta line"          "probe: tab rebuild   +0x48 delta:"       "$LOG"
check "per-row dump"        "probe: tab rebuild   +0x48 id="          "$LOG"
check "evidence line"       "tab rebuild #1 tab=#0 -> +0x48 4 rows kept by the composed keep-set" "$LOG"
checkf "host ran"           grep -q "window: done"                    "$OUT"

echo "== W3: probe_window on, a9ml surfaces the diagnostic"
OUT3=$("$A9ML" list --game "$scen_dir" 2>&1 || true)
if echo "$OUT3" | grep -qF "probe: window hooks on (diagnostic)"; then
    echo "   [ ok ] a9ml list shows the window probe flag"
else
    echo "   [FAIL] a9ml list missing the window probe flag"
    FAILED=$((FAILED + 1))
fi

harness_done "M4 PROBE HARNESS"
