use super::*;
use serde_json::json;

fn parse(spec: Value) -> Dialog {
    Dialog::parse(&spec).unwrap_or_else(|error| panic!("{error}"))
}

fn error(spec: Value) -> String {
    Dialog::parse(&spec).expect_err("dialog should be rejected")
}

fn click_id(dialog: &Dialog, index: usize) -> String {
    format!("{CLICK_PREFIX}{}/{index}", dialog.serial)
}

#[test]
fn every_dialog_type_validates_and_gets_its_namespace() {
    for (kind, extra) in [
        ("notice", json!({})),
        ("minecraft:notice", json!({"action": {"label": "OK"}})),
        (
            "confirmation",
            json!({"yes": {"label": "Yes"}, "no": {"label": "No"}}),
        ),
        (
            "multi_action",
            json!({"actions": [{"label": "A"}], "columns": 1}),
        ),
        ("server_links", json!({"button_width": 200})),
        (
            "dialog_list",
            json!({"dialogs": [{"type": "notice", "title": "Inner"}]}),
        ),
    ] {
        let mut spec = json!({"type": kind, "title": "Hello"});
        spec.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let dialog = parse(spec);
        let short = kind.strip_prefix("minecraft:").unwrap_or(kind);
        assert_eq!(dialog.wire()["type"], format!("minecraft:{short}"));
        assert_eq!(dialog.wire()["title"], "Hello");
    }
}

#[test]
fn plugin_actions_are_rewritten_and_their_payload_stays_in_rift() {
    let dialog = parse(json!({
        "type": "multi_action",
        "title": {"text": "Report", "color": "red"},
        "body": "Tell us what happened.",
        "inputs": [
            {"type": "text", "key": "reason", "label": "Reason", "max_length": 64},
            {"type": "boolean", "key": "anonymous", "label": "Anonymous"},
            {"type": "number_range", "key": "severity", "label": "Severity", "start": 1, "end": 5, "step": 1},
            {"type": "single_option", "key": "kind", "label": "Kind", "options": ["grief", {"id": "chat", "initial": true}]},
        ],
        "actions": [
            {"label": "Send", "action": {"type": "dynamic/custom", "id": "send", "additions": {"target": "Bob"}}},
            {"label": "Cancel", "action": {"type": "custom", "id": "cancel", "payload": [1, 2]}},
            {"label": "Website", "action": {"type": "open_url", "url": "https://example.com/rules"}},
            {"label": "Vote", "action": {"type": "custom", "id": "myserver:vote", "payload": {"x": 1}}},
        ],
    }));
    let actions = dialog.wire()["actions"].as_array().unwrap();
    assert_eq!(
        actions[0]["action"],
        json!({"type": "minecraft:dynamic/custom", "id": click_id(&dialog, 0)})
    );
    assert_eq!(
        actions[1]["action"],
        json!({"type": "minecraft:custom", "id": click_id(&dialog, 1)})
    );
    // Other actions, and namespaced custom ids, are sent as written.
    assert_eq!(actions[2]["action"]["url"], "https://example.com/rules");
    assert_eq!(
        actions[3]["action"],
        json!({"type": "minecraft:custom", "id": "myserver:vote", "payload": {"x": 1}})
    );
    // A string or a component body becomes a plain message; option
    // shorthands become objects.
    assert_eq!(
        dialog.wire()["body"],
        json!({"type": "minecraft:plain_message", "contents": "Tell us what happened."})
    );
    assert_eq!(
        dialog.wire()["inputs"][3]["options"],
        json!([{"id": "grief"}, {"id": "chat", "initial": true}])
    );
    assert!(!dialog.wire().to_string().contains("Bob"));

    // The client reports booleans as bytes and may add keys of its own.
    let sent = json!({"reason": "griefing", "anonymous": 1, "severity": 4.0, "kind": "grief", "target": "Eve", "extra": 1});
    let (response, closes) = dialog.resolve(&click_id(&dialog, 0), Some(sent)).unwrap();
    assert!(closes);
    assert_eq!(response.action, "send");
    assert_eq!(
        Value::Object(response.values),
        json!({"reason": "griefing", "anonymous": true, "severity": 4.0, "kind": "grief"})
    );
    assert_eq!(response.payload, Some(json!({"target": "Bob"})));

    // A static custom action carries no input values.
    let (response, _) = dialog
        .resolve(&click_id(&dialog, 1), Some(json!({"reason": "x"})))
        .unwrap();
    assert_eq!(response.action, "cancel");
    assert!(response.values.is_empty());
    assert_eq!(response.payload, Some(json!([1, 2])));
}

