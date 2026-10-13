//! The exe's FORM list and the proxy's allow-list files — pure parsing, so it
//! is unit-testable on any host. Behavioral port of `load_allowed()` in the C
//! reference:
//! - FORM list: UTF-16, comma-separated, NUL-terminated, at RVA `0x6FC430`
//!   (file `0x6FB830`); entries are trimmed of spaces/tabs; entry index =
//!   form id.
//! - allow-list txt: one FORM name per line, `#` comments, trimmed; matching
//!   is exact and case-sensitive (like the C).
//!
//! M5 W3 adds [`compose_filter`]: several mods may each declare a `train_list`
//! contribution and the loader composes them into ONE keep-bitmap instead of
//! letting a single mod own the filter.

use a9_mod::manifest::ListMode;

/// Parse the raw UTF-16 FORM list (may include the trailing NUL; parsing stops
/// at the first NUL). Entry order = form-id order.
pub fn parse_form_list(units: &[u16]) -> Vec<String> {
    let mut names = Vec::new();
    let mut start = 0usize;
    for (i, &u) in units.iter().enumerate() {
        if u == 0 {
            push_entry(&mut names, &units[start..i]);
            return names;
        }
        if u == u16::from(b',') {
            push_entry(&mut names, &units[start..i]);
            start = i + 1;
        }
    }
    push_entry(&mut names, &units[start..]);
    names
}

fn push_entry(names: &mut Vec<String>, raw: &[u16]) {
    let mut a = 0usize;
    let mut b = raw.len();
    while a < b && matches!(raw[a], 0x20 | 0x09) {
        a += 1;
    }
    while b > a && matches!(raw[b - 1], 0x20 | 0x09) {
        b -= 1;
    }
    names.push(String::from_utf16_lossy(&raw[a..b]));
}

/// Parse an allow-list txt: one FORM name per line, `#` comments, trimmed
/// (spaces/tabs, like the C; `\r` handled because we split on `\n`).
pub fn parse_allow_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim_matches(|c| c == ' ' || c == '\t' || c == '\r'))
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Max form id the C bitmap can hold (`FORMS_MAX` in the reference).
pub const FORMS_MAX: usize = 1024;

pub struct BitmapReport {
    pub bitmap: Box<[u8; FORMS_MAX / 8]>,
    /// allow-list lines that matched a FORM entry
    pub matched: usize,
    /// allow-list lines that matched nothing (silently ignored by the C too)
    pub unmatched: usize,
}

/// Build the allowed-id bitmap: every allow-list name is looked up in the
/// FORM list (exact, case-sensitive). Unmatched names are counted but
/// otherwise ignored, exactly like the C.
pub fn build_bitmap(forms: &[String], allowed: &[String]) -> BitmapReport {
    let mut bitmap = Box::new([0u8; FORMS_MAX / 8]);
    let (mut matched, mut unmatched) = (0usize, 0usize);
    for name in allowed {
        match forms.iter().position(|f| f == name) {
            Some(idx) if idx < FORMS_MAX => {
                bitmap[idx >> 3] |= 1u8 << (idx & 7);
                matched += 1;
            }
            _ => unmatched += 1,
        }
    }
    BitmapReport {
        bitmap,
        matched,
        unmatched,
    }
}

/// The keep-bit for form id `id` in a filter bitmap (out-of-range ids are
/// always "not kept", matching `my_filter`).
#[inline]
pub fn kept(bitmap: &[u8; FORMS_MAX / 8], id: usize) -> bool {
    id < FORMS_MAX && bitmap[id >> 3] & (1u8 << (id & 7)) != 0
}

/// A bitmap keeping every id in `0..forms_len` (capped at [`FORMS_MAX`]).
pub fn all_forms(forms_len: usize) -> Box<[u8; FORMS_MAX / 8]> {
    let mut bm = Box::new([0u8; FORMS_MAX / 8]);
    for i in 0..forms_len.min(FORMS_MAX) {
        bm[i >> 3] |= 1u8 << (i & 7);
    }
    bm
}

