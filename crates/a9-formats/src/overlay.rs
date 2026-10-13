//! The runtime overlay merge — what M3 serves through the `CFile::Open`
//! redirect: decode a family-B pack file from disk, apply the enabled mods'
//! entry operations, re-encode. Pure bytes in, bytes out — identical code
//! runs in the loader (in-game, at the game's own open time) and in `a9ml`
//! (host, authoring/diff).
//!
//! Three merge shapes share this module:
//! - family-B image entries ([`overlay_pack_b`]) and the 0x204 csv container
//!   ([`overlay_csvp`]) — entry-table edits by name;
//! - the family-A append ([`overlay_pack_a`], M5 W1) — new payloads
//!   concatenated onto a size-prefixed stream (`vp`/`tp`/`mcp`/`ep`/`cp`),
//!   whose computable offset ([`plan_family_a`]) the `i.dat` row writer reuses
//!   so a row's `off` and its bytes cannot drift apart.
//!
//! Semantics (PLAN.md §4.4): operations are applied in the given order
//! (the loader passes them in priority order); an op whose name matches an
//! existing entry REPLACES its payload/dimensions (the original name field
//! is kept verbatim); a name that does not exist APPENDS a new entry.
//! Two ops on the same name = the caller's conflict (the mod resolver
//! refuses those before they ever get here).
//!
//! The added-liveries pipeline (corpus notes/19 `inject_liveries.py`,
//! live-verified in the game) rebuilt every name field zero-padded and the
//! game accepted it — so replaced/added entries are serialized with clean
//! name fields, exactly like that proven pipeline.
//!
//! Budget: the synthesized container must stay within the engine's decode
//! workspace (200 MB out) — `budget` (the loader passes ~190 MB) is asserted
//! on the *decoded container size* before any work.

use crate::container::{self, CsvEntry, ImageEntry, NameField};
use crate::csv;
use crate::pack::{self, Family};
use crate::Error;

/// One [[pack.entry]] operation (family-B 0x200 containers: me*/mt* packs).
#[derive(Clone, Debug, PartialEq)]
pub struct ImageOp {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub payload: Vec<u8>,
}

/// One [[csv]] table operation (0x204 containers: csvp).
#[derive(Clone, Debug, PartialEq)]
pub struct CsvOp {
    pub name: String,
    pub kind: CsvOpKind,
    /// [`CsvOpKind::Replace`]: the whole table payload.
    /// [`CsvOpKind::Rows`]: the patch rows (a UTF-16 CSV keyed by first column).
    pub payload: Vec<u8>,
}

/// Whether a [`CsvOp`] replaces the whole table or row-patches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsvOpKind {
    /// the payload IS the table (M3 whole-table replace)
    Replace,
    /// the payload is a set of row patches merged into the table
    Rows,
}

/// One `[[asset]]` operation (M5 W1): a payload appended to a family-A host
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

/// The `.dir` group listings the loader **auto-merges** (M5-U): only those a
/// consumer PROVABLY matches by exact name, so listing order — and therefore
/// our append position — cannot matter.
///
/// `previewtexture.dir` is proven name-keyed: the preview annotator
/// (`preview_form_list_annotator`, memory #310) composes `form_<token>_*.dds`
/// and looks each up by exact name, so the entry order is not load-bearing.
/// The other shipped listings (`base.dir`, `tree.dir`, …) are consumed by
/// paths whose order dependence has not been established — those stay on the
/// manual `[[asset]] override_vanilla = true` escape hatch (a loader that
/// auto-merged into them would be guessing at a consumer contract).
pub const AUTO_MERGE_LISTINGS: &[&str] = &["previewtexture.dir"];

/// Whether a `.dir` listing row is on the auto-merge allow-list.
pub fn listing_auto_merge(name: &str) -> bool {
    AUTO_MERGE_LISTINGS.contains(&name)
}

