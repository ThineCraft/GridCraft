//! Sequences of shared commands for the co-authoring proofs (plan G1), and their JSON form.
//!
//! A sequence is `{seed, now, steps: [{id, params}]}`: the commands two co-authors' engines both
//! apply, in order, starting from a new workbook ("Sheet1"). Every step names its sheet and its
//! cells or lines explicitly; none relies on the selection or the active sheet. `now` is the fixed
//! clock (a date serial) and `seed` also seeds RAND, so volatile functions repeat.
//!
//! The generator emits only the candidate shared commands of the co-authoring design, in
//! [`SHARED_COMMANDS`]. Commands that read the selection or the active sheet even when given
//! explicit parameters are left out; `tests/fixtures/coauthor/selection_readers.md` lists them.
//!
//! This file is shared by the native replay (`tests/it/coauthor_replay.rs`) and is kept free of
//! file and clock access, so the browser-engine runs can use it too.

#![allow(dead_code)]

use std::sync::Arc;

use gridcraft_engine::core::{CellRef, Value};
use gridcraft_engine::model::{Cell, Formula, Sheet, Workbook};
use gridcraft_engine::{DocState, Session, Trust};
use serde_json::{Map, Value as Json, json};

/// The commands a sequence may contain: cell content, the formats a co-author can apply, row and
/// column inserts and deletes, and sheet add, rename and delete. Each with explicit targets.
pub const SHARED_COMMANDS: &[&str] = &[
    "cell.set",
    "edit.clearContents",
    "home.numberFormat",
    "home.bold",
    "home.italic",
    "home.fontName",
    "home.fontSize",
    "home.fontColor",
    "home.fillColor",
    "home.borders",
    "home.alignLeft",
    "home.alignCenter",
    "home.alignRight",
    "home.alignTop",
    "home.alignMiddle",
    "home.alignBottom",
    "home.wrapText",
    "home.insertRows",
    "home.deleteRows",
    "home.insertColumns",
    "home.deleteColumns",
    "home.insertSheet",
    "sheet.rename",
    "home.deleteSheet",
];

/// Commands that change rows, columns or sheets (about 15 % of generated steps).
pub const STRUCTURAL_COMMANDS: &[&str] =
    &["home.insertRows", "home.deleteRows", "home.insertColumns", "home.deleteColumns", "home.insertSheet", "sheet.rename", "home.deleteSheet"];

/// The 40 common functions formulas are drawn from.
pub const FUNCTIONS: [&str; 40] = [
    "SUM",
    "AVERAGE",
    "MIN",
    "MAX",
    "COUNT",
    "COUNTA",
    "PRODUCT",
    "MEDIAN",
    "SUMPRODUCT",
    "COUNTIF",
    "SUMIF",
    "AVERAGEIF",
    "IF",
    "IFERROR",
    "AND",
    "OR",
    "NOT",
    "ROUND",
    "ROUNDUP",
    "ROUNDDOWN",
    "ABS",
    "INT",
    "MOD",
    "POWER",
    "SQRT",
    "LEN",
    "LEFT",
    "RIGHT",
    "MID",
    "UPPER",
    "LOWER",
    "TRIM",
    "CONCAT",
    "TEXT",
    "VLOOKUP",
    "HLOOKUP",
    "INDEX",
    "MATCH",
    "XLOOKUP",
    "DATE",
];

/// The volatile functions, used now and then in generated formulas.
pub const VOLATILE: [&str; 4] = ["NOW()", "TODAY()", "RAND()", "RANDBETWEEN(1,6)"];

/// Rows and columns content is generated in (200 rows by 30 columns, A1:AD200).
pub const ROWS: u32 = 200;
pub const COLS: u32 = 30;
/// Most sheets a generated sequence keeps (and the fewest is one).
pub const MAX_SHEETS: usize = 6;

/// 2026-10-07 12:00 as a date serial: the clock of the hand-written sequences.
pub const DEFAULT_NOW: f64 = 46_302.5;

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub id: String,
    pub params: Json,
}

