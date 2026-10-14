//! Family-B image-entry merges (`me*`/`mt*` packs, the 0x200 container).

use super::merge_report;
use super::MergeReport;
use crate::container::{self, ImageEntry, NameField};
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
