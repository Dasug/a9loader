//! **Direct spec-row injection** (the mechanism). The `[[train]]` path
//! (`train.rs`): N rows, each with its own allocated name + donor, hooked at
//! `train_list_builder` (after the pipeline's fill loop) instead of
//! `spec_table_resize`, with the name-pointer clone fix. The simpler
//! resize-hook path (`spec.rs`) stays the harness seam (`a9loader_m7_rows`)
//! and is unchanged.
//!
//! The goal: add trains BEYOND the 417-name FORM catalog without
//! touching the FORM literal or the boot-time matcher. Post-boot everything
//! addresses spec rows by **index** (`base = [0x140e14528]`, `row = base +
//! id*0x140`), so an injected row's name never has to be in the FORM list.
//! This module is that injection, proven in the wine harness:
//!
//! 1. **grow the vector AFTER `spec_table_resize`** 0x14019f150. That function
//!    re-pins the vector to EXACTLY 421 (`mov ebx,0x1a5`), so a growth done
//!    earlier is silently undone. `spec::ensure` hooks it trampoline-only and
//!    injects after the original returns; `spec::inject_now` is the bare
//!    primitive the harness also drives directly (to show the wrong order
//!    being undone).
//! 2. the growth itself is `spec_table_realloc` 0x14019fc50 `(&vec,
//!    new_total_rows)` — the engine's OWN reallocate, which allocates the new
//!    block with the game's allocator (0x14040a838), copies the live rows,
//!    destroys + frees the old block and sets `{begin,end,cap}`. Using the
//!    engine's realloc is the point: the block is owned by the same allocator
//!    family the vector destructor frees with, so the teardown stays clean (the
//!    allocator lesson).
//! 3. an injected row is a **CLONE of a shipped template row** (`memcpy` of the
//!    whole 0x140 row) with the fields that matter patched: the JP/EN display
//!    lanes (`row+0xa8`/`row+0xb0` = `awName[0]`/`awName[1]`; the lane is chosen
//!    by `manager+0x90`, 0 = JP / 8 = EN) and `pFormName` (`row+0x138`). The new
//!    strings come from the engine's OWN formatted-name constructor
//!    `row_name_ctor` 0x1400372f0 — the same one `train_list_builder`'s tail
//!    uses to auto-name rows 417-420 (`&row->pFormName, L"form_id_%d", id`), so
//!    they are valid A9_RcStr blocks (refcount header at `data-0x18`), not
//!    zero-filled memory.
//! 4. every reserved row is **NAMED with a valid, unique `pFormName`**. Hard
//!    requirement: `save_train_group_restore` 0x1401b6d10's blank path clones
//!    the name via `wstr_clone_data(*(void**)(row+0x138) - 0x18)`, so a
//!    zero-filled reserved row dereferences a wild pointer. The mechanism
//!    refuses (fail-open, loud) when the template's `pFormName` is NULL or when
//!    the name constructor returns NULL for any of the three name fields.
//! 5. row stride is 0x140 and ids are **u16 in the save**, so
//!    the reserved range must stay ≤ 65535. It is also kept under
//!    [`crate::forms::FORMS_MAX`] = 1024 — the composed keep-set bitmap's width —
//!    so the loader can still admit its own trains; a reserved range above that
//!    would need the bitmap widened, and the mechanism refuses rather
//!    than inject rows it could never show.
//!
//! Every row address is resolved through the engine's own
//! `spec_table_row_accessor` 0x1401aacb0 (`*(vec) + id*0x140`, no bounds
//! check) so the id→row contract is the engine's, not a re-implementation.
//!
//! # Honest limits
//!
//! * The direct-injection clone shares the template's *other* name pointers (`awName[2..8]`,
//!   `awNameAlt[0..8]`) without a refcount increment, so the LIVE teardown could
//!   the template's same pointers (double-free risk), the `[[train]]` path fixes this with `wstr_clone_ref`
//!   0x14001d7d0 (`train.rs`); the plain seam is left as-is (harness-only).
//! * `spec::ensure` / `spec::inject_now` are driven from the harness through the
//!   [`a9loader_m7_rows`] export; the `[[train]]` path IS wired into
//!   `engine::boot` (`install_trains`).
//!
//! Module map:
//!
//! * [`spec`] — the resolved state, the reserve primitive, the resize
//!   hook and the `a9loader_m7_rows` harness export + the shared row-layout
//!   facts both paths read.
//! * [`train`] — `TrainRow` / `install_trains` (what `engine::boot`
//!   drives) + the `train_list_builder` hook + the `a9loader_m7_dump` export.
//! * [`car`] — the car build: the game's own model loader driven per declared
//!   car, and the CSV-column → row-field mapping.
//! * [`diag`] — opt-in diagnostics: the panic-hook replacement, the consist
//!   copier/creator probes, the per-call spec-row dump.

pub use train::{TrainRow, install_trains};

mod car;
mod diag;
mod spec;
mod train;
