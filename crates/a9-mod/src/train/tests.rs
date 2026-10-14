//! `[[train]]` compiler tests — allocation, naming, the four derived rows, the
//! effect keys, and every refusal mode (all offline, no game).

use super::*;
use a9_formats::csv::Table;

fn sec(key: &str, name: &str) -> TrainSection {
    TrainSection {
        key: key.into(),
        name: name.into(),
        name_en: None,
        from: None,
        price: None,
        capacity: None,
        running_cost: None,
        cruise_speed: None,
        reverse_speed: None,
        fare: None,
        cars: None,
        model: None,
        livery: None,
    }
}

fn one(t: TrainSection) -> CompiledTrain {
    let (mut v, r) = compile("mymod", &[t], BASE_ID);
    assert!(r.is_empty(), "unexpected refusals: {r:?}");
    assert_eq!(v.len(), 1);
    v.pop().unwrap()
}

#[test]
fn allocation_and_naming() {
    let c = one(sec("sample", "サンプル"));
    assert_eq!(c.form_name, "FORM_MYMOD_SAMPLE");
    assert_eq!(c.token, "form_mymod_sample");
    assert_eq!(c.id, BASE_ID); // 421
    assert_eq!(c.donor, DEFAULT_FROM);
    assert_eq!(c.name_en, "サンプル"); // defaults to the JP name
    assert_eq!(c.icons.len(), 3);
    assert_eq!(c.icons[0].name, "form_mymod_sample_f.dds");
    assert_eq!(c.icons[1].name, "form_mymod_sample_s.dds");
    assert_eq!(c.icons[2].name, "form_mymod_sample_s_1.dds");
    assert_eq!(c.icons[0].file, "assets/form_mymod_sample_f.dds");
    // the second train of a mod gets the next id
    let (v, _) = compile("mymod", &[sec("a", "A"), sec("b", "B")], BASE_ID);
    assert_eq!(v[0].id, BASE_ID);
    assert_eq!(v[1].id, BASE_ID + 1);
    assert_eq!(v[1].form_name, "FORM_MYMOD_B");
}

#[test]
fn form_name_folds_non_alphanumerics() {
    assert_eq!(form_name("my-mod.v2", "kiha_32"), "FORM_MY_MOD_V2_KIHA_32");
}

#[test]
fn derived_train_form_row_is_exact() {
    let c = one(sec("sample", "サンプル"));
    let f = &c.rows.form;
    assert_eq!(f.len(), 29);
    assert_eq!(f[0], "FORM_MYMOD_SAMPLE");
    assert_eq!(f[1], "ON"); // 有効 — the invalidation trap
    assert_eq!(f[2], DEFAULT_TYPE);
    assert_eq!(f[3], DEFAULT_COMPANY);
    assert_eq!(f[4], "65"); // cruise default
    assert_eq!(f[5], ""); // reverse default: empty
    assert_eq!(f[6], DEFAULT_ACCEL);
    assert_eq!(f[7], "300"); // fare default
    assert_eq!(f[8], DEFAULT_SOUNDS[0]);
    assert_eq!(f[10], DEFAULT_SOUNDS[2]);
    assert_eq!(f[15], "1");
    assert_eq!(f[16], ""); // 特殊車両フラグ — the second trap
    assert_eq!(f[17], DEFAULT_DRIVING);
    assert_eq!(f[19], "サンプル");
    assert!(f[20..].iter().all(|s| s.is_empty()));
    // and the serialized payload round-trips to exactly these fields
    let back = Table::parse(&c.form_payload()).unwrap();
    assert_eq!(back.rows.len(), 1);
    assert_eq!(back.rows[0].fields, *f);
}

