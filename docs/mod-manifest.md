# Mod manifest (`mod.toml`)

Strict schema: unknown fields refuse the whole mod (`deny_unknown_fields`). Memory sections
(`[[patch]]`, `[[hook]]`) run at boot; content sections are planned at boot and served when the
game opens the file itself.

## Root fields

Required:

| Field | Type | Notes |
|---|---|---|
| `id` | string | lowercase slug `[a-z0-9][a-z0-9._-]*` |
| `name` | string | human-readable |
| `version` | semver | e.g. `"1.0.0"` |
| `author` | string | |

Optional:

| Field | Type | Default | Notes |
|---|---|---|---|
| `description` | string | | |
| `license` | string | | |
| `url` | string | | |
| `game_builds` | array of strings | `[]` (all builds allowed) | if non-empty, the running build id must be in the list or the mod's memory effects are refused; an empty list applies memory effects on every build |
| `priority` | integer | `50` | lower = earlier; overridable via `a9loader.toml` `[order]` |
| `supplies` | array of strings | `[]` | capabilities a mod provides beyond its own id |
| `requires` | array of strings | `[]` | must be supplied by an enabled mod |
| `conflicts` | array of strings | `[]` | symmetric; the lower-priority mod of the pair is skipped unless `allow_conflicts = true` (then both load with a warning) |

Every mod implicitly supplies its own `id`. Lists must not contain empty entries; a mod may
not conflict with itself.

## Dependency & conflict semantics

- `requires: X` is satisfied iff any **enabled** mod supplies `X` (own id or capability);
  otherwise the mod (and its dependents) is skipped with a logged error naming the missing item.
- `conflicts` is symmetric: a declaration on either side counts. By default the lower-priority
  mod of the pair is skipped. `allow_conflicts = true` downgrades to a warning and both load.
- Priority comes from the manifest's `priority` field; `[order]` in `a9loader.toml` overrides it
  (`sequence`, or a per-mod id entry).
- Resolution order: `order`/priority sort → dependency check → conflict check → effect plan
  build → overlap detection. When two effects write the same target, the later effect is refused
  and the overlap is logged at error level; the boot continues, and the earlier mod keeps it
  (never last-writer-wins).
- Mod manifests are validated before any effect runs.

## Effect sections at a glance

| Section | Kind | Purpose |
|---|---|---|
| `[[patch]]` | memory write | raw bytes at an anchor + offset, gated on expected original bytes |
| `[[hook]]` | function replace | install a built-in handler (`train_list`, `nop`) |
| `[[pack]]` | content (family-B image pack) | replace-or-add entries by name |
| `[[csv]]` | content (csvp table) | whole-table replace or row-level patch |
| `[[asset]]` | content (new i.dat row) | append a payload to a family-A host pack + merged i.dat row |
| `[[loose_asset]]` | data (path redirect) | serve mod bytes in place of a game file opened by path |

`[[file]]` was **retired**: the loader writes nothing into the game folder. A manifest that still
declares it is refused at scan time with a pointer to `[[asset]]` (pack content) or
`[[loose_asset]]` (game files opened by path).

Sections are arrays of tables:

```toml
[[hook]]
target = "train_list_filter"
handler = "train_list"
mode = "allow"
list = "allowed_trains.txt"
```

Details: [memory-effects.md](./memory-effects.md), [content-effects.md](./content-effects.md).

## Targets (`TargetRef`)

Used by `[[patch]]` and `[[hook]]`:

- Named anchor (recommended): `target = "train_list_filter"` — resolved per running build from
  the loader's embedded address book.
- Raw escape hatch (diagnostic only): `target = { rva = 0x1A13B0, expect = "48,89,…" }` —
  raw targets **require** their own `expect` (validation refuses otherwise).

Overlaps are tracked by the target key (+ offset), so a patch and a hook writing the same target
conflict with each other.

The loader splices its own seams for content serving: `cfile_open` (the overlay hook),
`famb_runner_call`/`famb_runner_cave` (inject), the `pack_lzss_*` family-A decode sites
(tailgate), and `workspace_*`. A mod that patches one of those collides with the machinery the
loader writes there at boot. Treat them as loader-owned; a mod should not target them.


## Example manifest with values

```toml
id = "example-mod"
name = "Example locomotive pack"
version = "1.2.3"
author = "Example Author"
description = "Adds a new train and updates its form names."
license = "MIT"
url = "https://example.invalid/mod"
game_builds = ["5.00.5634"]
priority = 10
supplies = ["example-train"]
requires = ["base-assets"]
conflicts = ["legacy-pack"]
# allow_conflicts is a loader-global in a9loader.toml, not here.

[[patch]]
target = "train_list_whitelist"
offset = 0
data = "A1,00,00,00"          # u32 little-endian (example)
expect = "BB,00,00,00"

# Replaces the filter function with a NOP (keep all forms)
[[patch]]
target = { rva = 0x1A13B0, expect = "48,89,5C" }
offset = 0
data = "90,90,90"

[[hook]]
target = "train_list_filter"
handler = "train_list"
mode = "add"                 # allow | add | deny
list = "allowed_trains.txt"

[[hook]]
target = "train_list_filter"
handler = "nop"

[[pack]]
pack = "mtentr0"
[[pack.entry]]
name = "_d_h510_keifu_8000_tr_01.dds"
width = 512
height = 512
file = "entries/_d_h510_keifu_8000_tr_01.dds"

[[csv]]
name = "train_form.csv"
file = "csv/train_form.csv"
mode = "replace"             # replace | rows

[[csv]]
name = "train_comment.csv"
file = "csv/train_comment.patch.csv"
mode = "rows"

[[asset]]
name = "_t_mynew_a.x"
dir = "train\\mynew"
host = "vp"                   # vp | tp | mcp | ep | cp | auto
file = "assets/_t_mynew_a.x"
override_vanilla = false

[[asset]]
folder = "assets/ships"
host = "auto"
override_vanilla = false

[[loose_asset]]
match = "data/texture/atlas/*.bin"
file = "loose/atlas.bin"
```


---

← [Back to index](./README.md)
