# Mod folder layout

A mod is `mods/<id>/` where `<id>` matches the manifest `id` field. The loader scans
`<mods_dir>` (from `a9loader.toml`, default `mods`) one level deep for `mod.toml`.

```text
mods/
  my-mod/
    mod.toml
    entries/          # pack payloads referenced by [[pack]] entry.file
    csv/              # csvp payloads referenced by [[csv]].file
    assets/           # families/files referenced by [[asset]] (folder or single)
    allowed_trains.txt # side files referenced by [[hook]] list = ...
```

## Path rules

- All paths are relative to the **mod folder** for inputs (`file`, `folder`).
- Mod-side paths may contain `/` or `\`; the loader normalizes where needed.
- A side-file path referenced by a hook (`list = "..."`) is resolved relative to the mod folder.
- The no-absolute-path / no-drive / no-`..` rule is validated for two fields only:
  `[[loose_asset]].file` and `[[asset]].folder`. The other payload paths (`[[pack]].entry.file`,
  `[[csv]].file`, `[[asset]].file`, the hook `list`) are resolved relative to the mod folder but
  are not checked against that rule.

## `[[asset]] folder` layout guidance

Folder derivation uses the file's **subfolder path relative to the folder**:

```text
assets/
  train/
    mytrain/
      x.x                 # -> name = "x.x", dir = "train\mytrain"
      x_0.att             # -> name = "x_0.att", dir = "train\mytrain"
  x.dds                 # -> name = "x.dds", dir = "assets" (folder path itself)
```

So the natural layout is a container folder whose children are the game-side directories
(`train\…`), not a flat file list. A file directly in the folder takes the folder's own path
as its `dir`; when `folder = "."` (mod root), that falls back to the section's explicit `dir`.

## Name uniqueness

- Pack entries: names unique within one `[[pack]]` section.
- i.dat row names: globally unique across every mod and the shipped index.
- `.csv` table names and `[[loose_asset]]` patterns: unique within one mod (and within their
  respective arrays).

Duplicates across **different mods** are not a hard error: for assets they resolve by load
order, for CSV rows the earlier mod keeps the row, for pack entries they are a conflict. See
[diagnostics.md](./diagnostics.md).


---

← [Back to index](./README.md)