#[test]
fn text_components_in_a_body_list_are_plain_messages() {
    let dialog = parse(json!({
        "type": "notice",
        "title": "x",
        "body": [
            "plain",
            {"text": "styled", "color": "gray", "italic": true},
            {"type": "plain_message", "contents": "explicit", "width": 300},
        ],
    }));
    assert_eq!(
        dialog.wire()["body"],
        json!([
            {"type": "minecraft:plain_message", "contents": "plain"},
            {"type": "minecraft:plain_message", "contents": {"text": "styled", "color": "gray", "italic": true}},
            {"type": "minecraft:plain_message", "contents": "explicit", "width": 300},
        ])
    );
    // A component without an NBT form still fails, with its path.
    let message =
        error(json!({"type": "notice", "title": "x", "body": [{"text": "a", "extra": [null]}]}));
    assert!(message.contains("dialog.body[1]"), "{message}");
}

#[test]
fn mistyped_values_from_a_modified_client_are_dropped() {
    let dialog = parse(json!({
        "type": "notice",
        "title": "x",
        "inputs": [
            {"type": "boolean", "key": "flag", "label": "Flag"},
            {"type": "text", "key": "name", "label": "Name"},
        ],
        "action": {"label": "OK", "action": {"type": "dynamic/custom", "id": "ok"}},
    }));
    let (response, _) = dialog
        .resolve(&click_id(&dialog, 0), Some(json!({"flag": 7, "name": 3})))
        .unwrap();
    assert!(response.values.is_empty());
    let (response, _) = dialog
        .resolve(&click_id(&dialog, 0), Some(json!("not an object")))
        .unwrap();
    assert!(response.values.is_empty());
}

#[test]
fn clicks_must_name_this_dialog_and_an_existing_action() {
    let dialog = parse(json!({
        "type": "notice",
        "title": "x",
        "action": {"label": "OK", "action": {"type": "custom", "id": "ok"}},
    }));
    let other = parse(json!({
        "type": "notice",
        "title": "x",
        "action": {"label": "OK", "action": {"type": "custom", "id": "ok"}},
    }));
    assert_ne!(dialog.serial, other.serial);
    assert!(dialog.resolve(&click_id(&dialog, 0), None).is_some());
    assert!(dialog.resolve(&click_id(&other, 0), None).is_none());
    assert!(dialog.resolve(&click_id(&dialog, 1), None).is_none());
    assert!(dialog.resolve("rift:dialog/x/0", None).is_none());
    assert!(dialog.resolve("myserver:ok", None).is_none());
}

#[test]
fn after_action_decides_whether_a_click_closes_and_nested_dialogs_keep_their_own() {
    let dialog = parse(json!({
        "type": "multi_action",
        "title": "Outer",
        "pause": false,
        "after_action": "none",
        "actions": [
            {"label": "Stay", "action": {"type": "custom", "id": "stay"}},
            {"label": "More", "action": {"type": "show_dialog", "dialog": {
                "type": "notice",
                "title": "Inner",
                "inputs": [{"type": "text", "key": "note", "label": "Note"}],
                "action": {"label": "Done", "action": {"type": "dynamic/custom", "id": "done"}},
            }}},
        ],
    }));
    let (stay, closes) = dialog.resolve(&click_id(&dialog, 0), None).unwrap();
    assert_eq!((stay.action.as_str(), closes), ("stay", false));
    let (done, closes) = dialog
        .resolve(&click_id(&dialog, 1), Some(json!({"note": "hi"})))
        .unwrap();
    assert_eq!((done.action.as_str(), closes), ("done", true));
    assert_eq!(done.values["note"], "hi");
}