/// Parse a `.dir` group listing: a flat run of NUL-terminated UTF-16LE file
/// names — no count prefix, no stride, no padding (the row's `len` is the only
/// bound, memory #311). Returns the names in order.
///
/// `Err` when the payload is not a well-formed name run: an odd byte length
/// (not UTF-16LE units), a missing NUL terminator, an empty segment (two
/// consecutive NULs / a leading NUL), or a unit sequence that is not valid
/// UTF-16. That is the "refuse loudly" case — the caller logs the message and
/// leaves the shipped listing byte-for-byte untouched.
pub fn listing_parse(bytes: &[u8]) -> Result<Vec<String>, Error> {
    if bytes.is_empty() {
        return Err(Error::Container("listing payload is empty".into()));
    }
    if bytes.len() % 2 != 0 {
        return Err(Error::Container(format!(
            "listing length {} is odd — not a UTF-16LE name run",
            bytes.len()
        )));
    }
    if bytes[bytes.len() - 2..] != [0, 0] {
        return Err(Error::Container(
            "listing does not end with a NUL unit (the loader's walk needs it)".into(),
        ));
    }
    let mut names: Vec<String> = Vec::new();
    let mut cur: Vec<u16> = Vec::new();
    for ch in bytes.chunks_exact(2) {
        let u = u16::from_le_bytes([ch[0], ch[1]]);
        if u == 0 {
            if cur.is_empty() {
                return Err(Error::Container(format!(
                    "listing has an empty name at entry {}",
                    names.len()
                )));
            }
            names.push(String::from_utf16(&cur).map_err(|_| {
                Error::Container(format!("listing entry {} is not valid UTF-16", names.len()))
            })?);
            cur.clear();
        } else {
            cur.push(u);
        }
    }
    Ok(names)
}

/// Rebuild a `.dir` listing: the vanilla BYTES verbatim, followed by each name
/// in `names` (NUL-terminated UTF-16LE) that is **not already present** in the
/// vanilla listing and not already appended by this call.
///
/// Returns `(payload, appended, total)` — `total` is the resulting entry count
/// for the log line. Keeping the vanilla bytes verbatim (never re-serializing
/// them) both guarantees "vanilla order first" and makes the no-op case
/// byte-identical: if every `name` is already listed, `payload == vanilla`.
pub fn listing_extend(
    vanilla: &[u8],
    names: &[String],
) -> Result<(Vec<u8>, usize, usize), Error> {
    let existing = listing_parse(vanilla)?;
    let mut present: std::collections::BTreeSet<&str> =
        existing.iter().map(String::as_str).collect();
    let mut payload = vanilla.to_vec();
    let mut appended = 0usize;
    for n in names {
        if n.is_empty() || n.contains('\0') {
            return Err(Error::Container(format!(
                "listing name {n:?} is empty or contains NUL — refusing the extension"
            )));
        }
        if !present.insert(n.as_str()) {
            continue; // already listed (vanilla) or already appended here
        }
        for u in n.encode_utf16() {
            payload.extend_from_slice(&u.to_le_bytes());
        }
        payload.extend_from_slice(&[0, 0]);
        appended += 1;
    }
    Ok((payload, appended, existing.len() + appended))
}

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
}

fn merge_report(added: usize, replaced: usize, decoded: usize, encoded: usize) -> MergeReport {
    MergeReport {
        added,
        replaced,
        decoded,
        encoded,
        row_replaced: 0,
        row_appended: 0,
    }
}

