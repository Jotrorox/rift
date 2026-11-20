use super::*;
use serde_json::json;

fn parse(spec: Value) -> Menu {
    Menu::parse(&spec).unwrap_or_else(|error| panic!("{error}"))
}

fn error(spec: Value) -> String {
    Menu::parse(&spec).expect_err("menu should be rejected")
}

fn patch(spec: Value) -> Patch {
    Patch::parse(&spec).unwrap_or_else(|error| panic!("{error}"))
}

fn button(menu: &Menu, slot: usize) -> &Button {
    menu.slots()[slot]
        .as_ref()
        .unwrap_or_else(|| panic!("slot {slot} is empty"))
}

fn item(menu: &Menu, slot: usize) -> Option<&'static str> {
    menu.slots()[slot].as_ref().map(|button| button.look.item)
}

/// A chest of `rows` rows with one button.
fn chest(rows: u8, button: Value) -> Value {
    json!({"id": "test", "rows": rows, "slots": [button]})
}

fn selector() -> Value {
    json!({
        "id": "servers",
        "title": {"mini": "<dark_gray>Choose a server"},
        "layout": [
            "#########",
            "# S C M #",
            "####X####",
        ],
        "keys": {
            "#": {"item": "gray_stained_glass_pane", "name": " "},
            "S": {"item": "grass_block", "name": {"mini": "<green>Survival"},
                  "lore": ["Click to join"],
                  "id": "join", "payload": {"server": "survival"}},
            "C": {"item": "bricks", "name": {"mini": "<gold>Creative"},
                  "id": "join", "payload": {"server": "creative"}},
            "M": {"item": "bow", "name": {"mini": "<red>Minigames"},
                  "id": "join", "payload": {"server": "minigames"}},
            "X": {"item": "barrier", "name": {"mini": "<red>Close"}, "close": true},
        },
    })
}

#[test]
fn the_server_selector_lays_out_its_keys() {
    let menu = parse(selector());
    assert_eq!(menu.id(), "servers");
    assert_eq!(menu.kind(), Kind::Chest { rows: 3 });
    assert_eq!(menu.slots().len(), 27);
    assert_eq!(item(&menu, 0), Some("gray_stained_glass_pane"));
    assert_eq!(item(&menu, 10), None);
    assert_eq!(item(&menu, 11), Some("grass_block"));
    assert_eq!(item(&menu, 13), Some("bricks"));
    assert_eq!(item(&menu, 15), Some("bow"));
    assert_eq!(item(&menu, 22), Some("barrier"));
    assert_eq!(item(&menu, 26), Some("gray_stained_glass_pane"));

    let survival = button(&menu, 11);
    assert_eq!(survival.action.id.as_deref(), Some("join"));
    assert_eq!(survival.action.payload, Some(json!({"server": "survival"})));
    assert_eq!(
        survival.look.lore,
        vec![Text::component(json!("Click to join"), "x").unwrap()]
    );
    assert!(survival.action.reports(Click::Left));
    assert!(!survival.action.reports(Click::Drop));
    assert!(!survival.action.closes(Click::Left));

    let close = button(&menu, 22);
    assert_eq!(close.action.id, None);
    assert!(close.action.closes(Click::Right));
    assert!(!close.action.closes(Click::NumberKey));
    assert!(!close.action.reports(Click::Left));

    // Markup is resolved when the menu is parsed.
    let Some(Text::Component(title)) = menu.title() else {
        panic!("{:?}", menu.title());
    };
    assert_eq!(title["color"], "dark_gray");
    assert_eq!(menu.button(11), Some(survival));
    assert_eq!(menu.button(27), None);
    assert_eq!(menu.button(-1), None);
    assert_eq!(menu.button(10), None);
    assert_eq!(menu.state(), None);
}

#[test]
fn every_type_has_its_slots_and_menu() {
    let kinds = [
        (
            json!({"id": "a"}),
            Kind::Chest { rows: 3 },
            27,
            "generic_9x3",
        ),
        (
            json!({"id": "a", "rows": 1}),
            Kind::Chest { rows: 1 },
            9,
            "generic_9x1",
        ),
        (
            json!({"id": "a", "type": "chest", "rows": 6}),
            Kind::Chest { rows: 6 },
            54,
            "generic_9x6",
        ),
        (
            json!({"id": "a", "layout": ["         ", "         "]}),
            Kind::Chest { rows: 2 },
            18,
            "generic_9x2",
        ),
        (
            json!({"id": "a", "type": "hopper"}),
            Kind::Hopper,
            5,
            "hopper",
        ),
        (
            json!({"id": "a", "type": "dispenser"}),
            Kind::Dispenser,
            9,
            "generic_3x3",
        ),
        (json!({"id": "a", "type": "anvil"}), Kind::Anvil, 3, "anvil"),
    ];
    for (spec, kind, slots, name) in kinds {
        let menu = parse(spec);
        assert_eq!(menu.kind(), kind);
        assert_eq!(menu.slots().len(), slots);
        assert_eq!(kind.menu(), name);
        assert_eq!(kind.slots(), slots);
        assert_eq!(kind.lines() * kind.width(), slots);
        assert!(kind.menu_type(340).is_some() && kind.menu_type(777).is_some());
    }
    assert_eq!(
        Kind::Chest { rows: 4 }.menu_type(340),
        Some(rift_translation::codec::misc::screen::MenuType::Window {
            kind: "minecraft:chest",
            slots: 36
        })
    );
}