#[test]
fn derived_pattern_group_is_a_10_row_group_plus_separator() {
    let c = one(sec("sample", "サンプル"));
    let p = &c.rows.pattern;
    assert_eq!(p.len(), PATTERN_GROUP_ROWS + 1, "10 rows + separator");
    // head: the only keyed row
    assert_eq!(p[0][0], "FORM_MYMOD_SAMPLE");
    assert_eq!(p[0][1], "kh32");
    assert_eq!(p[0][2], "_t_kh32_a");
    assert_eq!(p[0][3], "106");
    assert_eq!(p[0][4], "26200");
    assert_eq!(p[0][5], "45000000");
    assert_eq!(p[0][6], "1"); // the 1-car consist template
    assert_eq!(p[0][16], "○");
    assert_eq!(p[0][17], "1");
    // continuation rows: NO key, only the group index
    for n in 2..=PATTERN_GROUP_ROWS {
        assert_eq!(p[n - 1][0], "", "continuation row {n} must have no key");
        assert_eq!(p[n - 1][17], n.to_string());
    }
    // the separator
    assert!(p[PATTERN_GROUP_ROWS].iter().all(|s| s.is_empty()));
    // the payload parses back to the same fields (the positional append's input)
    let back = Table::parse(&c.pattern_payload()).unwrap();
    assert_eq!(back.rows.len(), PATTERN_GROUP_ROWS + 1);
    assert_eq!(back.rows[0].fields, p[0]);
    assert_eq!(back.rows[1].fields, p[1]);
    assert_eq!(back.rows[1].key(), "", "continuation rows are keyless");
}

#[test]
fn derived_comment_and_custom_rows() {
    let c = one(sec("sample", "サンプル"));
    assert_eq!(c.rows.comment.len(), 11);
    assert_eq!(c.rows.comment[0], "FORM_MYMOD_SAMPLE");
    assert_eq!(c.rows.comment[1], "サンプル");
    assert_eq!(c.rows.comment[2], "サンプル"); // EN defaults to JP
    assert!(c.rows.comment[3..].iter().all(|s| s.is_empty()));

    assert_eq!(c.rows.custom.len(), 19);
    assert_eq!(c.rows.custom[0], "FORM_MYMOD_SAMPLE");
    assert_eq!(c.rows.custom[3], "1"); // パンタ不可
    assert_eq!(c.rows.custom[4], "0");
    assert_eq!(c.rows.custom[5], "0");
    assert_eq!(c.rows.custom[18], "サンプル");
}

#[test]
fn declarations_override_the_defaults() {
    let mut t = sec("sample", "サンプル");
    t.name_en = Some("Sample".into());
    t.price = Some(1_000);
    t.capacity = Some(10);
    t.running_cost = Some(20);
    t.cruise_speed = Some(120);
    t.reverse_speed = Some(30);
    t.fare = Some(9);
    t.from = Some("FORM_E1".into());
    t.model = Some(TrainModel {
        dir: "e1".into(),
        mesh: "_t_e1_a".into(),
    });
    let c = one(t);
    assert_eq!(c.donor, "FORM_E1");
    assert_eq!(c.rows.form[4], "120");
    assert_eq!(c.rows.form[5], "30");
    assert_eq!(c.rows.form[7], "9");
    assert_eq!(c.rows.pattern[0][1], "e1");
    assert_eq!(c.rows.pattern[0][2], "_t_e1_a");
    assert_eq!(c.rows.pattern[0][3], "10");
    assert_eq!(c.rows.pattern[0][4], "20");
    assert_eq!(c.rows.pattern[0][5], "1000");
    assert_eq!(c.rows.comment[2], "Sample");
    assert_eq!(c.cruise_speed, 120);
    assert_eq!(c.reverse_speed, Some(30));
    assert_eq!(c.fare, 9);
}

#[test]
fn effect_keys_are_the_documented_set() {
    let c = one(sec("sample", "サンプル"));
    let keys = c.effect_keys();
    for want in [
        "csv:train_form.csv:FORM_MYMOD_SAMPLE",
        "csv:train_pattern.csv:FORM_MYMOD_SAMPLE",
        "csv:train_comment.csv:FORM_MYMOD_SAMPLE",
        "csv:train_custom.csv:FORM_MYMOD_SAMPLE",
        "asset:form_mymod_sample_f.dds",
        "asset:form_mymod_sample_s.dds",
        "asset:form_mymod_sample_s_1.dds",
        "hook:train_list_filter",
        "train:FORM_MYMOD_SAMPLE",
    ] {
        assert!(keys.iter().any(|k| k == want), "missing key {want} in {keys:?}");
    }
    assert_eq!(keys.len(), 9);
}

