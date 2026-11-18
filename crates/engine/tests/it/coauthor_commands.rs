use gridcraft_engine::catalog::CATALOG;
use gridcraft_engine::coauthor::{CoauthorError, GRIDCRAFT_LIVE_FORMAT, decode_entry, is_shared_command};
use protocol::live::MAX_PAYLOAD_BYTES;
use serde_json::{Value as Json, json};

fn entry(command: Json) -> String {
    json!({"version": 1, "action": 1, "command": command}).to_string()
}

fn canonical_commands() -> Vec<Json> {
    vec![
        json!({"id":"cell.set","sheet":[0,0],"cell":"A1","input":"1","originYear":2026,"references":[]}),
        json!({"id":"edit.clearContents","sheet":[0,0],"range":"A1:B2"}),
        json!({"id":"home.numberFormat","sheet":[0,0],"range":"A1","code":"0.00"}),
        json!({"id":"home.fontName","sheet":[0,0],"range":"A1","name":"Arial"}),
        json!({"id":"home.fontSize","sheet":[0,0],"range":"A1","size":12.0}),
        json!({"id":"home.fontColor","sheet":[0,0],"range":"A1","color":{"theme":4,"tint":-0.25}}),
        json!({"id":"home.fillColor","sheet":[0,0],"range":"A1","color":"#FFFF00"}),
        json!({"id":"home.borders","sheet":[0,0],"range":"A1:B2","preset":"outside","style":"thin","color":"auto"}),
        json!({"id":"home.alignTop","sheet":[0,0],"range":"A1"}),
        json!({"id":"home.alignMiddle","sheet":[0,0],"range":"A1"}),
        json!({"id":"home.alignBottom","sheet":[0,0],"range":"A1"}),
        json!({"id":"home.bold","sheet":[0,0],"range":"A1","on":true}),
        json!({"id":"home.italic","sheet":[0,0],"range":"A1","on":true}),
        json!({"id":"home.wrapText","sheet":[0,0],"range":"A1","on":true}),
        json!({"id":"home.alignLeft","sheet":[0,0],"range":"A1","on":true}),
        json!({"id":"home.alignCenter","sheet":[0,0],"range":"A1","on":true}),
        json!({"id":"home.alignRight","sheet":[0,0],"range":"A1","on":true}),
        json!({"id":"home.underline","sheet":[0,0],"range":"A1","on":true,"style":"single"}),
        json!({"id":"home.doubleUnderline","sheet":[0,0],"range":"A1","on":true,"style":"double"}),
        json!({"id":"home.increaseFontSize","sheet":[0,0],"range":"A1","size":14.0}),
        json!({"id":"home.decreaseFontSize","sheet":[0,0],"range":"A1","size":10.0}),
        json!({"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"font":{"bold":true},"align":{"readingOrder":null}}}),
        json!({"id":"data.filterSet","sheet":[0,0],"range":"A1:C9"}),
        json!({"id":"data.filterRemove","sheet":[0,0],"range":"A1:C9"}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":"A1:C9","column":[0,1],"values":["East"],"blanks":false}),
        json!({"id":"data.clearFilter","sheet":[0,0],"range":"A1:C9"}),
        json!({"id":"data.reapply","sheet":[0,0],"range":"A1:C9"}),
        json!({"id":"home.insertRows","sheet":[0,0],"rows":{"before":[0,2],"first":[7,0],"count":2}}),
        json!({"id":"home.deleteRows","sheet":[0,0],"rows":[[0,2],[0,3]]}),
        json!({"id":"home.insertColumns","sheet":[0,0],"cols":{"before":[0,2],"first":[7,2],"count":1}}),
        json!({"id":"home.deleteColumns","sheet":[0,0],"cols":[[0,2]]}),
        json!({"id":"home.insertSheet","name":"Data","before":[0,0]}),
        json!({"id":"sheet.rename","sheet":[0,0],"name":"Inputs"}),
        json!({"id":"home.deleteSheet","sheet":[0,0]}),
    ]
}

#[test]
fn shared_entry_round_trips_with_format_one() {
    let payload = r#"{"version":1,"action":42,"command":{"id":"home.bold","sheet":[0,0],"range":"A1:B2","on":true}}"#;

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
        json!({"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"borders":{"top":{"style":"Thin"}}}}),
        json!({"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"borders":{"top":{"color":"Auto"}}}}),
        json!({"id":"home.fontColor","sheet":[0,0],"range":"A1","color":{"theme":4}}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":"A1:C9","column":[0,1],"values":[null,{"region":"East"},[1,2]]}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":"A1:C9","column":[0,1],"custom":{}}),
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
        let mut command = json!({"id":"data.filterBy","sheet":[0,0],"range":"A1:C9","column":[0,1]});
        command.as_object_mut().expect("command is an object").extend(criterion.as_object().expect("criterion is an object").clone());
        decode_entry(&entry(command)).expect("direct filter criteria decodes");
    }
}

#[test]
fn payload_limit_accepts_exactly_max_bytes_and_rejects_one_more() {
    let base = entry(json!({"id":"home.numberFormat","sheet":[0,0],"range":"A1","code":"0.00"}));
    let exact = format!("{base}{}", " ".repeat(MAX_PAYLOAD_BYTES - base.len()));
    let over = format!("{exact} ");

    decode_entry(&exact).expect("a payload exactly at the limit decodes");
    assert!(
        matches!(decode_entry(&over), Err(CoauthorError::OversizedPayload { bytes, max }) if bytes == MAX_PAYLOAD_BYTES + 1 && max == MAX_PAYLOAD_BYTES)
    );
}