#[test]
fn slots_outside_the_menu_name_the_range() {
    let stone = |slot: i64| json!({"slot": slot, "item": "stone"});
    assert_eq!(
        error(chest(3, stone(27))),
        "slot 27: a chest with 3 rows has slots 0 to 26"
    );
    assert_eq!(
        error(chest(1, stone(9))),
        "slot 9: a chest with 1 row has slots 0 to 8"
    );
    assert_eq!(
        error(chest(6, stone(-1))),
        "slot -1: a chest with 6 rows has slots 0 to 53"
    );
    for (kind, slot, message) in [
        ("hopper", 5, "slot 5: a hopper has slots 0 to 4"),
        ("dispenser", 9, "slot 9: a dispenser has slots 0 to 8"),
        ("anvil", 3, "slot 3: an anvil has slots 0 to 2"),
    ] {
        let spec = json!({"id": "a", "type": kind, "slots": [stone(slot)]});
        assert_eq!(error(spec), message);
    }
    let spec = json!({"id": "a", "slots": [{"slots": [0, 26, 27], "item": "stone"}]});
    assert_eq!(
        error(spec),
        "slot 27: a chest with 3 rows has slots 0 to 26"
    );
    // The last slot of each is fine.
    parse(chest(3, stone(26)));
    parse(json!({"id": "a", "type": "hopper", "slots": [stone(4)]}));
}

#[test]
fn layouts_must_fit_the_type() {
    let keys = json!({"#": {"item": "stone"}});
    assert_eq!(
        error(json!({"id": "a", "layout": ["#########", "########"], "keys": keys})),
        "menu.layout[2]: a line of a chest with 2 rows has 9 characters, not 8"
    );
    assert_eq!(
        error(json!({"id": "a", "layout": ["####Q####"], "keys": keys})),
        "menu.layout[1]: the character 'Q' has no entry in keys"
    );
    assert_eq!(
        error(json!({"id": "a", "type": "hopper", "layout": ["#####", "#####"], "keys": keys})),
        "menu.layout has 2 lines, but a hopper has 1"
    );
    assert_eq!(
        error(json!({"id": "a", "rows": 4, "layout": ["#########"], "keys": keys})),
        "menu.layout has 1 lines, but a chest with 4 rows has 4"
    );
    assert!(
        error(json!({"id": "a", "layout": vec!["#########"; 7], "keys": keys}))
            .contains("a chest has 1 to 6 rows")
    );
    assert!(
        error(
            json!({"id": "a", "type": "dispenser", "layout": ["##", "###", "###"], "keys": keys})
        )
        .contains("a line of a dispenser has 3 characters, not 2")
    );
    assert!(
        error(json!({"id": "a", "layout": ["#########"], "keys": {"##": {"item": "stone"}}}))
            .contains("\"##\" is not one character")
    );
    assert!(
        error(json!({"id": "a", "keys": {" ": {"item": "stone"}}}))
            .contains("a space is always an empty slot")
    );
    assert!(error(json!({"id": "a", "layout": [9]})).contains("menu.layout[1] must be a string"));
    // Characters, not bytes; unused keys are fine.
    let menu = parse(json!({
        "id": "a",
        "type": "hopper",
        "layout": ["é é é"],
        "keys": {"é": {"item": "stone"}, "z": {"item": "dirt"}},
    }));
    assert_eq!(item(&menu, 0), Some("stone"));
    assert_eq!(item(&menu, 1), None);
    assert_eq!(item(&menu, 4), Some("stone"));
    // An empty layout is no layout.
    assert_eq!(
        parse(json!({"id": "a", "layout": {}})).kind(),
        Kind::Chest { rows: 3 }
    );
}

#[test]
fn unknown_fields_are_errors() {
    for (spec, field) in [
        (
            json!({"id": "a", "size": 3}),
            "menu: unknown field \"size\"",
        ),
        (
            chest(1, json!({"slot": 0, "item": "stone", "amount": 2})),
            "menu.slots[1]: unknown field \"amount\"",
        ),
        (
            json!({"id": "a", "keys": {"#": {"item": "stone", "slot": 1}}}),
            "menu.keys.#: unknown field \"slot\"",
        ),
        (
            chest(
                1,
                json!({"slot": 0, "item": "stone", "model": {"ints": []}}),
            ),
            "menu.slots[1].model: unknown field \"ints\"",
        ),
        (
            chest(
                1,
                json!({"slot": 0, "item": "player_head", "head": {"name": "x"}}),
            ),
            "menu.slots[1].head: unknown field \"name\"",
        ),
    ] {
        let message = error(spec);
        assert!(message.starts_with(field), "{message}");
    }
    assert!(
        Patch::parse(&json!({"rows": 2}))
            .unwrap_err()
            .starts_with("patch: unknown field \"rows\"")
    );
    assert_eq!(error(json!([])), "menu must be an object");
    assert_eq!(error(json!({"type": "chest"})), "menu.id is required");
}

