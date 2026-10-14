//! `[[loose_asset]]` path matching — a tiny, pure, host-testable glob.
//!
//! The game opens some files BY PATH (not content inside a pack). The
//! motivating case names a PER-BOOT RANDOM temp directory
//! (`%TEMP%\<random>.adk\fe01.bin`), so a mod must match by PATTERN, never by
//! an exact path. The glob is deliberately minimal: `*` matches any run of
//! characters INCLUDING the empty run; every other character (including `?`
//! and `[`/`]`) is a literal. Matching is case-insensitive and
//! slash-insensitive (`/` ≡ `\`) — the same normalization the loader applies to
//! the game's own open path before it tests the table.
//!
//! [`normalize`] is the one place that rule lives. The loader's hot path calls
//! [`Pattern::matches_norm`] with an already-normalized path (its own
//! `path_to_string` output) and never re-normalizes per pattern; [`Pattern::
//! matches`] is the convenience wrapper that normalizes for tests and `a9ml`.

/// Normalize a game path or a manifest pattern: ASCII-lowercase, `/` → `\`.
/// (The loader's `path_to_string` additionally strips a leading `.\`; a
/// manifest pattern never carries one.)
pub fn normalize(s: &str) -> String {
    s.to_ascii_lowercase().replace('/', "\\")
}

/// A compiled `*`-glob. Matching is a left-to-right scan over the literal
/// segments between wildcards; `*` consumes any run (including empty).
///
/// The only metacharacter is `*`. `?` and `[`/`]` are ordinary characters — a
/// shipped name like `foo[1].bin` matches literally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    /// the literal segments between `*`s, in order. A pattern with no `*` is a
    /// single segment; a leading/trailing `*` leaves an empty first/last
    /// segment (the anchor flag is derived from that, not stored separately).
    parts: Vec<String>,
    /// the normalized pattern text (for logs)
    text: String,
}

impl Pattern {
    /// Compile a manifest pattern (normalized here). Infallible: an empty
    /// pattern is refused at manifest validation, and a pattern that is only
    /// wildcards is a legal match-anything.
    pub fn compile(pattern: &str) -> Self {
        let text = normalize(pattern);
        let parts = text.split('*').map(str::to_string).collect();
        Pattern { parts, text }
    }

    /// The normalized pattern text (what the loader logs).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Match an ALREADY-normalized path ([`normalize`]d). The loader's hot
    /// path uses this; tests and `a9ml` use [`Pattern::matches`].
    pub fn matches_norm(&self, s: &str) -> bool {
        let n = self.parts.len();
        // `n > 1` guards the empty pattern: `"".split('*')` is `[""]`, which
        // must stay an exact match on the empty string, not a match-anything.
        let lead = n > 1 && self.parts[0].is_empty();
        let tail = n > 1 && self.parts[n - 1].is_empty();
        let mut rest = s;
        for (i, seg) in self.parts.iter().enumerate() {
            let is_first = i == 0;
            let is_last = i + 1 == n;
            if is_first && !lead {
                // anchored at the start (there is no leading `*`)
                match rest.strip_prefix(seg.as_str()) {
                    Some(r) => rest = r,
                    None => return false,
                }
                if is_last {
                    return rest.is_empty(); // no `*` at all -> an exact match
                }
                continue;
            }
            if is_last && !tail {
                // anchored at the end (there is no trailing `*`)
                return rest.ends_with(seg.as_str());
            }
            // an interior segment (or a wildcard-adjacent empty one): find it
            // anywhere; `*` before it absorbs whatever precedes.
            match rest.find(seg.as_str()) {
                Some(pos) => rest = &rest[pos + seg.len()..],
                None => return false,
            }
        }
        true
    }

