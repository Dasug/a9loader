use super::*;

fn parse(text: &str) -> Result<Manifest, toml::de::Error> {
    toml::from_str(text)
}

const GOOD: &str = r#"
id = "jp-liveries"
name = "JP 2022 liveries"
version = "1.0.0"
author = "dasug"
game_builds = ["5.00.5634"]
priority = 10
supplies = ["jp-livery-set"]
requires = ["a9-base-assets"]
conflicts = ["stock-liveries"]

[[hook]]
target = "train_list_filter"
handler = "train_list"
mode = "allow"
list = "liveries.txt"

[[patch]]
target = "train_list_whitelist"
offset = 24
data = "88 00 00 00"
expect = "bb 00 00 00"
"#;

#[test]
fn parses_and_validates_good_manifest() {
    let m = parse(GOOD).unwrap();
    m.validate().unwrap();
    assert_eq!(m.id, "jp-liveries");
    assert_eq!(m.priority, 10);
    assert_eq!(m.hook.len(), 1);
    assert_eq!(m.hook[0].mode, Some(ListMode::Allow));
    assert_eq!(m.patch[0].offset, 24);
    assert!(matches!(m.patch[0].target, TargetRef::Anchor(_)));
}

#[test]
fn defaults_fill_in() {
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"0.1.0\"\nauthor=\"a\"\n").unwrap();
    m.validate().unwrap();
    assert_eq!(m.priority, DEFAULT_PRIORITY);
    assert!(m.game_builds.is_empty());
    assert!(m.patch.is_empty() && m.hook.is_empty());
}

#[test]
fn train_list_modes_allow_add_deny() {
    // `add` (M5 W3): offer the listed forms on top of the shipped whitelist
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[hook]]\ntarget=\"train_list_filter\"\nhandler=\"train_list\"\nmode=\"add\"\nlist=\"add.txt\"\n",
    )
    .unwrap();
    m.validate().unwrap();
    assert_eq!(m.hook[0].mode, Some(ListMode::Add));
    // an unknown mode is refused by the enum, like any typo
    assert!(parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[hook]]\ntarget=\"train_list_filter\"\nhandler=\"train_list\"\nmode=\"append\"\nlist=\"a.txt\"\n"
    )
    .is_err());
    // `add` still requires a list
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[hook]]\ntarget=\"train_list_filter\"\nhandler=\"train_list\"\nmode=\"add\"\n",
    )
    .unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("requires `list`"));
}

#[test]
fn unknown_fields_refuse_the_mod() {
    let bad = GOOD.to_owned() + "\n[[effect]]\nkind = \"x\"\n";
    let err = parse(&bad).unwrap_err().to_string();
    assert!(err.contains("unknown field"), "{err}");
    // [native] (mods that ship code) lands post-M4 — unknown field here
    // (the scan path adds the milestone pointer)
    let bad = GOOD.to_owned() + "\n[native]\nabi = 1\n";
    let err = parse(&bad).unwrap_err().to_string();
    assert!(err.contains("unknown field `native`"), "{err}");
    // the M3 sections are supported now and must parse
    let m3 = GOOD.to_owned()
        + "\n[[pack]]\npack = \"mtentr0\"\n\n[[pack.entry]]\nname = \"_d_x.dds\"\nwidth = 1\nheight = 2\nfile = \"e/x.dds\"\n"
        + "\n[[csv]]\nname = \"train_form.csv\"\nfile = \"csv/train_form.csv\"\n"
        + "\n[[file]]\ndest = \"data/texture/x.dds\"\nfile = \"files/x.dds\"\n";
    let m = parse(&m3).unwrap();
    m.validate().unwrap();
    assert_eq!(m.pack.len(), 1);
    assert_eq!(m.pack[0].entry.len(), 1);
    assert_eq!(m.pack[0].entry[0].width, 1);
    assert_eq!(m.csv.len(), 1);
    assert_eq!(m.file.len(), 1);
}

