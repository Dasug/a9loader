//! The car build: once an injected row exists, drive the game's OWN car-model
//! loader to give it real car data — the model objects, the model slots and the
//! formation block the CSV pipeline can never give the row.

use super::spec::{
    AccessorFn, MANAGER_RVA, MGR_ROWS, ROW_ACCEL, ROW_CAR_BLOCK, ROW_CAR_FLAGS, ROW_CARC0UNT,
    ROW_CONSIST_LEN, ROW_CRUISE, ROW_FARE, ROW_FIRST_CAR, ROW_LAST_CAR, ROW_MODE_ACCEL,
    ROW_MODE_HORN, ROW_MODEL_SLOT, ROW_PREVIEW_X, ROW_PREVIEW_Y, ROW_PREVIEW2D_X, ROW_PREVIEW2D_Y,
    ROW_REVERSE, ROW_SOUND_BRAKE, ROW_SOUND_DRIVE, ROW_SOUND_HORN, ROW_SOUND_START,
    ROW_SPECIAL_FLAG, ROW_STD_CAR, ROW_STRIDE, WRAP_CAPACITY, WRAP_PRICE, WRAP_RUNNING_COST,
};
use super::train::TrainState;
use crate::sys;
use core::ffi::c_void;

/// The game's own car-model loader, `train_model_fetch` 0x19B9F0:
/// `(wrapper, &mesh_path_cell, pattern_row, flag) -> u8` (success in AL). The
/// object it creates is stored into `wrapper[0]`.
type ModelFetchFn = unsafe extern "system" fn(*mut u8, *const *const u16, *const u8, u8) -> u8;
type ModelRegisterFn = unsafe extern "system" fn(*const u8, *const u8);
type ModelIndexFn = unsafe extern "system" fn(*const u8) -> u16;
/// The printf-style wide-string constructor the pipeline uses to compose the
/// mesh path (0x1400372F0, the same function the name lanes use):
/// `(dest, format, arg)`.
type CellCtorFn = unsafe extern "system" fn(*mut u8, *const u16, usize) -> *mut u8;
/// `(dest)` — constructs an empty string object at dest.
type RcStrCtorFn = unsafe extern "system" fn(*mut u8);
/// The car build: drive the game's own car-model loader once per declared car, so
/// an injected row gets the car data the CSV pipeline can never give it (that pass
/// resolves rows through the 417-entry FORM arrays and runs before our hook).
///
/// This is the SAME call the game's own pattern loop makes for an ordinary car
/// (`train_csv_pipeline_builder` 0x14019cb20..0x14019cbe9):
///
/// ```text
///   train_model_fetch(wrapper, &mesh_path_cell, pattern_row, flag)   // 0x19B9F0
///   if (wrapper[0]) { model_register(manager, wrapper); idx = model_index_alloc(manager); }
///   row->slot[car] = idx - 1;  row->carCount++;
/// ```
///
/// `wrapper` is the record the pipeline hands the loader:
/// One `train_form.csv` cell as a number (the vanilla cells are plain integers;
/// the loader never guesses at a value it cannot read).
fn cell_u16(cols: &[String], idx: usize) -> Option<u16> {
    cols.get(idx)?.trim().parse::<u16>().ok()
}

