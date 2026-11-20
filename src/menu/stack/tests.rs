//! Buttons converted for every client: each stack is decoded again with the
//! translation crate's reader for that protocol, and the item, count, name
//! and lore are read back in that version's own form.
use super::*;
use crate::menu::{Button, Menu};
use rift_translation::{codec::item::components, version::PROTOCOLS, wire::Reader};
use serde_json::{Value as Json, json};

fn button(spec: Json) -> Button {
    let mut entry = json!({"slot": 0});
    entry
        .as_object_mut()
        .unwrap()
        .extend(spec.as_object().unwrap().clone());
    let menu = Menu::parse(&json!({"id": "test", "rows": 1, "slots": [entry]}))
        .unwrap_or_else(|error| panic!("{error}"));
    menu.slots()[0].clone().unwrap()
}

fn no_heads(_: &str) -> Option<Textures> {
    None
}

fn newest(spec: Json) -> ItemStack {
    button(spec).look.stack(&no_heads).unwrap()
}

/// The button as a client on `protocol` gets it, decoded again.
fn sent(protocol: i32, spec: Json) -> ItemStack {
    decode(protocol, &encode(protocol, Some(&newest(spec))).unwrap())
}

fn decode(protocol: i32, bytes: &[u8]) -> ItemStack {
    let mut reader = Reader::new(bytes);
    let stack = item::read(protocol, &mut reader).unwrap().expect("a stack");
    reader.finish().unwrap();
    stack
}

fn protocols() -> impl Iterator<Item = i32> {
    PROTOCOLS.iter().map(|(protocol, _)| *protocol)
}

fn item_name(protocol: i32, stack: &ItemStack) -> &'static str {
    rift_translation::data::version(protocol)
        .registry("item")
        .unwrap()
        .name(stack.id)
        .unwrap()
}

fn tag(stack: &ItemStack) -> &Compound {
    stack.tag.as_ref().unwrap().as_compound().unwrap()
}

fn display(stack: &ItemStack) -> &Compound {
    tag(stack).compound("display").unwrap()
}

fn patch(stack: &ItemStack) -> &Components {
    stack.components.as_ref().unwrap()
}

/// A text component of a 1.20.5+ patch.
fn component(protocol: i32, value: &Value) -> wire::Component {
    let Value::Nbt(bytes) = value else {
        panic!("{value:?}");
    };
    let nbt = Nbt::from_bytes(bytes, false).unwrap().unwrap();
    wire::from_nbt(protocol, &nbt).unwrap()
}

/// What a client on `protocol` shows as the name and lore: words, whether
/// italic, and the colour of the first run, read in the version's own form.
#[derive(Debug, PartialEq)]
struct Shown {
    words: String,
    italic: bool,
}

fn shown_json(protocol: i32, json: &str) -> Shown {
    let component = wire::from_json_str(protocol, json).unwrap();
    Shown {
        words: component.plain_text(),
        italic: component.style.italic.unwrap_or(true),
    }
}

/// A `§` string the game prefixes with italics, as the legacy renderer
/// shows its first word: a colour code or a reset ends the italics, `§o`
/// starts them again.
fn shown_legacy(text: &str) -> Shown {
    let mut italic = true;
    let mut first = None;
    let mut words = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '§' {
            first.get_or_insert(italic);
            words.push(c);
            continue;
        }
        match chars.next() {
            Some(code) if code.is_ascii_hexdigit() || code == 'r' => italic = false,
            Some('o') => italic = true,
            _ => {}
        }
    }
    Shown {
        words,
        italic: first.unwrap_or(italic),
    }
}

fn name_and_lore(protocol: i32, stack: &ItemStack) -> (Option<Shown>, Vec<Shown>) {
    if version::item_components(protocol) {
        let patch = patch(stack);
        let shown = |value: &Value| {
            let component = component(protocol, value);
            Shown {
                words: component.plain_text(),
                italic: component.style.italic.unwrap_or(true),
            }
        };
        let name = patch.get("custom_name").map(shown);
        let lore = match patch.get("lore") {
            Some(Value::List(lines)) => lines.iter().map(shown).collect(),
            _ => Vec::new(),
        };
        return (name, lore);
    }
    let Some(display) = stack
        .tag
        .as_ref()
        .and_then(Nbt::as_compound)
        .and_then(|tag| tag.compound("display"))
    else {
        return (None, Vec::new());
    };
    let name = display.str("Name").map(|name| {
        if protocol < JSON_NAMES {
            shown_legacy(name)
        } else {
            shown_json(protocol, name)
        }
    });
    let lore = display
        .list("Lore")
        .unwrap_or(&[])
        .iter()
        .map(|line| {
            let line = line.as_str().unwrap();
            if protocol < JSON_LORE {
                shown_legacy(line)
            } else {
                shown_json(protocol, line)
            }
        })
        .collect();
    (name, lore)
}

