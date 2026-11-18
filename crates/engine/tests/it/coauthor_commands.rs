use gridcraft_engine::catalog::CATALOG;
use gridcraft_engine::coauthor::{
    CoauthorError, GRIDCRAFT_LIVE_FORMAT, IdentityPosition, IdentityStructural, SharedCommand, TargetGone, WorkbookIds, decode_entry, encode_command,
    is_shared_command, resolve_command,
};
use gridcraft_engine::{DocState, Session, Trust};
use gridcraft_formula::{parse, print};
use gridcraft_model::{Sheet, Workbook};
use protocol::live::MAX_PAYLOAD_BYTES;
use serde_json::{Value as Json, json};
use std::sync::Arc;

fn entry(command: Json) -> String {
    json!({"version": 1, "action": 1, "command": command}).to_string()
}

fn cell_identity(row: u32, col: u32) -> Json {
    json!({"sheet":[0,0],"row":[0,row],"col":[0,col]})
}

fn range_identity(a: (u32, u32), b: (u32, u32), kind: &str) -> Json {
    json!({"sheet":[0,0],"start":cell_identity(a.0,a.1),"end":cell_identity(b.0,b.1),"kind":kind})
}

fn session_with_sheets(names: &[&str]) -> Session {
    let mut workbook = Workbook::new();
    workbook.sheets.clear();
    workbook.sheets.extend(names.iter().map(|name| Arc::new(Sheet::new(*name))));
    let mut session = Session::new();
    session.add_document(DocState::new(workbook, None, "Book1".into(), Trust::Local));
    session
}

#[test]
fn base_workbook_identity_tables_are_single_runs() {
    let session = session_with_sheets(&["Sheet1", "Data"]);
    let workbook = &session.doc().expect("document exists").wb;

    let ids = WorkbookIds::from_workbook(workbook).expect("base identities build");

    assert_eq!(ids.sheets.len(), 2);
    assert_eq!(ids.sheets.runs(), 1);
    for sheet_index in 0..2u32 {
        let sheet_id = collab_live::ids::LiveId(0, sheet_index);
        let sheet = ids.by_id.get(&sheet_id).expect("base sheet identity exists");
        assert_eq!(sheet.id, sheet_id);
        assert_eq!(sheet.rows.runs(), 1);
        assert_eq!(sheet.cols.runs(), 1);
        assert_eq!(sheet.rows.id_at(0), Some(collab_live::ids::LiveId(0, 0)));
        assert_eq!(sheet.cols.id_at(0), Some(collab_live::ids::LiveId(0, 0)));
    }
}

#[test]
fn concurrent_row_inserts_keep_both_identity_runs() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let sheet = collab_live::ids::LiveId(0, 0);
    let before = IdentityPosition::Before(collab_live::ids::LiveId(0, 4));

    ids.apply_structural(&IdentityStructural::InsertRows { sheet, before, first: collab_live::ids::LiveId(1, 0), count: 2 })
        .expect("first insert applies");
    ids.apply_structural(&IdentityStructural::InsertRows { sheet, before, first: collab_live::ids::LiveId(2, 0), count: 2 })
        .expect("second insert applies");

    let rows = &ids.by_id.get(&sheet).expect("sheet identities exist").rows;
    assert_eq!(rows.id_at(4), Some(collab_live::ids::LiveId(1, 0)));
    assert_eq!(rows.id_at(5), Some(collab_live::ids::LiveId(1, 1)));
    assert_eq!(rows.id_at(6), Some(collab_live::ids::LiveId(2, 0)));
    assert_eq!(rows.id_at(7), Some(collab_live::ids::LiveId(2, 1)));
    assert_eq!(rows.id_at(8), Some(collab_live::ids::LiveId(0, 4)));
    assert_eq!(rows.len(), gridcraft_core::MAX_ROWS);
}

