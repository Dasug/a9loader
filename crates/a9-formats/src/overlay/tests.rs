use super::*;
use crate::container::{self, parse_csvs, parse_images, CsvEntry, ImageEntry, NameField};
use crate::pack::{self, Family};

fn op(name: &str, w: u32, h: u32, payload: &[u8]) -> ImageOp {
    ImageOp {
        name: name.into(),
        width: w,
        height: h,
        payload: payload.to_vec(),
    }
}

fn csv_op(name: &str, payload: &[u8]) -> CsvOp {
    CsvOp {
        name: name.into(),
        kind: CsvOpKind::Replace,
        payload: payload.to_vec(),
    }
}

fn csv_rows_op(name: &str, payload: &[u8]) -> CsvOp {
    CsvOp {
        name: name.into(),
        kind: CsvOpKind::Rows,
        payload: payload.to_vec(),
    }
}

fn utf16(s: &str) -> Vec<u8> {
    let mut out = vec![0xff, 0xfe];
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

#[test]
fn images_add_replace_roundtrip() {
    // base: two entries
    let base_entries = vec![
        ImageEntry {
            name: NameField::from_str("a.dds").unwrap(),
            width: 10,
            height: 20,
            payload: vec![1, 2, 3, 4],
        },
        ImageEntry {
            name: NameField::from_str("b.dds").unwrap(),
            width: 30,
            height: 40,
            payload: vec![5, 6, 7, 8],
        },
    ];
    let decoded = container::serialize_images(&base_entries);
    let file = pack::encode_pack_b(&decoded);

    // add one + replace one
    let (merged, report) = overlay_pack_b(
        &file,
        &[op("c.dds", 50, 60, &[9, 9]), op("a.dds", 11, 22, &[7, 7, 7, 7])],
        usize::MAX,
    )
    .unwrap();
    assert_eq!(report.added, 1);
    assert_eq!(report.replaced, 1);

    let back = pack::decode_pack(&merged, Family::B).unwrap();
    let entries = parse_images(&back).unwrap();
    assert_eq!(entries.len(), 3);
    let a = entries.iter().find(|e| e.name.as_str() == "a.dds").unwrap();
    assert_eq!((a.width, a.height), (11, 22));
    assert_eq!(a.payload, vec![7, 7, 7, 7]);
    let b = entries.iter().find(|e| e.name.as_str() == "b.dds").unwrap();
    assert_eq!(b.payload, vec![5, 6, 7, 8]); // untouched
    let c = entries.iter().find(|e| e.name.as_str() == "c.dds").unwrap();
    assert_eq!((c.width, c.height), (50, 60));
    assert_eq!(c.payload, vec![9, 9]);
    assert_eq!(report.decoded, back.len());
}

#[test]
fn images_duplicate_op_refused() {
    let decoded = container::serialize_images(&[]);
    let file = pack::encode_pack_b(&decoded);
    let err = overlay_pack_b(
        &file,
        &[op("x.dds", 1, 1, &[1]), op("x.dds", 2, 2, &[2])],
        usize::MAX,
    )
    .unwrap_err();
    assert!(err.to_string().contains("duplicate entry op"));
}

#[test]
fn images_budget_refused() {
    let decoded = container::serialize_images(&[]);
    let file = pack::encode_pack_b(&decoded);
    // 0x200 name + 12 B header + payload, budget below that
    let err = overlay_pack_b(&file, &[op("big.dds", 1, 1, &[0u8; 512])], 256).unwrap_err();
    assert!(err.to_string().contains("over the 256-byte budget"));
}

#[test]
fn csvs_add_replace_roundtrip() {
    let base = vec![
        CsvEntry {
            name: NameField::from_str("t1.csv").unwrap(),
            payload: "old1".as_bytes().to_vec(),
        },
    ];
    let decoded = container::serialize_csvs(&base);
    let file = pack::encode_pack_b(&decoded);
    let (merged, report) = overlay_csvp(
        &file,
        &[csv_op("t2.csv", b"new2".as_slice()), csv_op("t1.csv", b"replaced".as_slice())],
        usize::MAX,
    )
    .unwrap();
    assert_eq!((report.added, report.replaced), (1, 1));
    let back = parse_csvs(&pack::decode_pack(&merged, Family::B).unwrap()).unwrap();
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].payload, b"replaced".to_vec());
    assert_eq!(back[1].payload, b"new2".to_vec());
}