#[test]
fn m3_section_validation_rejects_the_expected_classes() {
    // csvp as a [[pack]] name
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"csvp\"\n[[pack.entry]]\nname=\"t.csv\"\nwidth=1\nheight=1\nfile=\"f\"\n").unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("[[csv]]"), "{e}");
    // duplicate pack sections
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"me0\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\"f\"\n[[pack]]\npack=\"me0\"\n[[pack.entry]]\nname=\"b\"\nwidth=1\nheight=1\nfile=\"f\"\n").unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("merge their entries"), "{e}");
    // duplicate entry names in one pack
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"me0\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\"f\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\"f\"\n").unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("two entries named"), "{e}");
    // missing file / empty
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"me0\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\" \"\n").unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("no `file`"));
    // uppercase pack name
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"Me0\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\"f\"\n").unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("slug"));
    // duplicate csv tables
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\n[[csv]]\nname=\"t.csv\"\nfile=\"g\"\n").unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("two [[csv]]"), "{e}");
    // csv mode: `rows` parses + validates; `mode` defaults to replace
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\nmode=\"rows\"\n").unwrap();
    m.validate().unwrap();
    assert_eq!(m.csv[0].mode, CsvMode::Rows);
    assert!(m.csv[0].row_keys.is_empty());
    let d = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\n").unwrap();
    assert_eq!(d.csv[0].mode, CsvMode::Replace);
    // an unknown mode is a hard refusal (deny_unknown_fields + enum)
    assert!(parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\nmode=\"cells\"\n").is_err());
    // `row_keys` is derived state — authoring it is refused (skipped field)
    assert!(parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\nrow_keys=[\"R1\"]\n").is_err());
    // file dest escaping the game dir (TOML needs \\ for a literal backslash)
    for dest in ["/abs/x.dds", "C:\\\\x.dds", "data/../../x.dds"] {
        let m = parse(&format!("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[file]]\ndest=\"{dest}\"\nfile=\"f\"\n")).unwrap();
        assert!(m.validate().is_err(), "dest {dest} must be refused");
    }
    // duplicate file dests
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[file]]\ndest=\"data/x.dds\"\nfile=\"f\"\n[[file]]\ndest=\"data/x.dds\"\nfile=\"g\"\n").unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("two [[file]]"));
}

#[test]
fn m5_asset_section_parses_and_validates() {
    // a minimal valid new asset, hosted in vp
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[asset]]\nname=\"_t_mynew_a.x\"\ndir=\"train\\\\mynew\"\nhost=\"vp\"\nfile=\"assets/_t_mynew_a.x\"\n",
    )
    .unwrap();
    m.validate().unwrap();
    assert_eq!(m.asset.len(), 1);
    assert_eq!(m.asset[0].name, "_t_mynew_a.x");
    assert_eq!(m.asset[0].dir, "train\\mynew");
    assert_eq!(m.asset[0].host, "vp");
    assert!(!m.asset[0].override_vanilla, "default is false");
    // override_vanilla round-trips
    let ov = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[asset]]\nname=\"_t_van_a.x\"\ndir=\"train\\\\van\"\nhost=\"tp\"\nfile=\"f\"\noverride_vanilla=true\n",
    )
    .unwrap();
    ov.validate().unwrap();
    assert!(ov.asset[0].override_vanilla);

    // every family-A host classifies; a family-B/unknown host is refused
    for host in ["tp", "mcp", "ep", "cp"] {
        let m = parse(&format!(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nname=\"_t_a.x\"\ndir=\"d\"\nhost=\"{host}\"\nfile=\"f\"\n"
        ))
        .unwrap();
        m.validate().unwrap();
    }
    for host in ["i.dat", "mtentr0", "me3", "csvp", "bogus", "VP"] {
        let m = parse(&format!(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nname=\"_t_a.x\"\ndir=\"d\"\nhost=\"{host}\"\nfile=\"f\"\n"
        ))
        .unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("family-A pack"), "host {host}: {e}");
    }
    // duplicate asset names in one mod
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[asset]]\nname=\"_t_a.x\"\ndir=\"d\"\nhost=\"vp\"\nfile=\"f\"\n\
         [[asset]]\nname=\"_t_a.x\"\ndir=\"d\"\nhost=\"vp\"\nfile=\"g\"\n",
    )
    .unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("two [[asset]]"), "{e}");
    // empty name / empty file
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\nname=\" \"\ndir=\"d\"\nhost=\"vp\"\nfile=\"f\"\n").unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("nonempty plain name"));
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\nname=\"a.x\"\ndir=\"d\"\nhost=\"vp\"\nfile=\" \"\n").unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("no `file`"));
    // unknown field inside the section is refused (strict schema)
    assert!(parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[asset]]\nname=\"a.x\"\ndir=\"d\"\nhost=\"vp\"\nfile=\"f\"\nwidth=1\n"
    )
    .is_err());
}

