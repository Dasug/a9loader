# A-Train 9 mod loader — mod developer documentation

This documentation describes how to build a mod for the a9loader.

## Index

- [Getting started](./getting-started.md)
- [Mod manifest (`mod.toml`)](./mod-manifest.md)
- [Memory effects: `[[patch]]`, `[[hook]]`](./memory-effects.md)
- [Content effects: packs, CSV, assets, loose assets](./content-effects.md)
- [Loader configuration (`a9loader.toml`)](./loader-config.md)
- [Mod folder layout](./folder-layout.md)
- [Authoring toolchain (`a9ml`)](./tooling.md)
- [Diagnostics & failure modes](./diagnostics.md)
- [Unsupported today](./limits.md)

## Core rules in one screen

- A mod is a folder containing a `mod.toml`.
- Install = copy the folder into the `mods/` folder; remove = delete the folder.
- The game directory stays vanilla: all content is served in memory from packs/assets; nothing is copied into `data/` at boot.
- The loader only supports the Steam international build `5.00.5634`.
- Every effect is validated before execution; on a failure the loader logs loudly and the game
  boots vanilla for that effect rather than corrupting memory.