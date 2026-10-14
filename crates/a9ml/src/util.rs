//! Small stateless helpers shared across the generators.
//!
//! These two were verbatim-identical copies before the restructure:
//! [`civil_stamp`] lived in both `gen_builds.rs` and `gen_dirindex.rs`, and
//! [`ascii_like`] in both `asset.rs` and `gen_dirindex.rs`. One copy each now.

/// A `YYYY-MM-DD HH:MM:SS` UTC stamp. A hand-rolled proleptic-Gregorian
/// conversion so the tool stays dependency-free (no chrono).
pub(crate) fn civil_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Is `b` mostly printable ASCII/whitespace (tabs..CR, space..tilde)? The
/// weak-kind fallback when a payload carries no strong magic.
pub(crate) fn ascii_like(b: &[u8]) -> bool {
    let good = b
        .iter()
        .filter(|&&c| (9..=13).contains(&c) || (32..=126).contains(&c))
        .count();
    good * 10 >= b.len().max(1) * 9
}
