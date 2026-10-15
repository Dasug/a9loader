# Unsupported today (deliberately out of v1)

These features are **not** implemented in the current loader. They are listed here so mod
authors do not assume they exist. Roadmap items may change this list later.

## Not supported

- **`[native]` / code mods.** Not implemented: there is no ABI for mods shipping their own DLL yet —
  only the built-in handlers (`train_list`, `nop`) run in the game's call path. Declarative effects
  only. (A design for a code-mod tier exists as a planned milestone; this entry changes when it ships.)
- **Generic scripting** (Lua/JS sandboxes) — non-goal for v1.
- **AOB (signature) anchors.** Anchors are named RVA + expected bytes; no signature scanning.
- **`:BEFORE / :AFTER / :FINAL` ordering** between mods — order is priority/sequence only.
- **Per-mod config / save stores** — no API; mods are stateless apart from their payload files.
- **Content bundles / update checks / SDK packaging** — each mod is a folder; hosting/updating is
  out of scope.
- **Catalog extension past the 417 registered FORM slots.** A new train repurposes one of the 16
  inactive dummy slots that already exist inside the 417 (one is used by the bundled new-train example, so 15 remain), and that is the cap. New-slot FORM
  names need an engineering pass (unshipped).
- **In-image `_a0_*` sign-board family.** Those assets are EXE-embedded `.rsrc` resources, not
  pack entries; an `[[rsrc]]` effect is a future follow-up, not currently supported.
- **Auto-merge for `.dir` listings not proven name-keyed.** `base.dir` and the model/group
  listings keep the manual `override_vanilla` form until the RE pass lands.
- **Second build support.** Only `5.00.5634` is in the address book; other builds would need
  an entry (no mod changes for simple mods, but not available today).
- **Mod manager GUI / auto-download / mod hosting.** CLI first; GUIs can sit on top of it.

## Bounds of the supported features

- **Family-A workspace.** 200 MB out / 150 MB in as shipped. `workspace_out_mb` raises the OUT
  size, to at most 3584 MiB; a value outside `0..=3584` is refused (the key keeps its previous
  value) rather than clamped. A host merge that does not fit refuses rather than overrunning.
- **Family-B inject.** The game's worker-row pool is fixed at 24 rows and is full (24/24), so the
  injector extends an existing row instead of adding one. The number of entries per pack is not
  otherwise bounded.
- **Family-A cannot inject.** `[[asset]]` serves via the tailgate or the merge; `pack_serving =
  inject` is family-B only.
- **Tailgate refusals.** The tailgate refuses to append to a pack whose decode came out shorter
  than its declared size, rather than writing past what the decoder produced; that host falls
  back to merge.
- **New trains and saves.** One new train consumes one of the 16 inactive dummy slots (one is already used by the bundled new-train example, so 15 remain) inside the
  417. A saved formation stores each car's form id as a `u16`, and the loader's keep-bitmap
  covers ids below 1024, so a repurposed id must stay in range.
- **Loose assets.** Re-materialised under `mods/cache/loose/` on every boot; they are real files
  on disk, not memory-served.
- **`enabled = false`.** The loader-off escape hatch skips everything: no scan, no memory
  effects, no hooks, and no content mods. The boot is a pure proxy passthrough (fully vanilla).

## Summary of what an author CAN rely on

- Folder + `mod.toml` with the exact schema in [mod-manifest.md](./mod-manifest.md).
- Memory effects via `[[patch]]` and `[[hook]]` (`train_list`, `nop`).
- Content via `[[pack]]`, `[[csv]]`, `[[asset]]`, `[[loose_asset]]`.
- Loader config keys in [loader-config.md](./loader-config.md).
- Authoring/diagnostic CLI in [tooling.md](./tooling.md).


---

← [Back to index](./README.md)