impl Step {
    pub fn new(id: &str, params: Json) -> Step {
        Step { id: id.to_string(), params }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sequence {
    pub seed: u64,
    /// The fixed clock, as a date serial.
    pub now: f64,
    pub steps: Vec<Step>,
}

impl Sequence {
    pub fn to_json(&self) -> Json {
        let steps: Vec<Json> = self.steps.iter().map(|s| json!({"id": s.id, "params": s.params})).collect();
        json!({"seed": self.seed, "now": self.now, "steps": steps})
    }

    /// The fixture text: pretty JSON (keys sorted, as serde_json's map keeps them) and a final
    /// line break.
    pub fn to_text(&self) -> String {
        let mut t = serde_json::to_string_pretty(&self.to_json()).unwrap_or_default();
        t.push('\n');
        t
    }

    pub fn from_json(v: &Json) -> Result<Sequence, String> {
        let o = v.as_object().ok_or("a sequence is a JSON object")?;
        if let Some(k) = o.keys().find(|k| !matches!(k.as_str(), "seed" | "now" | "steps")) {
            return Err(format!("unknown key `{k}`"));
        }
        let seed = o.get("seed").and_then(Json::as_u64).ok_or("`seed` must be an unsigned integer")?;
        let now = o.get("now").and_then(Json::as_f64).ok_or("`now` must be a number")?;
        let list = o.get("steps").and_then(Json::as_array).ok_or("`steps` must be an array")?;
        let mut steps = Vec::with_capacity(list.len());
        for (i, s) in list.iter().enumerate() {
            let so = s.as_object().ok_or(format!("step {i} is not an object"))?;
            if so.len() != 2 {
                return Err(format!("step {i} must have exactly `id` and `params`"));
            }
            let id = so.get("id").and_then(Json::as_str).ok_or(format!("step {i}: `id` must be a string"))?;
            let params = so.get("params").filter(|p| p.is_object()).ok_or(format!("step {i}: `params` must be an object"))?;
            steps.push(Step::new(id, params.clone()));
        }
        Ok(Sequence { seed, now, steps })
    }

    pub fn from_text(t: &str) -> Result<Sequence, String> {
        let v: Json = serde_json::from_str(t).map_err(|e| e.to_string())?;
        Sequence::from_json(&v)
    }
}

/// A session holding a new workbook, with the sequence's fixed clock and RAND seed.
pub fn fresh_session(seq: &Sequence) -> Session {
    let mut d = DocState::new(Workbook::new(), None, "Book1".into(), Trust::Local);
    d.calc.fixed_now = Some(seq.now);
    d.calc.set_rand_seed(seq.seed);
    let mut s = Session::new();
    s.add_document(d);
    s
}

/// Applies one step; the outcome is the command's result or its error message.
pub fn apply(s: &mut Session, step: &Step) -> Result<Json, String> {
    let r = s.execute(&step.id, step.params.clone()).map_err(|e| e.to_string());
    s.take_ui_requests();
    r
}

/// Workbook sizes measured by the rollback-and-re-run proof.
pub const TIMING_CELL_COUNTS: [usize; 3] = [10_000, 100_000, 1_000_000];
/// Pending-command counts measured at each workbook size.
pub const TIMING_PENDING_COUNTS: [usize; 3] = [1, 10, 50];
/// Measured repetitions after the excluded warm-up.
pub const TIMING_REPETITIONS: usize = 30;

/// Percentiles reported by one timing case, in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct TimingStats {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

/// A deterministic two-sheet workbook with exactly `cells` stored cells and one formula in five.
/// The proof sizes divide evenly across two 100-column sheets.
pub fn timing_workbook(cells: usize) -> Result<Workbook, String> {
    const SHEETS: usize = 2;
    const COLS: usize = 100;
    let row_width = SHEETS * COLS;
    if cells == 0 || !cells.is_multiple_of(row_width) {
        return Err(format!("timing workbook cell count must be a positive multiple of {row_width}, got {cells}"));
    }
    let rows = cells / row_width;
    let rows = u32::try_from(rows).map_err(|_| format!("timing workbook has too many rows: {rows}"))?;
    let mut wb = Workbook::new();
    wb.sheets.push(Arc::new(Sheet::new("Data")));
    for sheet_index in 0..SHEETS {
        let other = if sheet_index == 0 { "Data" } else { "Sheet1" };
        let sheet = wb.sheet_mut(sheet_index).ok_or_else(|| format!("timing workbook is missing sheet {sheet_index}"))?;
        for row in 0..rows {
            for col in 0..COLS as u32 {
                let ordinal = sheet_index * rows as usize * COLS + row as usize * COLS + col as usize;
                let cell = if ordinal.is_multiple_of(5) {
                    let formula = if row == 0 && col == 0 { "=SUM(B:B)".to_string() } else { format!("={other}!B{}+1", row + 1) };
                    Cell::formula(Formula::new(&formula))
                } else {
                    Cell::value(Value::Number((ordinal % 10_000) as f64))
                };
                sheet.set_cell(CellRef::new(row, col), cell);
            }
        }
    }
    Ok(wb)
}

/// The agreed document cloned by every timing repetition.
pub fn timing_document(cells: usize) -> Result<DocState, String> {
    let mut document = DocState::new(timing_workbook(cells)?, None, format!("Timing-{cells}"), Trust::Local);
    document.calc.fixed_now = Some(DEFAULT_NOW);
    document.calc.set_rand_seed(0xC0A7_7402_0000_0006);
    let wb = Arc::make_mut(&mut document.wb);
    document.calc.recalc_all(wb);
    Ok(document)
}

/// Fifty deterministic shared commands with explicit targets. Seven are structural (14 %), close
/// to the 15 % mix in co-author replay sequences; insert/delete pairs keep later targets stable.
pub fn timing_pending_commands() -> Vec<Step> {
    (0..50)
        .map(|index| {
            let row = 2 + index;
            match index % 7 {
                0 => Step::new("cell.set", json!({"sheet": "Sheet1", "cell": format!("B{row}"), "input": (10_000 + index).to_string()})),
                1 => Step::new("home.bold", json!({"sheet": "Sheet1", "range": format!("C{row}:D{row}"), "on": true})),
                2 => Step::new("home.fillColor", json!({"sheet": "Data", "range": format!("E{row}:F{row}"), "color": "#C6EFCE"})),
                3 => Step::new("home.numberFormat", json!({"sheet": "Sheet1", "range": format!("G{row}:H{row}"), "code": "0.00"})),
                4 => Step::new("home.alignRight", json!({"sheet": "Data", "range": format!("I{row}:J{row}"), "on": true})),
                5 => Step::new("home.borders", json!({"sheet": "Sheet1", "range": format!("K{row}:L{row}"), "preset": "bottom"})),
                _ => match index / 7 % 4 {
                    0 => Step::new("home.insertRows", json!({"sheet": "Sheet1", "rows": "10:10"})),
                    1 => Step::new("home.deleteRows", json!({"sheet": "Sheet1", "rows": "10:10"})),
                    2 => Step::new("home.insertColumns", json!({"sheet": "Data", "cols": "M:M"})),
                    _ => Step::new("home.deleteColumns", json!({"sheet": "Data", "cols": "M:M"})),
                },
            }
        })
        .collect()
}

/// Clone the agreed state, apply pending commands, then rebuild and recalculate everything.
/// Fingerprinting and validation deliberately happen outside callers' timed sections.
pub fn rerun_pending(agreed: &DocState, steps: &[Step]) -> Result<Session, String> {
    let mut session = Session::new();
    session.add_document(agreed.clone());
    for step in steps {
        apply(&mut session, step)?;
    }
    let document = session.doc_mut().map_err(|error| error.to_string())?;
    let wb = Arc::make_mut(&mut document.wb);
    document.calc.recalc_all(wb);
    Ok(session)
}

/// Nearest-rank p50 and p95 plus the maximum from a non-empty set of samples.
pub fn timing_stats(mut samples_ms: Vec<f64>) -> Result<TimingStats, String> {
    if samples_ms.is_empty() || samples_ms.iter().any(|sample| !sample.is_finite() || *sample < 0.0) {
        return Err("timing samples must be non-empty, finite and non-negative".into());
    }
    samples_ms.sort_by(f64::total_cmp);
    let rank = |percent: usize| (samples_ms.len() * percent).div_ceil(100).saturating_sub(1);
    Ok(TimingStats { p50_ms: samples_ms[rank(50)], p95_ms: samples_ms[rank(95)], max_ms: samples_ms[samples_ms.len() - 1] })
}

// ---------------------------------------------------------------- generator

/// SplitMix64: small, deterministic, the same on every platform.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0xC0A7_7402_5EED_0001)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    /// Inclusive range.
    pub fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1).max(1) as u64) as i64
    }
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

