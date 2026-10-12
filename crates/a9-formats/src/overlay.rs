//! The runtime overlay merge — what M3 serves through the `CFile::Open`
//! redirect: decode a family-B pack file from disk, apply the enabled mods'
//! entry operations, re-encode. Pure bytes in, bytes out — identical code
//! runs in the loader (in-game, at the game's own open time) and in `a9ml`
//! (host, authoring/diff).
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

/// One [[csv]] table operation (0x204 containers: csvp) — whole-table
/// replace-or-add by name.
#[derive(Clone, Debug, PartialEq)]
pub struct CsvOp {
    pub name: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeReport {
    pub added: usize,
    pub replaced: usize,
    /// decoded container size of the synthesized pack
    pub decoded: usize,
    /// encoded (on-disk) size of the synthesized pack
    pub encoded: usize,
}

fn merge_report(added: usize, replaced: usize, decoded: usize, encoded: usize) -> MergeReport {
    MergeReport {
        added,
        replaced,
        decoded,
        encoded,
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
pub fn overlay_csvs_decoded(
    decoded: &[u8],
    ops: &[CsvOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    let mut entries = container::parse_csvs(decoded)?;
    let mut added = 0usize;
    let mut replaced = 0usize;
    for op in ops {
        let name = NameField::from_str(&op.name)?;
        if ops.iter().filter(|o| o.name == op.name).count() > 1 {
            return Err(Error::Container(format!(
                "duplicate table op {:?} in one merge",
                op.name
            )));
        }
        match entries.iter().position(|e| e.name.as_str() == op.name) {
            Some(i) => {
                entries[i] = CsvEntry {
                    name,
                    payload: op.payload.clone(),
                };
                replaced += 1;
            }
            None => {
                entries.push(CsvEntry {
                    name,
                    payload: op.payload.clone(),
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
    let report = merge_report(added, replaced, out.len(), file.len());
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
            payload: payload.to_vec(),
        }
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
