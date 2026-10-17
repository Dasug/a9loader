# Wine harnesses

Fake-game harnesses driving `harness/host.c`, a fake game host that maps fake
pages at the real anchor RVAs with the real original bytes. They need **Wine
only** — not the game, not an install. Each script assembles per-scenario run
dirs under `harness/run/<name>/`, loads the built `version.dll` there, and
asserts both the end effects and the loader log. A script is green only when
it ends with `…: ALL SCENARIOS GREEN`.

Run one: `bash harness/m5.sh`. Run everything: `bash harness/run-all.sh`
(every script in name order, per-harness verdicts, one summary line).

Prerequisites: `wine`, the mingw-w64 cross toolchain (`x86_64-w64-mingw32-gcc`,
`x86_64-w64-mingw32-windres`), the Rust target (`rustup target add
x86_64-pc-windows-gnu` — every script runs
`cargo build --release --target x86_64-pc-windows-gnu` first and copies
`target/x86_64-pc-windows-gnu/release/version.dll` into the scenario dir), and
the live `ATrain9v5.exe` for the build-gate scenarios — default
`/run/media/frybarec/dasug/Games/steamlibrary/steamapps/common/A-Train9v5/
ATrain9v5.exe`, overridable via the first argument or `ATRAIN9_EXE=`.

`harness/lib.sh` holds the shared helpers (`new_scen`, `run_host`, `check`,
`checkf`, the fail counters); `host.rc` / `host_wrong.rc` are the two
VERSIONINFO resources (right build id / wrong build id) linked into
`host.exe`, and each scenario's assembled dir lives in `harness/run/<name>`.

## The harnesses and the host modes they drive

| script | what it proves | host modes |
|---|---|---|
| `m2.sh` | the full loader engine under wine: config → build gate (right/wrong VERSIONINFO) → mods scan → resolve → expect-verified `[[patch]]`/`[[hook]]` execution, `train_list` filter modes (allow/deny/add), nop; asserts the log journal per effect | `kept`, `none`, `nop`, `table113` (the `run_host`-default dispatch) |
| `m3.sh` | the runtime overlay: CFile::Open trampoline + expect bytes, redirect to the loader-managed pack cache, byte-exact content vs `a9ml mkpack`, passthrough for unmatched paths; family-A and CSV overlays | `overlaypack`, `overlaycsv` |
| `m4inj.sh` | `pack_serving = "inject"`: the famb pool-runner call site spliced for real, the in-memory row append over the vanilla slice, the missing-payload refusal (fail loud + vanilla serve), register/byte discipline | `inject` |
| `m4probe.sh` | the `probe_fetch`/`probe_save`/`probe_window` diagnostics: the four fetch anchors + the probe log lines with them on (and the absence with them off), plus the purchase-window type-cycle fix (`tabfix`), the custom-form second-scan fix (`formfix`), the window dump (`window`) and the clamp guard probe pages | `probe`, `tabfix`, `formfix`, `window`, `save`, `clamp` |
| `m5.sh` | `[[asset]]` in-place serving into a family-A pack's decoded stream: single-file and folder (`host = "auto"`, dir-index prefix resolution `train\<name>`) + the refusals (no shipped row, ambiguity across packs); i.dat/vp row model byte-exact | `overlaydat`, `overlaytp` |
| `m6.sh` | `[[loose_asset]]`: game files served by open-GLOB path match (absolute/relative/case-insensitive), non-matching paths untouched, unreadable payload fails open, two mods on one pattern | `loose` |
| `m7rows.sh` | direct spec-row injection: resize hook grows 421 → 422 with the game's own accessor seeing the row, the name-ctor / template-name refusals, the wrong-order hazard (pre-resize growth undone) | `rows [-off -rows -refuse -refuse-tmpl -order]` |
| `m7train.sh` | the `[[train]]` compiler end-to-end: real boot scan (train + csvs + model dir), the 4 derived csvp rows merged byte-exactly, the injected spec row, keep-set with reserved ids, and the refusals (bad model dir, id range exhausted, FORM name collision) | `train`, `train-nodump`, `train_retry`, `train_carbuild`, `train_badhelper` |

## Conventions

- **One workstream, one script**: name it `m<N>[suffix].sh` with a header
  block in the same shape (prove-what + usage + scenarios), one host mode per
  scenario family, and one table row above.
- The **host mode list is authoritative in `main`'s dispatch tail** of
  `host.c`; its head comment carries the running table of contents — keep
  both in step with this README when a milestone grows one of the three.
- Green = the script's last line is `<NAME>: ALL SCENARIOS GREEN`;
  `run-all.sh` runs every script in order, keeps each log in
  `harness/run/all.log/`, prints the per-harness verdicts, and exits
  non-zero when any script is not green.