/// Sheet names a sequence adds or renames to: spaces, an apostrophe, a digit-only name, a
/// non-ASCII name and one that looks like a cell all need quoting in formulas.
const NAMES: [&str; 12] = ["Data", "Q1 Plan", "Bob's", "Sales-2", "Summary", "Übersicht", "2026", "A B", "Notes", "Ref", "AB12", "Totals"];

/// Column letters (A, …, Z, AA, …).
pub fn letters(col: u32) -> String {
    gridcraft_engine::core::col_to_letters(col)
}

/// A sheet name as a formula writes it: quoted unless it is plain letters.
pub fn sheet_prefix(name: &str) -> String {
    if name.len() > 1 && name.chars().all(|c| c.is_ascii_alphabetic()) { format!("{name}!") } else { format!("'{}'!", name.replace('\'', "''")) }
}

/// Generates `steps` random shared commands (after the steps that add the starting sheets).
pub fn generate(seed: u64, steps: usize) -> Sequence {
    let mut g = Gen { rng: Rng::new(seed), sheets: vec!["Sheet1".to_string()], renamed: 0, written: Vec::new() };
    let now = g.rng.int(36_526, 54_789) as f64 + g.rng.int(0, 1439) as f64 / 1440.0;
    let mut out = Vec::new();
    // 1 to 6 sheets to start with.
    let start = 1 + g.rng.below(MAX_SHEETS as u64) as usize;
    while g.sheets.len() < start {
        out.push(g.add_sheet());
    }
    for _ in 0..steps {
        out.push(g.step());
    }
    Sequence { seed, now, steps: out }
}