#[test]
fn structural_axes_stay_fixed_and_reject_invalid_counts() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let sheet = collab_live::ids::LiveId(0, 0);

    assert!(
        ids.apply_structural(&IdentityStructural::InsertRows {
            sheet,
            before: IdentityPosition::Before(collab_live::ids::LiveId(0, 0)),
            first: collab_live::ids::LiveId(1, 0),
            count: 0,
        })
        .is_err()
    );
    ids.apply_structural(&IdentityStructural::InsertColumns {
        sheet,
        before: IdentityPosition::Before(collab_live::ids::LiveId(0, gridcraft_core::MAX_COLS - 1)),
        first: collab_live::ids::LiveId(1, 1),
        count: 1,
    })
    .expect("last-column insert applies");
    assert_eq!(ids.by_id.get(&sheet).expect("sheet exists").cols.len(), gridcraft_core::MAX_COLS);
    assert!(matches!(
        ids.by_id.get(&sheet).expect("sheet exists").cols.find(collab_live::ids::LiveId(0, gridcraft_core::MAX_COLS - 1)),
        collab_live::ids::Lookup::Deleted { .. }
    ));
}

#[test]
fn deleted_target_resolves_to_no_effect() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let target = ids.cell_id(0, gridcraft_core::CellRef::new(3, 2)).expect("cell identity resolves");

    ids.apply_structural(&IdentityStructural::DeleteRows { sheet: target.sheet, ids: vec![target.row], first: collab_live::ids::LiveId(9, 0) })
        .expect("row delete applies");

    let error = ids.resolve_cell(target).expect_err("deleted target must not move to another cell");
    assert_eq!(error.to_string(), "row identity LiveId(0, 3) was deleted");
}

#[test]
fn formula_references_round_trip_through_identities() {
    let session = session_with_sheets(&["Sheet1", "Data", "Tail"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let input = "=A1+$B$2+Data!C3+SUM(D4:E5,6:7,F:G,Sheet1:Tail!$H8)";
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);

    let entry = encode_command(&ids, &session, 7, "cell.set", &json!({"sheet":"Sheet1","cell":"J10","input":input}), 2026, &mut allocate)
        .expect("formula command encodes");
    let resolved = resolve_command(&ids, &session, &entry).expect("formula command resolves");
    let resolved_input = resolved.params["input"].as_str().expect("resolved input is text");

    assert_eq!(
        parse(resolved_input.trim_start_matches('=')).expect("resolved formula parses"),
        parse(input.trim_start_matches('=')).expect("source formula parses")
    );
    assert_eq!(
        print(&parse(resolved_input.trim_start_matches('=')).expect("resolved formula parses")),
        print(&parse(input.trim_start_matches('=')).expect("source formula parses"))
    );
}

#[test]
fn formula_deleted_reference_becomes_ref() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry = encode_command(&ids, &session, 8, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"=A1+B2"}), 2026, &mut allocate)
        .expect("formula command encodes");
    ids.apply_structural(&IdentityStructural::DeleteRows {
        sheet: collab_live::ids::LiveId(0, 0),
        ids: vec![collab_live::ids::LiveId(0, 0)],
        first: collab_live::ids::LiveId(9, 1),
    })
    .expect("row delete applies");

    let resolved = resolve_command(&ids, &session, &entry).expect("entry itself remains well formed");

    assert_eq!(resolved.params["input"], "=#REF!+B1");
}

#[test]
fn signed_formulas_use_identities_and_text_formatted_equals_stays_text() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry = encode_command(&ids, &session, 12, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"+A1"}), 2026, &mut allocate)
        .expect("signed formula encodes");
    ids.apply_structural(&IdentityStructural::DeleteRows {
        sheet: collab_live::ids::LiveId(0, 0),
        ids: vec![collab_live::ids::LiveId(0, 0)],
        first: collab_live::ids::LiveId(9, 2),
    })
    .expect("row delete applies");
    let resolved = resolve_command(&ids, &session, &entry).expect("signed formula resolves");
    assert_eq!(resolved.params["input"], "=+#REF!");

    let mut text_session = session_with_sheets(&["Sheet1"]);
    text_session.execute("home.numberFormat", json!({"sheet":"Sheet1","range":"C3","code":"@"})).expect("text format applies");
    let text_ids = WorkbookIds::from_workbook(&text_session.doc().expect("document exists").wb).expect("base identities build");
    let text_entry =
        encode_command(&text_ids, &text_session, 13, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"=A1"}), 2026, &mut allocate)
            .expect("text input encodes");
    match text_entry.command {
        SharedCommand::CellSet { input, formula, references, .. } => {
            assert_eq!(input, "=A1");
            assert!(!formula);
            assert!(references.is_empty());
        }
        other => panic!("expected cell.set, got {other:?}"),
    }
}