/// Compose every mod's `train_list` contribution into the ONE keep-bitmap the
/// filter serves (M5 W3 — additive composition across mods).
///
/// The **base** is the layer below the contributions:
/// - if any `Allow` contribution exists, the base is the **union of the allow
///   lists** — an allow mod declares which forms are offered; with a single
///   allow mod this is byte-identical to the pre-W3 filter (the shipped
///   `trainnames` mod);
/// - else if any `Add` contribution exists, the base is the **shipped purchase
///   whitelist** (`vanilla`) — a new-train mod says "offer my forms **in
///   addition to** what the game ships";
/// - else (only `Deny`) the base is **all** valid forms (the pre-W3 deny
///   semantics).
///
/// Every `Allow`/`Add` list is then unioned into the base and every `Deny`
/// list is subtracted last, so a `Deny` always wins over an `Allow`/`Add` of
/// the same form. Order-independent on purpose: a priority tie between two
/// mods must not change which trains are offered.
pub fn compose_filter(
    forms_len: usize,
    vanilla: &[u8; FORMS_MAX / 8],
    contribs: &[(ListMode, &[u8; FORMS_MAX / 8])],
) -> Box<[u8; FORMS_MAX / 8]> {
    let has_allow = contribs.iter().any(|(m, _)| *m == ListMode::Allow);
    let has_add = contribs.iter().any(|(m, _)| *m == ListMode::Add);
    let mut keep = if has_allow {
        Box::new([0u8; FORMS_MAX / 8])
    } else if has_add {
        Box::new(*vanilla)
    } else {
        all_forms(forms_len)
    };
    for (mode, bm) in contribs {
        if matches!(mode, ListMode::Allow | ListMode::Add) {
            for i in 0..keep.len() {
                keep[i] |= bm[i];
            }
        }
    }
    for (mode, bm) in contribs {
        if *mode == ListMode::Deny {
            for i in 0..keep.len() {
                keep[i] &= !bm[i];
            }
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn parses_comma_separated_nul_terminated() {
        let mut u = units("FORM_A, FORM_B ,FORM_C");
        u.push(0);
        assert_eq!(
            parse_form_list(&u),
            vec!["FORM_A", "FORM_B", "FORM_C"]
        );
    }

    #[test]
    fn parses_without_nul_and_trims_tabs_and_spaces() {
        assert_eq!(
            parse_form_list(&units("  X1  ,\tY2\t,, Z3")),
            vec!["X1", "Y2", "", "Z3"]
        );
    }

    #[test]
    fn allow_list_comments_and_blanks() {
        let txt = "# header\nFORM_A\n\n  FORM_B  \n\t# indented comment\nFORM_C\r\n";
        assert_eq!(parse_allow_list(txt), vec!["FORM_A", "FORM_B", "FORM_C"]);
    }

    #[test]
    fn bitmap_exact_and_case_sensitive() {
        let forms = vec!["FORM_A".to_string(), "FORM_B".to_string()];
        let allowed = vec![
            "FORM_B".to_string(),
            "form_a".to_string(), // case mismatch → unmatched (C parity)
            "NOPE".to_string(),  // unknown → unmatched
            "FORM_A".to_string(), // duplicate line → matched again (C parity)
        ];
        let rep = build_bitmap(&forms, &allowed);
        assert_eq!(rep.matched, 2);
        assert_eq!(rep.unmatched, 2);
        // ids 0 and 1 set
        assert_eq!(rep.bitmap[0], 0b0000_0011);
    }

    #[test]
    fn bitmap_out_of_range_form_ids_ignored() {
        let forms: Vec<String> = (0..1030).map(|i| format!("F{i}")).collect();
        let allowed = vec!["F1024".to_string(), "F1029".to_string(), "F0".to_string()];
        let rep = build_bitmap(&forms, &allowed);
        assert_eq!(rep.matched, 1);
        assert_eq!(rep.unmatched, 2);
    }

    /// A synthetic bitmap with the listed ids set.
    fn bm(ids: &[usize]) -> Box<[u8; FORMS_MAX / 8]> {
        let mut b = Box::new([0u8; FORMS_MAX / 8]);
        for &i in ids {
            b[i >> 3] |= 1u8 << (i & 7);
        }
        b
    }

    #[test]
    fn compose_single_allow_is_exactly_its_list() {
        // pre-W3 parity: a lone allow mod keeps ONLY its forms
        let allow = bm(&[3, 7]);
        let got = compose_filter(10, &bm(&[0, 1, 2]), &[(ListMode::Allow, &*allow)]);
        assert_eq!(got, allow);
    }

    #[test]
    fn compose_two_allows_union() {
        let a = bm(&[3]);
        let b = bm(&[5]);
        let got = compose_filter(10, &bm(&[]), &[(ListMode::Allow, &*a), (ListMode::Allow, &*b)]);
        assert!(kept(&got, 3) && kept(&got, 5));
        assert!(!kept(&got, 4), "unlisted forms stay out");
    }

    #[test]
    fn compose_add_seeds_from_the_shipped_whitelist() {
        // a lone `add` mod offers the shipped whitelist + its form
        let vanilla = bm(&[0, 1, 2]);
        let add = bm(&[9]);
        let got = compose_filter(10, &vanilla, &[(ListMode::Add, &*add)]);
        for i in [0, 1, 2, 9] {
            assert!(kept(&got, i), "id {i} must be kept");
        }
        assert!(!kept(&got, 5));
        // and it composes with an allow mod: the allow list is the base and
        // the add is unioned in — the vanilla whitelist is NOT forced back in,
        // because an allow mod owns the offered set
        let allow = bm(&[4]);
        let got = compose_filter(10, &vanilla, &[(ListMode::Allow, &*allow), (ListMode::Add, &*add)]);
        for i in [4, 9] {
            assert!(kept(&got, i), "id {i} must be kept");
        }
        for i in [0, 1, 2] {
            assert!(!kept(&got, i), "the allow layer supersedes the vanilla base");
        }
    }

    #[test]
    fn compose_deny_only_keeps_everything_else() {
        // pre-W3 parity: a lone deny mod keeps all forms but the listed ones
        let deny = bm(&[2]);
        let got = compose_filter(5, &bm(&[0, 1]), &[(ListMode::Deny, &*deny)]);
        for i in [0, 1, 3, 4] {
            assert!(kept(&got, i));
        }
        assert!(!kept(&got, 2));
        // out-of-range ids are never kept
        assert!(!kept(&got, FORMS_MAX));
    }

    #[test]
    fn compose_deny_beats_allow_and_add() {
        let allow = bm(&[3, 4]);
        let add = bm(&[9]);
        let deny = bm(&[4, 9]);
        let got = compose_filter(10, &bm(&[0, 1]), &[
            (ListMode::Allow, &*allow),
            (ListMode::Add, &*add),
            (ListMode::Deny, &*deny),
        ]);
        assert!(kept(&got, 3));
        assert!(!kept(&got, 4), "deny removes an allow");
        assert!(!kept(&got, 9), "deny removes an add");
    }

    #[test]
    fn compose_is_order_independent() {
        let a = bm(&[1, 2]);
        let b = bm(&[5]);
        let d = bm(&[2]);
        let one = compose_filter(8, &bm(&[0]), &[
            (ListMode::Allow, &*a),
            (ListMode::Add, &*b),
            (ListMode::Deny, &*d),
        ]);
        let two = compose_filter(8, &bm(&[0]), &[
            (ListMode::Deny, &*d),
            (ListMode::Add, &*b),
            (ListMode::Allow, &*a),
        ]);
        assert_eq!(one, two);
    }
}