struct Gen {
    rng: Rng,
    /// The sheets as the generator expects them after the steps so far.
    sheets: Vec<String>,
    /// Names handed out, for unique fallbacks once the list is used up.
    renamed: u32,
    /// Cells the sequence has written (sheet, row, column), as entered. Row and column inserts
    /// and deletes are not followed: the list only steers later steps towards filled areas.
    written: Vec<(String, u32, u32)>,
}

impl Gen {
    fn sheet(&mut self) -> String {
        self.rng.pick(&self.sheets).clone()
    }

    fn fresh_name(&mut self) -> String {
        let taken = |n: &str, sheets: &[String]| sheets.iter().any(|s| s.to_lowercase() == n.to_lowercase());
        for _ in 0..4 {
            let n = self.rng.pick(&NAMES).to_string();
            if !taken(&n, &self.sheets) {
                return n;
            }
        }
        loop {
            self.renamed += 1;
            let n = format!("{} {}", self.rng.pick(&NAMES), self.renamed);
            if !taken(&n, &self.sheets) {
                return n;
            }
        }
    }

    fn add_sheet(&mut self) -> Step {
        let name = self.fresh_name();
        let before = self.rng.below(self.sheets.len() as u64 + 1) as usize;
        self.sheets.insert(before, name.clone());
        Step::new("home.insertSheet", json!({"name": name, "before": before}))
    }