fn upright(words: &str) -> Shown {
    Shown {
        words: words.into(),
        italic: false,
    }
}

#[test]
fn names_and_lore_are_upright_in_every_version() {
    let spec = json!({
        "item": "diamond_sword",
        "count": 1,
        "name": "Blade",
        "lore": ["first", {"text": "second", "color": "gold"}, {"text": "slanted", "italic": true}],
    });
    for protocol in protocols() {
        let stack = sent(protocol, spec.clone());
        assert_eq!(item_name(protocol, &stack), "diamond_sword", "{protocol}");
        assert_eq!(stack.count, 1);
        let (name, lore) = name_and_lore(protocol, &stack);
        assert_eq!(name, Some(upright("Blade")), "{protocol}");
        assert_eq!(
            lore,
            vec![
                upright("first"),
                upright("second"),
                Shown {
                    words: "slanted".into(),
                    italic: true
                }
            ],
            "{protocol}"
        );
    }
    // The form changes along the way.
    assert_eq!(
        display(&sent(340, spec.clone())).str("Name"),
        Some("§rBlade")
    );
    let lore = display(&sent(404, spec.clone()))
        .list("Lore")
        .unwrap()
        .to_vec();
    assert_eq!(lore[0].as_str(), Some("§5first"));
    assert_eq!(lore[1].as_str(), Some("§6second"));
    let json_name = display(&sent(765, spec.clone()))
        .str("Name")
        .unwrap()
        .to_owned();
    assert!(json_name.contains(r#""italic":false"#), "{json_name}");
    assert!(patch(&sent(766, spec)).get("custom_name").is_some());
}

#[test]
fn coloured_names_keep_their_colour_on_legacy_clients() {
    let stack = sent(
        340,
        json!({"item": "stone", "name": {"mini": "<red>Danger"}}),
    );
    assert_eq!(display(&stack).str("Name"), Some("§cDanger"));
}

#[test]
fn old_clients_get_numeric_ids_and_data_values() {
    for (item, id, damage) in [
        ("red_wool", 35, 14),
        ("lime_stained_glass_pane", 160, 5),
        ("diamond_sword", 276, 0),
        ("player_head", 397, 3),
    ] {
        for protocol in [335, 338, 340] {
            let stack = sent(protocol, json!({"item": item}));
            assert_eq!(
                (stack.id, stack.damage),
                (id, damage),
                "{item} on {protocol}"
            );
        }
        let flat = sent(393, json!({"item": item}));
        assert_eq!((item_name(393, &flat), flat.damage), (item, 0));
    }
}

#[test]
fn items_a_client_lacks_become_named_stand_ins() {
    // 1.12 has no trident, 1.20.6 no music disc "Creator", 26.2 no straw
    // bed.
    let trident = sent(340, json!({"item": "trident"}));
    assert_eq!(item_name(340, &trident), "stone");
    assert_eq!(name_and_lore(340, &trident).0, Some(upright("Trident")));
    let trident = sent(393, json!({"item": "trident"}));
    assert_eq!(item_name(393, &trident), "trident");
    let disc = sent(766, json!({"item": "music_disc_creator"}));
    assert_ne!(item_name(766, &disc), "music_disc_creator");
    assert!(patch(&disc).get("item_name").is_some());
    let disc = sent(765, json!({"item": "music_disc_creator", "name": "Song"}));
    assert_eq!(name_and_lore(765, &disc).0, Some(upright("Song")));
    let bed = sent(776, json!({"item": "straw_bed"}));
    assert_eq!(item_name(776, &bed), "yellow_bed");
    // Whatever the conversions stash for the way back is not sent.
    for protocol in protocols() {
        let bytes = encode(
            protocol,
            Some(&newest(json!({"item": "straw_bed", "glint": true}))),
        )
        .unwrap();
        assert!(
            !bytes.windows(5).any(|window| window == b"rift:"),
            "{protocol}"
        );
    }
}

#[test]
fn counts_above_64_need_components() {
    for protocol in protocols() {
        let stack = sent(protocol, json!({"item": "stone", "count": 99}));
        let expected = if version::item_components(protocol) {
            99
        } else {
            64
        };
        assert_eq!(stack.count, expected, "{protocol}");
        assert_eq!(
            sent(protocol, json!({"item": "stone", "count": 7})).count,
            7
        );
    }
}

#[test]
fn details_are_hidden_unless_asked_for() {
    for protocol in protocols() {
        let hidden = sent(protocol, json!({"item": "diamond_sword"}));
        let shown = sent(protocol, json!({"item": "diamond_sword", "details": true}));
        if version::item_components(protocol) {
            let patch = patch(&hidden);
            if protocol >= 770 {
                let Some(Value::Struct(fields)) = patch.get("tooltip_display") else {
                    panic!("{protocol}: {patch:?}");
                };
                assert_eq!(fields[0], Value::Bool(false));
                let Value::List(kinds) = &fields[1] else {
                    panic!();
                };
                assert!(kinds.contains(&Value::Kind("attribute_modifiers")));
                assert!(kinds.contains(&Value::Kind("enchantments")));
            } else {
                // Before 1.21.5 the modifiers carry their own flag.
                let Some(Value::Struct(modifiers)) = patch.get("attribute_modifiers") else {
                    panic!("{protocol}: {patch:?}");
                };
                assert_eq!(modifiers[0], Value::List(Vec::new()));
                assert_eq!(modifiers[1], Value::Bool(false));
                assert!(patch.get("hide_additional_tooltip").is_some());
            }
            assert!(
                shown.components.as_ref().is_none_or(Components::is_empty),
                "{protocol}: {shown:?}"
            );
        } else {
            assert_eq!(tag(&hidden).int("HideFlags"), Some(HIDE_ALL), "{protocol}");
            assert_eq!(tag(&hidden).list("AttributeModifiers"), Some(&[][..]));
            // Damageable items carry their damage from 1.13 on, as vanilla
            // sends them.
            let tag = shown.tag.as_ref().and_then(Nbt::as_compound);
            assert!(
                tag.is_none_or(|tag| tag.iter().all(|(key, _)| key == "Damage")),
                "{protocol}: {shown:?}"
            );
        }
    }
}

#[test]
fn the_tooltip_can_be_hidden_or_left_with_an_empty_name() {
    let spec = json!({"item": "stone", "name": "Hidden", "lore": ["x"], "tooltip": false, "details": true});
    for protocol in protocols() {
        let stack = sent(protocol, spec.clone());
        if version::item_components(protocol) {
            let patch = patch(&stack);
            if protocol >= 770 {
                let Some(Value::Struct(fields)) = patch.get("tooltip_display") else {
                    panic!("{protocol}");
                };
                assert_eq!(fields[0], Value::Bool(true));
            } else {
                assert!(patch.get("hide_tooltip").is_some(), "{protocol}");
            }
            assert_eq!(name_and_lore(protocol, &stack).0, Some(upright("Hidden")));
        } else {
            let (name, lore) = name_and_lore(protocol, &stack);
            assert_eq!(
                name.map(|name| name.words),
                Some(String::new()),
                "{protocol}"
            );
            assert!(lore.is_empty());
            assert_eq!(tag(&stack).int("HideFlags"), Some(HIDE_ALL));
        }
    }
}

#[test]
fn glints_reach_every_version() {
    for protocol in protocols() {
        let stack = sent(
            protocol,
            json!({"item": "paper", "glint": true, "details": true}),
        );
        if version::item_components(protocol) {
            assert_eq!(
                patch(&stack).get("enchantment_glint_override"),
                Some(&Value::Bool(true))
            );
        } else if version::flattened(protocol) {
            // An enchantment that names none: a glint and no line.
            let list = tag(&stack).list("Enchantments").unwrap();
            assert_eq!(list.len(), 1, "{protocol}");
        } else {
            let list = tag(&stack).list("ench").unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(tag(&stack).int("HideFlags"), Some(HIDE_ENCHANTMENTS));
        }
        let plain = sent(
            protocol,
            json!({"item": "paper", "glint": false, "details": true}),
        );
        if version::item_components(protocol) {
            assert_eq!(
                patch(&plain).get("enchantment_glint_override"),
                Some(&Value::Bool(false))
            );
        } else {
            assert!(plain.tag.is_none(), "{protocol}: {plain:?}");
        }
    }
}

#[test]
fn models_reach_the_versions_that_have_them() {
    for protocol in protocols() {
        let stack = sent(
            protocol,
            json!({"item": "stick", "model": 7, "item_model": "rift:wand", "details": true}),
        );
        match protocol {
            769.. => {
                let Some(Value::Struct(fields)) = patch(&stack).get("custom_model_data") else {
                    panic!();
                };
                assert_eq!(fields[0], Value::List(vec![Value::Float(7.0)]));
            }
            766..769 => assert_eq!(patch(&stack).get("custom_model_data"), Some(&Value::Int(7))),
            477..766 => assert_eq!(tag(&stack).int("CustomModelData"), Some(7)),
            _ => {}
        }
        if protocol >= 768 {
            assert_eq!(
                patch(&stack).get("item_model"),
                Some(&Value::String("rift:wand".into()))
            );
        } else if version::item_components(protocol) {
            assert!(patch(&stack).get("item_model").is_none());
        }
    }
    let lists = newest(
        json!({"item": "stick", "model": {"floats": [1.0, 2.0], "strings": ["a"], "colors": ["#ff0000"]}}),
    );
    assert_eq!(
        patch(&lists).get("custom_model_data"),
        Some(&Value::Struct(vec![
            Value::List(vec![Value::Float(1.0), Value::Float(2.0)]),
            Value::List(vec![]),
            Value::List(vec![Value::String("a".into())]),
            Value::List(vec![Value::Int(0xff0000)]),
        ]))
    );
}

#[test]
fn heads_carry_their_texture_to_every_version() {
    let textures = Textures {
        value: "ZXhhbXBsZQ==".into(),
        signature: Some("c2ln".into()),
    };
    let heads = |player: &str| (player == "Notch").then(|| textures.clone());
    for spec in [
        json!({"item": "player_head", "head": {"texture": "ZXhhbXBsZQ==", "signature": "c2ln"}, "details": true}),
        json!({"item": "player_head", "head": {"player": "Notch"}, "details": true}),
    ] {
        let stack = button(spec).look.stack(&heads).unwrap();
        for protocol in protocols() {
            let sent = decode(protocol, &encode(protocol, Some(&stack)).unwrap());
            if version::item_components(protocol) {
                assert!(patch(&sent).get("profile").is_some(), "{protocol}");
                let bytes = encode(protocol, Some(&stack)).unwrap();
                assert!(bytes.windows(12).any(|window| window == b"ZXhhbXBsZQ=="));
            } else {
                let owner = tag(&sent).compound("SkullOwner").unwrap();
                assert!(owner.contains("Id"), "{protocol}: {owner:?}");
                let property = owner
                    .compound("Properties")
                    .unwrap()
                    .list("textures")
                    .unwrap()[0]
                    .as_compound()
                    .unwrap();
                assert_eq!(property.str("Value"), Some("ZXhhbXBsZQ=="), "{protocol}");
                assert_eq!(property.str("Signature"), Some("c2ln"));
            }
        }
    }
    // An offline player: the default skin.
    let offline =
        button(json!({"item": "player_head", "head": {"player": "Herobrine"}, "details": true}));
    assert!(
        offline
            .look
            .stack(&heads)
            .unwrap()
            .components
            .unwrap()
            .is_empty()
    );
    // The same texture always gets the same profile ID.
    let a = newest(json!({"item": "player_head", "head": {"texture": "abc"}}));
    let b = newest(json!({"item": "player_head", "head": {"texture": "abc"}}));
    assert_eq!(a, b);
}

#[test]
fn colours_dye_items_and_potions() {
    let leather = |protocol| {
        sent(
            protocol,
            json!({"item": "leather_chestplate", "color": "#A06540", "details": true}),
        )
    };
    assert_eq!(
        patch(&leather(777)).get("dyed_color"),
        Some(&Value::Int(0xa06540))
    );
    assert_eq!(display(&leather(765)).int("color"), Some(0xa06540));
    assert_eq!(display(&leather(340)).int("color"), Some(0xa06540));
    let potion = |protocol| {
        sent(
            protocol,
            json!({"item": "potion", "color": "#00FF00", "details": true}),
        )
    };
    let Some(Value::Struct(contents)) = patch(&potion(777)).get("potion_contents").cloned() else {
        panic!();
    };
    assert_eq!(contents[1], Value::some(Value::Int(0x00ff00)));
    assert_eq!(tag(&potion(765)).int("CustomPotionColor"), Some(0x00ff00));
    assert_eq!(tag(&potion(340)).int("CustomPotionColor"), Some(0x00ff00));
}

#[test]
fn the_newest_stack_is_what_the_button_says() {
    let stack =
        newest(json!({"item": "minecraft:emerald", "count": 3, "name": "Buy", "glint": true}));
    assert_eq!(newest::item_name(stack.id), Some("emerald"));
    assert_eq!(stack.count, 3);
    let patch = patch(&stack);
    let name = component(NEWEST, patch.get("custom_name").unwrap());
    assert_eq!(name.plain_text(), "Buy");
    assert_eq!(name.style.italic, Some(false));
    assert_eq!(
        patch.get("attribute_modifiers"),
        Some(&Value::List(Vec::new()))
    );
    assert!(patch.get("tooltip_display").is_some());
    // An explicitly italic name stays italic.
    let slanted = newest(json!({"item": "stone", "name": {"text": "x", "italic": true}}));
    let name = component(NEWEST, components_of(&slanted).get("custom_name").unwrap());
    assert_eq!(name.style.italic, Some(true));
}

fn components_of(stack: &ItemStack) -> &components::Components {
    stack.components.as_ref().unwrap()
}

#[test]
fn the_cache_converts_each_stack_once_per_protocol() {
    let cache = StackCache::new(3);
    let stone = newest(json!({"item": "stone", "name": "A"}));
    let first = cache.encode(340, Some(&stone)).unwrap();
    let again = cache.encode(340, Some(&stone)).unwrap();
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(cache.len(), 1);
    assert_eq!(*first, *encode(340, Some(&stone)).unwrap());
    cache.encode(777, Some(&stone)).unwrap();
    assert_eq!(cache.len(), 2);
    // Another player's text is another stack.
    let other = newest(json!({"item": "stone", "name": "B"}));
    let theirs = cache.encode(340, Some(&other)).unwrap();
    assert_ne!(theirs, first);
    assert_eq!(cache.len(), 3);
    // Empty slots are not kept; a full cache starts over.
    assert_eq!(&*cache.encode(340, None).unwrap(), &[0xff, 0xff]);
    assert_eq!(cache.len(), 3);
    cache.encode(393, Some(&stone)).unwrap();
    assert_eq!(cache.len(), 1);
    assert!(std::ptr::eq(StackCache::shared(), StackCache::shared()));
}

#[test]
fn menus_hand_out_their_slots_for_a_protocol() {
    let menu = Menu::parse(&json!({
        "id": "m",
        "type": "hopper",
        "slots": [{"slot": 0, "item": "stone"}],
    }))
    .unwrap();
    let cache = StackCache::new(16);
    let stone = menu.slot_stack(0, 340, &no_heads, &cache).unwrap();
    assert_eq!(decode(340, &stone).id, 1);
    assert_eq!(&*menu.slot_stack(1, 777, &no_heads, &cache).unwrap(), &[0]);
    assert!(menu.slot_stack(5, 777, &no_heads, &cache).is_err());
}

#[test]
fn an_anvil_s_left_item_is_named_exactly_on_1_12() {
    let menu = Menu::parse(&json!({"id": "nick", "type": "anvil", "text": "Steve"})).unwrap();
    let cache = StackCache::new(16);
    let left = |protocol| {
        decode(
            protocol,
            &menu
                .slot_stack(crate::menu::ANVIL_LEFT, protocol, &no_heads, &cache)
                .unwrap(),
        )
    };
    // 1.12 fills the field with the `§` string as it is.
    assert_eq!(display(&left(340)).str("Name"), Some("Steve"));
    // Other names, and the result, stay upright.
    let result = decode(
        340,
        &menu
            .slot_stack(crate::menu::ANVIL_RESULT, 340, &no_heads, &cache)
            .unwrap(),
    );
    assert_eq!(display(&result).str("Name"), Some("§rSteve"));
    // From 1.13 names are components, whose text the field shows.
    let name = display(&left(393)).str("Name").unwrap().to_owned();
    assert!(name.contains("\"Steve\""), "{name}");
}
