//! The `.dir` group listings the loader **auto-merges** — parse,
//! extend and the narrow allow-list of listings a consumer provably matches by
//! exact name.

use crate::Error;

/// The `.dir` group listings the loader **auto-merges**: only those a
/// consumer PROVABLY matches by exact name, so listing order — and therefore
/// our append position — cannot matter.
///
/// `previewtexture.dir` is proven name-keyed: the preview annotator
/// (the shipped preview annotator) composes `form_<token>_*.dds`
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
/// bound; the run is the row's exact declared size). Returns the names in order.
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