    fn step(&mut self) -> Step {
        let roll = self.rng.below(100);
        match roll {
            0..35 => self.cell_set(false),
            35..57 => self.cell_set(true),
            57..62 => self.with_range("edit.clearContents", Map::new(), false),
            62..67 => self.number_format(),
            67..73 => self.font(),
            73..77 => {
                let color = self.rng.pick(&[json!("#FFFF00"), json!("#C6EFCE"), json!("none"), json!({"theme": 5, "tint": 0.4})]).clone();
                self.with_range("home.fillColor", obj(&[("color", color)]), true)
            }
            77..81 => self.borders(),
            81..85 => self.alignment(),
            _ => self.structural(),
        }
    }

    /// A step on a range of a random sheet, `extra` merged into its params. `lines` allows whole
    /// rows or columns now and then.
    fn with_range(&mut self, id: &str, mut extra: Map<String, Json>, lines: bool) -> Step {
        let sheet = self.sheet();
        let range = if lines && self.rng.chance(8) { self.lines_range() } else { self.range(&sheet) };
        extra.insert("sheet".into(), json!(sheet));
        extra.insert("range".into(), json!(range));
        Step::new(id, Json::Object(extra))
    }

    /// A cell of `sheet` (row, column), zero-based: mostly next to or on a cell already written
    /// there, else mostly in the block A1:J40, sometimes anywhere in A1:AD200. So formulas read
    /// filled cells and clears and formats land on content, with some blanks left.
    fn spot(&mut self, sheet: &str) -> (u32, u32) {
        let near: Vec<(u32, u32)> = self.written.iter().filter(|(s, _, _)| s == sheet).map(|(_, r, c)| (*r, *c)).collect();
        if !near.is_empty() && self.rng.chance(75) {
            let (r, c) = *self.rng.pick(&near);
            let dr = self.rng.int(-2, 2);
            let dc = self.rng.int(-1, 1);
            return ((r as i64 + dr).clamp(0, ROWS as i64 - 1) as u32, (c as i64 + dc).clamp(0, COLS as i64 - 1) as u32);
        }
        if self.rng.chance(70) {
            (self.rng.below(40) as u32, self.rng.below(10) as u32)
        } else {
            (self.rng.below(ROWS as u64) as u32, self.rng.below(COLS as u64) as u32)
        }
    }

    fn cell(&mut self, sheet: &str) -> String {
        let (r, c) = self.spot(sheet);
        format!("{}{}", letters(c), r + 1)
    }

    /// A small block of `sheet`, mostly, starting at a [`spot`](Self::spot).
    fn range(&mut self, sheet: &str) -> String {
        let (r0, c0) = self.spot(sheet);
        let (h, w) = if self.rng.chance(10) {
            (self.rng.int(1, 40) as u32, self.rng.int(1, 10) as u32)
        } else {
            (self.rng.int(1, 8) as u32, self.rng.int(1, 4) as u32)
        };
        let r1 = (r0 + h - 1).min(ROWS - 1);
        let c1 = (c0 + w - 1).min(COLS - 1);
        if r0 == r1 && c0 == c1 { format!("{}{}", letters(c0), r0 + 1) } else { format!("{}{}:{}{}", letters(c0), r0 + 1, letters(c1), r1 + 1) }
    }

    /// Whole rows (`5:6`) or whole columns (`C:D`).
    fn lines_range(&mut self) -> String {
        if self.rng.chance(50) {
            let a = 1 + self.rng.below(ROWS as u64);
            format!("{a}:{}", a + self.rng.below(3))
        } else {
            let a = self.rng.below(COLS as u64) as u32;
            format!("{}:{}", letters(a), letters((a + self.rng.below(2) as u32).min(COLS - 1)))
        }
    }

    fn cell_set(&mut self, formula: bool) -> Step {
        let sheet = self.sheet();
        let (r, c) = self.spot(&sheet);
        self.written.push((sheet.clone(), r, c));
        let cell = format!("{}{}", letters(c), r + 1);
        let input = if formula { self.formula(&sheet) } else { self.literal() };
        Step::new("cell.set", json!({"sheet": sheet, "cell": cell, "input": input}))
    }

