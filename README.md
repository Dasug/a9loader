# a9loader

A mod loader for A-Train 9 v5 — a Rust workspace. Everything a mod declares
executes at boot, inside the game process, through a `version.dll` proxy:
installing a mod is copying its folder; the game directory stays vanilla.

**Status: in-memory inject serving.** Mods are data (`mod.toml`
manifests in `mods/`); the engine gates on the running build's identity,
resolves dependencies/conflicts, applies expect-verified, journaled memory
effects at boot, and — for content mods — serves the mod's image-pack
entries **straight from loader memory** by extending the game's own pack
loader at work (window-scoped injection, no file written anywhere; the game
directory stays vanilla). `[[csv]]` tables still go through the merge
overlay (`pack_serving = "merge"` switches image packs back to it). 

## Supported builds

- **International Steam build** — version 5.00, build 5634. The initial port hard-
codes this build's addresses.

The loader reads the running build's identity itself and refuses
to patch on a mismatch, so a game update fails loudly instead of corrupting
memory. Supporting further builds (e.g. the Japanese retail editions) then
means adding an entry to the per-build address book — no mod changes needed
for simple mods.

## Layout

```
crates/a9-formats  every data-format codec: pack cipher, LZSS, family-A/B
                   containers, i.dat, csvp, savegame members
crates/a9-mod      the mod model: manifest schema, dependency resolution,
                   build identity, anchor address book, loader config, the
                   embedded per-build dir index
crates/a9-loader   the cdylib product (lib name "version" -> version.dll)
crates/a9ml        authoring & diagnostics CLI (verify, inspect, list, doctor)
harness/           wine harness: the engine and serving scenarios (m*.sh)
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

- `version.dll` (the C reference proxy is obsolete and was outside this repo)
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

[[csv]]                         # csvp tables: whole-table replace (default)…
name = "train_form.csv"
file = "csv/train_form.csv"

[[csv]]                         # …or row-level patches: row identity
name = "train_comment.csv"      # is the first column, so independent mods can
file = "csv/train_comment.patch.csv"   # edit one table and append rows without
mode = "rows"                   # taking the whole table over

[[asset]]                       # a NEW i.dat row: the payload is
name = "_t_mynew_a.x"           # appended to a family-A host pack's decoded
dir  = "train\\mynew"           # stream and the merged i.dat gets the row
host = "vp"                     # host pack: vp/tp/mcp/ep/cp, or "auto" (default)
file = "assets/_t_mynew_a.x"    # exact pack chunk (len = file size)
override_vanilla = false        # true = allowed to REPLACE a shipped i.dat row

[[asset]]                       # …or a whole FOLDER: every file under
folder = "assets"               # it, recursively, becomes one row — the file's
host = "auto"                   # basename is the row name and its subfolder
override_vanilla = false        # path is the row dir
```

`[[asset]]` is how a mod adds genuinely-new content (a train's `.x`/`.att`
model + LODs, a shop preview) rather than replacing textures/names — one row
per `name`+`file`, or one row per file under a `folder`. It is
always served through the **merge** path — inject has no family-A seam — so
the host pack and `data/i.dat` are decoded, extended in memory and served via
the same `CFile::Open` redirect `[[csv]]` uses. The host pack's new bytes land
at a deterministic offset (`off = |V| + Σ|payloads|`), and the merged index
row carries exactly that offset — the two come from one shared layout function,
so they cannot drift. Every referenced host pack's cache is built **before**
the merged index (atomicity): if any host merge fails the index merge is
refused too and vanilla is served, so a row can never point past a vanilla
image.

`folder` derives each row's `dir` from the file's subfolder path **relative to
the folder**: `folder = "assets"` + a file `assets/train/mytrain/x.x` becomes
`name = "x.x"`, `dir = "train\mytrain"`. A file directly in the folder has no
subfolder path, so it takes the folder's own path as its `dir` (`assets/x.x` →
dir `assets`); when the folder is the mod root (`folder = "."`) that falls back
to the section's explicit `dir`. Duplicate basenames anywhere in one folder are
a hard error (bare i.dat names are globally unique). So the natural layout is a
container folder whose children are the game-side dirs (`assets/train/…`).