#[test]
fn m5b_asset_folder_form_parses_and_validates() {
    // folder form: name/file absent, dir optional, host defaults to auto
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[asset]]\nfolder=\"assets\"\n",
    )
    .unwrap();
    m.validate().unwrap();
    assert_eq!(m.asset[0].folder.as_deref(), Some("assets"));
    assert_eq!(m.asset[0].host, HOST_AUTO, "folder form defaults host to auto");
    assert!(m.asset[0].name.is_empty() && m.asset[0].file.is_empty());

    // host = "auto" is allowed explicitly in the single-file form too,
    // and is the default when `host` is omitted
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[asset]]\nname=\"_t_a.x\"\ndir=\"train\\\\a\"\nhost=\"auto\"\nfile=\"f\"\n\
         [[asset]]\nname=\"_t_b.x\"\ndir=\"train\\\\b\"\nfile=\"f\"\n",
    )
    .unwrap();
    m.validate().unwrap();
    assert_eq!(m.asset[0].host, HOST_AUTO);
    assert_eq!(m.asset[1].host, HOST_AUTO, "omitted host defaults to auto");

    // exactly one form: name/file with folder is refused; neither is refused
    let both = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[asset]]\nfolder=\"assets\"\nname=\"_t_a.x\"\nfile=\"f\"\n",
    )
    .unwrap();
    assert!(both.validate().unwrap_err().to_string().contains("not allowed together"));
    let neither = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\ndir=\"d\"\n",
    )
    .unwrap();
    assert!(neither.validate().unwrap_err().to_string().contains("nonempty plain name"));
    // folder paths escaping the mod dir are refused
    for folder in ["/abs", "..\\\\x", "C:\\\\x"] {
        let m = parse(&format!(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\nfolder=\"{folder}\"\n"
        ))
        .unwrap();
        assert!(m.validate().is_err(), "folder {folder} must be refused");
    }
    // the removed empty-name message stays: a bare name is required in the
    // single-file form (folder mode is the way to avoid naming rows)
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\nname=\" \"\nhost=\"vp\"\nfile=\"f\"\n").unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("nonempty plain name"));
    // unknown field stays refused
    assert!(parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[asset]]\nfolder=\"assets\"\nrecurse=false\n"
    )
    .is_err());
}

