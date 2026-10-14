//! Synthetic self-tests for the format library, run by `a9ml verify` before
//! the real-pack sweep. Each test proves one primitive end-to-end on data it
//! builds itself: cipher + md5 header, LZSS (incl. overlap + ring wrap),
//! image/csv container byte parity (verbatim name fields, incl. the garbage
//! padding some original packs carry), i.dat parity for both known build
//! paddings, the full family-B pack pipeline, and savegame member carving.
//!
//! [`self_tests`] is just the ordered list of the individual test functions —
//! the order is the report's order.

use a9_formats as fmt;

use super::verify::Check;

pub(super) fn self_tests() -> Vec<Check> {
    let mut out = Vec::new();
    out.push(test_crypt());
    out.push(test_lzss());
    out.extend(test_containers());
    out.push(test_idat());
    out.push(test_pack_pipeline());
    out.push(test_savegame());
    out
}

/// cipher round-trip + md5 header.
fn test_crypt() -> Check {
    let plain: Vec<u8> = (0..50_000u32).map(|i| (i * 2654435761 >> 24) as u8).collect();
    let file = fmt::crypt::encrypt_file(&plain);
    if fmt::crypt::header_matches(&file, &plain)
        && fmt::crypt::decrypt_body(&file).map(|d| d == plain).unwrap_or(false)
    {
        Check::ok("crypt", "encrypt/decrypt + md5 header")
    } else {
        Check::fail("crypt", "round-trip mismatch")
    }
}

/// lzss round-trip incl. overlap + ring wrap.
fn test_lzss() -> Check {
    let mut data = b"abcabcabcabc".to_vec();
    data.extend_from_slice(&vec![0u8; 8192]);
    data.extend_from_slice(b"abcabcabc tail");
    if fmt::lzss::decode(&fmt::lzss::encode(&data), 0) == data {
        Check::ok("lzss", "encode/decode round-trip (overlap + wrap)")
    } else {
        Check::fail("lzss", "round-trip mismatch")
    }
}

/// image + csv container parity (verbatim name fields, incl. garbage padding).
fn test_containers() -> Vec<Check> {
    let mut out = Vec::new();
    let imgs = vec![fmt::container::ImageEntry {
        name: fmt::container::NameField::from_str("_d_test_a.dds").unwrap(),
        width: 8,
        height: 4,
        payload: vec![9u8; 33],
    }];
    let ser = fmt::container::serialize_images(&imgs);
    out.push(
        if fmt::container::parse_images(&ser).map(|p| p == imgs).unwrap_or(false)
            && fmt::container::serialize_images(&fmt::container::parse_images(&ser).unwrap()) == ser
        {
            Check::ok("container-images", "parse/serialize byte parity")
        } else {
            Check::fail("container-images", "mismatch")
        },
    );
    let csvs = vec![fmt::container::CsvEntry {
        name: fmt::container::NameField::from_str("train_comment.csv").unwrap(),
        payload: b"a,b\r\n".to_vec(),
    }];
    let ser = fmt::container::serialize_csvs(&csvs);
    out.push(
        if fmt::container::parse_csvs(&ser).map(|p| p == csvs).unwrap_or(false)
            && fmt::container::serialize_csvs(&fmt::container::parse_csvs(&ser).unwrap()) == ser
        {
            Check::ok("container-csvs", "parse/serialize byte parity")
        } else {
            Check::fail("container-csvs", "mismatch")
        },
    );
    out
}

/// i.dat parity (both known build paddings).
fn test_idat() -> Check {
    let rows = vec![fmt::idat::Entry {
        name: "s_orm_02_f.dds".into(),
        dir: "dai".into(),
        tail: [1, 0, 0, 0, 0, 9, 0, 0, 0],
    }];
    let mut ok = true;
    for pad in [fmt::idat::PAD_DEMO, fmt::idat::PAD_STEAM] {
        let x = fmt::idat::Idat { pad, entries: rows.clone() };
        let ser = fmt::idat::serialize(&x);
        if let Ok(reparsed) = fmt::idat::parse(&ser) {
            ok &= reparsed == x && fmt::idat::serialize(&reparsed) == ser;
        } else {
            ok = false;
        }
    }
    if ok {
        Check::ok("idat", "parse/serialize byte parity (demo + steam pads)")
    } else {
        Check::fail("idat", "mismatch")
    }
}

/// full synthetic pack pipeline: family B file -> decode -> container.
fn test_pack_pipeline() -> Check {
    let imgs = vec![fmt::container::ImageEntry {
        name: fmt::container::NameField::from_str("_d_pipeline.dds").unwrap(),
        width: 2,
        height: 2,
        payload: vec![7u8; 16],
    }];
    let ser = fmt::container::serialize_images(&imgs);
    let file = fmt::pack::encode_pack_b(&ser);
    let dec = fmt::pack::decode_pack(&file, fmt::pack::Family::B).unwrap();
    if fmt::container::parse_images(&dec).map(|p| p == imgs).unwrap_or(false) {
        Check::ok("pack-pipeline", "synthetic family-B file round-trip")
    } else {
        Check::fail("pack-pipeline", "mismatch")
    }
}

/// savegame carving.
fn test_savegame() -> Check {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write as _;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(b"savegame member").unwrap();
    let z = e.finish().unwrap();
    let members = fmt::savegame::carve_members(&z);
    if members == vec![b"savegame member".to_vec()] {
        Check::ok("savegame", "zlib member carving")
    } else {
        Check::fail("savegame", "mismatch")
    }
}
