# Memory effects: `[[patch]]`, `[[hook]]`

These run in the memory stage at boot. If `game_builds` is non-empty it must contain the running
build id or the mod's memory effects are refused; an empty `game_builds` applies them on every
build. Every write verifies expected original bytes first.

## `[[patch]]`

Writes raw bytes at an anchor + offset. Schema (`deny_unknown_fields`):

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | anchor name or `{ rva, expect }` | yes | named anchors preferred |
| `offset` | integer | no | byte offset from the anchor (default `0`); e.g. whitelist index × 4 |
| `data` | hex string | yes | non-empty valid hex |
| `expect` | hex string | no (anchors) / yes (raw) | expected original bytes at `rva+offset`; optional for anchors, required for raw targets; validated non-empty if present |

Behavior:

- The anchor's recorded original bytes (from the address book) are verified at the anchor
  before writing (for anchors).
- If `expect` is given, those bytes are verified at `rva+offset`.
- The write uses `VirtualProtect` → verify → write → restore → `FlushInstructionCache`.
- Each write is journaled to `mods/a9loader.log` as one line, e.g.
  `patch <target> rva=… off=… len=… expect=verified write=ok` (there is no old-byte/new-byte
  field).
- A mismatch refuses the write, never corrupts:
  `{what}: memory differs from recorded bytes (found …, expected …) — refusing`.

Hex format: comma or no-comma hex bytes, same as the `expect` fields in `builds.toml`
(e.g. `"48,89,5c,24,08"` or `"48895c2408"`).

## `[[hook]]`

Installs a built-in handler at an anchor. Schema:

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | anchor name or raw | yes | |
| `handler` | string | yes | `"train_list"` or `"nop"` |
| `mode` | string | no | `"allow"` (default), `"add"`, or `"deny"`; only for `train_list` |
| `list` | string | yes (for `train_list`) | path to a list file inside the mod folder |

`nop` takes **no** `mode`/`list` (validation refuses them).

### `handler = "train_list"`

Filters the game's FORM render table using a list file. List file format: one FORM name per
line, read by the loader's `forms` parser. Effects:

- `allow` (default): keep only the listed forms.
- `add`: offer the listed forms **in addition to** the shipped 97-entry purchase whitelist
  (and any `allow` list). Use this for new-train mods.
- `deny`: drop the listed forms. Applied last, so deny always wins.

Multiple mods' `train_list` contributions are composed into **one** bitmap at boot:
`allow` mods union their lists; a deny subtracts; `add` starts from the shipped whitelist
baseline. Composition requires all contributions to resolve to the same target RVA
(`train_list_filter`). The filter hook is installed once. Progress is logged per contribution
(matched/unmatched counts).

The list file itself is a text file inside the mod folder; `train_list` is the only handler
that reads a side file.

### `handler = "nop"`

Writes a NOP stub at the anchor. Effect depends on the anchor. The only documented use in the
current loader is disabling a filter (all forms kept): where the anchor is the filter
function, `nop` keeps every form. No parameters.

## Named anchors available to mods

| Anchor | Meaning |
|---|---|
| `train_list_filter` | the FORM-list filter function (97-entry whitelist context) |
| `train_list_whitelist` | the 97-entry u32 whitelist table base |
| `form_list` | UTF-16 comma-separated FORM name list (read by the loader when building a bitmap) |

Other anchors exist in `builds.toml` (`cfile_open`, `pack_fetch`, `texture_fetch`, …) but are
diagnostic/probe anchors; do not use them for mod effects unless you know the RE. Several are
loader-owned seams: the loader splices `cfile_open` (overlay), `famb_runner_call` and
`famb_runner_cave` (inject), the `pack_lzss_*` family-A decode sites (tailgate), and
`workspace_*`. A mod that patches one collides with that machinery. Raw `rva` targets are the
only way to touch an unlisted address (with `expect`).


---

← [Back to index](./README.md)
