//! The per-build anchor address book (`builds.toml`) — manifests reference
//! **anchor names**, never raw RVAs; each known build maps anchors to
//! `{rva, expect}` pairs in the *running* image. The book ships **embedded**
//! in the loader and in `a9ml` (same file, `include_str!`), so there is no
//! loose file next to the game and adding a build is a loader build away.
//!
//! Every anchored write verifies the recorded `expect` bytes first — a game
//! update that shifts code fails loudly instead of corrupting memory.

use crate::hex;
use crate::ModError;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The address book as shipped in this repository (`crates/a9-mod/builds.toml`).
pub fn embedded() -> &'static str {
    include_str!("../builds.toml")
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct AddressBook {
    pub build: BTreeMap<String, BuildEntry>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct BuildEntry {
    #[serde(default)]
    pub anchors: BTreeMap<String, Anchor>,
}

impl BuildEntry {
    pub fn anchor(&self, name: &str) -> Option<&Anchor> {
        self.anchors.get(name)
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub rva: u32,
    /// original bytes at the anchor (hex) — verified before any anchored
    /// write; also used as a build fingerprint where recorded
    #[serde(default)]
    pub expect: Option<String>,
}

impl Anchor {
    pub fn expect_bytes(&self) -> Option<Vec<u8>> {
        self.expect.as_deref().and_then(hex::parse)
    }
}

pub fn parse(text: &str) -> Result<AddressBook, ModError> {
    toml::from_str(text).map_err(|e| ModError::Parse(format!("builds.toml: {e}")))
}

/// Parse the embedded book — used by the loader at boot and by tests to keep
/// the shipped book valid.
pub fn embedded_book() -> Result<AddressBook, ModError> {
    parse(embedded())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_book_is_valid_and_has_the_steam_build() {
        let book = embedded_book().unwrap();
        let b = book.build.get("5.00.5634").expect("steam build entry");
        let f = b.anchor("train_list_filter").unwrap();
        assert_eq!(f.rva, 0x1A13B0);
        let expect = f.expect_bytes().unwrap();
        assert_eq!(expect.len(), 16);
        assert_eq!(&expect[..4], &[0x48, 0x89, 0x5c, 0x24]);
        let w = b.anchor("train_list_whitelist").unwrap();
        assert_eq!(w.rva, 0x6FC1C0);
        assert_eq!(w.expect_bytes().unwrap(), vec![0x5f, 0x00, 0x00, 0x00]);
        let fl = b.anchor("form_list").unwrap();
        assert_eq!(fl.rva, 0x6FC430);
        assert_eq!(&fl.expect_bytes().unwrap()[..6], &[0x46, 0x00, 0x4f, 0x00, 0x52, 0x00]);
    }

    #[test]
    fn rejects_unknown_fields() {
        // no banner/banner_rva anymore — the in-image fingerprint is the
        // per-anchor expect verification (banner check dropped 2026-10-16:
        // the banner string lives in .rsrc = the same VERSIONINFO data the
        // build id already came from — checking it twice proved nothing)
        let bad = "[build.\"x\"]\nbanner = \"b\"\n";
        assert!(parse(bad).is_err());
        let bad = "[build.\"x\"]\nextra = 1\n";
        assert!(parse(bad).is_err());
        let bad = "[build.\"x\".anchors.a]\nrva = 2\nfoo = 3\n";
        assert!(parse(bad).is_err());
    }
}
