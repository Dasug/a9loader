# Content effects: packs, CSV, assets, loose assets

All of these are data-stage effects: they do **not** touch the exe's addresses, and several
may omit `game_builds`. Content is merged/served **in memory at the game's own file-open
time**; the game directory stays vanilla.

## Retired: `[[file]]`

`[[file]]` (a boot copy into the game tree, journaled to `mods/journal.json`) was removed: the
loader writes nothing into the game folder. A manifest that still declares it is refused at scan
time with a pointer to the replacements — `[[asset]]` for pack content, `[[loose_asset]]` for
game files opened by path.

## `[[pack]]` — family-B image pack entries

Adds or replaces entries inside a family-B pack (`data/<pack>.bin`), such as `me0`–`me3`,
`mtentr0`, `mtcutp0`, `mttkmu0`.

Schema:

| Field | Type | Required | Notes |
|---|---|---|---|
| `pack` | string slug | yes | pack key without path/extension; must match a family-B pack the game opens as `data\<pack>.bin`. `csvp` is illegal here — use `[[csv]]` |
| `entry` | array of tables | yes, non-empty | one or more entries |
| (per entry) `name` | string | yes | pack entry name; must be non-empty and contain no NUL; unique within the section |
| (per entry) `width` | u32 | yes | image width |
| (per entry) `height` | u32 | yes | image height |
| (per entry) `file` | string | yes | payload file relative to the mod dir; non-empty |

Rules:
- One `[[pack]]` section per pack name (merge duplicate sections into one).
- An entry with an existing pack name **replaces** that entry; a new name is added.
- Two mods touching the same entry name → overlap: the earlier/lower-priority mod keeps it, the
  later is refused.
- Entry payload is the raw DDS bytes (or whatever the pack stores); the loader does not
  re-encode the image.
- A `pack` that is not a family-B image pack is refused and skipped with a log line. Family-A
  streams (`vp`/`tp`/`mcp`/`ep`/`cp`) have no entry table, and `csvp` belongs in `[[csv]]`
  (validation also refuses `csvp`).

Serving is controlled by `pack_serving` (`inject` default, `merge` alternative):
- `inject`: appended to the family-B worker-row pool at inject time (no cache file, no whole-pack re-encode). `[[pack]]` family-B effects only.
- `merge`: the original pack is decoded, edited, re-encoded (LZSS + MD5/XOR), and redirected to a
  loader-managed, content-hashed cache under `<mods_dir>/cache/`.

## `[[csv]]` — csvp tables

Schema:

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | table name inside csvp, e.g. `"train_form.csv"`; no NUL, non-empty; unique per mod |
| `file` | string | yes | CSV payload relative to the mod dir |
| `mode` | `"replace"` / `"rows"` | no | default `"replace"` |

Modes:
- `replace` (default): the file **is** the whole table; it takes the table over. Existing
  whole-table mods (e.g. train-name mods) keep this exact behavior. A no-op replace reproduces
  the shipped table byte-for-byte.
