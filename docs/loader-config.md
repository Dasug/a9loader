# Loader configuration (`a9loader.toml`)

Lives next to the exe (default `<exe-dir>/a9loader.toml`). Missing file = defaults, no
warnings (that is normal). Broken file = defaults + warnings (the game must always boot).

Parsed leniently: unknown keys are **warnings**, not silent, because a typo such as `mods_dr`
must be visible in the log rather than quietly ignored. Bad values fall back to the default with
a warning.

## Keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `log_level` | `"error"|"warn"|"info"|"debug"` (also `"warning"`) | `"info"` | log verbosity |
| `mods_dir` | string | `"mods"` | mods folder relative to the exe dir; simple relative name, no `..`, trailing slashes stripped |
| `allow_conflicts` | bool | `false` | downgrade conflicts to warnings |
| `disabled` | array of mod ids | `[]` | disable specific mods (presence in the folder = enabled otherwise) |
| `enabled` | bool | `true` | loader-off escape hatch: `false` = no scan, no effects, no hooks — pure proxy passthrough, fully vanilla boot |
| `probe_fetch` | bool | `false` | diagnostic (off by default); hooks the four runtime fetch anchors trampoline-only and logs one line per fetch. No serving behavior change |
| `probe_window` | bool | `false` | diagnostic; dumps the spec-table manager's index vectors on window changes |
| `probe_save` | bool | `false` | diagnostic; logs save-path writes (caller RVA, size, first bytes) |
| `probe_consist` | bool | `false` | diagnostic; logs the consist object's per-car entries and the source row's scalar fields when the game builds a consist |
| `probe_opens` | bool | `false` | diagnostic; one line per intercepted file open (`open trace: <path>`) — was previously tied to `log_level = debug` |
| `probe_rows` | bool | `false` | diagnostic; the most verbose probe: at every `train_list_builder` call it dumps named spec rows (model array, per-car block, scalar fields) and the injected rows' 18 name lanes |
| `pack_serving` | `"inject"` / `"merge"` | `"inject"` | how `[[pack]]` image effects serve (see [content-effects.md](./content-effects.md)) |
| `asset_serving` | `"tailgate"` / `"merge"` | `"tailgate"` | how `[[asset]]` new assets serve (see [content-effects.md](./content-effects.md)) |
| `workspace_out_mb` | integer `0..=3584` | `0` | family-A workspace OUT size in MiB. `0` = auto: grow only when the planned family-A content (vanilla declared size + appended payloads) exceeds 200 MB, with 64 MB headroom |
| `order` | table | none | load-order control (see below) |

## `order` (load order)

```toml
[order]
sequence = ["mod-a", "mod-b"]   # explicit total order; unlisted mods sort by priority behind the listed
mod-x = 10                     # per-mod priority override (mod id = integer)
```

Rules:
- `sequence` is an array of mod ids.
- Any other key under `[order]` is a priority override (mod id → integer). Lower = earlier.
- If `order.sequence` is present, listed mods come first in that order; unlisted follow by priority.
- Mod priority declared in `mod.toml` is overridden by `order.<id>`.

## Example full config

```toml
log_level = "debug"
mods_dir = "mods"
allow_conflicts = false
disabled = ["broken-mod"]
pack_serving = "inject"
asset_serving = "tailgate"
workspace_out_mb = 0

[order]
sequence = ["base-assets", "jp-liveries"]
jp-liveries = 10
```


---

← [Back to index](./README.md)
