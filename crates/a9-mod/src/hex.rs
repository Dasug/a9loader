//! Hex-string helpers for manifest fields (`data`, `expect`) and address-book
//! `expect` values. Accepts the human-readable comma form used in
//! `builds.toml` ("48,89,5c" ) and compact hex ("48895c"), case-insensitive.

pub fn parse(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s
        .chars()
        .filter(|c| !matches!(c, ' ' | ',' | '\t' | '\n' | '\r'))
        .collect();
    if cleaned.len() % 2 != 0 {
        return None;
    }
    // empty string = empty byte vector (callers decide whether that is legal)
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).ok())
        .collect()
}

pub fn fmt(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_forms() {
        assert_eq!(parse("48,89,5c").unwrap(), vec![0x48, 0x89, 0x5c]);
        assert_eq!(parse("48895C").unwrap(), vec![0x48, 0x89, 0x5c]);
        assert_eq!(parse("48 89 5c\n24 08").unwrap(), vec![0x48, 0x89, 0x5c, 0x24, 0x08]);
        assert_eq!(parse("").unwrap(), Vec::<u8>::new());
        assert!(parse("4").is_none()); // odd length
        assert!(parse("zz").is_none()); // not hex
    }

    #[test]
    fn formats_comma_form() {
        assert_eq!(fmt(&[0x48, 0x89]), "48,89");
        assert_eq!(parse(&fmt(&[1, 2, 3])).unwrap(), vec![1, 2, 3]);
    }
}