#[test]
fn csvs_row_merge_on_top_of_a_container() {
    let base_tbl = utf16("\"KEY\",\"VAL\"\r\n\"R1\",\"a\"\r\n\"R2\",\"b\"\r\n\"R3\",\"c\"\r\n");
    let decoded = container::serialize_csvs(&[CsvEntry {
        name: NameField::from_str("t1.csv").unwrap(),
        payload: base_tbl.clone(),
    }]);
    let file = pack::encode_pack_b(&decoded);
    let (merged, report) = overlay_csvp(
        &file,
        &[
            csv_rows_op("t1.csv", &utf16("\"R2\",\"B2\"\r\n")),
            csv_rows_op("t1.csv", &utf16("\"R4\",\"d\"\r\n")),
        ],
        usize::MAX,
    )
    .unwrap();
    assert_eq!((report.added, report.replaced), (0, 1));
    assert_eq!((report.row_replaced, report.row_appended), (1, 1));
    let back = parse_csvs(&pack::decode_pack(&merged, Family::B).unwrap()).unwrap();
    let want = utf16("\"KEY\",\"VAL\"\r\n\"R1\",\"a\"\r\n\"R2\",\"B2\"\r\n\"R3\",\"c\"\r\n\"R4\",\"d\"\r\n");
    assert_eq!(back[0].payload, want);
}

#[test]
fn csvs_row_patch_on_missing_table_is_refused() {
    let decoded = container::serialize_csvs(&[]);
    let file = pack::encode_pack_b(&decoded);
    let err = overlay_csvp(
        &file,
        &[csv_rows_op("nope.csv", &utf16("\"R1\",\"a\"\r\n"))],
        usize::MAX,
    )
    .unwrap_err();
    assert!(err.to_string().contains("not in the container"), "{err}");
}

#[test]
fn csvs_two_whole_table_replaces_refused() {
    let decoded = container::serialize_csvs(&[CsvEntry {
        name: NameField::from_str("t.csv").unwrap(),
        payload: b"x".to_vec(),
    }]);
    let file = pack::encode_pack_b(&decoded);
    let err = overlay_csvp(
        &file,
        &[csv_op("t.csv", b"a"), csv_op("t.csv", b"b")],
        usize::MAX,
    )
    .unwrap_err();
    assert!(err.to_string().contains("whole-table replaces"), "{err}");
}

#[test]
fn family_a_layout_and_append_roundtrip() {
    let asset = |name: &str, p: &[u8]| AssetOp {
        name: name.into(),
        dir: "train\\w1probe".into(),
        payload: p.to_vec(),
    };
    // plan math: off = |V| + Σ|P_j|, len = |P_i|
    let ops = vec![asset("a.x", &[0xAA; 10]), asset("b.x", &[0xBB; 25])];
    assert_eq!(plan_family_a(100, &ops), vec![(100, 10), (110, 25)]);
    assert_eq!(plan_family_a_lens(100, &[10, 25]), vec![(100, 10), (110, 25)]);
    assert_eq!(plan_family_a_lens(0, &[]), Vec::<(usize, usize)>::new());
    assert_eq!(family_a_total(100, &ops), 135);
    assert_eq!(plan_family_a(0, &[]), Vec::<(usize, usize)>::new());
    // budget: total must fit the cap; the exact boundary fits
    assert!(family_a_fits(100, &ops, 135));
    assert!(!family_a_fits(100, &ops, 134));
    assert!(family_a_fits(0, &[], 0));

    // append + re-encode: the merged stream is exactly V ++ P0 ++ P1
    let v: Vec<u8> = (0..200u32).map(|i| (i * 7) as u8).collect();
    let file = pack::encode_pack_a(&v);
    let (merged, report) = overlay_pack_a(&file, &ops, usize::MAX).unwrap();
    assert_eq!(report.added, 2);
    assert_eq!(report.replaced, 0);
    assert_eq!(report.decoded, 235);
    let back = pack::decode_pack(&merged, Family::A).unwrap();
    assert_eq!(&back[..200], &v[..]);
    assert_eq!(&back[200..210], &[0xAA; 10]);
    assert_eq!(&back[210..235], &[0xBB; 25]);
    assert_eq!(report.decoded, back.len());
    assert_eq!(report.encoded, merged.len());
}

#[test]
fn family_a_budget_and_duplicates_refused() {
    let v = vec![0u8; 50];
    let file = pack::encode_pack_a(&v);
    let asset = |name: &str, n: usize| AssetOp {
        name: name.into(),
        dir: "d".into(),
        payload: vec![1u8; n],
    };
    // decoded merged size 50 + 512 = 562 > 256
    let err = overlay_pack_a(&file, &[asset("big.x", 512)], 256).unwrap_err();
    assert!(err.to_string().contains("over the 256-byte budget"), "{err}");
    // duplicate op names in one merge
    let err = overlay_pack_a(&file, &[asset("x.x", 1), asset("x.x", 2)], usize::MAX)
        .unwrap_err();
    assert!(err.to_string().contains("duplicate asset op"), "{err}");
}

