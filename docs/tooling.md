# Authoring toolchain (`a9ml`)

The CLI replaces the ad-hoc authoring scripts. There is **no install command**: installing a
mod is still a folder copy. The CLI is optional; none of its output is a required install step.

## Common commands for mod authors

| Command | Purpose |
|---|---|
| `a9ml list --game <dir>` | list mods, enabled state, priority, resolved resolution |
| `a9ml doctor --game <dir>` | build identity, mod resolution, gates, loader log tail |
| `a9ml classify <path>` | where each data effect is consumed during boot (`early:<window>` means it must be served by inject/merge; `pre-shader`/`pre-csv` means stage 1). An `[[asset]]` is labelled `early:<host>` and "merge-served, window-time only" |
| `a9ml extract-csv --game <dir> --table NAME ...` | dump a shipped csvp table (UTF-16 verbatim by default; `--utf8` for an editable view; `--list` to name tables) |
| `a9ml patch-csv --game <dir> --table NAME --patch FILE ...` | apply `mode="rows"` row patches offline and report the merge |
| `a9ml extract-asset --game <dir> --name ROW ...` | dump one shipped i.dat row's raw payload from its host pack |
| `a9ml merge-assets --game <dir> --mod-dir M --out DIR` | build the offline `[[asset]]` merge (merged host packs + merged i.dat; nothing installed) |
| `a9ml diff-pack --pack KEY --base BASE --modded MODDED --out DIR --id … --name … --author … [--game-build ID]` | generate a `[[pack]]` mod from a vanilla base pack + repacked result |
| `a9ml diff-csv --base … --modded … --out DIR … [--rows]` | generate a `[[csv]]` mod from a base csvp + repacked one; `--rows` ships only changed/added rows as `mode="rows"` |
| `a9ml mkpack --container images|csvs|a|idat --out FILE …` | build a pack from raw payloads (authoring fixtures) |
| `a9ml gen-dirindex --data-dir DIR --out crates/a9-mod/dirindex.toml [--check]` | regenerate the `host="auto"` dir→pack index; `--check` verifies against a data dir |

## Format checks (optional)

| Command | Purpose |
|---|---|
| `a9ml verify` | format-library self-test plus pack sweep (`--data-dir`, `--oracle`, `--expect-rebuild`, `--only`) |
| `a9ml inspect <pack> [--all]` | decode + describe one pack |

## Useful during the edit loop

- Author a pack diff with `diff-pack`, then manually trim/adjust `width`/`height`.
- Author a CSV diff with `diff-csv`, or `diff-csv --rows` for a coexisting patch.
- Dry-run a row patch with `patch-csv` before shipping it.
- Build your `[[asset]]` offline with `merge-assets` to confirm offsets and index rows.
- After adding a new `dir`, the `host="auto"` index must be regenerated and re-embedded. The
  index ships inside the loader (an `include_str!` of `crates/a9-mod/dirindex.toml`), so this is
  a loader-maintainer step, not something a mod author ships: run `gen-dirindex` (verify with
  `--check`) and rebuild the DLL.

`a9ml gen-builds` exists to regenerate the address book from an exe (provenance-stamped,
core anchors only; hand-recorded RE anchors are left in place). Mod authors rarely need it.


---

← [Back to index](./README.md)