#[test]
fn refusals_skip_only_the_bad_train() {
    // a duplicate key
    let (v, r) = compile("mymod", &[sec("a", "A"), sec("a", "B")], BASE_ID);
    assert_eq!(v.len(), 1);
    assert_eq!(r.len(), 1);
    assert!(r[0].contains("already used"), "{r:?}");

    // a bad key slug
    let (v, r) = compile("mymod", &[sec("Bad Key", "A")], BASE_ID);
    assert!(v.is_empty());
    assert!(r[0].contains("lowercase slug"), "{r:?}");

    // a bad model dir
    let mut t = sec("a", "A");
    t.model = Some(TrainModel {
        dir: "../evil".into(),
        mesh: "_t_x_a".into(),
    });
    let (v, r) = compile("mymod", &[t], BASE_ID);
    assert!(v.is_empty());
    assert!(r[0].contains("plain names"), "{r:?}");

    // cars != 1
    let mut t = sec("a", "A");
    t.cars = Some(4);
    let (v, r) = compile("mymod", &[t], BASE_ID);
    assert!(v.is_empty());
    assert!(r[0].contains("only 1 is supported"), "{r:?}");

    // an empty name
    let (v, r) = compile("mymod", &[sec("a", "  ")], BASE_ID);
    assert!(v.is_empty());
    assert!(r[0].contains("non-empty"), "{r:?}");

    // cruise over the engine cap
    let mut t = sec("a", "A");
    t.cruise_speed = Some(400);
    let (v, r) = compile("mymod", &[t], BASE_ID);
    assert!(v.is_empty());
    assert!(r[0].contains("360 cap"), "{r:?}");

    // a bad donor
    let mut t = sec("a", "A");
    t.from = Some("no spaces".into());
    let (v, r) = compile("mymod", &[t], BASE_ID);
    assert!(v.is_empty());
    assert!(r[0].contains("FORM name"), "{r:?}");
}

#[test]
fn range_exhaustion_refuses_loudly() {
    // the keep-bitmap ceiling: an id at or above FORMS_MAX can never be admitted
    let too_many = FORMS_MAX - BASE_ID; // exactly fills the range
    let trains: Vec<TrainSection> = (0..too_many)
        .map(|i| sec(&format!("t{i}"), &format!("T{i}")))
        .collect();
    let (v, r) = compile("mymod", &trains, BASE_ID);
    assert_eq!(v.len(), too_many);
    assert!(r.is_empty());
    assert_eq!(v.last().unwrap().id, FORMS_MAX - 1);
    // one more train overflows
    let mut trains2 = trains.clone();
    trains2.push(sec("overflow", "O"));
    let (v, r) = compile("mymod", &trains2, BASE_ID);
    assert_eq!(v.len(), too_many);
    assert_eq!(r.len(), 1);
    assert!(r[0].contains("keep-set width"), "{r:?}");
}

#[test]
fn compile_all_threads_ids_across_mods() {
    let mut a = Manifest {
        id: "a".into(),
        name: "A".into(),
        version: semver::Version::new(1, 0, 0),
        author: "x".into(),
        description: None,
        license: None,
        url: None,
        game_builds: Vec::new(),
        priority: 50,
        supplies: Vec::new(),
        requires: Vec::new(),
        conflicts: Vec::new(),
        patch: Vec::new(),
        hook: Vec::new(),
        pack: Vec::new(),
        csv: Vec::new(),
        asset: Vec::new(),
        loose_asset: Vec::new(),
        train: vec![sec("x", "X"), sec("y", "Y")],
    };
    let mut b = a.clone();
    b.id = "b".into();
    b.train = vec![sec("z", "Z")];
    a.train.truncate(1); // mod a: one train
    let (v, r) = compile_all(&[a, b], &[0, 1]);
    assert!(r.is_empty());
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].id, BASE_ID);
    assert_eq!(v[0].form_name, "FORM_A_X");
    assert_eq!(v[1].id, BASE_ID + 1);
    assert_eq!(v[1].form_name, "FORM_B_Z");
}
