# a9loader

A mod loader for A-Train 9 v5 — a Rust workspace. Everything a mod declares
executes at boot, inside the game process, through a `version.dll` proxy:
installing a mod is copying its folder; the game directory stays vanilla.

**Status: M1 — format library with proofs.** The `version.dll` proxy parity
port (M0) is live in the game install; the general engine (build-id gate, mod
manifests, runtime overlay) lands with M2/M3. The architecture plan lives in
the research corpus: `projects/mod-loader/PLAN.md` (A-Train 9 Decompilation
folder).

## Supported builds

- **International Steam build** — version 5.00, build 5634. The M0 port hard-
codes this build's addresses.

From M2 on, the loader reads the running build's identity itself and refuses
to patch on a mismatch, so a game update fails loudly instead of corrupting
memory. Supporting further builds (e.g. the Japanese retail editions) then
means adding an entry to the per-build address book — no mod changes needed
for simple mods.

## Layout

```
crates/a9-formats  every data-format codec: pack cipher, LZSS, family-A/B
                   containers, i.dat, csvp, savegame members
crates/a9-loader   the cdylib product (lib name "version" -> version.dll)
crates/a9ml        authoring & diagnostics CLI (verify, inspect)
harness/           wine A/B harness: fake-game host, compares this port with
                   the C reference across all six ini modes
```

## Build

Both paths produce the same artifact: `version.dll`.

### Native (Windows)

Default Rust toolchain (rustup, stable; the MSVC linker requirement is
satisfied by the Visual Studio Build Tools):

```
cargo build --release
:: -> target\release\version.dll
```

### Cross-compiling (Linux)

The development-tested path. Requires `rustup target add
x86_64-pc-windows-gnu` and `mingw-w64-gcc` (e.g. the `mingw-w64` package).

```
cargo build --release --target x86_64-pc-windows-gnu
# -> target/x86_64-pc-windows-gnu/release/version.dll
```

## Install (game dir, next to ATrain9v5.exe)

- `version.dll` (replaces the C reference proxy — the C original stays in the
  corpus `projects/dll-proxy/`)
- `atrain9_proxy.ini` + `atrain9_filter*.txt` — unchanged from the C proxy
  (M0 is behaviorally identical)
- launch via Steam launch options: `WINEDLLOVERRIDES="version=n,b" %command%`
- the loader logs to `a9loader.log` next to the exe

## Modes (`atrain9_proxy.ini`, `[patch]` `mode = …`)

| mode | effect |
|---|---|
| `filter` (default) | keep the forms listed in `atrain9_filter.txt` |
| `model` | keep the forms in `atrain9_filter_model.txt` |
| `modeltex` | keep the forms in `atrain9_filter_modeltex.txt` |
| `nop` | keep every valid form (all 417) |
| `table113` | legacy diagnostic: whitelist entry 6 = FORM_113 |
| `none` | pure proxy, no patch |

Semantics are identical to the C reference (see the corpus
`projects/dll-proxy/README.md`), including the sharp edges: a missing/empty
list file in a filter mode installs the hook with an **empty** allow list.

## Test

```
cargo test                      # unit tests (codecs, containers, i.dat, savegame)
harness/run.sh [game-exe-path]  # loader A/B against the C reference under
                                # wine: all six modes, real FORM list + real
                                # filter lists; exits nonzero on any mismatch
a9ml verify [--data-dir DIR] [--oracle DIR] [--expect-rebuild KEYS]
a9ml inspect <pack>             # decode + describe one pack
```

`a9ml verify` proves the format library against real game data: every pack
in each `--data-dir` is decrypted (MD5 integrity check), decoded, structurally
validated (container walks must consume exactly), re-encoded and round-tripped;
with `--oracle` (a dir holding the C reference tools `a9enc`/`a9dec`) it also
byte-differs the LZSS encoder/decoder against the C reference; with
`--expect-rebuild` it requires named packs to re-materialize byte-identically.
The harness is the M0 acceptance evidence; `a9ml verify` is the M1 one
(the live repacked packs `me3`, `mtcutp0`, `mtentr0`, `csvp` re-materialize
byte-identically through the library).

## AI usage disclosure

Development of this project — code, tests, build harness, and documentation —
was done with the assistance of an AI coding agent. All changes are human-reviewed.

## License

CC0-1.0 (see LICENSE).

I believe code that in large part generated with AI should not be copy-
rightable. You are free to use any of the code in this repository for any
reason. Just in case legislation disagrees in your or my jurisdiction, this
codebase is also licensed under CC0-1.0.
