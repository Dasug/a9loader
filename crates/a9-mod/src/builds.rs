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
        let cf = b.anchor("cfile_open").unwrap();
        assert_eq!(cf.rva, 0x41D40C);
        let cfe = cf.expect_bytes().unwrap();
        assert_eq!(cfe, vec![0x40, 0x53, 0x55, 0x56, 0x57, 0x41, 0x56, 0x41, 0x57, 0x48, 0x81, 0xec, 0x88, 0x02, 0x00, 0x00]);
        let fl = b.anchor("form_list").unwrap();
        assert_eq!(fl.rva, 0x6FC430);
        assert_eq!(&fl.expect_bytes().unwrap()[..6], &[0x46, 0x00, 0x4f, 0x00, 0x52, 0x00]);
        // the fetch-probe anchors (machine-verified boundaries; see the
        // comment blocks in builds.toml for why these lengths differ)
        let pf = b.anchor("pack_fetch").unwrap();
        assert_eq!(pf.rva, 0x306E20);
        let pfe = pf.expect_bytes().unwrap();
        assert_eq!(pfe.len(), 22);
        assert_eq!(&pfe[..7], &[0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x56]);
        assert_eq!(&pfe[15..22], &[0x48, 0x81, 0xec, 0x00, 0x09, 0x00, 0x00]);
        let tf = b.anchor("texture_fetch").unwrap();
        assert_eq!(tf.rva, 0x3010D0);
        let tfe = tf.expect_bytes().unwrap();
        assert_eq!(tfe.len(), 19);
        assert_eq!(&tfe[11..19], &[0x48, 0x8d, 0xac, 0x24, 0x20, 0xe0, 0xff, 0xff]);
        let cf = b.anchor("ccommon_fetch").unwrap();
        assert_eq!(cf.rva, 0x2EC0E0);
        let cfe = cf.expect_bytes().unwrap();
        assert_eq!(cfe.len(), 20);
        assert_eq!(&cfe[12..20], &[0x48, 0x8d, 0xac, 0x24, 0x50, 0xe8, 0xff, 0xff]);
        let cg = b.anchor("fetch_cache_get").unwrap();
        assert_eq!(cg.rva, 0x3089B0);
        let cge = cg.expect_bytes().unwrap();
        assert_eq!(cge, vec![
            0x48, 0x8b, 0xc4, 0x57, 0x41, 0x54, 0x41, 0x55,
            0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xec, 0x40,
        ]);
        let fr = b.anchor("famb_runner_call").unwrap();
        assert_eq!(fr.rva, 0x3F0259);
        assert_eq!(
            fr.expect_bytes().unwrap(),
            vec![0xe8, 0x62, 0x02, 0xc6, 0xff, 0x85, 0xdb]
        );
        let fc = b.anchor("famb_runner_cave").unwrap();
        assert_eq!(fc.rva, 0x59A071);
        assert_eq!(
            fc.expect_bytes().unwrap(),
            vec![0xcc; 16]
        );
        // the tailgate anchors (the family-A decode funnel + its cave)
        let dec = b.anchor("pack_lzss_decode").unwrap();
        assert_eq!(dec.rva, 0x314BF0);
        assert_eq!(
            dec.expect_bytes().unwrap(),
            vec![0x41, 0x54, 0x41, 0x56, 0x41, 0x57, 0xb8, 0x60, 0x10, 0x00, 0x00]
        );
        let vp = b.anchor("pack_lzss_vp_call").unwrap();
        assert_eq!(vp.rva, 0x3F1434);
        assert_eq!(vp.expect_bytes().unwrap(), vec![0xe8, 0xb7, 0x37, 0xf2, 0xff]);
        let tp = b.anchor("pack_lzss_tp_call").unwrap();
        assert_eq!(tp.rva, 0x3F0F4D);
        assert_eq!(tp.expect_bytes().unwrap(), vec![0xe8, 0x9e, 0x3c, 0xf2, 0xff]);
        let idat = b.anchor("pack_lzss_idat_call").unwrap();
        assert_eq!(idat.rva, 0x31516F);
        assert_eq!(idat.expect_bytes().unwrap(), vec![0xe8, 0x7c, 0xfa, 0xff, 0xff]);
        let mcp = b.anchor("pack_lzss_mcp_call").unwrap();
        assert_eq!(mcp.rva, 0x3EE93A);
        assert_eq!(mcp.expect_bytes().unwrap(), vec![0xe8, 0xb1, 0x62, 0xf2, 0xff]);
        let ep = b.anchor("pack_lzss_ep_call").unwrap();
        assert_eq!(ep.rva, 0x3EFC16);
        assert_eq!(ep.expect_bytes().unwrap(), vec![0xe8, 0xd5, 0x4f, 0xf2, 0xff]);
        let cp = b.anchor("pack_lzss_cp_call").unwrap();
        assert_eq!(cp.rva, 0x3EF326);
        assert_eq!(cp.expect_bytes().unwrap(), vec![0xe8, 0xc5, 0x58, 0xf2, 0xff]);
        let cav = b.anchor("pack_lzss_cave").unwrap();
        assert_eq!(cav.rva, 0x5A1D13);
        assert_eq!(cav.expect_bytes().unwrap(), vec![0xcc; 16]);
        // the save-path probe anchors
        let pw = b.anchor("mmf_paged_write").unwrap();
        assert_eq!(pw.rva, 0x308FE0);
        assert_eq!(
            pw.expect_bytes().unwrap(),
            vec![
                0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x48, 0x89, 0x7c,
                0x24, 0x20, 0x41, 0x56
            ]
        );
        let pwb = b.anchor("mmf_paged_write_bulk").unwrap();
        assert_eq!(pwb.rva, 0x308E20);
        assert_eq!(pwb.expect_bytes().unwrap().len(), 17);
        let wb = b.anchor("save_mmf_write_block").unwrap();
        assert_eq!(wb.rva, 0x41500);
        assert_eq!(
            wb.expect_bytes().unwrap(),
            vec![
                0x48, 0x89, 0x6c, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x48, 0x89, 0x4c,
                0x24, 0x08, 0x57
            ]
        );
        let rv = b.anchor("mmf_read_u32_vector").unwrap();
        assert_eq!(rv.rva, 0x41820);
        assert_eq!(rv.expect_bytes().unwrap().len(), 16);
        // the save-id clamp guard anchors
        let sh = b.anchor("save_group_head").unwrap();
        assert_eq!(sh.rva, 0x1B6F0C);
        assert_eq!(
            sh.expect_bytes().unwrap(),
            vec![0x49, 0x63, 0xc6, 0x48, 0x8d, 0x0c, 0x80]
        );
        let shc = b.anchor("save_group_head_cave").unwrap();
        assert_eq!(shc.rva, 0x5D28F0);
        assert_eq!(shc.expect_bytes().unwrap(), vec![0xcc; 16]);
        let scr = b.anchor("save_car_read").unwrap();
        assert_eq!(scr.rva, 0x61F8B);
        assert_eq!(
            scr.expect_bytes().unwrap(),
            vec![0xe8, 0x90, 0x6f, 0x2a, 0x00]
        );
        let scrc = b.anchor("save_car_read_cave").unwrap();
        assert_eq!(scrc.rva, 0x66790);
        assert_eq!(scrc.expect_bytes().unwrap(), vec![0xcc; 16]);
        // the direct spec-row injection anchors
        let sr = b.anchor("spec_table_resize").unwrap();
        assert_eq!(sr.rva, 0x19F150);
        assert_eq!(
            sr.expect_bytes().unwrap(),
            vec![
                0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x10, 0x57, 0x48, 0x83,
                0xec, 0x30, 0x4c, 0x8b, 0x41, 0x08
            ]
        );
        let rr = b.anchor("spec_table_realloc").unwrap();
        assert_eq!(rr.rva, 0x19FC50);
        assert_eq!(
            rr.expect_bytes().unwrap(),
            vec![
                0x41, 0x56, 0x48, 0x83, 0xec, 0x40, 0x48, 0xc7, 0x44, 0x24, 0x30, 0xfe, 0xff,
                0xff, 0xff, 0x48, 0x89, 0x5c, 0x24, 0x50
            ]
        );
        let ra = b.anchor("spec_table_row_accessor").unwrap();
        assert_eq!(ra.rva, 0x1AACB0);
        assert_eq!(
            ra.expect_bytes().unwrap(),
            vec![0x48, 0x8d, 0x04, 0x92, 0x48, 0xc1, 0xe0, 0x06, 0x48, 0x03, 0x01, 0xc3]
        );
        let nc = b.anchor("row_name_ctor").unwrap();
        assert_eq!(nc.rva, 0x372F0);
        assert_eq!(
            nc.expect_bytes().unwrap(),
            vec![
                0x48, 0x89, 0x54, 0x24, 0x10, 0x4c, 0x89, 0x44, 0x24, 0x18, 0x4c, 0x89, 0x4c,
                0x24, 0x20, 0x48, 0x83, 0xec, 0x28
            ]
        );
        // the train injection anchors
        let tlb = b.anchor("train_list_builder").unwrap();
        assert_eq!(tlb.rva, 0x19D9C0);
        assert_eq!(
            tlb.expect_bytes().unwrap(),
            vec![0x40, 0x53, 0x56, 0x57, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0x41, 0x30, 0x48, 0x89, 0x6c, 0x24, 0x48]
        );
        let wcr = b.anchor("wstr_clone_ref").unwrap();
        assert_eq!(wcr.rva, 0x1D7D0);
        assert_eq!(
            wcr.expect_bytes().unwrap(),
            vec![0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xd9, 0x48, 0x8b, 0x0a, 0x48, 0x83, 0xe9, 0x18]
        );
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
