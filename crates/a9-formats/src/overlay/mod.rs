//! The runtime overlay merge — what the `CFile::Open` redirect serves from
//! redirect: decode a family-B pack file from disk, apply the enabled mods'
//! entry operations, re-encode. Pure bytes in, bytes out — identical code
//! runs in the loader (in-game, at the game's own open time) and in `a9ml`
//! (host, authoring/diff).
//!
//! Three merge shapes share this module:
//! - family-B image entries ([`overlay_pack_b`]) and the 0x204 csv container
//!   ([`overlay_csvp`]) — entry-table edits by name;
//! - the family-A append ([`overlay_pack_a`]) — new payloads
//!   concatenated onto a size-prefixed stream (`vp`/`tp`/`mcp`/`ep`/`cp`),
//!   whose computable offset ([`plan_family_a`]) the `i.dat` row writer reuses
//!   so a row's `off` and its bytes cannot drift apart.
//!
//! Semantics: operations are applied in the given order
//! (the loader passes them in priority order); an op whose name matches an
//! existing entry REPLACES its payload/dimensions (the original name field
//! is kept verbatim); a name that does not exist APPENDS a new entry.
//! Two ops on the same name = the caller's conflict (the mod resolver
//! refuses those before they ever get here).
//!
//! The added-liveries pipeline rebuilt every name field zero-padded and the
//! game accepted it — so replaced/added entries are serialized with clean
//! name fields, exactly like that proven pipeline.
//!
//! Budget: the synthesized container must stay within the engine's decode
//! workspace (200 MB out) — `budget` (the loader passes ~190 MB) is asserted
//! on the *decoded container size* before any work.
//!
//! The three shapes live in [`images`], [`csvs`] and [`family_a`]; the `.dir`
//! listing helpers in [`listings`].

mod csvs;
mod family_a;
mod images;
mod listings;

pub use csvs::{overlay_csvp, overlay_csvs_decoded, CsvOp, CsvOpKind};
pub use family_a::{
    family_a_fits, family_a_total, overlay_assets_decoded, overlay_pack_a, plan_family_a,
    plan_family_a_lens, AssetOp,
};
pub use images::{overlay_images_decoded, overlay_pack_b, ImageOp};
pub use listings::{listing_auto_merge, listing_extend, listing_parse, AUTO_MERGE_LISTINGS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeReport {
    pub added: usize,
    pub replaced: usize,
    /// decoded container size of the synthesized pack
    pub decoded: usize,
    /// encoded (on-disk) size of the synthesized pack
    pub encoded: usize,
    /// csvp rows replaced / appended across all tables
    pub row_replaced: usize,
    pub row_appended: usize,
    /// csvp rows added by a positional GROUP append (train_pattern groups)
    pub row_group_appended: usize,
}

fn merge_report(added: usize, replaced: usize, decoded: usize, encoded: usize) -> MergeReport {
    MergeReport {
        added,
        replaced,
        decoded,
        encoded,
        row_replaced: 0,
        row_appended: 0,
        row_group_appended: 0,
    }
}

#[cfg(test)]
mod tests;
