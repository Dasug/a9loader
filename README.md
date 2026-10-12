# a9loader

A mod loader for A-Train 9 v5 — a Rust workspace. Everything a mod declares
executes at boot, inside the game process, through a `version.dll` proxy:
installing a mod is copying its folder; the game directory stays vanilla.

**Status: M4 — in-memory inject serving.** Mods are data (`mod.toml`
manifests in `mods/`); the engine gates on the running build's identity,
resolves dependencies/conflicts, applies expect-verified, journaled memory
effects at boot, and — for content mods — serves the mod's image-pack
entries **straight from loader memory** by extending the game's own pack
loader at work (window-scoped injection, no file written anywhere; the game
directory stays vanilla). `[[csv]]` tables still go through the merge
overlay (`pack_serving = "merge"` switches image packs back to it). The
architecture plan lives in the research corpus: `projects/mod-loader/PLAN.md`
(A-Train 9 Decompilation folder).

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
crates/a9-mod      the mod model: manifest schema, dependency resolution,
                   build identity, anchor address book, loader config
crates/a9-loader   the cdylib product (lib name "version" -> version.dll)
crates/a9ml        authoring & diagnostics CLI (verify, inspect, list, doctor)
harness/           wine harness: the full M2 engine scenarios (m2.sh)
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
- launch via Steam launch options: `WINEDLLOVERRIDES="version=n,b" %command%`
- the loader logs to `mods/a9loader.log`

The old C proxy's `atrain9_proxy.ini` is not read at all (that support was
dropped before anything shipped) — a filter is a mod like any other: copy
`atrain9_filter_model.txt` into a mod folder and point `[[hook]] list` at it.

## Mods (`mods/<id>/mod.toml`)

A mod is a folder in `mods/` with a `mod.toml`; installing = copying the
folder, removing = deleting it. Required fields: `id`, `name`, `version`,
`author`. Optional: `game_builds` (exact build ids gating memory effects),
`priority`, `supplies`/`requires`/`conflicts` (every mod implicitly supplies
its own id; conflicts are symmetric; lower priority loses unless
`allow_conflicts = true`).

```toml
id = "trainnames"
name = "Train names"
version = "1.0.0"
author = "you"

[[hook]]
target = "train_list_filter"     # named anchor (per-build address book)
handler = "train_list"            # built-in handler registry
mode = "allow"                   # or "deny" (drop the listed forms)
list = "atrain9_filter_model.txt"# inside the mod folder
```

`[[patch]]` writes raw bytes (`data` hex) at an anchor + offset, gated on
`expect` original bytes (raw `rva` targets are a diagnostic escape hatch and
require their own `expect`). Every anchored write verifies the address
book's recorded original bytes first — a game update refuses loudly instead
of corrupting memory.

Content effects (nothing materialized in the game dir):

```toml
[[pack]]                        # me*/mt* packs: entry replace-or-add by name
pack = "mtentr0"
[[pack.entry]]
name = "_d_h510_keifu_8000_tr_01.dds"
width = 512
height = 512
file = "entries/_d_h510_keifu_8000_tr_01.dds"   # inside the mod folder

[[csv]]                         # csvp tables: whole-table replace-or-add
name = "train_form.csv"
file = "csv/train_form.csv"

[[file]]                        # genuinely-new files, copied at boot (journaled)
dest = "data/texture/custom/x.dds"
file = "files/x.dds"
```

By default (`pack_serving = "inject"`) the loader does **not** rebuild the
pack at all: at the game's own load of `data/mtentr0.bin` it lets the
loader run, then hands it extended work — the mod's entries are appended to
the pack's last worker row from a loader-owned allocation, and the game's
own per-entry texture creation registers them. Nothing is written to disk
and the game reads its own original file. If the row pool, an anchor byte
or a payload is wrong, the loader logs loudly and the packs serve vanilla.

`pack_serving = "merge"` selects the M3 path instead: the loader decodes
the original pack, applies the mods' entries, re-encodes (LZSS + MD5/XOR
via `a9-formats`) and redirects the game's open to a content-hashed cache
under `mods/cache/`. `[[csv]]` tables always use the merge path.

The loader (`a9loader.toml`, next to the exe) can change `mods_dir`,
`pack_serving`, disable mods, override priorities/`order`, and allow
conflicts. `a9ml list --game <dir>` shows the offline resolution;
`a9ml classify <mods-dir>` reports where each target is consumed in the boot
windows (an image-pack texture must be served during its window — injection
or merge — which is why a post-boot cache poke does nothing). `a9ml doctor
--game <dir>` adds the build identity and the log tail. Mod authoring:
`a9ml diff-pack` / `a9ml diff-csv` generate a mod from a vanilla base pack +
your repacked result; `a9ml mkpack` builds packs from raw payloads.

## Test

```
cargo test                      # unit tests (codecs, mod model, engine logic)
harness/m2.sh [game-exe-path]   # M2 engine scenarios under wine: manifest
                                # mods, deny, wrong-build gate, overlap,
                                # requires/conflicts, patches, game_builds
                                # gate, broken lists; exits nonzero on failure
harness/m3.sh                   # M3 merge scenarios under wine: redirect,
                                # add/replace merges (byte-exact vs expected
                                # packs), csv tables, overlap, cache
                                # staleness/reuse, [[file]] copies + journal,
                                # fail-open on broken mods
harness/m4probe.sh              # M4 fetch-probe scenarios (diagnostic hooks)
harness/m4inj.sh                # M4 inject scenarios: the pack-window splice,
                                # row extension, pack capture, fail-open, and
                                # the runner's register/retry discipline
a9ml verify [--data-dir DIR] [--oracle DIR] [--expect-rebuild KEYS]
a9ml inspect <pack>             # decode + describe one pack
a9ml list --game DIR            # offline mod resolution for a game dir
a9ml doctor --game DIR          # + build identity, gates, loader log tail
a9ml diff-pack/diff-csv …       # generate a mod from a vanilla base + your
                                # repacked result
a9ml mkpack …                   # build a pack from raw payloads
```

`a9ml verify` proves the format library against real game data (the M1
acceptance: the live repacked packs re-materialize byte-identically);
`harness/m2.sh` is the M2 engine acceptance; `harness/m3.sh` is the M3
overlay acceptance (its byte-exact cache comparisons re-derive the M0
parity baseline through the whole overlay pipeline).

## AI usage disclosure

Development of this project — code, tests, build harness, and documentation —
was done with the assistance of an AI coding agent. All changes are human-reviewed.

## License

CC0-1.0 (see LICENSE).

I believe code that in large part generated with AI should not be copy-
rightable. You are free to use any of the code in this repository for any
reason. Just in case legislation disagrees in your or my jurisdiction, this
codebase is also licensed under CC0-1.0.