    fn literal(&mut self) -> Json {
        match self.rng.below(100) {
            0..30 => json!(self.rng.int(-1000, 1000).to_string()),
            30..45 => json!(format!("{}.{:02}", self.rng.int(-500, 500), self.rng.below(100))),
            45..50 => json!(*self.rng.pick(&["1E+300", "-2.5E-300", "123456789012345", "0.1", "-0", "1E-7", "9.99999999999999E+307"])),
            50..72 => json!(*self.rng.pick(&["apple", "Banana", "  padded ", "x y z", "North", "south", "ÄÖÜ ß", "a\"quote", "1st", "TRUE story"])),
            72..74 => json!(""),
            74..80 => json!(*self.rng.pick(&["TRUE", "FALSE"])),
            80..85 => json!(format!("{}%", self.rng.int(-50, 150))),
            85..90 => json!(format!("2026-{:02}-{:02}", self.rng.int(1, 12), self.rng.int(1, 28))),
            90..94 => json!(*self.rng.pick(&["$1,234.50", "(42)", "1,000", "12:30", "2026-03-04 08:15"])),
            94..97 => json!(*self.rng.pick(&["'0012", "'=not a formula", "#N/A", "#DIV/0!"])),
            // Numbers as JSON numbers (the command takes them as typed).
            _ => json!(self.rng.int(-99, 99)),
        }
    }

    /// The sheet a reference points at, this one or (30 %) another, and the prefix it needs.
    fn target(&mut self, here: &str) -> (String, String) {
        if self.sheets.len() > 1 && self.rng.chance(30) {
            let other = self.sheet();
            if other != here {
                return (sheet_prefix(&other), other);
            }
        }
        (String::new(), here.to_string())
    }

    fn cell_ref(&mut self, here: &str) -> String {
        let (p, on) = self.target(here);
        let c = self.cell(&on);
        if self.rng.chance(15) {
            let (letters_part, digits): (String, String) = c.chars().partition(|ch| ch.is_ascii_alphabetic());
            format!("{p}${letters_part}${digits}")
        } else {
            format!("{p}{c}")
        }
    }

    fn range_ref(&mut self, here: &str) -> String {
        let (p, on) = self.target(here);
        let r = self.range(&on);
        let r = if r.contains(':') { r } else { format!("{r}:{r}") };
        format!("{p}{r}")
    }

    fn num(&mut self, here: &str) -> String {
        if self.rng.chance(65) { self.cell_ref(here) } else { self.rng.int(-20, 100).to_string() }
    }

    fn crit(&mut self) -> String {
        self.rng.pick(&["\">5\"", "\"<=0\"", "\"apple\"", "\"<>\"", "\"=North\"", "10"]).to_string()
    }

    fn formula(&mut self, here: &str) -> Json {
        let body = if self.rng.chance(4) {
            let v = *self.rng.pick(&VOLATILE);
            match self.rng.below(3) {
                0 => v.to_string(),
                1 => format!("ROUND({v}*100,2)"),
                _ => format!("{v}+{}", self.num(here)),
            }
        } else {
            self.call(here)
        };
        let body = match self.rng.below(10) {
            0 => format!("IFERROR({body},-1)"),
            1 => format!("{body}+{}", self.num(here)),
            _ => body,
        };
        json!(format!("={body}"))
    }