    /// Match a raw path (normalized first).
    pub fn matches(&self, raw: &str) -> bool {
        self.matches_norm(&normalize(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        Pattern::compile(pattern).matches(path)
    }

    #[test]
    fn exact_paths_match_case_insensitively() {
        // no `*` = an exact (case-insensitive) path
        assert!(m("data\\bmf\\fy01.bin", "data\\bmf\\fy01.bin"));
        assert!(m("data\\bmf\\fy01.bin", "DATA\\BMF\\FY01.BIN"));
        // a leading `.\` (the game sometimes opens that way) is NOT part of the
        // normalized path the loader tests — normalization here only lowercases
        // and slashes, so an author writes the bare path
        assert!(!m("data\\bmf\\fy01.bin", ".\\data\\bmf\\fy01.bin"));
        // a longer path is NOT an exact match
        assert!(!m("data\\bmf\\fy01.bin", "data\\bmf\\fy01.bin.bak"));
        // slash-insensitivity: the game may pass `/`
        assert!(m("data\\bmf\\fy01.bin", "data/bmf/fy01.bin"));
    }

    #[test]
    fn suffix_glob_matches_the_motivating_case() {
        // the random per-boot temp dir: `*.adk\fe01.bin`
        let p = "*.adk\\fe01.bin";
        assert!(m(p, "c:\\users\\dasug\\appdata\\local\\temp\\8avqouma.adk\\fe01.bin"));
        assert!(m(p, "C:/Users/dasug/Temp/8AVQOUMA.ADK/FE01.BIN"));
        assert!(m(p, "z:\\tmp\\x.adk\\fe01.bin"));
        // a different name in the same dir does not match
        assert!(!m(p, "z:\\tmp\\x.adk\\fs02.bin"));
        // a different extension does not match
        assert!(!m(p, "z:\\tmp\\x.adk\\fe01.dds"));
        // the suffix must be the tail, not somewhere in the middle
        assert!(!m(p, "z:\\tmp\\x.adk\\fe01.bin.bak"));
    }

    #[test]
    fn prefix_glob() {
        let p = "data\\bmf\\*";
        assert!(m(p, "data\\bmf\\fy01.bin"));
        assert!(m(p, "data\\bmf\\a\\b\\c.dds"));
        // the empty run is allowed: `data\bmf\` alone matches
        assert!(m(p, "data\\bmf\\"));
        assert!(!m(p, "data\\other\\fy01.bin"));
        // prefix must anchor at the start
        assert!(!m(p, "x\\data\\bmf\\fy01.bin"));
    }

    #[test]
    fn no_match() {
        assert!(!m("*.adk\\fe01.bin", "data\\mtfake.bin"));
        assert!(!m("data\\bmf\\fy01.bin", "data\\bmf\\fy02.bin"));
        assert!(!m("data\\bmf\\*", ""));
        // an empty path never matches a non-empty pattern
        assert!(!m("a", ""));
    }

    #[test]
    fn empty_wildcard_run() {
        // `*` matches the empty run
        assert!(m("ab*cd", "abcd"));
        assert!(m("ab*cd", "abXYZcd"));
        assert!(!m("ab*cd", "abXYZc"));
        // adjacent wildcards collapse
        assert!(m("a**b", "ab"));
        assert!(m("a**b", "aXXXb"));
    }

    #[test]
    fn star_at_both_ends() {
        let p = "*bmf*";
        assert!(m(p, "data\\bmf\\fy01.bin"));
        assert!(m(p, "bmf"));
        assert!(m(p, "x\\y\\bmf"));
        assert!(m(p, "prebmfpost"));
        assert!(!m(p, "data\\other\\fy01.bin"));
    }

    #[test]
    fn lone_star_matches_anything() {
        assert!(m("*", ""));
        assert!(m("*", "anything\\at\\all.bin"));
    }

    #[test]
    fn question_and_brackets_are_literals() {
        // `?` and `[`/`]` are NOT metacharacters
        assert!(m("a?b", "a?b"));
        assert!(!m("a?b", "axb"));
        assert!(m("foo[1].bin", "foo[1].bin"));
        assert!(!m("foo[1].bin", "foo1.bin"));
    }

    #[test]
    fn matches_norm_skips_re_normalization() {
        let p = Pattern::compile("*.ADK\\FE01.BIN");
        assert_eq!(p.text(), "*.adk\\fe01.bin");
        // the caller passes an already-normalized path
        assert!(p.matches_norm("z:\\t\\x.adk\\fe01.bin"));
        assert!(!p.matches_norm("z:\\t\\x.adk\\FE01.BIN"), "raw case is the caller's job");
    }

    #[test]
    fn normalize_rule() {
        assert_eq!(normalize("Data/Bmf/Fy01.BIN"), "data\\bmf\\fy01.bin");
        assert_eq!(normalize("a\\b"), "a\\b");
        assert_eq!(normalize(""), "");
    }
}
