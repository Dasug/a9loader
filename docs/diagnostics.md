# Diagnostics & failure modes

## Loader log

`mods/a9loader.log` (UTF-8). Each line is `[HH:MM:SS] <tag><message>`, where the tag is empty
for info, or `DBG:  `, `WARN: `, `ERR:  ` (`crates/a9-loader/src/log.rs`). Verbosity follows
`log_level` in `a9loader.toml` (`error`/`warn`/`info`/`debug`, default `info`). On the first
write of a boot, a log file already larger than 1 MB is deleted and the boot starts a fresh one;
no previous file is kept.

Every boot writes:
- startup banner + build-id verdict (which build was detected, gate pass/fail);
- per-mod plan (enabled, skipped, reasons);
- patch journal, one line per write: `patch <target> rva=… off=… len=… expect=verified write=ok`;
- overlay/merge decisions and cache paths;
- conflicts, overlaps, and refusals with reasons;
- a closing count line: `result: N memory effect(s) applied, M refused — game continues`.

`a9ml doctor --game <dir>` resolves the mods offline, checks whether `version.dll` is present
next to the exe, and prints the last 30 lines of the log. There is no game-directory integrity
or pristine-install check anywhere: nothing compares `data/` against a known-good copy, and no
"Steam verify files" / revert detection exists. `a9ml verify` is unrelated to the game directory;
it is a format-library proof (self-tests plus a decrypt/decode/round-trip sweep of pack files,
with an optional C oracle).

The probes `probe_fetch`, `probe_save`, `probe_window`, `probe_consist` and `probe_rows` are off by
default and log extra lines; they are not part of normal operation. Each answered a specific
question during development — `probe_rows` (the spec-row dump) is the one that solved the
wrong-mesh and consist-selector questions and is kept for future train work, but it is also the most
verbose, so it is strictly opt-in. See [loader-config.md](./loader-config.md).

## Fail-open philosophy

The loader never lets a mod corrupt the game:
- An unknown/unsupported build ⇒ the loader stops before scanning mods: no memory or content
  effects run and the game boots vanilla. A loud log line names the detected build and the
  supported set.
- A mod whose non-empty `game_builds` excludes the running build ⇒ that mod's memory effects are
  refused; its content effects still serve.
- A recorded-bytes mismatch ⇒ the write is refused, not guessed; the refusal names both strings:
  `{what}: memory differs from recorded bytes (found …, expected …) — refusing`.
- A raw RVA that is not readable memory ⇒ refusal (typo/race protection).
- A merge that changes nothing reproduces the shipped table byte-identically; added rows are
  written canonically. A merge with an oversized host refuses rather than overrunning.
- If a host-pack cache build fails, the i.dat merge is also refused (atomicity), and the game is
  served the original pack.
- A refused inject or tailgate falls back to merge serving for the affected packs, per host; the
  original pack is served only if the merge also fails.
- The tailgate refuses to append when a pack decodes shorter than its declared size, rather than
  writing past what the decoder produced.

## Conflict/overlap handling

| Situation | Result |
|---|---|
| `requires` not supplied | mod skipped, logged with missing capability |
| `conflicts` pair | lower priority skipped, logged; `allow_conflicts = true` makes both load with a warning |
| Two memory effects same target key | later effect refused and logged at error level; the boot continues, earlier mod keeps it |
| Two [[pack]] entries same name in one mod | validation error |
| Two mods same pack entry name | conflict; earlier wins |
| Two mods same new asset name | higher priority/earlier wins; later logged and skipped (no hard error) |
| Two mods patching same [[csv]] row (rows mode) | earlier keeps the row, later refused (other rows still apply) |
| Two [[asset]] same name with vanilla collision | hard refusal unless `override_vanilla = true` |
| Ambiguous `host = "auto"` dir | refusal unless explicit `host` |

## Cache & journals

- Merged packs/CSVs: content-hashed cache under `<mods_dir>/cache/` (staleness fingerprint =
  mod ids/versions + entry-file mtime/size).
- Loose assets: `<mods_dir>/cache/loose/` (real files, path rewrite).
- Cache misses/regenerations are automatic; no manual invalidation needed.


---

← [Back to index](./README.md)