#[test]
fn items_are_named_as_in_the_newest_version() {
    let menu = parse(json!({"id": "a", "slots": [
        {"slot": 0, "item": "minecraft:diamond_sword"},
        {"slot": 1, "item": "straw_bed"},
    ]}));
    assert_eq!(item(&menu, 0), Some("diamond_sword"));
    assert_eq!(item(&menu, 1), Some("straw_bed"));
    for name in [
        "dimond_sword",
        "air",
        "minecraft:air",
        "other:stone",
        "Stone",
        "",
    ] {
        let message = error(chest(1, json!({"slot": 0, "item": name})));
        assert_eq!(
            message,
            format!("menu.slots[1].item: unknown item {name:?}")
        );
    }
    assert_eq!(
        error(chest(1, json!({"slot": 0}))),
        "menu.slots[1].item is required"
    );
}

#[test]
fn ids_counts_and_lore_have_limits() {
    for id in ["", "Upper", "with space", "ü", &"x".repeat(65)] {
        assert!(error(json!({"id": id})).starts_with("menu.id must be 1 to 64 characters"));
        assert!(
            error(chest(1, json!({"slot": 0, "item": "stone", "id": id})))
                .starts_with("menu.slots[1].id must be 1 to 64 characters")
        );
    }
    parse(json!({"id": "a-z_0.9/x"}));
    parse(json!({"id": "x".repeat(64)}));

    let count = |count: Value| chest(1, json!({"slot": 0, "item": "stone", "count": count}));
    for bad in [json!(0), json!(100), json!(1.5), json!("2")] {
        assert_eq!(
            error(count(bad)),
            "menu.slots[1].count must be an integer from 1 to 99"
        );
    }
    assert_eq!(button(&parse(count(json!(99))), 0).look.count, 99);
    assert_eq!(
        button(&parse(chest(1, json!({"slot": 0, "item": "stone"}))), 0)
            .look
            .count,
        1
    );

    let lore = |lines: usize| {
        chest(
            1,
            json!({"slot": 0, "item": "stone", "lore": vec!["line"; lines]}),
        )
    };
    assert_eq!(button(&parse(lore(MAX_LORE_LINES)), 0).look.lore.len(), 32);
    assert_eq!(error(lore(33)), "menu.slots[1].lore: at most 32 lines");
    assert_eq!(
        error(chest(
            1,
            json!({"slot": 0, "item": "stone", "lore": "one line"})
        )),
        "menu.slots[1].lore must be a list"
    );
}

