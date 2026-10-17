#!/usr/bin/env bash
# Inject harness — pack_serving = "inject" under wine. The fake host
# carries the famb pool-runner call site at the real anchor RVA (builds.toml
# `famb_runner_call` bytes: E8 + the read-only test ebx,ebx pair), a fake
# runner at the call target (its own image + 0x504c0), and the pool /
# workspace / capacity globals the injector stub reads. The host calls the
# spliced site the way the game's loader does: after a hooked cfile_open of
# the pack (the capture seam), with the vanilla row refill already in place.
#
# Scenarios: S1 merge mode is the cache-overlay behavior with the inject pages present;
# S2 inject mode appends one worker row over the vanilla slice (fake runner
# sees count+1, forwarded rax, EBX sentinel preserved, site continuation
# bytes untouched); S3 a missing payload fails loud + vanilla serve; S4
# repeats S2's setup asserting the register/byte discipline alone.
#
# usage: harness/m4inj.sh
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"
source "$(dirname "$0")/lib.sh"

RUN="$REPO/harness/run/m4inj"
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

echo "== payload fixtures (raw entry content, 16x16-style blobs)"
mk_bin() { # mk_bin <name> <bytes>
    printf '%b' "$2" > "$RUN/$1"
}
mkdir -p "$RUN/data"
mk_bin a.bin 'AAAA'
mk_bin b.bin 'BBBBBBBB'
mk_bin n4.bin 'N4N4N4'
"$A9ML" mkpack --container images --out "$RUN/data/mtfake.bin" \
    "a.dds=64:64:$RUN/a.bin" "b.dds=128:128:$RUN/b.bin"
ls -la "$RUN/n4.bin" "$RUN/data/mtfake.bin" >/dev/null

scen_fixtures() {
    cp "$RUN/data/mtfake.bin" "$scen_dir/data/"
}

# m4inj's needles carry regex metacharacters — match them as FIXED strings
check() { # check <label> <needle> <file>
    if grep -qF -- "$2" "$3"; then
        echo "   [ ok ] $1"
    else
        echo "   [FAIL] $1 (missing: $2 in $3)"
        FAILED=$((FAILED + 1))
    fi
}

paymissing() { # a missing payload: manifest only, no file copied
    true
}

echo "== S1: merge mode (default) — cache-overlay behavior, no splice"
new_scen s1-merge
# S1 exercises the MERGE path — pin it (inject is the default now)
printf 'pack_serving = "merge"\n' > "$scen_dir/a9loader.toml"
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
pay addone "entries/n1.bin" n4.bin
run_host inject
check "merge redirect (cache overlay)"     "path=mods\\cache\\mtfake.bin" "$scen_dir/out.txt"
check "vanilla pool run (no extension)"  "inject runner: count=3"     "$scen_dir/out.txt"
check "cache built by the merge"         "1 entry merged (1 added"    "$scen_dir/mods/a9loader.log"
if grep -q "inject:" "$scen_dir/mods/a9loader.log"; then
    echo "   [FAIL] inject lines in merge mode"
    FAILED=$((FAILED + 1))
else
    echo "   [ ok ] no inject machinery in merge mode"
fi

echo "== S2: inject mode — one added entry rides a new worker row"
new_scen s2-inject
printf 'pack_serving = "inject"\n' > "$scen_dir/a9loader.toml"
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
pay addone "entries/n1.bin" n4.bin
run_host inject
LOG="$scen_dir/mods/a9loader.log"
check "splice installed"              "inject: famb splice installed rva=0x3f0259 expect=verified" "$LOG"
check "capture hook installed"        "inject capture hook" "$LOG"
check "extension journaled"           "inject famb: mtfake extended by 1 entry" "$LOG"
check "open went through the capture" "cfile_open_body: path=data\\mtfake.bin" "$scen_dir/out.txt"
check "runner saw the pool count"     "inject runner: count=3" "$scen_dir/out.txt"
check "last row extended (slice)"     "row2: start=2 entries=2 mode=0x5a" "$scen_dir/out.txt"
check "runner rax forwarded"          "runner rax forwarded = 0x4711 (PASS)" "$scen_dir/out.txt"
check "EBX sentinel preserved"        "ebx sentinel PRESERVED (PASS)" "$scen_dir/out.txt"
check "site continuation bytes intact" "site continuation bytes untouched (PASS)" "$scen_dir/out.txt"
if [ -f "$scen_dir/mods/cache/mtfake.bin" ]; then
    echo "   [FAIL] inject mode must not build a merge cache"
    FAILED=$((FAILED + 1))
else
    echo "   [ ok ] no merge cache in inject mode"
fi

echo "== S3: inject mode, missing payload — loud failure, vanilla serve"
new_scen s3-missing
printf 'pack_serving = "inject"\n' > "$scen_dir/a9loader.toml"
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
paymissing
run_host inject
LOG="$scen_dir/mods/a9loader.log"
check "loud failure"        "payload unreadable" "$LOG"
check "vanilla this boot"   "vanilla serving this boot" "$LOG"
check "vanilla pool run"    "inject runner: count=3" "$scen_dir/out.txt"

echo "== S4: inject mode — register/byte discipline (JEQ over the spliced site)"
new_scen s4-retry
printf 'pack_serving = "inject"\n' > "$scen_dir/a9loader.toml"
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
pay addone "entries/n1.bin" n4.bin
run_host inject
check "EBX sentinel preserved"           "ebx sentinel PRESERVED (PASS)" "$scen_dir/out.txt"
check "runner rax forwarded"             "runner rax forwarded = 0x4711 (PASS)" "$scen_dir/out.txt"
check "site continuation bytes untouched" "site continuation bytes untouched (PASS)" "$scen_dir/out.txt"
check "host still returns cleanly"       "inject host: cfile_open" "$scen_dir/out.txt"

echo "== P: a9ml surfaces the pack-serving mode"
printf 'pack_serving = "inject"\n' > "$scen_dir/a9loader.toml"
OUT=$("$A9ML" list --game "$scen_dir" 2>&1 || true)
if echo "$OUT" | grep -qF 'packs: inject — mtfake (1 entry)'; then
    echo "   [ ok ] a9ml list shows the inject plan"
else
    echo "   [FAIL] a9ml list missing the inject plan"
    echo "$OUT" | sed -n '1,14p'
    FAILED=$((FAILED + 1))
fi

harness_done "M4 INJECT HARNESS"