/// Apply a train's `train_form` row — the CSV columns in the vanilla layout — to
/// the cloned spec row. Column → field map from the RE agent's Ghidra pass
///:
///
/// | col | header | field |
/// |---|---|---|
/// | 4 | 巡航速度 | `wCruiseSpeed` +0x0c |
/// | 5 | 反転速度 | `wReverseSpeed` +0x0e |
/// | 6 | 加速 | `wAccel` +0x10 |
/// | 7 | 基本運賃 | `wBaseFare` +0x12 |
/// | 8..11 | 発進/駆動/警笛/ブレーキ音ID | +0x1c / +0x1e / +0x20 / +0x22 |
/// | 12..15 | preview x/y, 2D preview x/y | +0x24 / +0x26 / +0x28 / +0x2a |
/// | 16 | 特殊車両フラグ | `bSpecialFlag` +0x9e |
/// | 17 / 18 | 運転モード警笛 / 加減速 | +0x9f / +0xa0 |
///
/// The game resolves cols 2/3 (車両種別ID / 鉄道会社ID) and 8..11, 17, 18 through
/// name tables, so only a *numeric* cell is taken here; anything else keeps the
/// donor's value instead of writing a wrong id (fail open). Column 1 (有効) is
/// forced to valid: a row the loader materialises is always meant to be live, and
/// a mod disables a train through its own `enabled` flag, not through this cell.
pub(super) unsafe fn apply_form_row(row: *mut u8, cols: &[String]) {
    let set16 = |off: usize, v: Option<u16>| {
        if let Some(v) = v {
            unsafe { core::ptr::write_unaligned(row.add(off) as *mut u16, v) };
        }
    };
    let set8 = |off: usize, v: Option<u16>| {
        if let Some(v) = v {
            unsafe { *row.add(off) = v as u8 };
        }
    };
    set16(ROW_CRUISE, cell_u16(cols, 4));
    set16(ROW_REVERSE, cell_u16(cols, 5));
    set16(ROW_ACCEL, cell_u16(cols, 6));
    set16(ROW_FARE, cell_u16(cols, 7));
    set16(ROW_SOUND_START, cell_u16(cols, 8));
    set16(ROW_SOUND_DRIVE, cell_u16(cols, 9));
    set16(ROW_SOUND_HORN, cell_u16(cols, 10));
    set16(ROW_SOUND_BRAKE, cell_u16(cols, 11));
    set16(ROW_PREVIEW_X, cell_u16(cols, 12));
    set16(ROW_PREVIEW_Y, cell_u16(cols, 13));
    set16(ROW_PREVIEW2D_X, cell_u16(cols, 14));
    set16(ROW_PREVIEW2D_Y, cell_u16(cols, 15));
    set8(ROW_SPECIAL_FLAG, cell_u16(cols, 16));
    set8(ROW_MODE_HORN, cell_u16(cols, 17));
    set8(ROW_MODE_ACCEL, cell_u16(cols, 18));
    unsafe { *row = 1 }; // bValid
}

/// `{+0x00 obj, +0x08 capacity u32, +0x0c running cost u32, +0x10 price u64}` —
/// those three numbers are the pattern row's CSV columns 3/4/5, i.e. the same
/// per-car figures the shop's 定員積載 / 運行費用 / 購入価格 display aggregates
/// over the formation. `pattern_row` is the 17-cell row (key, dir, mesh, the three
/// numbers, ten block bytes, the standard flag); `flag` selects CModelCUTTrain over
/// CModelTrain and is 0 for every id a mod can allocate.
///
/// Cells are built exactly the way the pipeline builds them: `rcstr_ctor(dest)`
/// first, because both the empty-string constructor AND the printf-style
/// constructor 0x1400372F0 ASSIGN into an already-live A9_RcStr — calling either
/// on a zeroed buffer dereferences NULL (that was crash #1 and #2 of this build).
/// The format passed to the printf-style constructor is always a static `"%s"`, so
/// author text can never be read as a format string.
///
/// The row's 100-byte car/formation block at `+0x39` is deliberately NOT written
/// here: the game fills it from the group's *formation* columns, and this build logs
/// the clone's bytes instead so the next boot's evidence settles their meaning
/// (one theory read them as a 10x10 formation matrix, the donor's bytes disagree).
///
/// Fail-open: any missing anchor or a refused load leaves the row exactly as the
/// clone left it (the donor's car data) — the behaviour before this build.
/// Car-build scratch: the live bytes each buffer needs, plus a canary tail that is
/// checked after the game's loader returns.
const CAR_ROW_BYTES: usize = 0x88; // 17 pattern-row cells, 8 bytes each
/// The pattern group's car rows (one per car type / consist length).
const CAR_ROWS_MAX: usize = 10;
const CAR_WRAP_BYTES: usize = 0x38; // the pipeline's own wrapper local is `[rbp-0x38]`
const CAR_CELL_BYTES: usize = 8; // one A9_RcStr pointer slot (the mesh-path cell)
const CAR_CANARY: usize = 0x20;
const CAR_CANARY_FILL: u8 = 0xA5;