#[test]
fn state_payload_and_the_whole_menu_have_size_limits() {
    // A JSON string of n bytes takes n + 2 with its quotes.
    let text = |bytes: usize| json!("x".repeat(bytes - 2));
    let menu = parse(json!({"id": "a", "state": text(MAX_STATE_BYTES)}));
    assert_eq!(menu.state(), Some(&text(MAX_STATE_BYTES)));
    assert_eq!(
        error(json!({"id": "a", "state": text(MAX_STATE_BYTES + 1)})),
        "menu.state exceeds 16 KiB"
    );
    let state = json!({"page": 2, "filter": ["a", "b"]});
    assert_eq!(
        parse(json!({"id": "a", "state": state})).state(),
        Some(&state)
    );

    let payload = |bytes| {
        chest(
            1,
            json!({"slot": 0, "item": "stone", "id": "x", "payload": text(bytes)}),
        )
    };
    parse(payload(MAX_PAYLOAD_BYTES));
    assert_eq!(
        error(payload(MAX_PAYLOAD_BYTES + 1)),
        "menu.slots[1].payload exceeds 4 KiB"
    );

    let lore: Vec<String> = vec!["y".repeat(1000); 30];
    let slots: Vec<Value> = (0..9)
        .map(|slot| json!({"slot": slot, "item": "stone", "lore": lore}))
        .collect();
    let big = json!({"id": "a", "rows": 1, "slots": slots});
    assert!(serde_json::to_vec(&big).unwrap().len() > MAX_MENU_BYTES);
    assert_eq!(error(big.clone()), "menu exceeds 256 KiB");
    assert_eq!(
        Menu::parse_json(&big.to_string()).unwrap_err(),
        "menu exceeds 256 KiB"
    );
    assert!(
        Menu::parse_json("{")
            .unwrap_err()
            .starts_with("menu: invalid JSON")
    );
    assert_eq!(Menu::parse_json(r#"{"id": "j"}"#).unwrap().id(), "j");
}

#[test]
fn clicks_default_to_the_mouse_and_need_an_id() {
    let with = |clicks: Value| {
        let menu = parse(chest(
            1,
            json!({"slot": 0, "item": "stone", "id": "x", "clicks": clicks}),
        ));
        button(&menu, 0).action.clicks
    };
    let default = button(
        &parse(chest(1, json!({"slot": 0, "item": "stone", "id": "x"}))),
        0,
    )
    .action
    .clicks;
    assert_eq!(default, Clicks::MOUSE);
    for click in [
        Click::Left,
        Click::Right,
        Click::ShiftLeft,
        Click::ShiftRight,
    ] {
        assert!(default.contains(click));
    }
    for click in [
        Click::Middle,
        Click::Drop,
        Click::DropAll,
        Click::NumberKey,
        Click::Offhand,
    ] {
        assert!(!default.contains(click));
    }
    assert_eq!(with(json!("all")), Clicks::ALL);
    assert!(Click::ALL.iter().all(|click| Clicks::ALL.contains(*click)));
    let some = with(json!(["drop", "number_key"]));
    assert!(some.contains(Click::Drop) && some.contains(Click::NumberKey));
    assert!(!some.contains(Click::Left));
    assert_eq!(with(json!([])), Clicks::NONE);
    assert_eq!(with(json!({})), Clicks::NONE);
    for click in Click::ALL {
        assert_eq!(Click::from_name(click.name()), Some(click));
    }
    let unknown = error(chest(
        1,
        json!({"slot": 0, "item": "stone", "id": "x", "clicks": ["double"]}),
    ));
    assert!(
        unknown.starts_with("menu.slots[1].clicks[1]: unknown click kind \"double\""),
        "{unknown}"
    );
    assert!(
        error(chest(
            1,
            json!({"slot": 0, "item": "stone", "id": "x", "clicks": "left"})
        ))
        .contains("must be a list of click kinds or \"all\"")
    );
    for field in ["clicks", "payload"] {
        let spec = chest(1, json!({"slot": 0, "item": "stone", field: ["left"]}));
        assert_eq!(
            error(spec),
            format!("menu.slots[1].{field}: a button without an id reports no clicks")
        );
    }
    // A close button needs no id.
    let menu = parse(chest(
        1,
        json!({"slot": 0, "item": "barrier", "close": true}),
    ));
    assert!(button(&menu, 0).action.closes(Click::Left));
}

#[test]
fn slots_replace_what_the_layout_put_there() {
    let menu = parse(json!({
        "id": "a",
        "type": "hopper",
        "layout": ["#####"],
        "keys": {"#": {"item": "stone"}},
        "slots": [
            {"slot": 1, "item": "dirt"},
            {"slots": [2, 3], "item": "sand", "id": "pick"},
            {"slot": 4, "empty": true},
        ],
    }));
    assert_eq!(item(&menu, 0), Some("stone"));
    assert_eq!(item(&menu, 1), Some("dirt"));
    assert_eq!(item(&menu, 2), Some("sand"));
    assert_eq!(item(&menu, 3), Some("sand"));
    assert_eq!(item(&menu, 4), None);
    let entry = |extra: Value| {
        let mut entry = json!({"item": "stone"});
        entry
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        error(json!({"id": "a", "slots": [entry]}))
    };
    assert_eq!(
        entry(json!({"slot": 0, "slots": [1]})),
        "menu.slots[1]: give slot or slots, not both"
    );
    assert_eq!(entry(json!({})), "menu.slots[1] needs slot or slots");
    assert_eq!(
        entry(json!({"slots": []})),
        "menu.slots[1].slots must list at least one slot"
    );
    assert_eq!(
        entry(json!({"slot": "1"})),
        "menu.slots[1].slot must be an integer"
    );
    assert_eq!(
        error(json!({"id": "a", "slots": [{"slot": 0, "empty": true, "name": "x"}]})),
        "menu.slots[1]: an empty slot takes no \"name\""
    );
    // `empty = false` is an ordinary button.
    let menu = parse(json!({"id": "a", "slots": [{"slot": 0, "empty": false, "item": "dirt"}]}));
    assert_eq!(item(&menu, 0), Some("dirt"));
}

#[test]
fn texts_are_resolved_and_checked() {
    let menu = parse(json!({
        "id": "a",
        "title": "",
        "slots": [
            {"slot": 0, "item": "stone", "name": ""},
            {"slot": 1, "item": "stone", "name": {"text": "Hi", "color": "gold"}},
            {"slot": 2, "item": "stone", "name": {"legacy": "&cRed"}, "lore": ["", {"mini": "<b>Bold"}]},
        ],
    }));
    assert_eq!(menu.title(), None);
    assert_eq!(button(&menu, 0).look.name, Some(Text::Plain(String::new())));
    assert_eq!(item(&menu, 3), None);
    let Some(Text::Component(red)) = &button(&menu, 2).look.name else {
        panic!();
    };
    assert!(red.to_string().contains("red"), "{red}");
    let lore = &button(&menu, 2).look.lore;
    assert_eq!(lore[0], Text::Plain(String::new()));
    assert!(matches!(&lore[1], Text::Component(bold) if bold.to_string().contains("bold")));
    assert_eq!(
        button(&parse(chest(1, json!({"slot": 0, "item": "stone"}))), 0)
            .look
            .name,
        None
    );
    assert!(
        error(chest(1, json!({"slot": 0, "item": "stone", "name": 5})))
            .starts_with("menu.slots[1].name")
    );
    assert!(error(json!({"id": "a", "title": {"mini": ""}})).starts_with("menu.title"));
}

#[test]
fn looks_take_models_heads_and_colours() {
    let look = |extra: Value| {
        let mut entry = json!({"slot": 0, "item": "stone"});
        entry
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        button(&parse(chest(1, entry)), 0).look.clone()
    };
    assert_eq!(
        look(json!({"model": 7})).model,
        Some(Model {
            floats: vec![7.0],
            flags: vec![],
            strings: vec![],
            colors: vec![]
        })
    );
    assert_eq!(
        look(json!({"model": {"floats": [1.5], "flags": [true], "strings": ["s"], "colors": ["#00ff00", 255]}})).model,
        Some(Model {
            floats: vec![1.5],
            flags: vec![true],
            strings: vec!["s".into()],
            colors: vec![0x00ff00, 255],
        })
    );
    let flags: Vec<bool> = vec![true; 65];
    assert!(
        error(chest(
            1,
            json!({"slot": 0, "item": "stone", "model": {"flags": flags}})
        ))
        .contains("at most 64 values")
    );
    assert!(
        error(chest(
            1,
            json!({"slot": 0, "item": "stone", "model": {"colors": ["red"]}})
        ))
        .contains("must be a colour")
    );
    assert!(
        error(chest(
            1,
            json!({"slot": 0, "item": "stone", "model": "seven"})
        ))
        .contains("must be an object")
    );

    assert_eq!(
        look(json!({"item_model": "rift:gem"}))
            .item_model
            .as_deref(),
        Some("rift:gem")
    );
    assert!(
        error(chest(
            1,
            json!({"slot": 0, "item": "stone", "item_model": "Bad Model"})
        ))
        .contains("is not a valid identifier")
    );

    assert_eq!(look(json!({"glint": false})).glint, Some(false));
    assert_eq!(look(json!({})).glint, None);
    assert!(!look(json!({})).details && look(json!({})).tooltip);
    let shown = look(json!({"details": true, "tooltip": false}));
    assert!(shown.details && !shown.tooltip);

    assert_eq!(look(json!({"color": "#A06540"})).color, Some(0xa06540));
    for bad in ["A06540", "#a0654", "#gg0000", "red"] {
        assert!(
            error(chest(1, json!({"slot": 0, "item": "stone", "color": bad})))
                .contains("must be a colour")
        );
    }

    let head = |head: Value| {
        let spec = chest(1, json!({"slot": 0, "item": "player_head", "head": head}));
        Menu::parse(&spec).map(|menu| button(&menu, 0).look.head.clone())
    };
    assert_eq!(
        head(json!({"texture": "abc", "signature": "sig"})).unwrap(),
        Some(Head::Texture(Textures {
            value: "abc".into(),
            signature: Some("sig".into())
        }))
    );
    assert_eq!(
        head(json!({"player": "Notch"})).unwrap(),
        Some(Head::Player("Notch".into()))
    );
    assert_eq!(
        head(json!({"player": "069a79f4-44e9-4726-a5be-fca90e38aaf5"})).unwrap(),
        Some(Head::Player("069a79f4-44e9-4726-a5be-fca90e38aaf5".into()))
    );
    for bad in [
        json!({}),
        json!({"texture": "a", "player": "b"}),
        json!({"texture": ""}),
        json!({"player": "has space"}),
        json!({"player": "Notch", "signature": "x"}),
    ] {
        assert!(head(bad.clone()).is_err(), "{bad}");
    }
    assert_eq!(
        error(chest(
            1,
            json!({"slot": 0, "item": "stone", "head": {"player": "Notch"}})
        )),
        "menu.slots[1].head is for player_head only"
    );
}

#[test]
fn an_anvil_without_buttons_is_a_text_prompt() {
    let mut menu = parse(
        json!({"id": "nick", "type": "anvil", "title": "Choose a nickname", "text": "Steve"}),
    );
    let left = button(&menu, ANVIL_LEFT);
    assert_eq!(left.look.item, "paper");
    assert_eq!(left.look.name, Some(Text::Plain("Steve".into())));
    assert!(left.action.id.is_none());
    assert!(menu.slots()[ANVIL_RIGHT].is_none());
    let result = button(&menu, ANVIL_RESULT);
    assert_eq!(result.look.item, "paper");
    assert_eq!(result.look.name, Some(Text::Plain("Steve".into())));
    assert_eq!(result.action.id.as_deref(), Some(ANVIL_SUBMIT));
    assert!(result.action.reports(Click::Left));
    assert_eq!(menu.text(), "Steve");
    assert!(!menu.typing());

    // Typing renames the result, not the left item (sending that again
    // would reset the field).
    let before = menu.clone();
    menu.typed("Alex".into());
    assert_eq!(menu.text(), "Alex");
    assert_eq!(
        button(&menu, ANVIL_RESULT).look.name,
        Some(Text::Plain("Alex".into()))
    );
    assert_eq!(
        button(&menu, ANVIL_LEFT).look.name,
        Some(Text::Plain("Steve".into()))
    );
    let diff = before.diff(&menu);
    assert_eq!(diff.slots, vec![ANVIL_RESULT]);
    assert!(diff.actions.is_empty());

    // A redraw keeps what the player typed.
    let mut redrawn =
        parse(json!({"id": "nick", "type": "anvil", "title": "Try again", "text": "Steve"}));
    redrawn.keep_text(&menu);
    assert_eq!(redrawn.text(), "Alex");
    let diff = menu.diff(&redrawn);
    assert!(diff.title && diff.slots.is_empty(), "{diff:?}");

    // Only a patch with text replaces it, and resets the field.
    redrawn.apply(&patch(json!({"text": "Bob"}))).unwrap();
    assert_eq!(redrawn.text(), "Bob");
    assert_eq!(
        button(&redrawn, ANVIL_LEFT).look.name,
        Some(Text::Plain("Bob".into()))
    );
    assert_eq!(
        button(&redrawn, ANVIL_RESULT).look.name,
        Some(Text::Plain("Bob".into()))
    );

    let empty = parse(json!({"id": "nick", "type": "anvil", "typing": true}));
    assert_eq!(empty.text(), "");
    assert!(empty.typing());
    assert_eq!(
        button(&empty, ANVIL_LEFT).look.name,
        Some(Text::Plain(String::new()))
    );
}

#[test]
fn an_anvil_with_buttons_keeps_its_left_slot_filled() {
    let menu = parse(json!({
        "id": "a",
        "type": "anvil",
        "text": "x",
        "layout": ["P X"],
        "keys": {
            "P": {"item": "name_tag", "name": "ignored"},
            "X": {"item": "barrier", "close": true},
        },
    }));
    assert_eq!(item(&menu, ANVIL_LEFT), Some("name_tag"));
    assert_eq!(
        button(&menu, ANVIL_LEFT).look.name,
        Some(Text::Plain("x".into()))
    );
    assert_eq!(item(&menu, ANVIL_RIGHT), None);
    assert_eq!(item(&menu, ANVIL_RESULT), Some("barrier"));
    assert!(button(&menu, ANVIL_RESULT).action.id.is_none());

    let only_result = parse(
        json!({"id": "a", "type": "anvil", "slots": [{"slot": 2, "item": "lime_dye", "id": "ok"}]}),
    );
    assert_eq!(item(&only_result, ANVIL_LEFT), Some("paper"));
    assert_eq!(item(&only_result, ANVIL_RESULT), Some("lime_dye"));
    let mut typed = only_result.clone();
    typed.typed("abc".into());
    assert_eq!(button(&typed, ANVIL_RESULT).look.name, None);

    // The left slot cannot be emptied.
    let mut cleared = menu.clone();
    cleared
        .apply(&patch(json!({"slots": [{"slot": 0, "empty": true}]})))
        .unwrap();
    assert_eq!(item(&cleared, ANVIL_LEFT), Some("paper"));
}

#[test]
fn an_anvil_result_is_never_empty() {
    // A left button alone gets the default result, a copy of it.
    let left_only = parse(json!({
        "id": "a", "type": "anvil", "text": "x", "layout": ["P  "],
        "keys": {"P": {"item": "name_tag"}},
    }));
    let result = button(&left_only, ANVIL_RESULT);
    assert_eq!(result.look.item, "name_tag");
    assert_eq!(result.look.name, Some(Text::Plain("x".into())));
    assert_eq!(result.action.id.as_deref(), Some(ANVIL_SUBMIT));
    // Emptying a custom result brings the default back.
    let mut custom = parse(
        json!({"id": "a", "type": "anvil", "slots": [{"slot": 2, "item": "lime_dye", "id": "ok"}]}),
    );
    custom
        .apply(&patch(json!({"slots": [{"slot": 2, "empty": true}]})))
        .unwrap();
    assert_eq!(item(&custom, ANVIL_RESULT), Some("paper"));
    custom.typed("abc".into());
    assert_eq!(
        button(&custom, ANVIL_RESULT).look.name,
        Some(Text::Plain("abc".into()))
    );
}

#[test]
fn every_anvil_event_carries_the_text() {
    let mut menu = parse(json!({"id": "nick", "type": "anvil", "text": "Steve", "state": 1}));
    menu.typed("Alex".into());
    let click = MenuEvent::click(&menu, ANVIL_RESULT, Click::Left, None);
    assert_eq!(click.text.as_deref(), Some("Alex"));
    assert_eq!(click.action.as_deref(), Some(ANVIL_SUBMIT));
    let text = MenuEvent::text(&menu);
    assert_eq!(text.kind, EventKind::Text);
    assert_eq!(
        (text.text.as_deref(), text.state),
        (Some("Alex"), Some(json!(1)))
    );
    let close = MenuEvent::close(&menu, CloseReason::Player);
    assert_eq!(close.text.as_deref(), Some("Alex"));
    // Other menus carry none.
    let chest = parse(json!({"id": "a"}));
    assert_eq!(MenuEvent::close(&chest, CloseReason::Player).text, None);
}

#[test]
fn the_field_follows_the_text_only_when_rift_sends_the_left_slot() {
    let mut menu = parse(json!({"id": "nick", "type": "anvil", "text": "Steve"}));
    menu.typed("Alex".into());
    assert_eq!((menu.text(), menu.field()), ("Alex", "Steve"));
    menu.field_from_text();
    assert_eq!(menu.field(), "Alex");
    assert_eq!(
        button(&menu, ANVIL_LEFT).look.name,
        Some(Text::Plain("Alex".into()))
    );
    // A client before 1.17 takes 35 characters.
    let mut long = parse(json!({"id": "a", "type": "anvil", "text": "x".repeat(50)}));
    long.fit_text(754);
    assert_eq!((long.text().len(), long.field().len()), (35, 35));
    long.fit_text(777);
    assert_eq!(long.text().len(), 35);
}

#[test]
fn renames_are_filtered_and_over_long_ones_ignored() {
    assert_eq!(anvil_rename(777, "a\u{7}b§c").as_deref(), Some("abc"));
    assert_eq!(anvil_rename(777, "").as_deref(), Some(""));
    assert_eq!(
        anvil_rename(777, &"x".repeat(50)).map(|text| text.len()),
        Some(50)
    );
    assert_eq!(anvil_rename(777, &"x".repeat(51)), None);
    assert_eq!(
        anvil_rename(754, &"x".repeat(35)).map(|text| text.len()),
        Some(35)
    );
    assert_eq!(anvil_rename(754, &"x".repeat(36)), None);
    assert_eq!(
        anvil_rename(335, &format!("{}§§", "x".repeat(35))).map(|text| text.len()),
        Some(35)
    );
}

#[test]
fn anvil_fields_are_checked() {
    assert_eq!(
        error(json!({"id": "a", "text": "x"})),
        "menu.text is for anvils only"
    );
    assert_eq!(
        error(json!({"id": "a", "type": "hopper", "typing": true})),
        "menu.typing is for anvils only"
    );
    assert_eq!(
        error(json!({"id": "a", "type": "anvil", "rows": 1})),
        "menu.rows is for chests only"
    );
    assert_eq!(
        error(json!({"id": "a", "type": "anvil", "text": "a\u{7}b"})),
        "menu.text must not contain control characters or '§'"
    );
    assert!(error(json!({"id": "a", "type": "anvil", "text": "§cred"})).contains("'§'"));
    assert_eq!(
        error(json!({"id": "a", "type": "anvil", "text": "x".repeat(51)})),
        "menu.text is longer than an anvil takes (50 characters)"
    );
    parse(json!({"id": "a", "type": "anvil", "text": "x".repeat(50)}));
    assert!(
        error(json!({"id": "a", "type": "furnace"}))
            .starts_with("menu.type: unknown type \"furnace\"")
    );
    let chest = parse(json!({"id": "a"}));
    let mut changed = chest.clone();
    assert_eq!(
        changed.apply(&patch(json!({"text": "x"}))).unwrap_err(),
        "patch.text is for anvils only"
    );
    changed.typed("ignored".into());
    assert_eq!(changed, chest);
}

#[test]
fn anvil_text_is_cleaned_to_the_client_limit() {
    assert_eq!(anvil_text_limit(754), ANVIL_TEXT_MAX_BEFORE_1_17);
    assert_eq!(anvil_text_limit(755), ANVIL_TEXT_MAX);
    assert_eq!(anvil_text_limit(335), 35);
    assert_eq!(anvil_text_limit(777), 50);
    let long = "x".repeat(60);
    assert_eq!(clean_anvil_text(754, &long).len(), 35);
    assert_eq!(clean_anvil_text(777, &long).len(), 50);
    assert_eq!(clean_anvil_text(777, "a\u{0}b\nc\u{7f}d§e\tf"), "abcdef");
    assert_eq!(clean_anvil_text(777, "Grüße"), "Grüße");
    // The game counts UTF-16 units: a character outside the BMP takes two.
    let emoji = "😀".repeat(30);
    assert_eq!(clean_anvil_text(777, &emoji).chars().count(), 25);
    assert_eq!(
        clean_anvil_text(754, &format!("{}😀", "x".repeat(34))),
        "x".repeat(34)
    );
}

#[test]
fn patches_change_slots_keys_title_and_state() {
    let mut menu = parse(json!({
        "id": "a",
        "rows": 1,
        "layout": ["#C#######"],
        "keys": {"#": {"item": "stone"}, "C": {"item": "clock", "name": "10"}},
        "slots": [{"slot": 2, "item": "dirt"}],
        "state": {"page": 1},
    }));
    menu.apply(&patch(json!({
        "keys": {"#": {"item": "glass"}},
        "slots": [{"slot": 1, "item": "clock", "name": "9"}, {"slot": 3, "empty": true}],
        "title": {"text": "Countdown"},
        "state": {"page": 2},
    })))
    .unwrap();
    // `keys` reaches the slots the layout gave `#`, but not slot 2, which
    // `slots` replaced, nor slot 3, which the patch emptied.
    assert_eq!(item(&menu, 0), Some("glass"));
    assert_eq!(item(&menu, 2), Some("dirt"));
    assert_eq!(item(&menu, 3), None);
    assert_eq!(item(&menu, 8), Some("glass"));
    assert_eq!(
        button(&menu, 1).look.name,
        Some(Text::component(json!("9"), "x").unwrap())
    );
    assert!(menu.title().is_some());
    assert_eq!(menu.state(), Some(&json!({"page": 2})));
    // Slot 1 no longer belongs to `C`.
    menu.apply(&patch(json!({"keys": {"C": {"item": "barrier"}}})))
        .unwrap();
    assert_eq!(item(&menu, 1), Some("clock"));
    // An empty title clears it; a patch without one keeps it.
    menu.apply(&patch(json!({}))).unwrap();
    assert!(menu.title().is_some());
    menu.apply(&patch(json!({"title": ""}))).unwrap();
    assert_eq!(menu.title(), None);
}

#[test]
fn a_patch_outside_the_menu_changes_nothing() {
    let menu = parse(json!({"id": "a", "rows": 1}));
    let mut changed = menu.clone();
    let error = changed
        .apply(&patch(
            json!({"slots": [{"slot": 0, "item": "stone"}, {"slot": 9, "item": "stone"}]}),
        ))
        .unwrap_err();
    assert_eq!(error, "slot 9: a chest with 1 row has slots 0 to 8");
    assert_eq!(changed, menu);
    assert!(
        Patch::parse(&json!({"slots": [{"slot": 0, "item": "nope"}]}))
            .unwrap_err()
            .contains("unknown item")
    );
    assert!(Patch::parse(&json!({"text": "\n"})).is_err());
    assert!(
        Patch::parse_json("[")
            .unwrap_err()
            .starts_with("patch: invalid JSON")
    );
    assert!(Patch::parse(&json!({"state": "x".repeat(MAX_STATE_BYTES)})).is_err());
}

#[test]
fn diffs_separate_looks_from_actions() {
    let base = json!({
        "id": "shop",
        "rows": 1,
        "title": "Shop",
        "slots": [
            {"slot": 0, "item": "clock", "name": "10s"},
            {"slot": 1, "item": "emerald", "id": "buy", "payload": {"item": 1}},
            {"slot": 2, "item": "arrow", "id": "page"},
        ],
    });
    let old = parse(base.clone());
    assert!(old.diff(&old.clone()).is_empty());

    let mut new = old.clone();
    new.apply(&patch(json!({"slots": [
        // Only the look changes: still reported, no stale clicks.
        {"slot": 0, "item": "clock", "name": "9s"},
        // Only the payload changes: the same picture, but a new action.
        {"slot": 1, "item": "emerald", "id": "buy", "payload": {"item": 2}},
        // A decoration where nothing was: a new look, the same (no) action.
        {"slot": 3, "item": "stone"},
        // A button where nothing was: both.
        {"slot": 4, "item": "stone", "close": true},
    ]})))
    .unwrap();
    let diff = old.diff(&new);
    assert!(!diff.reopen && !diff.title);
    assert_eq!(diff.slots, vec![0, 3, 4]);
    assert_eq!(diff.actions, vec![1, 4]);

    // Clicks are not part of what a click does.
    let mut clicks = old.clone();
    clicks
        .apply(&patch(
            json!({"slots": [{"slot": 2, "item": "arrow", "id": "page", "clicks": "all"}]}),
        ))
        .unwrap();
    assert!(old.diff(&clicks).is_empty());

    let mut titled = old.clone();
    titled.apply(&patch(json!({"title": "Shop 2"}))).unwrap();
    assert_eq!(
        old.diff(&titled),
        Diff {
            title: true,
            ..Diff::default()
        }
    );

    let mut bigger = base.clone();
    bigger["rows"] = json!(2);
    let diff = old.diff(&parse(bigger));
    assert!(diff.reopen);
    assert_eq!(diff.slots, (0..18).collect::<Vec<_>>());
    assert!(
        old.diff(&parse(json!({"id": "shop", "type": "dispenser"})))
            .reopen
    );
}
