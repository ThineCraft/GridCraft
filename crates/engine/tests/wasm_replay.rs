//! Native-versus-WebAssembly replay for the co-authoring proof fixtures.
//!
//! Native Rust writes and verifies `fixtures/coauthor/native-digests.json`. The wasm32 test runs
//! the byte-identical embedded fixture texts under Node and compares its canonical workbook walk
//! and fingerprints with that native record.

#[path = "support/sequences.rs"]
mod sequences;

use gridcraft_engine::Session;
use gridcraft_engine::fingerprint::{DocId, FingerprintItem, fingerprint, fingerprint_items};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use sequences::{Sequence, apply, fresh_session};

const NATIVE_DIGESTS: &str = include_str!("fixtures/coauthor/native-digests.json");

const FIXTURES: [(&str, &str); 26] = [
    ("hand-1-arithmetic-across-sheets.json", include_str!("fixtures/coauthor/hand-1-arithmetic-across-sheets.json")),
    ("hand-2-insert-rows-moves-references.json", include_str!("fixtures/coauthor/hand-2-insert-rows-moves-references.json")),
    ("hand-3-deleted-column-gives-ref-errors.json", include_str!("fixtures/coauthor/hand-3-deleted-column-gives-ref-errors.json")),
    ("hand-4-renamed-then-deleted-sheet.json", include_str!("fixtures/coauthor/hand-4-renamed-then-deleted-sheet.json")),
    ("hand-5-formats-outlive-cleared-content.json", include_str!("fixtures/coauthor/hand-5-formats-outlive-cleared-content.json")),
    ("hand-6-text-lookup-and-logic.json", include_str!("fixtures/coauthor/hand-6-text-lookup-and-logic.json")),
    ("random-001.json", include_str!("fixtures/coauthor/random-001.json")),
    ("random-002.json", include_str!("fixtures/coauthor/random-002.json")),
    ("random-003.json", include_str!("fixtures/coauthor/random-003.json")),
    ("random-004.json", include_str!("fixtures/coauthor/random-004.json")),
    ("random-005.json", include_str!("fixtures/coauthor/random-005.json")),
    ("random-006.json", include_str!("fixtures/coauthor/random-006.json")),
    ("random-007.json", include_str!("fixtures/coauthor/random-007.json")),
    ("random-008.json", include_str!("fixtures/coauthor/random-008.json")),
    ("random-009.json", include_str!("fixtures/coauthor/random-009.json")),
    ("random-010.json", include_str!("fixtures/coauthor/random-010.json")),
    ("random-011.json", include_str!("fixtures/coauthor/random-011.json")),
    ("random-012.json", include_str!("fixtures/coauthor/random-012.json")),
    ("random-013.json", include_str!("fixtures/coauthor/random-013.json")),
    ("random-014.json", include_str!("fixtures/coauthor/random-014.json")),
    ("random-015.json", include_str!("fixtures/coauthor/random-015.json")),
    ("random-016.json", include_str!("fixtures/coauthor/random-016.json")),
    ("random-017.json", include_str!("fixtures/coauthor/random-017.json")),
    ("random-018.json", include_str!("fixtures/coauthor/random-018.json")),
    ("random-019.json", include_str!("fixtures/coauthor/random-019.json")),
    ("random-020.json", include_str!("fixtures/coauthor/random-020.json")),
];

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct NativeDigests {
    fixtures: Vec<NativeFixture>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct NativeFixture {
    name: String,
    entered: String,
    computed: String,
    items: Vec<NativeItem>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct NativeItem {
    stream: String,
    key: String,
    canonical: String,
}

fn replay(name: &str, text: &str) -> Result<Session, String> {
    let seq = Sequence::from_text(text).map_err(|e| format!("{name}: invalid fixture: {e}"))?;
    let mut session = fresh_session(&seq);
    for (index, step) in seq.steps.iter().enumerate() {
        if let Err(error) = apply(&mut session, step)
            && error.contains("panic")
        {
            return Err(format!("{name}: step {index} `{}` panicked: {error}", step.id));
        }
    }
    Ok(session)
}

fn native_item(item: FingerprintItem) -> NativeItem {
    let canonical = if item.stream == "entered" {
        let digest = Sha256::digest(item.canonical.as_bytes());
        let hex = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        format!("sha256 {hex}")
    } else {
        item.canonical
    };
    NativeItem { stream: item.stream.to_string(), key: item.key, canonical }
}

#[cfg(not(target_arch = "wasm32"))]
fn current_native_digests() -> Result<NativeDigests, String> {
    let mut fixtures = Vec::with_capacity(FIXTURES.len());
    for (name, text) in FIXTURES {
        let session = replay(name, text)?;
        let fingerprint = fingerprint(&session, DocId(0));
        let (entered, computed) = fingerprint.hex();
        let items = fingerprint_items(&session, DocId(0)).into_iter().map(native_item).collect();
        fixtures.push(NativeFixture { name: name.to_string(), entered, computed, items });
    }
    Ok(NativeDigests { fixtures })
}

#[cfg(not(target_arch = "wasm32"))]
fn digest_text() -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(&current_native_digests()?).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "writes the native reference file"]
fn write_native_digests() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/coauthor/native-digests.json");
    let text = digest_text().expect("native fixture replay succeeds");
    std::fs::write(&path, text).expect("native digest file is written");
    eprintln!("wrote {}", path.display());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn native_digests_are_current() {
    let current = digest_text().expect("native fixture replay succeeds");
    assert_eq!(NATIVE_DIGESTS.replace("\r\n", "\n"), current, "run the ignored write_native_digests test to refresh the native reference");
}

/// The embedded list must name every sequence in the fixtures folder, so a fixture added there
/// is replayed in WebAssembly too, not only by the native tests that read the folder.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn embedded_fixtures_match_the_folder() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/coauthor");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .expect("the fixtures folder is readable")
        .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
        .filter(|n| n.ends_with(".json") && n != "native-digests.json")
        .collect();
    on_disk.sort();
    let embedded: Vec<String> = FIXTURES.iter().map(|(name, _)| name.to_string()).collect();
    assert_eq!(embedded, on_disk, "add the new fixture to FIXTURES in tests/wasm_replay.rs and rewrite the native digests");
}