    fn call(&mut self, here: &str) -> String {
        let f = *self.rng.pick(&FUNCTIONS);
        let args = match f {
            "SUM" | "AVERAGE" | "MIN" | "MAX" | "COUNT" | "COUNTA" | "PRODUCT" | "MEDIAN" => {
                if self.rng.chance(30) {
                    format!("{},{}", self.range_ref(here), self.num(here))
                } else {
                    self.range_ref(here)
                }
            }
            "SUMPRODUCT" => {
                let r = self.range(here);
                let (a, b) = r.split_once(':').unwrap_or((&r, &r));
                let shift = |c: &str| -> String {
                    // The same shape one column to the right, so the arrays match.
                    let (l, d): (String, String) = c.chars().partition(|ch| ch.is_ascii_alphabetic());
                    let col = gridcraft_engine::core::CellRef::parse(&format!("{l}1")).map_or(0, |x| x.col);
                    format!("{}{d}", letters((col + 1).min(COLS)))
                };
                format!("{a}:{b},{}:{}", shift(a), shift(b))
            }
            "COUNTIF" => format!("{},{}", self.range_ref(here), self.crit()),
            "SUMIF" | "AVERAGEIF" => format!("{},{}", self.range_ref(here), self.crit()),
            "IF" => format!("{}>{},{},{}", self.num(here), self.num(here), self.num(here), self.text_arg(here)),
            "IFERROR" => format!("1/{},0", self.num(here)),
            "AND" | "OR" => format!("{}>0,{}<50", self.num(here), self.num(here)),
            "NOT" => format!("{}=0", self.num(here)),
            "ROUND" | "ROUNDUP" | "ROUNDDOWN" => format!("{}/7,{}", self.num(here), self.rng.int(-1, 3)),
            "ABS" | "INT" | "SQRT" => self.num(here),
            "MOD" => format!("{},{}", self.num(here), self.rng.int(1, 9)),
            "POWER" => format!("{},{}", self.num(here), self.rng.int(0, 3)),
            "LEN" | "UPPER" | "LOWER" | "TRIM" => self.text_arg(here),
            "LEFT" | "RIGHT" => format!("{},{}", self.text_arg(here), self.rng.int(0, 4)),
            "MID" => format!("{},{},{}", self.text_arg(here), self.rng.int(1, 3), self.rng.int(0, 4)),
            "CONCAT" => format!("{},\"-\",{}", self.text_arg(here), self.num(here)),
            "TEXT" => format!("{},{}", self.num(here), self.rng.pick(&["\"0.00\"", "\"#,##0\"", "\"0%\"", "\"yyyy-mm-dd\""])),
            "VLOOKUP" | "HLOOKUP" => format!("{},{},{},FALSE", self.num(here), self.range_ref(here), self.rng.int(1, 2)),
            "INDEX" => format!("{},{},1", self.range_ref(here), self.rng.int(1, 3)),
            "MATCH" => format!("{},{},0", self.num(here), self.range_ref(here)),
            "XLOOKUP" => {
                let (p, on) = self.target(here);
                let (r, c) = self.spot(&on);
                let c = c.min(COLS - 2);
                let (col, next) = (letters(c), letters(c + 1));
                let (r0, r1) = (r.saturating_sub(5) + 1, r + 6);
                format!("{},{p}{col}{r0}:{col}{r1},{p}{next}{r0}:{next}{r1},\"none\"", self.num(here))
            }
            "DATE" => format!("2026,{},{}", self.rng.int(1, 12), self.num(here)),
            _ => self.range_ref(here),
        };
        format!("{f}({args})")
    }

    fn text_arg(&mut self, here: &str) -> String {
        if self.rng.chance(70) { self.cell_ref(here) } else { self.rng.pick(&["\"  Mixed Case \"", "\"abc\"", "\"\""]).to_string() }
    }

    fn number_format(&mut self) -> Step {
        let mut extra = Map::new();
        if self.rng.chance(60) {
            let f = *self.rng.pick(&[
                "General",
                "Number",
                "Currency",
                "Accounting",
                "Short Date",
                "Long Date",
                "Time",
                "Percentage",
                "Fraction",
                "Scientific",
                "Text",
            ]);
            extra.insert("format".into(), json!(f));
        } else {
            let c = *self.rng.pick(&["0.00", "#,##0", "0.0%", "yyyy-mm-dd", "[Red]0;0", "0.000E+00", "\"x\"@"]);
            extra.insert("code".into(), json!(c));
        }
        self.with_range("home.numberFormat", extra, true)
    }