#[test]
fn m6_loose_asset_section_parses_and_validates() {
    // the motivating form: a glob over a per-boot random temp path
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[loose_asset]]\nmatch=\"*.adk\\\\fe01.bin\"\nfile=\"bmf/fe01.bin\"\n",
    )
    .unwrap();
    m.validate().unwrap();
    assert_eq!(m.loose_asset.len(), 1);
    assert_eq!(m.loose_asset[0].match_, "*.adk\\fe01.bin");
    assert_eq!(m.loose_asset[0].file, "bmf/fe01.bin");
    // an exact game path works too (no `*`)
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[loose_asset]]\nmatch=\"data\\\\bmf\\\\fy01.bin\"\nfile=\"f\"\n",
    )
    .unwrap();
    m.validate().unwrap();
    assert_eq!(m.loose_asset[0].match_, "data\\bmf\\fy01.bin");

    // empty match / empty file
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[loose_asset]]\nmatch=\" \"\nfile=\"f\"\n").unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("nonempty path/glob"));
    let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[loose_asset]]\nmatch=\"*.bin\"\nfile=\" \"\n").unwrap();
    assert!(m.validate().unwrap_err().to_string().contains("no `file`"));

    // the payload must not escape the mod dir
    for file in ["/abs/x.bin", "C:\\\\x.bin", "a/../../x.bin", "..\\\\x.bin"] {
        let m = parse(&format!(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[loose_asset]]\nmatch=\"*.bin\"\nfile=\"{file}\"\n"
        ))
        .unwrap();
        assert!(m.validate().is_err(), "file {file} must be refused");
    }

    // duplicate patterns (case-insensitively) are refused within one mod
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[loose_asset]]\nmatch=\"*.ADK\\\\fe01.bin\"\nfile=\"a\"\n\
         [[loose_asset]]\nmatch=\"*.adk\\\\fe01.bin\"\nfile=\"b\"\n",
    )
    .unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("two [[loose_asset]] sections match"), "{e}");

    // an unknown field inside the section is refused (strict schema)
    assert!(parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[loose_asset]]\nmatch=\"*.bin\"\nfile=\"f\"\nkind=\"x\"\n"
    )
    .is_err());
    // `match` is the TOML key; the Rust field name is not accepted
    assert!(parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[loose_asset]]\nmatch_=\"*.bin\"\nfile=\"f\"\n"
    )
    .is_err());
}

#[test]
fn validation_rejects_the_expected_classes() {
    // raw patch without expect fails AT PARSE (the Raw target requires it)
    let raw_no_expect = GOOD.replace(
        "target = \"train_list_whitelist\"",
        "target = { rva = 0x6FC1C0 }",
    );
    assert!(parse(&raw_no_expect).is_err());
    // raw target with empty expect fails validation
    let raw_empty = r#"
id = "diag-table113"
name = "Diagnostic table113"
version = "1.0.0"
author = "t"

[[patch]]
target = { rva = 0x6FC1C0, expect = "" }
offset = 24
data = "88 00 00 00"
"#;
    let m = parse(raw_empty).unwrap();
    let raw_expect_err = m.validate().unwrap_err().to_string();
    assert!(raw_expect_err.contains("non-empty `expect`"), "{raw_expect_err}");
    // bad hex data
    let m = parse(&GOOD.replace("88 00 00 00", "zz")).unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("non-empty valid hex"), "{e}");
    // unknown handler
    let m = parse(&GOOD.replace("handler = \"train_list\"", "handler = \"trainlst\"")).unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("unknown handler"), "{e}");
    // train_list without list
    let m = parse(&GOOD.replace("list = \"liveries.txt\"", "")).unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("requires `list`"), "{e}");
    // nop with params
    let m = parse(&GOOD.replace("handler = \"train_list\"", "handler = \"nop\"")).unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("no parameters"), "{e}");
    // bad id
    let m = parse(&GOOD.replace("id = \"jp-liveries\"", "id = \"JP Liveries\"")).unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("slug"), "{e}");
    // self-conflict
    let m = parse(&GOOD.replace("\"stock-liveries\"", "\"jp-liveries\"")).unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("itself"), "{e}");
    // duplicate target within one mod
    let dup = GOOD.to_owned()
        + "\n[[patch]]\ntarget = \"train_list_whitelist\"\noffset = 24\ndata = \"01\"\nexpect = \"bb\"\n";
    let m = parse(&dup).unwrap();
    let e = m.validate().unwrap_err().to_string();
    assert!(e.contains("same target"), "{e}");
}

#[test]
fn raw_target_repr() {
    let m = parse(
        "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
         [[patch]]\ntarget = { rva = 0x1A13B0, expect = \"48,89\" }\ndata = \"c3\"\n",
    )
    .unwrap();
    m.validate().unwrap();
    assert_eq!(m.patch[0].target.key(0), "raw:0x1a13b0+0");
    assert!(m.patch[0].target.anchor().is_none());
}
