use gridcraft_engine::coauthor::{CoauthorError, GRIDCRAFT_LIVE_FORMAT, decode_entry, is_shared_command};

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