- `rows`: the file holds **only** the rows to edit/add, keyed by the first column. Existing key
  replaces that row; a new key appends after the last row. Independent mods can edit different
  rows of one table. Two mods editing the **same** row refuse the later patch (the rest of
  that mod's rows still apply).

CSV payloads may be UTF-16LE (game-native) or UTF-8 for `mode = "rows"` row-patch files; the
game's own tables are UTF-16LE. All CSV tables go through the merge overlay regardless of
`pack_serving`.

## `[[asset]]` — new i.dat rows (family-A)

A mod can add a new content row (model, texture, `.dir` listing, etc.) whose payload is
appended to a family-A host pack's decoded stream (`vp`/`tp`/`mcp`/`ep`/`cp`) and registered
in the merged `i.dat`. Served via the family-A tailgate or the merge. Family-A content cannot
use inject at all: `pack_serving = inject` is family-B only and does not apply here.

Schema (single-file form — one row per section):

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes (single) | bare i.dat / master-map key, no directory; globally unique |
| `dir` | string | no | i.dat `dir` field (consumer grouping), e.g. `"train\\mynew"`; backslashes allowed |
| `host` | string | no | `"auto"` (default) or `vp`/`tp`/`mcp`/`ep`/`cp` (family-A opaque stream) |
| `file` | string | yes (single) | payload relative to the mod dir; `len` = file size (a `.x` includes its 12-byte ADX0 header) |
| `override_vanilla` | bool | no | default `false`; see override rules |

Folder form — one section expands to one row per file:

| Field | Type | Required | Notes |
|---|---|---|---|
| `folder` | string | yes (folder) | mod-relative folder; recursively scanned |
| `host` | string | no | default `"auto"` |
| `dir` | string | no | used only when `folder == "."` (mod root) as the fallback dir |
| `override_vanilla` | bool | no | applies to every file of the folder |

Exactly one of {`name`+`file`} or {`folder`} per section (validation).

Folder derivation: file `assets/train/mytrain/x.x` with `folder = "assets"` becomes
`name = "x.x"`, `dir = "train\mytrain"`. A file directly in the folder takes the folder path as
its `dir`; when `folder = "."` that falls back to the section's explicit `dir`. Duplicate
basenames anywhere in one folder (expanded) are a hard error — bare i.dat names are globally
unique.

`host = "auto"` resolves the host pack offline from an embedded per-build `dir -> pack` index:
exact name → exact dir → longest known dir prefix (e.g. `train\mytrain` → the `train\*`
family → `vp`); unknown or ambiguous dirs are refused. Ambiguous means the shipped rows for
that dir span multiple packs, so no single host can be correct (explicit `host` still overrides).

Override rules (shipped vs mod-mod collisions):
- `override_vanilla` concerns **shipped** assets only. Replacing a row already in the game's
  own `i.dat` requires `override_vanilla = true`; without it the collision is a hard refusal
  (the engine's master-map insert-or-find would otherwise silently overwrite a shipped asset).
  Authoritative check runs at merge time; `a9ml list` reports it offline too.
- Mod-vs-mod collisions need **no** flag: two mods declaring the same new asset name resolve by
  load order (higher priority/earlier wins); the loser is logged and skipped — never a hard
  error (same policy as `[[csv]] mode = "rows"` row patches).

Every host-pack cache is built **before** the merged i.dat (atomicity): any host merge failure
refuses the i.dat merge too, so a row can never point past a vanilla image.

`asset_serving` in `a9loader.toml` (`tailgate` default, `merge` alternative):
- `tailgate`: appends payloads/index rows in place into the family-A decode workspace at the
  game's decode call (no cache file).
- `merge`: decodes → appends → re-encodes the host pack to `<mods_dir>/cache/` and redirects
  its open; also the automatic fallback when tailgate cannot run (missing anchor/expect bytes,
  a failed splice, unreadable declared size, or a host whose payloads don't fit the workspace
  slack).
- The tailgate refuses to append to a pack whose decode came out shorter than its declared
  size, rather than writing past what the decoder produced; that host falls back to merge.

## `[[loose_asset]]` — game files opened by path

The game opens some files by exact/random path rather than through a pack. The loader serves
the mod's bytes in place of the file on disk, matched by a case-insensitive glob over the game's
own open path.

Schema:

| Field | Type | Required | Notes |
|---|---|---|---|
| `match` | string | yes | case-insensitive glob; `*` = any run (incl. empty); `?`/`[`/`]` are ordinary characters; no `*` = exact path. Backslashes are normalized. Non-empty, no NUL. Unique per pattern (case-insensitively) |
| `file` | string | yes | payload relative to the mod dir; no absolute/drive/`..` |

Behavior: each payload is materialized into a real file under `<mods_dir>/cache/loose/` and the
game's open path is rewritten to it (the handle is plain Win32, so no memory-source branch is
needed). The cache file is named `<mod-id>__<sanitized pattern>` (both parts sanitized to
`[a-z0-9._-]`; with a `__2`, `__3`, … counter when two patterns in one mod sanitize to the same
string), so two mods never collide. Payloads are written fresh on every boot. Use this when the game writes to a
per-boot random temp directory and reads it back (e.g. bitmap-font atlases `fe01.bin`…`fs02.bin`).

## Auto-merged `.dir` listings

A `.dir` row is a flat run of NUL-terminated UTF-16LE names that scopes a group load in a
family-A pack (e.g. `previewtexture.dir`, `base.dir`). Because the listing is a whole row, two
mods adding assets to one group would collide on `asset:<group>.dir`.

The loader derives the names a mod contributes to a group from its own `[[asset]]` rows
(`dir = D` ⇒ append that row's name to the `D.dir` listing if it exists), unions them across
all enabled mods, and rebuilds the listing payload (vanilla order first, new names appended,
deduped). Authors therefore never write a `.dir` file. Auto-merge ships only for listings
proven name-keyed (`previewtexture.dir` is proven; `base.dir` and others keep the manual
`override_vanilla` form until their RE pass).

## Automatic host

For new assets, `host = "auto"` consumes the shipped row's window timing — the host must be
the pack resident while the asset's consumers run. A wrong host does not miss cleanly; it reads
another pack's bytes. The embedded index recovers that timing by verifying every shipped row
against every decoded pack buffer and aggregating per `dir`.

## Capacity notes

- Family-A workspace is 200 MB out / 150 MB in by default. `workspace_out_mb` in
  `a9loader.toml` raises the OUT size (0 = auto: grow only when the planned content does not
  fit 200 MB); values outside `0..=3584` are refused rather than clamped.
- Engine decode workspace assertion: rebuilt packs ≤ ~190 MB decoded. An oversized host merge
  refuses rather than corrupting.
- Extension past the 417 registered FORM slots is **not** part of the supported authoring
  surface. A new train repurposes one of the 16 inactive dummy slots inside the 417 (one is already used by the bundled new-train example, so 15 remain), and that
  is the cap on repurposed slots.
- A saved formation stores each car's form id as a `u16`, and the loader's keep-bitmap covers
  ids below 1024. See [limits.md](./limits.md).


---

← [Back to index](./README.md)