#[test]
fn templates_name_existing_inputs() {
    parse(json!({
        "type": "notice",
        "title": "x",
        "inputs": [{"type": "text", "key": "player", "label": "Player"}],
        "action": {"label": "Go", "action": {"type": "dynamic/run_command", "template": "msg $(player) hi"}},
    }));
    for (template, message) in [
        ("msg $(nobody)", "names no input"),
        ("msg $(player", "unterminated"),
        ("msg $(a-b)", "invalid template variable"),
    ] {
        let message_error = error(json!({
            "type": "notice",
            "title": "x",
            "inputs": [{"type": "text", "key": "player", "label": "Player"}],
            "action": {"label": "Go", "action": {"type": "dynamic/run_command", "template": template}},
        }));
        assert!(
            message_error.contains(message),
            "{template}: {message_error}"
        );
    }
}

#[test]
fn definitions_a_client_would_reject_fail_with_a_path() {
    for (spec, message) in [
        (json!("notice"), "dialog must be an object"),
        (json!({"type": "notice"}), "dialog.title is required"),
        (
            json!({"type": "popup", "title": "x"}),
            "unsupported type \"popup\"",
        ),
        (
            json!({"type": "notice", "title": "x", "colour": 1}),
            "unknown field \"colour\"",
        ),
        (
            json!({"type": "notice", "title": "x", "after_action": "none"}),
            "pauses the game",
        ),
        (
            json!({"type": "notice", "title": "x", "after_action": "later"}),
            "close, none or wait_for_response",
        ),
        (
            json!({"type": "confirmation", "title": "x", "yes": {"label": "y"}}),
            "dialog.no is required",
        ),
        (
            json!({"type": "multi_action", "title": "x", "actions": []}),
            "at least one button",
        ),
        (
            json!({"type": "multi_action", "title": "x", "actions": {}}),
            "at least one button",
        ),
        (
            json!({"type": "multi_action", "title": "x", "actions": [{"label": "a"}], "columns": 0}),
            "dialog.columns must be an integer",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "width": 2000}}),
            "dialog.action.width",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"tooltip": "a"}}),
            "dialog.action.label is required",
        ),
        (
            json!({"type": "dialog_list", "title": "x", "dialogs": ["minecraft:server_links"]}),
            "only inline dialogs",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "show_dialog", "dialog": "minecraft:quick_actions"}}}),
            "only inline dialogs",
        ),
        (
            json!({"type": "notice", "title": "x", "body": {"type": "item", "item": {"id": "minecraft:stone"}}}),
            "item bodies are not supported",
        ),
        (
            json!({"type": "notice", "title": "x", "body": [{"type": "minecraft:item"}]}),
            "dialog.body[1]: item bodies",
        ),
        (
            json!({"type": "notice", "title": "x", "inputs": [{"type": "text", "key": "a b", "label": "x"}]}),
            "dialog.inputs[1].key",
        ),
        (
            json!({"type": "notice", "title": "x", "inputs": [{"type": "text", "key": "a", "label": "x"}, {"type": "boolean", "key": "a", "label": "y"}]}),
            "duplicate key",
        ),
        (
            json!({"type": "notice", "title": "x", "inputs": [{"type": "text", "key": "a", "label": "x", "initial": "long", "max_length": 2}]}),
            "longer than max_length",
        ),
        (
            json!({"type": "notice", "title": "x", "inputs": [{"type": "text", "key": "a", "label": "x", "multiline": {"height": 600}}]}),
            "multiline.height",
        ),
        (
            json!({"type": "notice", "title": "x", "inputs": [{"type": "single_option", "key": "a", "label": "x", "options": [{"id": "1", "initial": true}, {"id": "2", "initial": true}]}]}),
            "only one option",
        ),
        (
            json!({"type": "notice", "title": "x", "inputs": [{"type": "number_range", "key": "a", "label": "x", "start": 0, "end": 1, "initial": 2}]}),
            "between start and end",
        ),
        (
            json!({"type": "notice", "title": "x", "inputs": [{"type": "number_range", "key": "a", "label": "x", "start": 0, "end": 1, "step": 0}]}),
            "greater than 0",
        ),
        (
            json!({"type": "notice", "title": "x", "inputs": [{"type": "number_range", "key": "a", "label": "x", "start": 0}]}),
            "requires start and end",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "open_url", "url": "ftp://x"}}}),
            "http or https",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "run_command", "command": "say \u{a7}cx"}}}),
            "control characters",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "change_page", "page": 0}}}),
            "page must be an integer",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "custom", "id": "rift:x"}}}),
            "reserved",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "custom", "id": "Bad:Id"}}}),
            "not a valid identifier",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "custom", "id": ""}}}),
            "1..=64 bytes",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "dynamic/custom", "id": "a", "additions": [1]}}}),
            "additions must be an object",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "custom", "id": "a", "payload": [null]}}}),
            "payload",
        ),
        (
            json!({"type": "notice", "title": "x", "action": {"label": "a", "action": {"type": "teleport"}}}),
            "unsupported type",
        ),
        (json!({"type": "notice", "title": 5}), "dialog.title"),
    ] {
        let message_error = error(spec.clone());
        assert!(
            message_error.contains(message),
            "{spec}: expected {message:?} in {message_error:?}"
        );
    }
}

