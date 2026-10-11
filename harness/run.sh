#!/usr/bin/env bash
# M0 A/B harness — runs the C reference proxy and the Rust port through the
# same fake-game host under wine and compares behavior across all six ini
# modes, using the real FORM list (extracted from the game exe) and the real
# filter list files from the live game dir.
#
# usage: harness/run.sh [ATrain9v5.exe]
# env:   ATRAIN9_EXE  same as the positional arg
#        C_REF_DLL    C reference version.dll (default: corpus dll-proxy)
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"

GAME_EXE="${1:-${ATRAIN9_EXE:-/run/media/frybarec/dasug/Games/steamlibrary/steamapps/common/A-Train9v5/ATrain9v5.exe}}"
C_REF_DLL="${C_REF_DLL:-/home/dasug/Nextcloud/ClientSync/Projekte/A-Train 9 Decompilation/projects/dll-proxy/version.dll}"
GAME_DIR="$(dirname "$GAME_EXE")"
RUN="harness/run"
PREFIX="$REPO/$RUN/.prefix"

echo "== building host + Rust version.dll"
mkdir -p "$RUN"
x86_64-w64-mingw32-gcc -O2 -o "$RUN/host.exe" harness/host.c
cargo build --release --target x86_64-pc-windows-gnu
RUST_DLL="$REPO/target/x86_64-pc-windows-gnu/release/version.dll"

echo "== extracting FORM list from $GAME_EXE (file offset 0x6FB830)"
python3 - "$GAME_EXE" "$RUN/form_list.bin" <<'EOF'
import sys
exe, out = sys.argv[1], sys.argv[2]
data = open(exe, 'rb').read()
off = 0x6FB830
units = []
i = off
while i + 1 < len(data) and len(units) < 16384:
    u = data[i] | (data[i + 1] << 8)
    if u == 0:
        break
    units.append(u)
    i += 2
raw = b''.join(u.to_bytes(2, 'little') for u in units) + b'\x00\x00'
open(out, 'wb').write(raw)
names = ''.join(chr(u) for u in units).split(',')
print(f"   {len(names)} names, {len(raw)} bytes -> {out}")
EOF

FAILED=0
for MODE in none nop table113 filter model modeltex; do
  for VAR in c rust; do
    DIR="$RUN/$VAR"
    mkdir -p "$DIR"
    cp "$RUN/host.exe" "$RUN/form_list.bin" "$DIR/"
    if [ "$VAR" = c ]; then
      cp "$C_REF_DLL" "$DIR/version.dll"
    else
      cp "$RUST_DLL" "$DIR/version.dll"
    fi
    printf '[patch]\nmode = %s\n' "$MODE" > "$DIR/atrain9_proxy.ini"
    cp "$GAME_DIR"/atrain9_filter*.txt "$DIR/" 2>/dev/null \
      || echo "   (no atrain9_filter*.txt in game dir — kept=0 expected for filter modes)"
    (
      cd "$DIR"
      WINEPREFIX="$PREFIX" WINEDEBUG=-all wine host.exe form_list.bin "$MODE"
    ) > "$DIR/out.txt" 2>&1 || true
  done
  echo "== mode=$MODE"
  echo "--- C:";    sed 's/^/    /' "$RUN/c/out.txt"
  echo "--- Rust:"; sed 's/^/    /' "$RUN/rust/out.txt"
  # compare only the harness's own output lines — deterministic even if wine
  # prints banner/debug noise into the captured output
  if diff -q <(grep -E '^(mode=|forward )' "$RUN/c/out.txt") \
             <(grep -E '^(mode=|forward )' "$RUN/rust/out.txt") >/dev/null; then
    echo "--- PARITY OK"
  else
    echo "--- MISMATCH"
    FAILED=1
  fi
done

echo
if [ "$FAILED" = 0 ]; then
  echo "ALL MODES: Rust port behaviorally identical to the C reference."
else
  echo "MISMATCHES FOUND — see above."
fi
exit "$FAILED"