    fn font(&mut self) -> Step {
        match self.rng.below(5) {
            0 | 1 => {
                let id = if self.rng.chance(50) { "home.bold" } else { "home.italic" };
                let on = self.rng.chance(70);
                self.with_range(id, obj(&[("on", json!(on))]), true)
            }
            2 => {
                let n = *self.rng.pick(&["Calibri", "Arial", "Georgia", "Courier New"]);
                self.with_range("home.fontName", obj(&[("name", json!(n))]), true)
            }
            3 => {
                let size = *self.rng.pick(&[8, 10, 11, 12, 14, 18, 24]);
                self.with_range("home.fontSize", obj(&[("size", json!(size))]), true)
            }
            _ => {
                let c = self.rng.pick(&[json!("#C00000"), json!("#1F4E79"), json!("auto"), json!({"theme": 4, "tint": -0.25})]).clone();
                self.with_range("home.fontColor", obj(&[("color", c)]), true)
            }
        }
    }

    fn borders(&mut self) -> Step {
        let preset = *self.rng.pick(&[
            "bottom",
            "top",
            "left",
            "right",
            "none",
            "all",
            "outside",
            "thickOutside",
            "thickBottom",
            "doubleBottom",
            "topBottom",
            "inside",
            "insideHorizontal",
            "insideVertical",
            "diagonalDown",
        ]);
        let mut extra = obj(&[("preset", json!(preset))]);
        if self.rng.chance(40) {
            extra.insert("style".into(), json!(*self.rng.pick(&["thin", "medium", "thick", "dashed", "double", "hair"])));
        }
        if self.rng.chance(30) {
            extra.insert("color".into(), json!(*self.rng.pick(&["#FF0000", "#00B050", "auto"])));
        }
        self.with_range("home.borders", extra, false)
    }

    fn alignment(&mut self) -> Step {
        match self.rng.below(7) {
            // Horizontal alignment toggles back to General from the active cell unless `on` is given.
            0..3 => {
                let id = *self.rng.pick(&["home.alignLeft", "home.alignCenter", "home.alignRight"]);
                self.with_range(id, obj(&[("on", json!(true))]), true)
            }
            3..5 => {
                let id = *self.rng.pick(&["home.alignTop", "home.alignMiddle", "home.alignBottom"]);
                self.with_range(id, Map::new(), true)
            }
            _ => {
                let on = self.rng.chance(70);
                self.with_range("home.wrapText", obj(&[("on", json!(on))]), true)
            }
        }
    }

    fn structural(&mut self) -> Step {
        let roll = self.rng.below(15);
        match roll {
            0..7 => {
                let id = if roll < 4 { "home.insertRows" } else { "home.deleteRows" };
                let sheet = self.sheet();
                let a = 1 + self.spot(&sheet).0 as u64;
                let b = a + self.rng.below(3);
                Step::new(id, json!({"sheet": sheet, "rows": format!("{a}:{b}")}))
            }
            7..12 => {
                let id = if roll < 10 { "home.insertColumns" } else { "home.deleteColumns" };
                let sheet = self.sheet();
                let a = self.spot(&sheet).1;
                let b = a + self.rng.below(2) as u32;
                Step::new(id, json!({"sheet": sheet, "cols": format!("{}:{}", letters(a), letters(b))}))
            }
            12 if self.sheets.len() < MAX_SHEETS => self.add_sheet(),
            13 => {
                let i = self.rng.below(self.sheets.len() as u64) as usize;
                let old = self.sheets[i].clone();
                let name = self.fresh_name();
                self.sheets[i] = name.clone();
                for w in self.written.iter_mut().filter(|w| w.0 == old) {
                    w.0 = name.clone();
                }
                Step::new("sheet.rename", json!({"sheet": old, "name": name}))
            }
            _ if self.sheets.len() > 1 => {
                let i = self.rng.below(self.sheets.len() as u64) as usize;
                let name = self.sheets.remove(i);
                self.written.retain(|w| w.0 != name);
                Step::new("home.deleteSheet", json!({"sheet": name}))
            }
            _ => self.add_sheet(),
        }
    }
}

fn obj(pairs: &[(&str, Json)]) -> Map<String, Json> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}