/// A leaked scratch buffer: zeroed for the live bytes, `CAR_CANARY_FILL` after them.
/// Leaked on purpose — the game's loader is handed pointers into these buffers, so a
/// game-side write past one of them must land in our own allocation instead of this
/// function's frame (an unexplained spurious loop iteration came from exactly that),
/// and nothing may dangle if the loader keeps a pointer past the call.
fn new_scratch(live: usize) -> &'static mut [u8] {
    let mut v = vec![0u8; live + CAR_CANARY].into_boxed_slice();
    for b in &mut v[live..] {
        *b = CAR_CANARY_FILL;
    }
    Box::leak(v)
}

/// True while `base[live..total]` still holds the untouched canary.
fn canary_ok(base: *const u8, live: usize, total: usize) -> bool {
    (live..total).all(|k| unsafe { *base.add(k) } == CAR_CANARY_FILL)
}
pub(super) unsafe fn build_cars(g: &TrainState) -> Result<usize, String> {
    unsafe {
        let (Some(fetch_addr), Some(register), Some(index_alloc), Some(rcstr)) = (
            g.model_fetch,
            g.model_register,
            g.model_index_alloc,
            g.rcstr_ctor,
        ) else {
            return Err(
                "the car-build helper anchors are not all verified (see the boot \
                        warnings)"
                    .into(),
            );
        };
        let manager = (g.base + MANAGER_RVA) as *const u8;
        let vec = (g.base + MANAGER_RVA + MGR_ROWS) as *mut u8;
        let accessor: AccessorFn = core::mem::transmute(g.accessor);
        let fetch: ModelFetchFn = core::mem::transmute(fetch_addr);
        let register_fn: ModelRegisterFn = core::mem::transmute(register);
        let index_fn: ModelIndexFn = core::mem::transmute(index_alloc);
        let cell_ctor: CellCtorFn = core::mem::transmute(g.name_ctor);
        let rcstr_ctor: RcStrCtorFn = core::mem::transmute(rcstr);

        let wide = |s: &str| -> Vec<u16> {
            let mut v: Vec<u16> = s.encode_utf16().collect();
            v.push(0);
            v
        };
        // the object a car is built from is freed through its vtable's first slot,
        // with `1` (scalar deleting destructor) — what the pipeline does on failure.
        unsafe fn destroy_car(obj: *mut u8) {
            unsafe {
                let vt = *(obj as *const *const usize);
                let dtor: unsafe extern "system" fn(*mut u8, u32) =
                    core::mem::transmute(*vt as *const ());
                dtor(obj, 1);
            }
        }

        let fmt = wide("%s");
        let mut built = 0usize;
        // The row list is immutable for the duration of the build, so the count is
        // taken ONCE and indexed with `get` (never a slice iterator whose bounds the
        // codegen may reload after the raw game calls). `intact` then re-checks the
        // list's *shape* — lengths only, no dereferences — around every raw game call:
        // anything that moves those fields is refused loudly instead of feeding the
        // loader a garbage row (which is what a spurious second iteration did).
        let nrows = g.rows.len();
        let shape = g
            .rows
            .first()
            .map(|(_, t)| (t.model_dir.len(), t.form_name.len(), t.model_mesh.len()));
        let intact = |step: &str| -> Result<(), String> {
            let now = g
                .rows
                .first()
                .map(|(_, t)| (t.model_dir.len(), t.form_name.len(), t.model_mesh.len()));
            if g.rows.len() == nrows && now == shape {
                Ok(())
            } else {
                Err(format!(
                    "the train list changed shape during the car build ({step}: {} rows now, \
                     {nrows} before) — refusing, the reserved row keeps the donor's car data",
                    g.rows.len()
                ))
            }
        };
        for i in 0..nrows {
            let Some((_, t)) = g.rows.get(i) else { break };
            // ---- the car types: the pattern group's modelled rows -------------
            //
            // The group's rows ARE the consist's car types: row 1 is the lead (its
            // ディレクトリ/モデル/定員積載/運行費用/購入価格 columns), further rows are more
            // modelled types, and each row's coupling columns (6..15) say which types
            // make up an N-car consist (row N = the trailing cars' type numbers, -1
            // terminated). AR4 is the reference: rows 1-2 are two modelled types, row 4
            // carries `1,2,2,-1`, and the shop's 4-car figures (708 / $20,160,000) are
            // 2x type-1 + 2x type-2.
            let cell = |r: &[String], i: usize| -> String {
                r.get(i).map(|s| s.trim().to_string()).unwrap_or_default()
            };
            // (group row index, directory, mesh, capacity, running cost, price)
            let mut types: Vec<(usize, String, String, u32, u64, u64)> = Vec::new();
            for (ri, r) in t.pattern_rows.iter().take(CAR_ROWS_MAX).enumerate() {
                let mesh = cell(r, 2);
                if mesh.is_empty() {
                    continue; // not a modelled type
                }
                types.push((
                    ri,
                    cell(r, 1),
                    mesh,
                    cell(r, 3).parse().unwrap_or(0),
                    cell(r, 4).parse().unwrap_or(0),
                    cell(r, 5).parse().unwrap_or(0),
                ));
            }
            if types.is_empty() {
                g.logger.warn(&format!(
                    "car build: train '{}' has no modelled car type in its pattern group \
                     (column 2 モデル) — the row keeps the donor's car data",
                    t.form_name
                ));
                continue;
            }
            // ---- one model per type -------------------------------------------
            let mut indices: Vec<u16> = Vec::with_capacity(types.len());
            for (ri, dir, mesh, capacity, running_cost, price) in &types {
                let path = format!("train\\{dir}\\{mesh}.x");
                let src = t.pattern_rows.get(*ri).cloned().unwrap_or_default();
                let mut cells: Vec<String> = vec![
                    t.form_name.clone(),
                    dir.clone(),
                    mesh.clone(),
                    capacity.to_string(),
                    running_cost.to_string(),
                    price.to_string(),
                ];
                for k in 6..16 {
                    cells.push(cell(&src, k)); // the row's coupling columns
                }
                // The cell the game's model loader parses as "this car is the standard
                // one" must be a NUMBER (the pipeline `crt_atoi`s pattern col16), so a
                // 標準 marker becomes 1 and anything else 0.
                let flag = cell(&src, 16);
                cells.push(if flag == "○" || flag == "1" {
                    "1".into()
                } else {
                    "0".into()
                });

                g.logger.debug(&format!(
                    "car build: constructing cells for '{}' (mesh {path})",
                    t.form_name
                ));
                let row = new_scratch(CAR_ROW_BYTES);
                let wrapper = new_scratch(CAR_WRAP_BYTES);
                let path_cell = new_scratch(CAR_CELL_BYTES);
                let mut sources: Vec<Vec<u16>> = Vec::with_capacity(cells.len() + 1);
                for (k, c) in cells.iter().enumerate() {
                    let w = wide(c);
                    let field = row.as_mut_ptr().add(k * 8);
                    rcstr_ctor(field);
                    let out = cell_ctor(field, fmt.as_ptr(), w.as_ptr() as usize);
                    if out.is_null() {
                        return Err(format!("the cell constructor refused cell {k} ({c:?})"));
                    }
                    sources.push(w);
                }
                intact("after the cell construction")?;
                let path_w = wide(&path);
                rcstr_ctor(path_cell.as_mut_ptr());
                let path_cell_ok = cell_ctor(
                    path_cell.as_mut_ptr(),
                    fmt.as_ptr(),
                    path_w.as_ptr() as usize,
                );
                sources.push(path_w);
                if path_cell_ok.is_null() {
                    return Err(format!(
                        "the cell constructor refused the mesh path {path:?}"
                    ));
                }
                // The loader's second argument is the MESH NAME cell, not a composed
                // path: a vanilla model object carries the directory and the bare mesh
                // name as separate strings (live dump of registry[3]:
                //   [+0x120]="FORM_283OA" [+0x128]="283oa" [+0x1b0]="283oa"
                //   [+0x1b8]="_t_283oa_b" [+0x1c8]="38600" [+0x1d0]="124000000"
                // ) while OUR objects carried none of them — which is why the renderer
                // had no mesh to draw and fell back to a placeholder. Passing the whole
                // `train\dir\mesh.x` path as the mesh name is the likely reason.
                let mesh_w = wide(&mesh);
                let mesh_cell = new_scratch(CAR_CELL_BYTES);
                rcstr_ctor(mesh_cell.as_mut_ptr());
                let mesh_cell_ok = cell_ctor(
                    mesh_cell.as_mut_ptr(),
                    fmt.as_ptr(),
                    mesh_w.as_ptr() as usize,
                );
                sources.push(mesh_w);
                if mesh_cell_ok.is_null() {
                    return Err(format!(
                        "the cell constructor refused the mesh name {mesh:?}"
                    ));
                }
                // the wrapper is ours: the numbers the shop publishes for this car
                *(wrapper.as_mut_ptr().add(WRAP_CAPACITY) as *mut u32) = *capacity;
                *(wrapper.as_mut_ptr().add(WRAP_RUNNING_COST) as *mut u32) = *running_cost as u32;
                *(wrapper.as_mut_ptr().add(WRAP_PRICE) as *mut u64) = *price;
                g.logger.debug(&format!(
                    "car build: calling the game's car-model loader ({path}; capacity {}, \
                     running cost {}, price {})",
                    capacity, running_cost, price
                ));
                let ok = fetch(
                    wrapper.as_mut_ptr(),
                    path_cell.as_ptr() as *const *const u16,
                    row.as_ptr(),
                    0,
                );
                intact("after the model loader returned")?;
                let obj = *(wrapper.as_ptr() as *const *mut u8);
                g.logger.debug(&format!(
                    "car build: the loader returned {ok} (object {obj:p}) for {path}"
                ));
                if ok == 0 {
                    if !obj.is_null() {
                        destroy_car(obj);
                    }
                    g.logger.warn(&format!(
                        "car build: the game's loader refused {} (train '{}') — the row keeps \
                         the donor's car data",
                        path, t.form_name
                    ));
                    continue;
                }
                for (what, base, live) in [
                    ("pattern row", row.as_ptr(), CAR_ROW_BYTES),
                    ("wrapper", wrapper.as_ptr(), CAR_WRAP_BYTES),
                    ("mesh-path cell", path_cell.as_ptr(), CAR_CELL_BYTES),
                ] {
                    if !canary_ok(base, live, live + CAR_CANARY) {
                        g.logger.warn(&format!(
                            "car build: the game's car-model loader wrote past our {what} \
                             buffer for {path} — the scratch buffers are loader-owned, so \
                             nothing else is affected"
                        ));
                    }
                }
                register_fn(manager, wrapper.as_ptr());
                let idx = index_fn(manager);
                intact("after registering the car model")?;
                g.logger.debug(&format!(
                    "car build: registered the car and took model index {idx} for {path}"
                ));
                indices.push(idx.wrapping_sub(1));
            }
            if indices.is_empty() {
                g.logger.warn(&format!(
                    "car build: none of train '{}''s car models could be loaded — the row \
                     keeps the donor's car data",
                    t.form_name
                ));
                continue;
            }
            // ---- the row: model slots, per-car block, counts -------------------
            let row_ptr = accessor(vec, t.id);
            if row_ptr.is_null() || !sys::is_readable(row_ptr as *const c_void, ROW_STRIDE) {
                return Err(format!(
                    "reserved row {} is unreadable for the car build",
                    t.id
                ));
            }
            // DIAGNOSTIC (boot-time evidence): the reserved row's raw bytes. The probe
            // runs BEFORE the injection, where these rows do not exist yet, so the dump
            // lives here instead.
            {
                let mut hex = String::new();
                for k in 0..0x140 {
                    hex.push_str(&format!("{:02x}", *row_ptr.add(k)));
                }
                g.logger
                    .debug(&format!("train row bytes: reserved({}) {}", t.id, hex));
            }
            // The donor's own slots settle what this array MEANS (live row dump
            // 2026-10-15): AR4's +0x2c reads 206, 207 — and the shipped FORM list's
            // index 206 is FORM_123, 207 is FORM_125. So the slots hold the FORMS of
            // the consist's cars, not model indices: `train_row_car_wiring` looks each
            // up as a spec row and takes `aModelIdx[sub - 1]` from it. Our cars' models
            // belong to OUR row, so the slot must name OUR form (which is what makes
            // the lookup resolve back to the model slots below).
            // Unused model slots are ZERO, never 0xffff. Live row dump 2026-10-16 of
            // five vanilla rows: FORM_205 `6 7 8 77 95 68`, AR4 `206 207 0 0 0 0`,
            // 183NREX `325 326 327 328 329 0`, kh32 `399 0 0 0 0 0`, FORM_285
            // `421 422 423 0 0 0` — the tail is always zero, and ours were 0xffff,
            // which is not a model index at all (the registry has ~900 entries).
            // That is what made the renderer decline our models.
            for k in 0..6 {
                *(row_ptr.add(ROW_MODEL_SLOT + k * 2) as *mut u16) =
                    indices.get(k).copied().unwrap_or(0);
            }
            // NOT written here: the per-car sub-index that `train_row_car_wiring` feeds
            // to `aModelIdx[sub - 1]`. The live row dump (2026-10-15) shows +0x2c is the
            // flat u16[6] MODEL array — the donor reads `ce 00 cf 00 …` = models 206/207
            // — while the 0x18-stride consist array lives in the TRAIN OBJECT the
            // builder creates, so the sub-index is the builder's business. Writing it at
            // +0x2e + k*0x18 lands on the model array's second slot and corrupts the
            // panel's figures (816 -> 564). Find the builder's source for it instead.
            // The per-car block (+0x39, ten bytes per group row) is what the game's
            // consist builder reads to build a train object's car entries: the first byte
            // of a row's ten is "this car exists", the rest is its coupling data, filled
            // by the pipeline from the pattern row's columns 6..15. A cloned row inherits
            // the donor's block, and our clone was taken mid-pipeline while the donor's
            // was still zero — live 2026-10-15 that made the built car entry carry slot 0
            // and selecting the train crashed in `train_row_car_wiring` (0x1401aba4e:
            // `aModelIdx[slot - 1]` with slot 0 walks a wild index).
            // Every consist length needs a VALID record. A record's leading byte is
            // that car's SUB-INDEX: `train_row_car_wiring` does
            // `spec_row.aModelIdx[sub - 1]`, so a 0 (an untouched cell, or the donor's
            // zero block) or a bare terminator makes it walk a wild index — live
            // 2026-10-15, that crashed the game on the 1-car option and again on the
            // 2-car one, once with each wrong fill I tried.
            //
            // The vanilla convention is visible in AR4's own row: its 4-car record
            // reads `01 02 02 ff` — the cars' sub-indices, with `ff` meaning "the
            // default type" (which is why the shop's 4-car figures are 2x type-1 +
            // 2x type-2: 1,2,2,default). So an unspecified cell is `ff`, never 0, and
            // a length the author does not describe is a consist of default-typed cars.
            let (mut first, mut last, mut std_car, mut present, mut consist) =
                (0u16, 0u16, 0u16, 0u8, 0u16);
            for (i, r) in t.pattern_rows.iter().take(CAR_ROWS_MAX).enumerate() {
                // A group row's coupling columns (6..15) are what mark it as an
                // offered consist length; a row with only a group index in col 17 is
                // not. AR4 offers 4..10 because exactly those rows carry a coupling,
                // and its +0x14/+0x16 read 4 and 10 — so first/last are those lengths.
                let has_coupling = r.iter().skip(6).take(10).any(|c| !c.trim().is_empty());
                if !has_coupling {
                    continue;
                }
                let pos = (i + 1) as u16;
                if first == 0 {
                    first = pos;
                }
                last = pos;
                present = present.saturating_add(1);
                // 標準 marks the consist the shop offers first
                let flag = r.get(16).map(|c| c.trim()).unwrap_or("");
                if flag == "○" || flag == "1" {
                    std_car = pos;
                    consist = pos;
                }
            }
            if consist == 0 {
                consist = if last > 0 { last } else { 1 };
            }
            if std_car == 0 {
                std_car = last;
            }
            // Back to the fill that WORKS live (the user's U-Shape evidence disproved
            // my "records only from the standard consist upwards" reading: a train
            // whose default is 5 also offers 4, so the offered set is the records that
            // carry data, not everything from the default up).
            //
            // Every cell reads `ff` — the default-type sentinel — so every offered
            // length builds a consist of default-typed cars and none of them makes
            // `train_row_car_wiring` walk `aModelIdx[sub - 1]` with sub = 0.
            //
            // STILL OPEN: the currently selected length is missing from the selector's
            // list (every shipped train shows its own). This needs the editor's own
            // list-building code read, not another fill experiment.
            // The block is `u8[10][10]` at +0x39: record (L-1) is the coupling for a
            // consist of L cars, and the values are the author's own — verbatim from
            // the pattern row's columns 6..15, one per car, 1-based model sub-indices
            // with a NEGATIVE value meaning "that car runs reversed" (AR4's 4-car row
            // reads `1 2 2 -1` and its live record is `01 02 02 ff`; 183NREX's 5-car
            // `1 4 5 3 -2` is `01 04 05 03 fe`). Live rows for lengths the author does
            // not describe are all zeros — and the game handles that: it falls back to
            // the models the group's earlier rows introduced (AR4's 1..3 are zero and
            // its 1/2/3-car options work). Substituting 0xff for an empty cell is what
            // crashed the 1-car selection: 255 is a car index far past the model array.
            for i in 0..CAR_ROWS_MAX {
                let rec = row_ptr.add(ROW_CAR_BLOCK + i * 10);
                let src = t.pattern_rows.get(i);
                for j in 0..10 {
                    let v = src
                        .and_then(|r| r.get(6 + j))
                        .map(|s| s.trim())
                        .and_then(|s| s.parse::<i8>().ok())
                        .unwrap_or(0);
                    *rec.add(j) = v as u8;
                }
            }
            *row_ptr.add(ROW_CARC0UNT) = indices.len().min(255) as u8;
            *(row_ptr.add(ROW_CONSIST_LEN) as *mut u16) = consist;
            // +0x121 is NOT the consist length: a live dump of six shipped rows (enabled,
            // disabled and the overflow row alike) reads a constant 99 there, so writing
            // our length into it only makes the row differ from every shipped one. The
            // offered lengths come from the per-car block's records, and the selected
            // length from +0x1a; both are written above/below.
            *(row_ptr.add(ROW_FIRST_CAR) as *mut u16) = first;
            *(row_ptr.add(ROW_LAST_CAR) as *mut u16) = last;
            *(row_ptr.add(ROW_STD_CAR) as *mut u16) = std_car;
            *row_ptr.add(ROW_CAR_FLAGS) = present;
            g.logger.line(&format!(
                "car build: row {} now has {} car(s) (model indices {:?}), offered {}..{}, consist {} \
                 (standard car {})",
                t.id,
                indices.len(),
                indices,
                first,
                last,
                consist,
                std_car
            ));
            built += 1;
        }
        Ok(built)
    }
}