/// Encode names exactly as a `.dir` row carries them.
fn listing_bytes(names: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in names {
        for u in n.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]);
    }
    out
}

#[test]
fn listing_parse_roundtrips_and_bounds_by_the_payload() {
    let b = listing_bytes(&["a.dds", "form_kh32_f.dds", "form_kh32_s.dds"]);
    assert_eq!(
        listing_parse(&b).unwrap(),
        vec!["a.dds", "form_kh32_f.dds", "form_kh32_s.dds"]
    );
    // a name with no terminator cannot be read (the loader needs the NUL)
    let mut unt = b.clone();
    unt.truncate(unt.len() - 1);
    assert!(listing_parse(&unt).is_err());
}

#[test]
fn listing_extend_appends_new_and_dedups() {
    let vanilla = listing_bytes(&["a.dds", "b.dds"]);
    let names = vec![
        "b.dds".to_string(), // already listed -> skipped
        "c.dds".to_string(),
        "c.dds".to_string(), // duplicate within the contributions -> skipped
        "d.dds".to_string(),
    ];
    let (payload, appended, total) = listing_extend(&vanilla, &names).unwrap();
    assert_eq!(appended, 2);
    assert_eq!(total, 4);
    // vanilla bytes stay verbatim as the prefix
    assert_eq!(&payload[..vanilla.len()], &vanilla[..]);
    assert_eq!(
        listing_parse(&payload).unwrap(),
        vec!["a.dds", "b.dds", "c.dds", "d.dds"]
    );
}

#[test]
fn listing_extend_noop_is_byte_identical() {
    let vanilla = listing_bytes(&["a.dds", "b.dds"]);
    // nothing to add
    let (p, n, t) = listing_extend(&vanilla, &[]).unwrap();
    assert_eq!(p, vanilla);
    assert_eq!((n, t), (0, 2));
    // everything already present -> still byte-identical
    let names = vec!["a.dds".to_string(), "b.dds".to_string()];
    let (p, n, t) = listing_extend(&vanilla, &names).unwrap();
    assert_eq!(p, vanilla, "a no-op rebuild must not disturb a byte");
    assert_eq!((n, t), (0, 2));
}

#[test]
fn listing_extend_refuses_a_non_utf16_payload_loudly() {
    // odd length
    assert!(listing_extend(&[0x61, 0x00, 0x00], &["x.dds".into()]).is_err());
    // no NUL terminator at the end
    assert!(listing_extend(&[0x61, 0x00], &["x.dds".into()]).is_err());
    // an empty segment (two consecutive NULs)
    assert!(listing_extend(&[0x00, 0x00], &["x.dds".into()]).is_err());
    // empty payload
    assert!(listing_extend(&[], &["x.dds".into()]).is_err());
    // a lone surrogate unit is invalid UTF-16
    assert!(listing_extend(&[0x00, 0xd8, 0x00, 0x00], &["x.dds".into()]).is_err());
    // an empty / NUL-bearing contribution is refused too
    assert!(listing_extend(&listing_bytes(&["a.dds"]), &["".into()]).is_err());
    assert!(listing_extend(&listing_bytes(&["a.dds"]), &["a\0b".into()]).is_err());
}

#[test]
fn listing_auto_merge_allow_list_is_deliberately_narrow() {
    assert!(listing_auto_merge("previewtexture.dir"));
    // the positional/unknown-consumer listings stay on the manual path
    assert!(!listing_auto_merge("base.dir"));
    assert!(!listing_auto_merge("tree.dir"));
    assert!(!listing_auto_merge("previewtexture")); // the GROUP, not the row
    assert_eq!(AUTO_MERGE_LISTINGS, &["previewtexture.dir"]);
}

#[test]
fn real_pack_when_available() {
    if let Ok(p) = std::env::var("A9_STEAM_MTENTR0") {
        let file = std::fs::read(&p).unwrap();
        let (merged, report) = overlay_pack_b(&file, &[op("_d_test_probe.dds", 1, 1, &[1, 2])], usize::MAX).unwrap();
        assert_eq!(report.added, 1);
        let entries = parse_images(&pack::decode_pack(&merged, Family::B).unwrap()).unwrap();
        assert!(entries.iter().any(|e| e.name.as_str() == "_d_test_probe.dds"));
        // budget sanity with the real pack: the same merge over a small budget refuses
        let base_len = pack::decode_pack(&file, Family::B).unwrap().len();
        assert!(overlay_pack_b(&file, &[op("_d_test_probe.dds", 1, 1, &[1, 2])], base_len).is_err());
    }
}