fn function_with_inputs(session: &Session, key: &str) -> Option<String> {
    let (sheet_name, cell_name) = key.rsplit_once('!')?;
    let document = session.documents().first()?;
    let sheet_index = document.wb.sheet_index(sheet_name)?;
    let cell_ref = gridcraft_engine::core::CellRef::parse(cell_name)?;
    let formula = document.wb.sheet(sheet_index)?.cell(cell_ref)?.formula.as_ref()?.text.as_str();
    let open = formula.find('(')?;
    let name_start = formula[..open].rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))).map_or(0, |i| i + 1);
    let name = &formula[name_start..open];
    let close = formula[open + 1..].rfind(')').map_or(formula.len(), |i| open + 1 + i);
    let inputs = &formula[open + 1..close];
    (!name.is_empty()).then(|| format!("; function {name} inputs ({inputs})"))
}

fn first_difference(expected: &[NativeItem], actual: &[NativeItem], session: &Session) -> String {
    for stream in ["entered", "computed"] {
        let mut left = expected.iter().filter(|item| item.stream == stream);
        let mut right = actual.iter().filter(|item| item.stream == stream);
        loop {
            let message = match (left.next(), right.next()) {
                (None, None) => break,
                (Some(item), None) => format!("{stream}: {} ({}) only in native", item.key, item.canonical),
                (None, Some(item)) => format!("{stream}: {} ({}) only in wasm", item.key, item.canonical),
                (Some(a), Some(b)) if a.key != b.key => {
                    format!("{stream}: native has {} ({}) where wasm has {} ({})", a.key, a.canonical, b.key, b.canonical)
                }
                (Some(a), Some(b)) if a.canonical != b.canonical => {
                    // The reference stores entered items hashed; name what this engine has instead.
                    let context = if stream == "computed" {
                        function_with_inputs(session, &a.key).unwrap_or_default()
                    } else {
                        fingerprint_items(session, DocId(0))
                            .into_iter()
                            .find(|item| item.stream == stream && item.key == b.key)
                            .map(|item| format!("; wasm reads {}", item.canonical))
                            .unwrap_or_default()
                    };
                    format!("{stream} {}: native {} != wasm {}{context}", a.key, a.canonical, b.canonical)
                }
                (Some(_), Some(_)) => continue,
            };
            return message;
        }
    }
    "digests differ but the canonical walks agree".to_string()
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn difference_reports_function_and_inputs() {
    let session = replay("diagnostic", FIXTURES[1].1).expect("fixture replays");
    let expected: Vec<NativeItem> = fingerprint_items(&session, DocId(0)).into_iter().map(native_item).collect();
    let mut actual: Vec<NativeItem> = fingerprint_items(&session, DocId(0)).into_iter().map(native_item).collect();
    let item = actual.iter_mut().find(|item| item.stream == "computed" && item.key == "Sheet1!A3").expect("the fixture has the SUM result");
    item.canonical = "num 0000000000000000 (0)".to_string();
    let difference = first_difference(&expected, &actual, &session);
    assert!(difference.contains("computed Sheet1!A3"), "{difference}");
    assert!(difference.contains("function SUM inputs (A1:A2)"), "{difference}");

    let mut actual: Vec<NativeItem> = fingerprint_items(&session, DocId(0)).into_iter().map(native_item).collect();
    let item = actual.iter_mut().find(|item| item.stream == "entered" && item.key == "Sheet1!A3").expect("the fixture has the SUM cell");
    item.canonical = "sha256 0".to_string();
    let difference = first_difference(&expected, &actual, &session);
    assert!(difference.contains("entered Sheet1!A3"), "{difference}");
    assert!(difference.contains("wasm reads formula"), "{difference}");
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen_test::wasm_bindgen_test]
fn wasm_matches_native_fixtures() {
    let expected: NativeDigests = serde_json::from_str(NATIVE_DIGESTS).expect("native digest file is valid JSON");
    let mut failures = Vec::new();
    for (name, text) in FIXTURES {
        let native = match expected.fixtures.iter().find(|fixture| fixture.name == name) {
            Some(native) => native,
            None => {
                failures.push(format!("{name}: missing native digest"));
                continue;
            }
        };
        let session = match replay(name, text) {
            Ok(session) => session,
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
        let fingerprint = fingerprint(&session, DocId(0));
        let (entered, computed) = fingerprint.hex();
        if entered != native.entered || computed != native.computed {
            let actual: Vec<NativeItem> = fingerprint_items(&session, DocId(0)).into_iter().map(native_item).collect();
            failures.push(format!("{name}: {}", first_difference(&native.items, &actual, &session)));
        }
    }
    assert!(failures.is_empty(), "{} native/wasm difference(s):\n{}", failures.len(), failures.join("\n"));
}