/// Merge image-entry ops into a family-B pack FILE (decode → apply →
/// re-encode). Returns the new pack file bytes.
pub fn overlay_pack_b(
    original_file: &[u8],
    ops: &[ImageOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    let decoded = pack::decode_pack(original_file, Family::B)?;
    overlay_images_decoded(&decoded, ops, budget)
}

/// Merge csv-table ops into the csvp FILE (family-B, 0x204 container).
pub fn overlay_csvp(
    original_file: &[u8],
    ops: &[CsvOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    let decoded = pack::decode_pack(original_file, Family::B)?;
    overlay_csvs_decoded(&decoded, ops, budget)
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
/// within `cap`. The M5 tailgate uses this against the 200 MB out-cap to
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

/// The image merge on already-decoded container bytes.
pub fn overlay_images_decoded(
    decoded: &[u8],
    ops: &[ImageOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    let mut entries = container::parse_images(decoded)?;
    let mut added = 0usize;
    let mut replaced = 0usize;
    for op in ops {
        let name = NameField::from_str(&op.name)?;
        // duplicate names *within one op list* are the caller's bug — refuse
        if ops.iter().filter(|o| o.name == op.name).count() > 1 {
            return Err(Error::Container(format!(
                "duplicate entry op {:?} in one merge",
                op.name
            )));
        }
        match entries
            .iter()
            .position(|e| e.name.as_str() == op.name)
        {
            Some(i) => {
                entries[i] = ImageEntry {
                    name,
                    width: op.width,
                    height: op.height,
                    payload: op.payload.clone(),
                };
                replaced += 1;
            }
            None => {
                entries.push(ImageEntry {
                    name,
                    width: op.width,
                    height: op.height,
                    payload: op.payload.clone(),
                });
                added += 1;
            }
        }
    }
    let out = container::serialize_images(&entries);
    if out.len() > budget {
        return Err(Error::Container(format!(
            "merged container is {} bytes — over the {}-byte budget (refusing; \
             balance the packs across mods)",
            out.len(),
            budget
        )));
    }
    let file = pack::encode_pack_b(&out);
    let report = merge_report(added, replaced, out.len(), file.len());
    Ok((file, report))
}

/// The csv merge on already-decoded container bytes.
///
/// Ops are grouped by table name (first-appearance order). A table with a
/// whole-table [`CsvOpKind::Replace`] op takes that payload as its base; any
/// [`CsvOpKind::Rows`] ops for the same table then apply on top in op order.
/// A row patch for a table the container does not hold is refused loudly.
pub fn overlay_csvs_decoded(
    decoded: &[u8],
    ops: &[CsvOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    let mut entries = container::parse_csvs(decoded)?;

    // group op indices by table name, preserving first-appearance order
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, op) in ops.iter().enumerate() {
        if !groups.contains_key(&op.name) {
            order.push(op.name.clone());
        }
        groups.entry(op.name.clone()).or_default().push(i);
    }

    let mut added = 0usize;
    let mut replaced = 0usize;
    let mut row_replaced = 0usize;
    let mut row_appended = 0usize;

    for name in &order {
        let idxs = &groups[name];
        let replaces: Vec<usize> = idxs
            .iter()
            .copied()
            .filter(|&i| ops[i].kind == CsvOpKind::Replace)
            .collect();
        if replaces.len() > 1 {
            return Err(Error::Container(format!(
                "csv table {name:?}: {} whole-table replaces in one merge",
                replaces.len()
            )));
        }
        let existing = entries.iter().position(|e| e.name.as_str() == *name);
        let mut base_bytes = match (existing, replaces.first()) {
            (_, Some(&ri)) => ops[ri].payload.clone(),
            (Some(p), None) => entries[p].payload.clone(),
            (None, None) => {
                return Err(Error::Container(format!(
                    "csv table {name:?}: row patch(es) but the table is not in the container"
                )))
            }
        };
        let row_ops: Vec<&CsvOp> = idxs
            .iter()
            .map(|&i| &ops[i])
            .filter(|o| o.kind == CsvOpKind::Rows)
            .collect();
        if !row_ops.is_empty() {
            let base = csv::Table::parse(&base_bytes)?;
            let patches: Vec<csv::Table> = row_ops
                .iter()
                .map(|o| csv::Table::parse(&o.payload))
                .collect::<Result<_, _>>()?;
            let (merged, stats) = csv::merge_rows(&base, &patches, csv::MergeMode::Rows)?;
            row_replaced += stats.replaced;
            row_appended += stats.appended;
            base_bytes = merged.serialize();
        }
        let name_field = NameField::from_str(name)?;
        match existing {
            Some(p) => {
                entries[p] = CsvEntry {
                    name: name_field,
                    payload: base_bytes,
                };
                replaced += 1;
            }
            None => {
                entries.push(CsvEntry {
                    name: name_field,
                    payload: base_bytes,
                });
                added += 1;
            }
        }
    }

    let out = container::serialize_csvs(&entries);
    if out.len() > budget {
        return Err(Error::Container(format!(
            "merged csvp container is {} bytes — over the {}-byte budget",
            out.len(),
            budget
        )));
    }
    let file = pack::encode_pack_b(&out);
    let mut report = merge_report(added, replaced, out.len(), file.len());
    report.row_replaced = row_replaced;
    report.row_appended = row_appended;
    Ok((file, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{parse_images, parse_csvs};

    fn op(name: &str, w: u32, h: u32, payload: &[u8]) -> ImageOp {
        ImageOp {
            name: name.into(),
            width: w,
            height: h,
            payload: payload.to_vec(),
        }
    }

    fn csv_op(name: &str, payload: &[u8]) -> CsvOp {
        CsvOp {
            name: name.into(),
            kind: CsvOpKind::Replace,
            payload: payload.to_vec(),
        }
    }

    fn csv_rows_op(name: &str, payload: &[u8]) -> CsvOp {
        CsvOp {
            name: name.into(),
            kind: CsvOpKind::Rows,
            payload: payload.to_vec(),
        }
    }

    fn utf16(s: &str) -> Vec<u8> {
        let mut out = vec![0xff, 0xfe];
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    #[test]
    fn images_add_replace_roundtrip() {
        // base: two entries
        let base_entries = vec![
            ImageEntry {
                name: NameField::from_str("a.dds").unwrap(),
                width: 10,
                height: 20,
                payload: vec![1, 2, 3, 4],
            },
            ImageEntry {
                name: NameField::from_str("b.dds").unwrap(),
                width: 30,
                height: 40,
                payload: vec![5, 6, 7, 8],
            },
        ];
        let decoded = container::serialize_images(&base_entries);
        let file = pack::encode_pack_b(&decoded);

        // add one + replace one
        let (merged, report) = overlay_pack_b(
            &file,
            &[op("c.dds", 50, 60, &[9, 9]), op("a.dds", 11, 22, &[7, 7, 7, 7])],
            usize::MAX,
        )
        .unwrap();
        assert_eq!(report.added, 1);
        assert_eq!(report.replaced, 1);

        let back = pack::decode_pack(&merged, Family::B).unwrap();
        let entries = parse_images(&back).unwrap();
        assert_eq!(entries.len(), 3);
        let a = entries.iter().find(|e| e.name.as_str() == "a.dds").unwrap();
        assert_eq!((a.width, a.height), (11, 22));
        assert_eq!(a.payload, vec![7, 7, 7, 7]);
        let b = entries.iter().find(|e| e.name.as_str() == "b.dds").unwrap();
        assert_eq!(b.payload, vec![5, 6, 7, 8]); // untouched
        let c = entries.iter().find(|e| e.name.as_str() == "c.dds").unwrap();
        assert_eq!((c.width, c.height), (50, 60));
        assert_eq!(c.payload, vec![9, 9]);
        assert_eq!(report.decoded, back.len());
    }

    #[test]
    fn images_duplicate_op_refused() {
        let decoded = container::serialize_images(&[]);
        let file = pack::encode_pack_b(&decoded);
        let err = overlay_pack_b(
            &file,
            &[op("x.dds", 1, 1, &[1]), op("x.dds", 2, 2, &[2])],
            usize::MAX,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate entry op"));
    }

    #[test]
    fn images_budget_refused() {
        let decoded = container::serialize_images(&[]);
        let file = pack::encode_pack_b(&decoded);
        // 0x200 name + 12 B header + payload, budget below that
        let err = overlay_pack_b(&file, &[op("big.dds", 1, 1, &[0u8; 512])], 256).unwrap_err();
        assert!(err.to_string().contains("over the 256-byte budget"));
    }

    #[test]
    fn csvs_add_replace_roundtrip() {
        let base = vec![
            CsvEntry {
                name: NameField::from_str("t1.csv").unwrap(),
                payload: "old1".as_bytes().to_vec(),
            },
        ];
        let decoded = container::serialize_csvs(&base);
        let file = pack::encode_pack_b(&decoded);
        let (merged, report) = overlay_csvp(
            &file,
            &[csv_op("t2.csv", b"new2".as_slice()), csv_op("t1.csv", b"replaced".as_slice())],
            usize::MAX,
        )
        .unwrap();
        assert_eq!((report.added, report.replaced), (1, 1));
        let back = parse_csvs(&pack::decode_pack(&merged, Family::B).unwrap()).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].payload, b"replaced".to_vec());
        assert_eq!(back[1].payload, b"new2".to_vec());
    }

    #[test]
    fn csvs_row_merge_on_top_of_a_container() {
        let base_tbl = utf16("\"KEY\",\"VAL\"\r\n\"R1\",\"a\"\r\n\"R2\",\"b\"\r\n\"R3\",\"c\"\r\n");
        let decoded = container::serialize_csvs(&[CsvEntry {
            name: NameField::from_str("t1.csv").unwrap(),
            payload: base_tbl.clone(),
        }]);
        let file = pack::encode_pack_b(&decoded);
        let (merged, report) = overlay_csvp(
            &file,
            &[
                csv_rows_op("t1.csv", &utf16("\"R2\",\"B2\"\r\n")),
                csv_rows_op("t1.csv", &utf16("\"R4\",\"d\"\r\n")),
            ],
            usize::MAX,
        )
        .unwrap();
        assert_eq!((report.added, report.replaced), (0, 1));
        assert_eq!((report.row_replaced, report.row_appended), (1, 1));
        let back = parse_csvs(&pack::decode_pack(&merged, Family::B).unwrap()).unwrap();
        let want = utf16("\"KEY\",\"VAL\"\r\n\"R1\",\"a\"\r\n\"R2\",\"B2\"\r\n\"R3\",\"c\"\r\n\"R4\",\"d\"\r\n");
        assert_eq!(back[0].payload, want);
    }

    #[test]
    fn csvs_row_patch_on_missing_table_is_refused() {
        let decoded = container::serialize_csvs(&[]);
        let file = pack::encode_pack_b(&decoded);
        let err = overlay_csvp(
            &file,
            &[csv_rows_op("nope.csv", &utf16("\"R1\",\"a\"\r\n"))],
            usize::MAX,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not in the container"), "{err}");
    }

    #[test]
    fn csvs_two_whole_table_replaces_refused() {
        let decoded = container::serialize_csvs(&[CsvEntry {
            name: NameField::from_str("t.csv").unwrap(),
            payload: b"x".to_vec(),
        }]);
        let file = pack::encode_pack_b(&decoded);
        let err = overlay_csvp(
            &file,
            &[csv_op("t.csv", b"a"), csv_op("t.csv", b"b")],
            usize::MAX,
        )
        .unwrap_err();
        assert!(err.to_string().contains("whole-table replaces"), "{err}");
    }

    #[test]
    fn family_a_layout_and_append_roundtrip() {
        let asset = |name: &str, p: &[u8]| AssetOp {
            name: name.into(),
            dir: "train\\w1probe".into(),
            payload: p.to_vec(),
        };
        // plan math: off = |V| + Σ|P_j|, len = |P_i|
        let ops = vec![asset("a.x", &[0xAA; 10]), asset("b.x", &[0xBB; 25])];
        assert_eq!(plan_family_a(100, &ops), vec![(100, 10), (110, 25)]);
        assert_eq!(plan_family_a_lens(100, &[10, 25]), vec![(100, 10), (110, 25)]);
        assert_eq!(plan_family_a_lens(0, &[]), Vec::<(usize, usize)>::new());
        assert_eq!(family_a_total(100, &ops), 135);
        assert_eq!(plan_family_a(0, &[]), Vec::<(usize, usize)>::new());
        // budget: total must fit the cap; the exact boundary fits
        assert!(family_a_fits(100, &ops, 135));
        assert!(!family_a_fits(100, &ops, 134));
        assert!(family_a_fits(0, &[], 0));

        // append + re-encode: the merged stream is exactly V ++ P0 ++ P1
        let v: Vec<u8> = (0..200u32).map(|i| (i * 7) as u8).collect();
        let file = pack::encode_pack_a(&v);
        let (merged, report) = overlay_pack_a(&file, &ops, usize::MAX).unwrap();
        assert_eq!(report.added, 2);
        assert_eq!(report.replaced, 0);
        assert_eq!(report.decoded, 235);
        let back = pack::decode_pack(&merged, Family::A).unwrap();
        assert_eq!(&back[..200], &v[..]);
        assert_eq!(&back[200..210], &[0xAA; 10]);
        assert_eq!(&back[210..235], &[0xBB; 25]);
        assert_eq!(report.decoded, back.len());
        assert_eq!(report.encoded, merged.len());
    }

    #[test]
    fn family_a_budget_and_duplicates_refused() {
        let v = vec![0u8; 50];
        let file = pack::encode_pack_a(&v);
        let asset = |name: &str, n: usize| AssetOp {
            name: name.into(),
            dir: "d".into(),
            payload: vec![1u8; n],
        };
        // decoded merged size 50 + 512 = 562 > 256
        let err = overlay_pack_a(&file, &[asset("big.x", 512)], 256).unwrap_err();
        assert!(err.to_string().contains("over the 256-byte budget"), "{err}");
        // duplicate op names in one merge
        let err = overlay_pack_a(&file, &[asset("x.x", 1), asset("x.x", 2)], usize::MAX)
            .unwrap_err();
        assert!(err.to_string().contains("duplicate asset op"), "{err}");
    }

    /// Encode names exactly as a `.dir` row carries them.
    fn listing_bytes(names: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in names {
            for u in n.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out.extend_from_slice(&[0, 0]);
        }
        out
    }

    #[test]
    fn listing_parse_roundtrips_and_bounds_by_the_payload() {
        let b = listing_bytes(&["a.dds", "form_kh32_f.dds", "form_kh32_s.dds"]);
        assert_eq!(
            listing_parse(&b).unwrap(),
            vec!["a.dds", "form_kh32_f.dds", "form_kh32_s.dds"]
        );
        // a name with no terminator cannot be read (the loader needs the NUL)
        let mut unt = b.clone();
        unt.truncate(unt.len() - 1);
        assert!(listing_parse(&unt).is_err());
    }

    #[test]
    fn listing_extend_appends_new_and_dedups() {
        let vanilla = listing_bytes(&["a.dds", "b.dds"]);
        let names = vec![
            "b.dds".to_string(), // already listed -> skipped
            "c.dds".to_string(),
            "c.dds".to_string(), // duplicate within the contributions -> skipped
            "d.dds".to_string(),
        ];
        let (payload, appended, total) = listing_extend(&vanilla, &names).unwrap();
        assert_eq!(appended, 2);
        assert_eq!(total, 4);
        // vanilla bytes stay verbatim as the prefix
        assert_eq!(&payload[..vanilla.len()], &vanilla[..]);
        assert_eq!(
            listing_parse(&payload).unwrap(),
            vec!["a.dds", "b.dds", "c.dds", "d.dds"]
        );
    }

    #[test]
    fn listing_extend_noop_is_byte_identical() {
        let vanilla = listing_bytes(&["a.dds", "b.dds"]);
        // nothing to add
        let (p, n, t) = listing_extend(&vanilla, &[]).unwrap();
        assert_eq!(p, vanilla);
        assert_eq!((n, t), (0, 2));
        // everything already present -> still byte-identical
        let names = vec!["a.dds".to_string(), "b.dds".to_string()];
        let (p, n, t) = listing_extend(&vanilla, &names).unwrap();
        assert_eq!(p, vanilla, "a no-op rebuild must not disturb a byte");
        assert_eq!((n, t), (0, 2));
    }

    #[test]
    fn listing_extend_refuses_a_non_utf16_payload_loudly() {
        // odd length
        assert!(listing_extend(&[0x61, 0x00, 0x00], &["x.dds".into()]).is_err());
        // no NUL terminator at the end
        assert!(listing_extend(&[0x61, 0x00], &["x.dds".into()]).is_err());
        // an empty segment (two consecutive NULs)
        assert!(listing_extend(&[0x00, 0x00], &["x.dds".into()]).is_err());
        // empty payload
        assert!(listing_extend(&[], &["x.dds".into()]).is_err());
        // a lone surrogate unit is invalid UTF-16
        assert!(listing_extend(&[0x00, 0xd8, 0x00, 0x00], &["x.dds".into()]).is_err());
        // an empty / NUL-bearing contribution is refused too
        assert!(listing_extend(&listing_bytes(&["a.dds"]), &["".into()]).is_err());
        assert!(listing_extend(&listing_bytes(&["a.dds"]), &["a\0b".into()]).is_err());
    }

    #[test]
    fn listing_auto_merge_allow_list_is_deliberately_narrow() {
        assert!(listing_auto_merge("previewtexture.dir"));
        // the positional/unknown-consumer listings stay on the manual path
        assert!(!listing_auto_merge("base.dir"));
        assert!(!listing_auto_merge("tree.dir"));
        assert!(!listing_auto_merge("previewtexture")); // the GROUP, not the row
        assert_eq!(AUTO_MERGE_LISTINGS, &["previewtexture.dir"]);
    }

    #[test]
    fn real_pack_when_available() {
        if let Ok(p) = std::env::var("A9_STEAM_MTENTR0") {
            let file = std::fs::read(&p).unwrap();
            let (merged, report) = overlay_pack_b(&file, &[op("_d_test_probe.dds", 1, 1, &[1, 2])], usize::MAX).unwrap();
            assert_eq!(report.added, 1);
            let entries = parse_images(&pack::decode_pack(&merged, Family::B).unwrap()).unwrap();
            assert!(entries.iter().any(|e| e.name.as_str() == "_d_test_probe.dds"));
            // budget sanity with the real pack: the same merge over a small budget refuses
            let base_len = pack::decode_pack(&file, Family::B).unwrap().len();
            assert!(overlay_pack_b(&file, &[op("_d_test_probe.dds", 1, 1, &[1, 2])], base_len).is_err());
        }
    }
}