#[test]
fn size_nesting_and_action_limits_hold() {
    let huge = "x".repeat(MAX_DIALOG_BYTES);
    assert!(error(json!({"type": "notice", "title": huge})).contains("exceeds 64 KiB"));
    assert!(Dialog::parse_json(&format!("\"{}\"", "x".repeat(MAX_DIALOG_BYTES))).is_err());
    assert!(
        Dialog::parse_json("{")
            .unwrap_err()
            .contains("invalid JSON")
    );

    let mut nested = json!({"type": "notice", "title": "bottom"});
    for _ in 0..=MAX_NESTING {
        nested = json!({"type": "dialog_list", "title": "x", "dialogs": [nested]});
    }
    assert!(error(nested).contains("nest more than"));

    let actions: Vec<_> = (0..=MAX_CALLBACKS)
        .map(|index| json!({"label": "b", "action": {"type": "custom", "id": format!("a{index}")}}))
        .collect();
    let message = error(json!({"type": "multi_action", "title": "x", "actions": actions}));
    assert!(message.contains("at most"), "{message}");
}

#[test]
fn lua_tables_become_json_lists_and_objects() {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua
        .load(r#"return { type = "notice", title = { text = "Hi" }, inputs = {}, body = { "a", "b" }, width = 2.0, ratio = 0.5 }"#)
        .eval()
        .unwrap();
    assert_eq!(
        from_lua(value).unwrap(),
        json!({"type": "notice", "title": {"text": "Hi"}, "inputs": {}, "body": ["a", "b"], "width": 2, "ratio": 0.5})
    );
    let mixed: mlua::Value = lua.load("return { 1, x = 2 }").eval().unwrap();
    assert!(from_lua(mixed).unwrap_err().contains("dense list"));
    let function: mlua::Value = lua.load("return { f = print }").eval().unwrap();
    assert!(
        from_lua(function)
            .unwrap_err()
            .contains("unsupported function")
    );
    let deep: mlua::Value = lua
        .load("local t = {} local c = t for i = 1, 40 do c.n = {} c = c.n end return t")
        .eval()
        .unwrap();
    assert!(from_lua(deep).unwrap_err().contains("levels"));
}
