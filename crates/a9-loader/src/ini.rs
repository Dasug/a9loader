//! `atrain9_proxy.ini` reader — M0 compat, mirrors the single
//! `GetPrivateProfileStringA("patch", "mode", "filter", …)` call in the C
//! reference. Section and key matching is ASCII-case-insensitive (like the
//! profile API); the value is taken verbatim. Missing file/section/key →
//! `"filter"`, exactly the C default.

use std::path::Path;

pub const DEFAULT_MODE: &str = "filter";

pub fn read_mode(exe_dir: &Path) -> String {
    std::fs::read_to_string(exe_dir.join("atrain9_proxy.ini"))
        .map(|t| parse_mode(&t))
        .unwrap_or_else(|_| DEFAULT_MODE.to_string())
}

pub fn parse_mode(text: &str) -> String {
    let mut in_patch = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest.split(']').next().unwrap_or("").trim();
            in_patch = name.eq_ignore_ascii_case("patch");
            continue;
        }
        if !in_patch || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim().eq_ignore_ascii_case("mode") {
            return unquote(value.trim()).to_string();
        }
    }
    DEFAULT_MODE.to_string()
}

fn unquote(v: &str) -> &str {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_when_empty_or_missing_section() {
        assert_eq!(parse_mode(""), DEFAULT_MODE);
        assert_eq!(parse_mode("[other]\nmode = nop\n"), DEFAULT_MODE);
    }

    #[test]
    fn reads_mode_case_insensitive_section_and_key() {
        assert_eq!(parse_mode("[PATCH]\nMODE = model\n"), "model");
        assert_eq!(parse_mode("[patch]\nmode=modeltex\n"), "modeltex");
    }

    #[test]
    fn skips_comments_and_other_keys() {
        let ini = "; comment\n[patch]\n;mode = nop\nother = 1\nmode = table113\n";
        assert_eq!(parse_mode(ini), "table113");
    }

    #[test]
    fn value_verbatim_and_trimmed() {
        assert_eq!(parse_mode("[patch]\nmode =   model  \n"), "model");
    }

    #[test]
    fn quoted_value_unquoted() {
        assert_eq!(parse_mode("[patch]\nmode = \"nop\"\n"), "nop");
    }
}
