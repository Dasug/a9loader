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

harness_done "M4 PROBE HARNESS"
