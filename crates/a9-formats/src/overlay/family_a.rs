//! The family-A append — new payloads concatenated onto a
//! size-prefixed stream (`vp`/`tp`/`mcp`/`ep`/`cp`), plus the pure layout
//! helpers the `i.dat` row writer shares with the merge.

use super::merge_report;
use super::MergeReport;
use crate::pack::{self, Family};
use crate::Error;

/// One `[[asset]]` operation: a payload appended to a family-A host
/// pack's decoded stream (`vp`/`tp`/`mcp`/`ep`/`cp`), plus the `i.dat` row
/// identity it will carry (`name` = the bare master-map key; `dir` = the
/// consumer grouping). The payload bytes are the exact pack chunk, so the
/// row's `len` is `payload.len()` (for a `.x` that includes the 12-byte
/// `ADX0` header — the mod ships the whole chunk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetOp {
    pub name: String,
    pub dir: String,
    pub payload: Vec<u8>,
}

/// The pure length-only form of [`plan_family_a`] — the loader plans the
/// `i.dat` rows from payload lengths without materializing the payloads a
/// second time. The single source of truth for the family-A append layout.
pub fn plan_family_a_lens(vanilla_decoded_len: usize, lens: &[usize]) -> Vec<(usize, usize)> {
    let mut off = vanilla_decoded_len;
    let mut out = Vec::with_capacity(lens.len());
    for &len in lens {
        out.push((off, len));
        off += len;
    }
    out
}

/// The family-A append layout: a family-A stream is a bare concatenation of
/// records (no per-entry header, no alignment), so appending payloads makes
/// `merged = V ++ P_0 .. P_n`. Given the vanilla decoded length `|V|` this
/// returns `(off, len)` per op in op order:
/// `off_i = |V| + Σ_{j<i} |P_j|`, `len_i = |P_i|`.
///
/// Pure and deterministic. The host-pack merge and the `i.dat` row writer
/// **must** both call this (with the same ordered op list), or a row's offset
/// would disagree with where its bytes landed.
pub fn plan_family_a(vanilla_decoded_len: usize, ops: &[AssetOp]) -> Vec<(usize, usize)> {
    let lens: Vec<usize> = ops.iter().map(|o| o.payload.len()).collect();
    plan_family_a_lens(vanilla_decoded_len, &lens)
}

/// The decoded size of the merged family-A image (`|V| + Σ|P_j|`) — the value
/// [`overlay_pack_a`] declares as the pack's output size.
pub fn family_a_total(vanilla_decoded_len: usize, ops: &[AssetOp]) -> usize {
    vanilla_decoded_len + ops.iter().map(|o| o.payload.len()).sum::<usize>()
}

/// Whether a family-A append fits the engine's decode workspace: the appended
/// bytes land at `|V|`, so the whole merged image (`family_a_total`) must stay
/// within `cap`. The tailgate uses this against the 200 MB out-cap to
/// decide, per host, between tailgate and merge; [`overlay_pack_a`] asserts
/// the same quantity against its own budget.
pub fn family_a_fits(vanilla_decoded_len: usize, ops: &[AssetOp], cap: usize) -> bool {
    family_a_total(vanilla_decoded_len, ops) <= cap
}

/// Merge asset ops into a family-A pack FILE (decode → append → re-encode).
/// Returns the new pack file bytes.
pub fn overlay_pack_a(
    original_file: &[u8],
    ops: &[AssetOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    let decoded = pack::decode_pack(original_file, Family::A)?;
    overlay_assets_decoded(&decoded, ops, budget)
}

/// The family-A append on already-decoded stream bytes: `V ++ P_0 .. P_n`,
/// re-encoded as `[u32 |merged|][LZSS(merged)]` + cipher. The declared output
/// size is updated by [`pack::encode_pack_a`] automatically.
///
/// The budget is asserted on the **decoded** merged size (the engine decode
/// workspace is 200 MB out), exactly like the family-B merges. For family-A
/// adds there is nothing to replace, so [`MergeReport::added`] is the payload
/// count and [`MergeReport::replaced`] is always 0.
pub fn overlay_assets_decoded(
    decoded: &[u8],
    ops: &[AssetOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    for (i, op) in ops.iter().enumerate() {
        // duplicate names *within one op list* are the caller's bug — refuse
        if ops[..i].iter().any(|o| o.name == op.name) {
            return Err(Error::Container(format!(
                "duplicate asset op {:?} in one merge",
                op.name
            )));
        }
    }
    let total = family_a_total(decoded.len(), ops);
    if total > budget {
        return Err(Error::Container(format!(
            "merged family-A image is {total} bytes — over the {budget}-byte budget \
             (refusing; balance the host packs across mods)"
        )));
    }
    let mut merged = Vec::with_capacity(total);
    merged.extend_from_slice(decoded);
    for op in ops {
        merged.extend_from_slice(&op.payload);
    }
    let file = pack::encode_pack_a(&merged);
    let report = merge_report(ops.len(), 0, merged.len(), file.len());
    Ok((file, report))
}
