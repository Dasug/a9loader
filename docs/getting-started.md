# Getting started

## Install the loader

1. Build (or obtain) `version.dll` and place it next to `ATrain9v5.exe`.
2. (If you're on Linux and run the game via Wine/Proton, launch the game with `WINEDLLOVERRIDES="version=n,b" %command%` in Steam launch options).
3. Create a `mods/` folder next to the exe if it does not exist.
4. Put each mod in its own subfolder: `mods/<mod-id>/mod.toml` plus its payload files.
5. The loader writes `mods/a9loader.log` at boot; use it to verify behavior.

There is no install command and no package format: a mod is a folder.

## Minimum mod

`mods/hello/mod.toml`:

```toml
id = "hello"
name = "Hello mod"
version = "1.0.0"
author = "you"
```

This mod does nothing but is valid and scanned. Add effects as sections below.

## Version compatibility

The loader supports only the Steam international build:

- canonical id `5.00.5634`

Mods that touch memory (`[[patch]]`, `[[hook]]`) should declare the build they were written
for:

```toml
game_builds = ["5.00.5634"]
```

A non-empty `game_builds` must contain the running build id or the mod's memory effects are
refused; an empty list applies them on every build.

Pure data mods (`[[pack]]`, `[[csv]]`, `[[asset]]`, `[[loose_asset]]`) may omit `game_builds`;
they are served in memory at the game's own file-open time and are not build-gated.

## Order of execution at boot

config read → scan + build-id gate → resolve (order/conflicts/overlaps) →
memory stage (`[[patch]]`/`[[hook]]`) → data stage (overlay plan + tailgate; merges happen
later, at the game's own file-open time) → runtime stage.

Memory effects are gated on `game_builds` when the mod declares one; the data and runtime
stages are not build-gated.


---

← [Back to index](./README.md)
