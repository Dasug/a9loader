//! The exe's FORM list and the proxy's allow-list files — pure parsing, so it
//! is unit-testable on any host. Behavioral port of `load_allowed()` in the C
//! reference:
//! - FORM list: UTF-16, comma-separated, NUL-terminated, at RVA `0x6FC430`
//!   (file `0x6FB830`); entries are trimmed of spaces/tabs; entry index =
//!   form id.
//! - allow-list txt: one FORM name per line, `#` comments, trimmed; matching
//!   is exact and case-sensitive (like the C).

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
}