#[test]
fn multi_area_and_delete_count_targets_survive_encoding() {
    let session = session_with_sheets(&["Sheet1"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut next = 0u32;
    let mut allocate = |count| {
        let first = collab_live::ids::LiveId(7, next);
        next = next.checked_add(count).ok_or(collab_live::ids::IdError::Exhausted)?;
        Ok(first)
    };
    let clear = encode_command(&ids, &session, 14, "edit.clearContents", &json!({"sheet":"Sheet1","range":"A1,C1"}), 2026, &mut allocate)
        .expect("multi-area clear encodes");
    assert_eq!(resolve_command(&ids, &session, &clear).expect("multi-area clear resolves").params["range"], "A1,C1");

    let delete = encode_command(&ids, &session, 15, "home.deleteRows", &json!({"sheet":"Sheet1","rows":"3","count":2}), 2026, &mut allocate)
        .expect("counted delete encodes");
    assert_eq!(resolve_command(&ids, &session, &delete).expect("counted delete resolves").params["rows"], "3:3,4:4");
}

#[test]
fn whole_axis_ranges_ignore_orthogonal_deleted_identities() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let row_range = ids.range_id(0, gridcraft_core::RangeRef::rows(4, 4)).expect("row identity range builds");
    ids.apply_structural(&IdentityStructural::DeleteColumns {
        sheet: collab_live::ids::LiveId(0, 0),
        ids: vec![collab_live::ids::LiveId(0, 0)],
        first: collab_live::ids::LiveId(9, 3),
    })
    .expect("column delete applies");

    assert_eq!(ids.resolve_range(row_range).expect("row target survives orthogonal delete").1.a1(), "5:5");
}

#[test]
fn structurally_divergent_three_d_reference_becomes_ref() {
    let session = session_with_sheets(&["Sheet1", "Middle", "Tail"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry =
        encode_command(&ids, &session, 16, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"=SUM(Sheet1:Tail!A1)"}), 2026, &mut allocate)
            .expect("3-D formula encodes");
    ids.apply_structural(&IdentityStructural::InsertRows {
        sheet: collab_live::ids::LiveId(0, 0),
        before: IdentityPosition::Before(collab_live::ids::LiveId(0, 0)),
        first: collab_live::ids::LiveId(8, 0),
        count: 1,
    })
    .expect("one sheet shifts");

    let resolved = resolve_command(&ids, &session, &entry).expect("3-D entry resolves safely");
    assert_eq!(resolved.params["input"], "=SUM(#REF!)");
}

#[test]
fn identity_payloads_are_closed_and_local_encoding_is_bounded() {
    let extra_cell = json!({
        "version":1,
        "action":17,
        "command":{
            "id":"cell.set",
            "sheet":[0,0],
            "cell":{"sheet":[0,0],"row":[0,0],"col":[0,0],"extra":false},
            "input":"1",
            "originYear":2026,
            "references":[]
        }
    });
    assert!(matches!(decode_entry(&extra_cell.to_string()), Err(CoauthorError::MalformedPayload(_))));

    let extra_range = json!({
        "version":1,
        "action":17,
        "command":{
            "id":"home.bold",
            "sheet":[0,0],
            "range":{
                "sheet":[0,0],
                "start":cell_identity(0,0),
                "end":cell_identity(0,0),
                "kind":"cells",
                "extra":false
            },
            "on":true
        }
    });
    assert!(matches!(decode_entry(&extra_range.to_string()), Err(CoauthorError::MalformedPayload(_))));

    let extra_reference = json!({
        "version":1,
        "action":17,
        "command":{
            "id":"cell.set",
            "sheet":[0,0],
            "cell":cell_identity(0,0),
            "input":"=A1",
            "originYear":2026,
            "formula":true,
            "references":[{
                "sheet":{"kind":"current","sheet":[0,0],"extra":false},
                "reference":{"kind":"cell","anchor":{"row":[0,0],"col":[0,0],"row_abs":false,"col_abs":false}}
            }]
        }
    });
    assert!(matches!(decode_entry(&extra_reference.to_string()), Err(CoauthorError::MalformedPayload(_))));

    let session = session_with_sheets(&["Sheet1"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    assert!(matches!(
        encode_command(&ids, &session, 18, "home.deleteRows", &json!({"sheet":"Sheet1","rows":"1:50000"}), 2026, &mut allocate,),
        Err(CoauthorError::OversizedPayload { .. })
    ));
}

#[test]
fn mismatched_identity_targets_are_refused() {
    let session = session_with_sheets(&["Sheet1", "Data"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let payload = json!({
        "version":1,
        "action":9,
        "command":{
            "id":"home.bold",
            "sheet":[0,0],
            "range":{
                "sheet":[0,1],
                "start":{"sheet":[0,1],"row":[0,0],"col":[0,0]},
                "end":{"sheet":[0,1],"row":[0,0],"col":[0,0]},
                "kind":"cells"
            },
            "on":true
        }
    });
    let entry = decode_entry(&payload.to_string()).expect("typed entry decodes");

    assert!(matches!(resolve_command(&ids, &session, &entry), Err(TargetGone::Malformed(_))));

    let mismatched_cell = decode_entry(
        &json!({
            "version":1,
            "action":10,
            "command":{
                "id":"cell.set",
                "sheet":[0,0],
                "cell":{"sheet":[0,1],"row":[0,0],"col":[0,0]},
                "input":"1",
                "originYear":2026,
                "references":[]
            }
        })
        .to_string(),
    )
    .expect("typed cell entry decodes");
    assert!(matches!(resolve_command(&ids, &session, &mismatched_cell), Err(TargetGone::Malformed(_))));

    let mismatched_current_reference = decode_entry(
        &json!({
            "version":1,
            "action":11,
            "command":{
                "id":"cell.set",
                "sheet":[0,1],
                "cell":{"sheet":[0,1],"row":[0,0],"col":[0,0]},
                "input":"=A1",
                "originYear":2026,
                "formula":true,
                "references":[{
                    "sheet":{"kind":"current","sheet":[0,0]},
                    "reference":{"kind":"cell","anchor":{"row":[0,0],"col":[0,0],"row_abs":false,"col_abs":false}}
                }]
            }
        })
        .to_string(),
    )
    .expect("typed formula entry decodes");
    assert!(matches!(resolve_command(&ids, &session, &mismatched_current_reference), Err(TargetGone::Malformed(_))));
}

fn canonical_commands() -> Vec<Json> {
    vec![
        json!({"id":"cell.set","sheet":[0,0],"cell":cell_identity(0,0),"input":"1","originYear":2026,"references":[]}),
        json!({"id":"edit.clearContents","sheet":[0,0],"range":range_identity((0,0),(1,1),"cells")}),
        json!({"id":"home.numberFormat","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"code":"0.00"}),
        json!({"id":"home.fontName","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"name":"Arial"}),
        json!({"id":"home.fontSize","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"size":12.0}),
        json!({"id":"home.fontColor","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"color":{"theme":4,"tint":-0.25}}),
        json!({"id":"home.fillColor","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"color":"#FFFF00"}),
        json!({"id":"home.borders","sheet":[0,0],"range":range_identity((0,0),(1,1),"cells"),"preset":"outside","style":"thin","color":"auto"}),
        json!({"id":"home.alignTop","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells")}),
        json!({"id":"home.alignMiddle","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells")}),
        json!({"id":"home.alignBottom","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells")}),
        json!({"id":"home.bold","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.italic","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.wrapText","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.alignLeft","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.alignCenter","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.alignRight","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.underline","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true,"style":"single"}),
        json!({"id":"home.doubleUnderline","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true,"style":"double"}),
        json!({"id":"home.increaseFontSize","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"size":14.0}),
        json!({"id":"home.decreaseFontSize","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"size":10.0}),
        json!({"id":"home.formatCells","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"style":{"font":{"bold":true},"align":{"readingOrder":null}}}),
        json!({"id":"data.filterSet","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells")}),
        json!({"id":"data.filterRemove","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells")}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells"),"column":[0,1],"values":["East"],"blanks":false}),
        json!({"id":"data.clearFilter","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells")}),
        json!({"id":"data.reapply","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells")}),
        json!({"id":"home.insertRows","sheet":[0,0],"rows":{"before":{"before":[0,2]},"first":[7,0],"count":2}}),
        json!({"id":"home.deleteRows","sheet":[0,0],"rows":{"ids":[[0,2],[0,3]],"first":[7,4]}}),
        json!({"id":"home.insertColumns","sheet":[0,0],"cols":{"before":{"before":[0,2]},"first":[7,2],"count":1}}),
        json!({"id":"home.deleteColumns","sheet":[0,0],"cols":{"ids":[[0,2]],"first":[7,6]}}),
        json!({"id":"home.insertSheet","name":"Data","before":{"before":{"before":[0,0]},"id":[7,3]}}),
        json!({"id":"sheet.rename","sheet":[0,0],"name":"Inputs"}),
        json!({"id":"home.deleteSheet","sheet":[0,0]}),
    ]
}

#[test]
fn shared_entry_round_trips_with_format_one() {
    let payload = r#"{"version":1,"action":42,"command":{"id":"home.bold","sheet":[0,0],"range":{"sheet":[0,0],"start":{"sheet":[0,0],"row":[0,0],"col":[0,0]},"end":{"sheet":[0,0],"row":[0,1],"col":[0,1]},"kind":"cells"},"on":true}}"#;

    let entry = decode_entry(payload).expect("gridcraft/1 entry decodes");

    assert_eq!(GRIDCRAFT_LIVE_FORMAT, "gridcraft/1");
    assert_eq!(entry.version, 1);
    assert_eq!(entry.action, 42);
    assert_eq!(serde_json::to_string(&entry).expect("entry encodes"), payload);
}

#[test]
fn shared_entry_rejects_unknown_keys_and_versions() {
    let extra = r#"{"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true},"extra":false}"#;
    let version = r#"{"version":2,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true}}"#;
    let variant = r#"{"version":1,"action":1,"command":{"id":"home.sparkle","sheet":[0,0],"range":"A1"}}"#;

    assert!(matches!(decode_entry(extra), Err(CoauthorError::MalformedPayload(_))));
    assert_eq!(decode_entry(version).expect_err("version 2 is unsupported").to_string(), "unsupported GridCraft live entry version 2");
    assert_eq!(decode_entry(variant).expect_err("unknown command is unsupported").to_string(), "command `home.sparkle` is not shared");

    let oversized = " ".repeat(262_145);
    assert!(matches!(decode_entry(&oversized), Err(CoauthorError::OversizedPayload { bytes: 262_145, max: 262_144 })));
}

#[test]
fn unlisted_commands_are_refused_in_live_mode() {
    for id in ["data.filter", "edit.undo", "macro.run", "automate.runMacro"] {
        assert!(!is_shared_command(id), "{id} must stay out of the live log");
    }
    for id in ["cell.set", "edit.clearContents", "home.bold", "data.filterSet", "home.insertRows", "sheet.rename"] {
        assert!(is_shared_command(id), "{id} is in the G3 shared surface");
    }
}

#[test]
fn shared_commands_require_explicit_targets() {
    let cases = [
        (r#"{"version":1,"action":1,"command":{"id":"home.bold","range":"A1","on":true}}"#, "command `home.bold` is missing required `sheet`"),
        (
            r#"{"version":1,"action":1,"command":{"id":"edit.clearContents","sheet":[0,0]}}"#,
            "command `edit.clearContents` is missing required `range`",
        ),
        (r#"{"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1"}}"#, "command `home.bold` is missing required `on`"),
        (
            r#"{"version":1,"action":1,"command":{"id":"home.increaseFontSize","sheet":[0,0],"range":"A1"}}"#,
            "command `home.increaseFontSize` is missing required `size`",
        ),
    ];

    for (payload, expected) in cases {
        assert_eq!(decode_entry(payload).expect_err("required target is absent").to_string(), expected);
    }
}

#[test]
fn every_planned_command_accepts_its_canonical_payload_and_other_catalog_commands_are_refused() {
    let commands = canonical_commands();
    assert_eq!(commands.len(), 34);

    for command in &commands {
        let id = command["id"].as_str().expect("canonical command id is a string");
        let decoded = decode_entry(&entry(command.clone())).unwrap_or_else(|error| panic!("canonical `{id}` was refused: {error}"));
        assert_eq!(serde_json::to_value(decoded).expect("canonical entry re-encodes")["command"], *command);
        assert!(is_shared_command(id), "canonical `{id}` is absent from the allow-list");
    }

    for (id, _, _, _) in CATALOG {
        let planned = commands.iter().any(|command| command["id"] == *id);
        assert_eq!(is_shared_command(id), planned, "catalog command `{id}` has the wrong live-mode status");
    }
}

#[test]
fn shared_entry_rejects_extra_and_duplicate_keys_at_every_wire_level() {
    let cases = [
        r#"{"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true},"extra":false}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true,"extra":false}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"font":{"bold":true},"extra":false}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"font":{"bold":true,"extra":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"fill":{"pattern":"Solid","extra":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"borders":{"top":{"style":"Thin","extra":false}}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"align":{"wrap":true,"extra":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"protection":{"locked":true,"extra":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.fontColor","sheet":[0,0],"range":"A1","color":{"theme":4,"extra":false}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"data.filterBy","sheet":[0,0],"range":"A1:B2","column":[0,0],"custom":{"op":"=","value":"x","extra":false}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"data.filterBy","sheet":[0,0],"range":"A1:B2","column":[0,0],"top":{"count":10,"extra":false}}}"#,
        r#"{"version":1,"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.bold","id":"home.bold","sheet":[0,0],"range":"A1","on":true}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"font":{"bold":true,"bold":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"data.filterBy","sheet":[0,0],"range":"A1:B2","column":[0,0],"top":{"count":10,"count":5}}}"#,
    ];

    for payload in cases {
        assert!(matches!(decode_entry(payload), Err(CoauthorError::MalformedPayload(_))), "accepted non-closed payload: {payload}");
    }
}

#[test]
fn shared_commands_accept_engine_defaulted_nested_fields_and_arbitrary_filter_values() {
    let commands = [
        json!({"id":"home.formatCells","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"style":{"borders":{"top":{"style":"Thin"}}}}),
        json!({"id":"home.formatCells","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"style":{"borders":{"top":{"color":"Auto"}}}}),
        json!({"id":"home.fontColor","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"color":{"theme":4}}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells"),"column":[0,1],"values":[null,{"region":"East"},[1,2]]}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells"),"column":[0,1],"custom":{}}),
    ];

    for command in commands {
        decode_entry(&entry(command.clone())).unwrap_or_else(|error| panic!("engine-compatible command was refused: {command}: {error}"));
    }
}

#[test]
fn filter_by_accepts_each_direct_closed_criteria_shape() {
    let criteria = [
        json!({"values":["East",2,true],"blanks":true}),
        json!({"custom":{"op":">=","value":2,"op2":"<","value2":40,"and":true}}),
        json!({"top":{"count":10,"bottom":false,"percent":true}}),
        json!({"aboveAverage":true}),
        json!({"clear":true}),
    ];

    for criterion in criteria {
        let mut command = json!({"id":"data.filterBy","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells"),"column":[0,1]});
        command.as_object_mut().expect("command is an object").extend(criterion.as_object().expect("criterion is an object").clone());
        decode_entry(&entry(command)).expect("direct filter criteria decodes");
    }
}

#[test]
fn payload_limit_accepts_exactly_max_bytes_and_rejects_one_more() {
    let base = entry(json!({"id":"home.numberFormat","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"code":"0.00"}));
    let exact = format!("{base}{}", " ".repeat(MAX_PAYLOAD_BYTES - base.len()));
    let over = format!("{exact} ");

    decode_entry(&exact).expect("a payload exactly at the limit decodes");
    assert!(
        matches!(decode_entry(&over), Err(CoauthorError::OversizedPayload { bytes, max }) if bytes == MAX_PAYLOAD_BYTES + 1 && max == MAX_PAYLOAD_BYTES)
    );
}