`host = "auto"` (the default in both forms) resolves the host pack **offline**
from an embedded per-build `dir -> pack` index (`crates/a9-mod/dirindex.toml`,
generated by `a9ml gen-dirindex` from a live install's i.dat + packs). A-Train 9
addresses a row's payload as `workspace + off`, so the host must be the pack that
is resident while the asset's consumers run — a wrong host does not miss cleanly,
it reads another pack's bytes. The shipped rows encode that timing, so the
generator recovers it: every shipped row is verified against every decoded pack
buffer (ADX0 headers for `.x`, image magics for textures) and aggregated per
`dir`. Resolution: exact name, then exact dir, then the **longest known dir
prefix** (`train\mytrain` → the `train\*` family → vp); otherwise a **loud
refusal**. A dir whose shipped rows span packs is *ambiguous* — its consumers span
boot windows, so no single host can be correct — and refuses too. An explicit
`host = "vp"` (…) always overrides auto.

Two rules govern a name that already exists:
- **`override_vanilla` is about SHIPPED assets only.** Replacing a row that is
  already in the game's own `i.dat` requires `override_vanilla = true`;
  without it such a collision is a hard refusal (the engine's master-map
  insert-or-find would otherwise silently overwrite a shipped asset). The
  authoritative check runs at merge time, where the parsed vanilla table is in
  hand; `a9ml list` reports it offline too.
- **Mod-vs-mod collisions need no flag.** Two mods declaring the same new
  asset name are resolved by load order (higher priority / earlier wins) and
  the loser is logged and skipped — never a hard error. (Same policy as
  `[[csv]] mode = "rows"` row patches.)

By default (`pack_serving = "inject"`) the loader does **not** rebuild the
pack at all: at the game's own load of `data/mtentr0.bin` it lets the
loader run, then hands it extended work — the mod's entries are appended to
the pack's last worker row from a loader-owned allocation, and the game's
own per-entry texture creation registers them. Nothing is written to disk
and the game reads its own original file. If the row pool, an anchor byte
or a payload is wrong, the loader logs loudly and the packs serve vanilla.

`pack_serving = "merge"` selects the cache path instead: the loader decodes
the original pack, applies the mods' entries, re-encodes (LZSS + MD5/XOR
via `a9-formats`) and redirects the game's open to a content-hashed cache
under `mods/cache/`. `[[csv]]` tables always use the merge path. A
`[[csv]] mode = "rows"` section is a set of row patches keyed by the first
column: a key that exists replaces that row, a new key appends (after the
last row). Two mods editing different rows of one table coexist; two mods
editing the **same** row are an overlap (the earlier mod keeps the row,
the later patch is refused — the rest of that mod's rows still apply).
A merge that changes nothing reproduces the shipped table byte-identically;
added/replaced rows are written canonically (UTF-16, every field quoted,
the table's own line terminator).

The loader (`a9loader.toml`, next to the exe) can change `mods_dir`,
`pack_serving`, disable mods, override priorities/`order`, and allow
conflicts. `a9ml list --game <dir>` shows the offline resolution (including a
`[[asset]]` roll-up: new rows vs vanilla overrides, a warning for a name
that collides with a shipped row without `override_vanilla`, and the resolved
host for every `host = "auto"` row — or its refusal);
`a9ml classify <mods-dir>` reports where each target is consumed in the boot
windows (an image-pack texture must be served during its window — injection
or merge — which is why a post-boot cache poke does nothing; a `[[asset]]` is
labelled `early:<host>` and "merge-served, window-time only"). `a9ml doctor
--game <dir>` adds the build identity and the log tail. Mod authoring:
`a9ml diff-pack` / `a9ml diff-csv` generate a mod from a vanilla base pack +
your repacked result; `a9ml mkpack` builds packs from raw payloads (containers
`images`, `csvs`, and the family-A fixtures `a` and `idat`); `a9ml gen-dirindex`
regenerates the `dir -> pack` index (see below). Data-effect dry-runs:
`a9ml extract-csv` / `a9ml patch-csv` dump and merge the csvp tables;
`a9ml extract-asset` dumps one shipped `i.dat` row's raw bytes from whichever
pack hosts it, and `a9ml merge-assets` builds the exact merged `<host>.bin` +
`i.dat` a mod's `[[asset]]` sections would produce — both offline, using the
same format primitives the loader runs.

## Test

```
cargo test                      # unit tests (codecs, mod model, engine logic)
harness/m2.sh [game-exe-path]   # boot-engine scenarios under wine: manifest
                                # mods, deny, wrong-build gate, overlap,
                                # requires/conflicts, patches, game_builds
                                # gate, broken lists; exits nonzero on failure
harness/m3.sh                   # merge scenarios under wine: redirect,
                                # add/replace merges (byte-exact vs expected
                                # packs), csv tables, csv ROW patches (two mods
                                # on one table, byte-exact), row overlaps,
                                # cache staleness/reuse, fail-open on
                                # broken mods
harness/m4probe.sh              # fetch-probe scenarios (diagnostic hooks)
harness/m4inj.sh                # inject scenarios: the pack-window splice,
                                # row extension, pack capture, fail-open, and
                                # the runner's register/retry discipline
harness/m5.sh                   # [[asset]] scenarios: a family-A
                                # append + its i.dat row, the folder form with
                                # host = "auto" (host resolved from the embedded
                                # dir index), and the auto refusals (unknown /
                                # ambiguous dir) — byte-exact vs a9ml-built
                                # expectations; vanilla collision refusal,
                                # override_vanilla replace, mod-vs-mod load order
a9ml verify [--data-dir DIR] [--oracle DIR] [--expect-rebuild KEYS]
a9ml inspect <pack>             # decode + describe one pack
a9ml list --game DIR            # offline mod resolution for a game dir
a9ml doctor --game DIR          # + build identity, gates, loader log tail
a9ml diff-pack/diff-csv …       # generate a mod from a vanilla base + your
                                # repacked result (diff-csv --rows ships only
                                # the changed/added rows as a row patch)
a9ml mkpack …                   # build a pack from raw payloads
                                # (containers: images, csvs, a, idat)
a9ml gen-dirindex --data-dir DIR --out crates/a9-mod/dirindex.toml
                                # regenerate the host = "auto" dir -> pack
                                # index from a live install (--check verifies)
a9ml extract-csv --game DIR [--table NAME] [--out FILE] [--utf8] [--list]
                                # dump a shipped csvp table (or a UTF-8 view)
a9ml patch-csv --game DIR --table NAME --patch FILE [--out FILE] [--utf8]
                                # apply mode="rows" row patches offline + report
a9ml extract-asset --game DIR --name ROW [--out FILE] [--pack P]
                                # dump one shipped i.dat row's raw payload from
                                # its host pack (dir index, or a content scan)
a9ml merge-assets --game DIR --mod-dir M --out DIR
                                # build the offline [[asset]] merge: the merged
                                # <host>.bin pack(s) + merged i.dat (nothing is
                                # installed; the loader's own primitives)
```

`a9ml verify` proves the format library against real game data (the live
repacked packs re-materialize byte-identically); `harness/m2.sh` is the
engine acceptance; `harness/m3.sh` is the overlay acceptance (its byte-exact
comparisons re-derive the original-build parity baseline through the whole
overlay pipeline).

## AI usage disclosure

Development of this project — code, tests, build harness, and documentation —
was done with the assistance of an AI coding agent. All changes are human-reviewed.

## License

CC0-1.0 (see LICENSE).

I believe code that in large part generated with AI should not be copy-
rightable. You are free to use any of the code in this repository for any
reason. Just in case legislation disagrees in your or my jurisdiction, this
codebase is also licensed under CC0-1.0.
