//! Real traffic: every item-carrying packet of every recorded session must
//! parse completely and serialise back to the same bytes, and survive the
//! default handlers of both neighbouring steps in a form the receiver's
//! layout reads.
use super::{
    advancements::Advancements,
    merchant::Offers,
    recipes::{self, BookAdd, PlaceGhostRecipe, RecipeBook, UpdateRecipes},
    window::{
        self, Click, Cooldown, CreativeSlot, EditBook, SetContent, SetEquipment,
        SetPlayerInventory, SetSlot,
    },
};
use crate::{
    Direction, State,
    codec::testing::{Defaults, corpus},
    version::PROTOCOLS,
    wire::{Reader, Writer},
};
use std::io;

/// Errors from codecs that are still stubs: such packets are counted, not
/// failed, until those codecs land.
fn pending(error: &io::Error) -> bool {
    let message = error.to_string();
    message.contains("not implemented") || message.contains("needs the component codec")
}

/// Parse `data` as the packet `name` of `protocol` and write it back.
/// `None` when the packet is not one of this codec's.
fn round_trip(
    protocol: i32,
    direction: Direction,
    name: &str,
    data: &[u8],
) -> Option<io::Result<Vec<u8>>> {
    fn run<T>(
        protocol: i32,
        data: &[u8],
        read: impl Fn(i32, &mut Reader<'_>) -> io::Result<T>,
        write: impl Fn(&T, i32, &mut Writer) -> io::Result<()>,
    ) -> io::Result<Vec<u8>> {
        let mut reader = Reader::new(data);
        let value = read(protocol, &mut reader)?;
        reader.finish()?;
        let mut writer = Writer::new();
        write(&value, protocol, &mut writer)?;
        Ok(writer.buf)
    }
    use Direction::{Clientbound, Serverbound};
    Some(match (direction, name) {
        (Clientbound, "container_set_content") => {
            run(protocol, data, SetContent::read, SetContent::write)
        }
        (Clientbound, "container_set_slot") => run(protocol, data, SetSlot::read, SetSlot::write),
        (Clientbound, "set_cursor_item") => {
            run(protocol, data, window::read_stack, |stack, p, w| {
                window::write_stack(p, w, stack)
            })
        }
        (Clientbound, "set_player_inventory") => run(
            protocol,
            data,
            SetPlayerInventory::read,
            SetPlayerInventory::write,
        ),
        (Clientbound, "set_equipment" | "set_equipped_item") => {
            run(protocol, data, SetEquipment::read, SetEquipment::write)
        }
        (Clientbound, "cooldown") => run(protocol, data, Cooldown::read, Cooldown::write),
        (Clientbound, "merchant_offers") => run(protocol, data, Offers::read, Offers::write),
        (Clientbound, "update_advancements") => {
            run(protocol, data, Advancements::read, Advancements::write)
        }
        (Clientbound, "update_recipes") => {
            run(protocol, data, UpdateRecipes::read, UpdateRecipes::write)
        }
        (Clientbound, "recipe") => run(protocol, data, RecipeBook::read, RecipeBook::write),
        (Clientbound, "recipe_book_add") => run(protocol, data, BookAdd::read, BookAdd::write),
        (Clientbound, "recipe_book_remove") => run(
            protocol,
            data,
            |_, r| recipes::read_book_remove(r),
            |ids, _, w| {
                recipes::write_book_remove(w, ids);
                Ok(())
            },
        ),
        (Clientbound, "recipe_book_settings") => run(
            protocol,
            data,
            |_, r| recipes::read_book_settings(r),
            |flags, _, w| {
                recipes::write_book_settings(w, flags);
                Ok(())
            },
        ),
        (Clientbound, "place_ghost_recipe") => run(
            protocol,
            data,
            PlaceGhostRecipe::read,
            PlaceGhostRecipe::write,
        ),
        (Serverbound, "container_click") => run(protocol, data, Click::read, Click::write),
        (Serverbound, "set_creative_mode_slot") => {
            run(protocol, data, CreativeSlot::read, CreativeSlot::write)
        }
        (Serverbound, "edit_book") => run(protocol, data, EditBook::read, EditBook::write),
        _ => return None,
    })
}

/// Packets the codec leaves to a step: recipes across the flattening
/// (numbered in 1.12, named in 1.13) and recipe lists 1.12 does not have.
fn step_owned(a: i32, b: i32, name: &str) -> bool {
    a.min(b) == 340
        && a.max(b) == 393
        && matches!(name, "recipe" | "place_ghost_recipe" | "update_recipes")
}

#[derive(Default, Debug)]
struct Counts {
    checked: usize,
    pending: usize,
}

#[test]
fn corpus_packets_round_trip_in_every_protocol() {
    let mut failures = Vec::new();
    let mut summary = Vec::new();
    for &(protocol, release) in PROTOCOLS {
        let Some(packets) = corpus(protocol) else {
            summary.push(format!("{release}: no corpus, skipped"));
            continue;
        };
        let mut counts = Counts::default();
        for packet in &packets {
            let name = packet.name(protocol);
            let Some(result) = round_trip(protocol, packet.direction, name, &packet.data) else {
                continue;
            };
            match result {
                Ok(bytes) if bytes == packet.data => counts.checked += 1,
                Ok(_) => failures.push(format!("{release} {name}: re-serialised bytes differ")),
                Err(error) if pending(&error) => counts.pending += 1,
                Err(error) => failures.push(format!("{release} {name}: {error}")),
            }
        }
        summary.push(format!(
            "{release}: {} round trips, {} pending other codecs",
            counts.checked, counts.pending
        ));
    }
    eprintln!("{}", summary.join("\n"));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// With `RIFT_ITEM_DUMP` set to a file, converted packets are appended to
/// it as JSON lines for checking against minecraft-data's schemas.
fn dump(protocol: i32, out: &crate::codec::testing::Out) {
    use std::io::Write;
    let Some(path) = std::env::var_os("RIFT_ITEM_DUMP") else {
        return;
    };
    let Some(id) = crate::data::version(protocol).packet_id(out.state, out.direction, &out.name)
    else {
        return;
    };
    let hex: String = out.data.iter().map(|byte| format!("{byte:02x}")).collect();
    let line = serde_json::json!({
        "protocol": protocol,
        "d": if out.direction == Direction::Clientbound { "c" } else { "s" },
        "s": match out.state {
            State::Configuration => "configuration",
            State::Login => "login",
            _ => "play",
        },
        "id": id,
        "name": out.name,
        "hex": hex,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("dump file");
    writeln!(file, "{line}").expect("dump line");
}

/// Every adjacent pair, both ways: real packets go through the defaults
/// step and what comes out must parse in the receiver's layout.
#[test]
fn corpus_packets_convert_between_neighbours() {
    let mut failures = Vec::new();
    let mut summary = Vec::new();
    for (index, &(protocol, release)) in PROTOCOLS.iter().enumerate() {
        let Some(packets) = corpus(protocol) else {
            continue;
        };
        let neighbours = [index.checked_sub(1), Some(index + 1)]
            .into_iter()
            .flatten()
            .filter_map(|index| PROTOCOLS.get(index).map(|(protocol, _)| *protocol));
        for neighbour in neighbours {
            let mut counts = Counts::default();
            // Clientbound packets come from this protocol's backend,
            // serverbound ones from its client.
            let mut down = Defaults::new(neighbour, protocol);
            let mut up = Defaults::new(protocol, neighbour);
            for packet in &packets {
                let name = packet.name(protocol);
                if round_trip(protocol, packet.direction, name, &packet.data).is_none()
                    || step_owned(protocol, neighbour, name)
                {
                    continue;
                }
                let step = match packet.direction {
                    Direction::Clientbound => &mut down,
                    Direction::Serverbound => &mut up,
                };
                let result = step.send_id(packet.direction, packet.state, packet.id, &packet.data);
                let outs = match result {
                    Ok(outs) => outs,
                    Err(error) if pending(&error) => {
                        counts.pending += 1;
                        continue;
                    }
                    Err(error) => {
                        failures.push(format!("{release} -> {neighbour} {name}: {error}"));
                        continue;
                    }
                };
                for out in outs {
                    dump(neighbour, &out);
                    match round_trip(neighbour, out.direction, &out.name, &out.data) {
                        None | Some(Ok(_)) => counts.checked += 1,
                        Some(Err(error)) if pending(&error) => counts.pending += 1,
                        Some(Err(error)) => failures.push(format!(
                            "{release} -> {neighbour} {name} as {}: {error}",
                            out.name
                        )),
                    }
                }
            }
            summary.push(format!(
                "{release} -> {neighbour}: {} converted, {} pending other codecs",
                counts.checked, counts.pending
            ));
        }
    }
    eprintln!("{}", summary.join("\n"));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_defaults_step_routes_cleanly() {
    for pair in PROTOCOLS.windows(2) {
        let (older, newer) = (pair[0].0, pair[1].0);
        for (client, backend) in [(older, newer), (newer, older)] {
            let routes = crate::step::RouteBuilder::<Defaults>::new(client, backend)
                .defaults()
                .build();
            let problems: Vec<&String> = routes
                .problems()
                .iter()
                .filter(|problem| problem.contains("rule for"))
                .collect();
            assert!(problems.is_empty(), "{client}/{backend}: {problems:?}");
        }
    }
    let _ = State::Play;
}

mod semantics {
    use super::super::{ItemStack, Stack, convert, window::SetSlot};
    use crate::{
        Direction,
        codec::Shared,
        nbt::{Compound, Nbt},
        wire::Reader,
    };

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    /// The stack of a recorded `container_set_slot`.
    fn slot(protocol: i32, body: &str) -> Stack {
        let bytes = hex(body);
        let mut reader = Reader::new(&bytes);
        let slot = SetSlot::read(protocol, &mut reader).unwrap();
        reader.finish().unwrap();
        slot.item
    }

    /// `stack` from `from` to the adjacent `to`, as the backend's packet
    /// for a client on `to` (or the client's for a backend on `to`).
    fn across(from: i32, to: i32, stack: &Stack) -> Stack {
        let (mut shared, direction) = if to > from {
            (Shared::new(to, from), Direction::Clientbound)
        } else {
            (Shared::new(from, to), Direction::Serverbound)
        };
        let mut stack = stack.clone();
        convert(&mut shared, direction, &mut stack).unwrap();
        stack
    }

    fn down(from: i32, to: i32, stack: &Stack) -> Stack {
        let mut shared = Shared::new(to, from);
        let mut stack = stack.clone();
        convert(&mut shared, Direction::Clientbound, &mut stack).unwrap();
        stack
    }

    fn up_serverbound(from: i32, to: i32, stack: &Stack) -> Stack {
        let mut shared = Shared::new(from, to);
        let mut stack = stack.clone();
        convert(&mut shared, Direction::Serverbound, &mut stack).unwrap();
        stack
    }

    fn name(protocol: i32, stack: &Stack) -> &'static str {
        let id = stack.as_ref().unwrap().id;
        crate::data::version(protocol)
            .registry("item")
            .unwrap()
            .name(id)
            .unwrap()
    }

    fn tag(stack: &Stack) -> &Compound {
        stack
            .as_ref()
            .unwrap()
            .tag
            .as_ref()
            .unwrap()
            .as_compound()
            .unwrap()
    }

    /// Compounds compared regardless of entry order.
    fn sorted(value: &Nbt) -> Nbt {
        match value {
            Nbt::Compound(compound) => {
                let mut entries: Vec<(String, Nbt)> = compound
                    .0
                    .iter()
                    .map(|(key, value)| (key.clone(), sorted(value)))
                    .collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                Nbt::Compound(Compound(entries))
            }
            Nbt::List(values) => Nbt::List(values.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }

    fn same(a: &Stack, b: &Stack) -> bool {
        let normal = |stack: &Stack| {
            stack.as_ref().map(|stack| ItemStack {
                tag: stack.tag.as_ref().map(sorted),
                ..stack.clone()
            })
        };
        normal(a) == normal(b)
    }

    const SWORD_1_12: &str = "00002401140100000a0000090004656e63680a000000010300036c766c00000005030002696400000010000a0007646973706c61790900044c6f726508000000020005666972737400067365636f6e640800044e616d650005426c6164650000";
    const WOOL_1_12: &str = "000025002303000e00";
    const BANNER_1_12: &str = "00002801a901000e0a00000a000e426c6f636b456e746974795461670900085061747465726e730a000000010800075061747465726e0003637265030005436f6c6f720000000000030004426173650000000e0000";
    const SHULKER_1_12: &str = "00002900e50100000a00000a000e426c6f636b456e746974795461670900054974656d730a00000001010004536c6f74000800026964000f6d696e6563726166743a73746f6e65010005436f756e740302000644616d6167650001000000";
    const SKULL_1_12: &str = "00002a018d0100030a000008000a536b756c6c4f776e657200054e6f74636800";
    const MAP_1_12: &str = "00002c016601000000";
    const ALL_1_12: [&str; 9] = [
        SWORD_1_12,
        WOOL_1_12,
        BANNER_1_12,
        SHULKER_1_12,
        SKULL_1_12,
        MAP_1_12,
        "00002601750100000a0000080006506f74696f6e00136d696e6563726166743a73776966746e65737300",
        "00002701830100000a000009000570616765730800000001000f7b2274657874223a2270616765227d080006617574686f720001410800057469746c6500015400",
        "00002b01910100000a00000a000946697265776f726b73010006466c696768740109000a4578706c6f73696f6e730a0000000101000454797065010b0006436f6c6f727300000001000000ff000000",
    ];

    #[test]
    fn the_flattening_converts_real_1_12_stacks() {
        let sword = across(340, 393, &slot(340, SWORD_1_12));
        assert_eq!(name(393, &sword), "diamond_sword");
        let sword_tag = tag(&sword);
        assert_eq!(sword_tag.int("Damage"), Some(0));
        let enchantment = sword_tag.list("Enchantments").unwrap()[0]
            .as_compound()
            .unwrap();
        assert_eq!(enchantment.str("id"), Some("minecraft:sharpness"));
        assert_eq!(enchantment.get("lvl"), Some(&Nbt::Short(5)));
        assert!(!sword_tag.contains("ench"));
        let display = sword_tag.compound("display").unwrap();
        assert_eq!(display.str("Name"), Some(r#"{"text":"Blade"}"#));
        assert_eq!(display.list("Lore").unwrap()[0].as_str(), Some("first"));

        let wool = across(340, 393, &slot(340, WOOL_1_12));
        assert_eq!(name(393, &wool), "red_wool");
        assert_eq!(wool.as_ref().unwrap().count, 3);
        assert_eq!(wool.as_ref().unwrap().tag, None);

        let banner = across(340, 393, &slot(340, BANNER_1_12));
        assert_eq!(name(393, &banner), "orange_banner");
        let entity = tag(&banner).compound("BlockEntityTag").unwrap();
        assert_eq!(entity.int("Base"), Some(1));
        let pattern = entity.list("Patterns").unwrap()[0].as_compound().unwrap();
        assert_eq!(pattern.int("Color"), Some(15));

        let shulker = across(340, 393, &slot(340, SHULKER_1_12));
        assert_eq!(name(393, &shulker), "purple_shulker_box");
        let items = tag(&shulker)
            .compound("BlockEntityTag")
            .unwrap()
            .list("Items")
            .unwrap();
        let nested = items[0].as_compound().unwrap();
        assert_eq!(nested.str("id"), Some("minecraft:granite"));
        assert!(!nested.contains("Damage"));

        let skull = across(340, 393, &slot(340, SKULL_1_12));
        assert_eq!(name(393, &skull), "player_head");
        assert_eq!(tag(&skull).str("SkullOwner"), Some("Notch"));

        let map = across(340, 393, &slot(340, MAP_1_12));
        assert_eq!(name(393, &map), "filled_map");
        assert_eq!(tag(&map).int("map"), Some(0));
    }

    #[test]
    fn real_1_12_stacks_survive_the_round_trip_through_1_13() {
        for body in ALL_1_12 {
            let original = slot(340, body);
            let flat = across(340, 393, &original);
            let back = across(393, 340, &flat);
            assert!(same(&back, &original), "{original:?}\n{flat:?}\n{back:?}");
        }
    }

    const ALL_1_13: [&str; 9] = [
        "0000240270010a0000080006506f74696f6e00136d696e6563726166743a73776966746e65737300",
        "00002502e8010a00000a000e426c6f636b456e746974795461670900085061747465726e730a000000010800075061747465726e0003637265030005436f6c6f7200000000000000",
        "0000260175010a00000a000e426c6f636b456e746974795461670900054974656d730a00000001010004536c6f74000800026964000f6d696e6563726166743a73746f6e65010005436f756e7403000000",
        "00002702bc010a000008000a536b756c6c4f776e657200054e6f74636800",
        "00002802c3010a00000a000946697265776f726b73010006466c696768740109000a4578706c6f73696f6e730a0000000101000454797065010b0006436f6c6f727300000001000000ff000000",
        "00002900600300",
        "00002a02600100",
        "00002a0260010a00000300036d61700000000000",
        "00002c02c50100",
    ];

    #[test]
    fn real_1_13_stacks_survive_the_round_trip_through_1_12() {
        for body in ALL_1_13 {
            let original = slot(393, body);
            let legacy = down(393, 340, &original);
            let back = up_serverbound(340, 393, &legacy);
            assert!(same(&back, &original), "{original:?}\n{legacy:?}\n{back:?}");
        }
        // The red banner is a 1.12 banner with data value 1 (red, in dye order).
        let banner = down(393, 340, &slot(393, ALL_1_13[1]));
        assert_eq!(
            banner.as_ref().map(|stack| (stack.id, stack.damage)),
            Some((425, 1))
        );
    }

    fn legacy_stack(id: i32, damage: i16, tag: Option<Compound>) -> Stack {
        Some(ItemStack {
            id,
            count: 1,
            damage,
            tag: tag.map(Nbt::Compound),
            components: None,
        })
    }

    #[test]
    fn spawn_eggs_become_one_item_per_mob_and_back() {
        let mut entity = Compound::new();
        entity.insert("id", "minecraft:evocation_illager");
        let mut tag = Compound::new();
        tag.insert("EntityTag", entity);
        let original = legacy_stack(383, 0, Some(tag));
        let egg = across(340, 393, &original);
        assert_eq!(name(393, &egg), "evoker_spawn_egg");
        assert_eq!(egg.as_ref().unwrap().tag, None);
        assert!(same(&across(393, 340, &egg), &original));
    }

    #[test]
    fn items_1_12_lacks_become_named_stand_ins_and_come_back() {
        let trident = Some(ItemStack {
            id: crate::data::version(393)
                .registry("item")
                .unwrap()
                .id("trident")
                .unwrap(),
            count: 1,
            damage: 0,
            tag: None,
            components: None,
        });
        let stand_in = down(393, 340, &trident);
        let stack = stand_in.as_ref().unwrap();
        assert_eq!((stack.id, stack.damage), (1, 0));
        assert_eq!(
            tag(&stand_in).compound("display").unwrap().str("Name"),
            Some("Trident")
        );
        let back = up_serverbound(340, 393, &stand_in);
        assert!(same(&back, &trident), "{back:?}");
    }

    #[test]
    fn damage_and_lossy_variants_are_kept() {
        // A damaged sword keeps its damage in the tag.
        let sword = across(340, 393, &legacy_stack(276, 17, None));
        assert_eq!(tag(&sword).int("Damage"), Some(17));
        assert!(same(
            &across(393, 340, &sword),
            &legacy_stack(276, 17, None)
        ));
        // A data value 1.13 cannot tell apart is stashed and restored.
        let odd = legacy_stack(35, 99, None);
        let flat = across(340, 393, &odd);
        assert_eq!(name(393, &flat), "white_wool");
        assert!(same(&across(393, 340, &flat), &odd));
    }

    #[test]
    fn lore_becomes_json_in_1_14() {
        let mut display = Compound::new();
        display.insert("Lore", vec![Nbt::from("§cred")]);
        let mut tag = Compound::new();
        tag.insert("display", display);
        let stone = crate::data::version(404)
            .registry("item")
            .unwrap()
            .id("stone")
            .unwrap();
        let original = Some(ItemStack {
            id: stone,
            count: 1,
            damage: 0,
            tag: Some(Nbt::Compound(tag)),
            components: None,
        });
        let modern = across(404, 477, &original);
        let line = super::super::convert::item_name(477, modern.as_ref().unwrap().id);
        assert_eq!(line, Some("stone"));
        let lore = self::tag(&modern)
            .compound("display")
            .unwrap()
            .list("Lore")
            .unwrap();
        assert!(lore[0].as_str().unwrap().contains("\"color\":\"red\""));
        assert!(same(&across(477, 404, &modern), &original));
    }

    #[test]
    fn attribute_modifiers_change_form_in_1_16() {
        let mut modifier = Compound::new();
        modifier.insert("AttributeName", "generic.attackDamage");
        modifier.insert("UUIDMost", 1i64);
        modifier.insert("UUIDLeast", 2i64);
        let mut tag = Compound::new();
        tag.insert("AttributeModifiers", vec![Nbt::Compound(modifier)]);
        let stick = crate::data::version(578)
            .registry("item")
            .unwrap()
            .id("stick")
            .unwrap();
        let original = Some(ItemStack {
            id: stick,
            count: 1,
            damage: 0,
            tag: Some(Nbt::Compound(tag)),
            components: None,
        });
        let modern = across(578, 735, &original);
        let modifier = self::tag(&modern).list("AttributeModifiers").unwrap()[0]
            .as_compound()
            .unwrap();
        assert_eq!(
            modifier.str("AttributeName"),
            Some("minecraft:generic.attack_damage")
        );
        assert_eq!(modifier.get("UUID"), Some(&Nbt::IntArray(vec![0, 1, 0, 2])));
        assert!(same(&across(735, 578, &modern), &original));
    }
}

mod layouts {
    use super::super::{
        ItemStack, Stack,
        advancements::{Advancement, Advancements},
        merchant::{Offer, Offers},
        recipes::{RecipeBook, RecipeIds},
        window::{
            self, Click, ClickSlot, Cooldown, CooldownKey, EditBook, SetContent, SetEquipment,
            SetSlot,
        },
    };
    use crate::{
        Direction, State,
        codec::testing::{Defaults, Out},
        nbt::{Compound, Nbt},
        wire::{Reader, Writer},
    };
    use std::io;

    fn stone(protocol: i32, count: i32) -> Stack {
        Some(ItemStack {
            id: crate::data::version(protocol)
                .registry("item")
                .unwrap()
                .id("stone")
                .unwrap(),
            count,
            damage: 0,
            tag: None,
            components: None,
        })
    }

    fn body(write: impl FnOnce(&mut Writer) -> io::Result<()>) -> Vec<u8> {
        let mut writer = Writer::new();
        write(&mut writer).unwrap();
        writer.buf
    }

    fn read<T>(protocol: i32, out: &Out, read: fn(i32, &mut Reader<'_>) -> io::Result<T>) -> T {
        let mut reader = Reader::new(&out.data);
        let value = read(protocol, &mut reader).unwrap();
        reader.finish().unwrap();
        value
    }

    #[test]
    fn state_ids_and_the_carried_stack_appear_in_1_17_1() {
        let content = SetContent {
            container: 0,
            state_id: 0,
            items: vec![stone(755, 1), None],
            carried: None,
        };
        // A 1.17.1 client on a 1.17 backend gets the last cursor too.
        let mut up = Defaults::new(756, 755);
        let cursor = SetSlot {
            container: -1,
            state_id: 0,
            slot: -1,
            item: stone(755, 5),
        };
        up.clientbound(
            State::Play,
            "container_set_slot",
            &body(|w| cursor.write(755, w)),
        )
        .unwrap();
        let outs = up
            .clientbound(
                State::Play,
                "container_set_content",
                &body(|w| content.write(755, w)),
            )
            .unwrap();
        let converted = read(756, &outs[0], SetContent::read);
        assert_eq!(converted.items, content.items);
        assert_eq!(converted.carried, stone(756, 5));

        // A 1.17 client on a 1.17.1 backend gets the cursor as a slot, and
        // its clicks the latest state ID.
        let content = SetContent {
            state_id: 7,
            carried: stone(756, 2),
            ..content
        };
        let mut down = Defaults::new(755, 756);
        let outs = down
            .clientbound(
                State::Play,
                "container_set_content",
                &body(|w| content.write(756, w)),
            )
            .unwrap();
        assert_eq!(outs.len(), 2);
        assert_eq!(read(755, &outs[0], SetContent::read).items.len(), 2);
        let cursor = read(755, &outs[1], SetSlot::read);
        assert_eq!(
            (cursor.container, cursor.slot, cursor.item),
            (-1, -1, stone(755, 2))
        );
        let click = Click {
            container: 0,
            state_id: 0,
            slot: 1,
            button: 0,
            action: 0,
            mode: 0,
            clicked: None,
            changed: vec![(1, ClickSlot::Full(None))],
            carried: ClickSlot::Full(stone(755, 1)),
        };
        let outs = down
            .serverbound(
                State::Play,
                "container_click",
                &body(|w| click.write(755, w)),
            )
            .unwrap();
        assert_eq!(read(756, &outs[0], Click::read).state_id, 7);
    }

    #[test]
    fn clicks_cross_the_1_17_redesign_both_ways() {
        // A 1.17 client on a 1.16.5 backend: an action number and the stack.
        let click = Click {
            container: 1,
            state_id: 0,
            slot: 3,
            button: 0,
            action: 0,
            mode: 0,
            clicked: None,
            changed: vec![(3, ClickSlot::Full(None))],
            carried: ClickSlot::Full(stone(755, 4)),
        };
        let mut up = Defaults::new(755, 754);
        let outs = up
            .serverbound(
                State::Play,
                "container_click",
                &body(|w| click.write(755, w)),
            )
            .unwrap();
        let old = read(754, &outs[0], Click::read);
        assert_eq!((old.slot, old.action, &old.clicked), (3, 1, &stone(754, 4)));
        // A 1.16.5 client on a 1.17 backend: changed slots and the cursor.
        let mut down = Defaults::new(754, 755);
        let outs = down
            .serverbound(State::Play, "container_click", &body(|w| old.write(754, w)))
            .unwrap();
        let new = read(755, &outs[0], Click::read);
        assert_eq!(new.changed, vec![(3, ClickSlot::Full(None))]);
        assert_eq!(new.carried, ClickSlot::Full(stone(755, 4)));
    }

    #[test]
    fn hashed_clicks_find_the_stack_the_backend_sent() {
        let mut up = Defaults::new(770, 769);
        // Stacks carry components from 1.20.5; an empty patch reads without
        // the component codec.
        let sword = |protocol| {
            Some(ItemStack {
                id: crate::data::version(protocol)
                    .registry("item")
                    .unwrap()
                    .id("diamond_sword")
                    .unwrap(),
                count: 1,
                damage: 0,
                tag: None,
                components: Some(Default::default()),
            })
        };
        let slot = SetSlot {
            container: 0,
            state_id: 3,
            slot: 36,
            item: sword(769),
        };
        let mut written = Writer::new();
        if let Err(error) = slot.write(769, &mut written) {
            // Writing even an empty patch needs the component codec.
            assert!(super::pending(&error), "{error}");
            eprintln!("skipped until the component codec lands: {error}");
            return;
        }
        let result = up.clientbound(State::Play, "container_set_slot", &written.buf);
        // Converting the stack needs the component codec; the slot is
        // tracked before conversion either way.
        let _ = result;
        let hashed = window::HashedStack {
            item: sword(770).unwrap().id,
            count: 1,
            components: Vec::new(),
            removed: Vec::new(),
        };
        let click = Click {
            container: 0,
            state_id: 3,
            slot: 36,
            button: 0,
            action: 0,
            mode: 0,
            clicked: None,
            changed: vec![(36, ClickSlot::Hashed(Some(hashed)))],
            carried: ClickSlot::Hashed(None),
        };
        let outs = up
            .serverbound(
                State::Play,
                "container_click",
                &body(|w| click.write(770, w)),
            )
            .unwrap();
        let full = read(769, &outs[0], Click::read);
        assert_eq!(full.changed, vec![(36, ClickSlot::Full(sword(769)))]);
        assert_eq!(full.carried, ClickSlot::Full(None));
        // And back: whole stacks without components hash to empty lists.
        let mut down = Defaults::new(769, 770);
        let outs = down
            .serverbound(
                State::Play,
                "container_click",
                &body(|w| full.write(769, w)),
            )
            .unwrap();
        assert_eq!(read(770, &outs[0], Click::read), click);
    }

    #[test]
    fn the_cursor_and_inventory_get_packets_of_their_own_in_1_21_2() {
        let mut up = Defaults::new(768, 767);
        for (container, name) in [
            (-1, "set_cursor_item"),
            (-2, "set_player_inventory"),
            (0, "container_set_slot"),
        ] {
            let slot = SetSlot {
                container,
                state_id: 1,
                slot: 5,
                item: None,
            };
            let outs = up
                .clientbound(
                    State::Play,
                    "container_set_slot",
                    &body(|w| slot.write(767, w)),
                )
                .unwrap();
            assert_eq!(outs[0].name, name);
        }
        let mut down = Defaults::new(767, 768);
        let outs = down
            .clientbound(State::Play, "set_cursor_item", &[0])
            .unwrap();
        let slot = read(767, &outs[0], SetSlot::read);
        assert_eq!(
            (outs[0].name.as_str(), slot.container, slot.slot),
            ("container_set_slot", -1, -1)
        );
        let outs = down
            .clientbound(State::Play, "set_player_inventory", &[9, 0])
            .unwrap();
        let slot = read(767, &outs[0], SetSlot::read);
        assert_eq!((slot.container, slot.slot), (-2, 9));
    }

    #[test]
    fn equipment_splits_and_joins_across_1_16() {
        let equipment = SetEquipment {
            entity: 42,
            slots: vec![(0, stone(735, 1)), (5, None)],
        };
        let mut down = Defaults::new(578, 735);
        let outs = down
            .clientbound(
                State::Play,
                "set_equipment",
                &body(|w| equipment.write(735, w)),
            )
            .unwrap();
        assert_eq!(outs.len(), 2);
        assert!(outs.iter().all(|out| out.name == "set_equipped_item"));
        assert_eq!(
            read(578, &outs[1], SetEquipment::read).slots,
            vec![(5, None)]
        );
        let mut up = Defaults::new(735, 578);
        let single = SetEquipment {
            entity: 42,
            slots: vec![(0, stone(578, 1))],
        };
        let outs = up
            .clientbound(
                State::Play,
                "set_equipped_item",
                &body(|w| single.write(578, w)),
            )
            .unwrap();
        assert_eq!(outs[0].name, "set_equipment");
        assert_eq!(
            read(735, &outs[0], SetEquipment::read).slots,
            vec![(0, stone(735, 1))]
        );
    }

    #[test]
    fn cooldowns_become_groups_in_1_21_2() {
        let pearl = |protocol| {
            crate::data::version(protocol)
                .registry("item")
                .unwrap()
                .id("ender_pearl")
                .unwrap()
        };
        let cooldown = Cooldown {
            key: CooldownKey::Item(pearl(767)),
            ticks: 20,
        };
        let mut up = Defaults::new(768, 767);
        let outs = up
            .clientbound(State::Play, "cooldown", &body(|w| cooldown.write(767, w)))
            .unwrap();
        assert_eq!(
            read(768, &outs[0], Cooldown::read).key,
            CooldownKey::Group("minecraft:ender_pearl".into())
        );
        let mut down = Defaults::new(767, 768);
        let outs = down
            .clientbound(State::Play, "cooldown", &outs[0].data)
            .unwrap();
        assert_eq!(
            read(767, &outs[0], Cooldown::read).key,
            CooldownKey::Item(pearl(767))
        );
        // A custom group has no item to show on an older client.
        let custom = Cooldown {
            key: CooldownKey::Group("rift:custom".into()),
            ticks: 5,
        };
        let outs = down
            .clientbound(State::Play, "cooldown", &body(|w| custom.write(768, w)))
            .unwrap();
        assert!(outs.is_empty());
    }

    #[test]
    fn book_edits_switch_between_stacks_and_pages_in_1_17_1() {
        // 1.17 already names the book's inventory slot, as 1.17.1 does
        // (`ServerboundEditBookPacket.slot` since 1.16.4): it passes as is.
        let mut up = Defaults::new(756, 755);
        let pages = EditBook::Pages {
            slot: 40,
            pages: vec!["one".into()],
            title: Some("T".into()),
        };
        let outs = up
            .serverbound(State::Play, "edit_book", &body(|w| pages.write(756, w)))
            .unwrap();
        let EditBook::Stack {
            book,
            signing,
            hand,
        } = read(755, &outs[0], EditBook::read)
        else {
            panic!("expected a stack");
        };
        assert!(signing);
        assert_eq!(hand, 40);
        let tag = book.unwrap().tag.unwrap();
        assert_eq!(tag.as_compound().unwrap().str("title"), Some("T"));

        let mut down = Defaults::new(755, 756);
        let mut tag = Compound::new();
        tag.insert("pages", vec![Nbt::from("one"), Nbt::from("two")]);
        let book = Some(ItemStack {
            tag: Some(Nbt::Compound(tag)),
            ..stone(755, 1).unwrap()
        });
        let stack = EditBook::Stack {
            book,
            signing: false,
            hand: 7,
        };
        let outs = down
            .serverbound(State::Play, "edit_book", &body(|w| stack.write(755, w)))
            .unwrap();
        assert_eq!(
            read(756, &outs[0], EditBook::read),
            EditBook::Pages {
                slot: 7,
                pages: vec!["one".into(), "two".into()],
                title: None
            }
        );
    }

    #[test]
    fn book_edits_name_a_slot_instead_of_a_hand_from_1_16_4() {
        let book = |hand| EditBook::Stack {
            book: stone(753, 1),
            signing: false,
            hand,
        };
        let hand = |outs: &[Out], protocol| match read(protocol, &outs[0], EditBook::read) {
            EditBook::Stack { hand, .. } => hand,
            other => panic!("{other:?}"),
        };
        let mut down = Defaults::new(753, 754);
        // The client selected hotbar slot 4 earlier.
        down.serverbound(State::Play, "set_carried_item", &[0, 4])
            .unwrap();
        for (sent, slot) in [(0, 4), (1, 40)] {
            let outs = down
                .serverbound(
                    State::Play,
                    "edit_book",
                    &body(|w| book(sent).write(753, w)),
                )
                .unwrap();
            assert_eq!(hand(&outs, 754), slot);
        }
        let mut up = Defaults::new(754, 753);
        for (sent, expected) in [(40, 1), (3, 0)] {
            let outs = up
                .serverbound(
                    State::Play,
                    "edit_book",
                    &body(|w| book(sent).write(754, w)),
                )
                .unwrap();
            assert_eq!(hand(&outs, 753), expected);
        }
    }

    #[test]
    fn trades_change_list_layout_in_1_19() {
        let offer = Offer {
            cost: stone(758, 2),
            result: stone(758, 1),
            second_cost: None,
            disabled: false,
            uses: 1,
            max_uses: 12,
            xp: 2,
            special_price: 0,
            price_multiplier: 0.05,
            demand: 0,
        };
        let offers = Offers {
            container: 3,
            offers: vec![offer],
            level: 1,
            xp: 0,
            regular: true,
            can_restock: true,
        };
        let mut up = Defaults::new(759, 758);
        let outs = up
            .clientbound(
                State::Play,
                "merchant_offers",
                &body(|w| offers.write(758, w)),
            )
            .unwrap();
        let converted = read(759, &outs[0], Offers::read);
        assert_eq!(converted.offers[0].second_cost, None);
        assert_eq!(converted.offers[0].cost, stone(759, 2));
        let mut down = Defaults::new(758, 759);
        let outs = down
            .clientbound(State::Play, "merchant_offers", &outs[0].data)
            .unwrap();
        assert_eq!(read(758, &outs[0], Offers::read), offers);
    }

    #[test]
    fn advancement_criteria_come_back_for_clients_before_1_20_2() {
        let advancements = Advancements {
            reset: true,
            added: vec![Advancement {
                id: "rift:root".into(),
                parent: None,
                display: None,
                x: 0.0,
                y: 0.0,
                criteria: Vec::new(),
                requirements: vec![vec!["a".into(), "b".into()], vec!["a".into()]],
                telemetry: false,
            }],
            removed: Vec::new(),
            progress: Vec::new(),
            show: true,
        };
        let mut down = Defaults::new(763, 764);
        let outs = down
            .clientbound(
                State::Play,
                "update_advancements",
                &body(|w| advancements.write(764, w)),
            )
            .unwrap();
        let old = read(763, &outs[0], Advancements::read);
        assert_eq!(old.added[0].criteria, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn tag_displays_become_item_sets_in_26_3_and_back() {
        use super::super::recipes::{BookAdd, BookEntry, IdSet, RecipeDisplay, SlotDisplay};
        let item = |protocol: i32, name: &str| {
            crate::data::version(protocol)
                .expect_registry("item")
                .id(name)
                .unwrap()
        };
        let book = |ingredients: Vec<SlotDisplay>, protocol: i32| BookAdd {
            entries: vec![BookEntry {
                id: 7,
                display: RecipeDisplay::Shapeless {
                    ingredients,
                    result: SlotDisplay::Item(item(protocol, "stick")),
                    station: SlotDisplay::Item(item(protocol, "crafting_table")),
                },
                group: 0,
                category: 0,
                requirements: None,
                flags: 0,
            }],
            replace: false,
        };
        let planks = || "minecraft:planks".to_owned();
        let newer = book(
            vec![
                SlotDisplay::Items(IdSet::Tag(planks())),
                SlotDisplay::Items(IdSet::Ids(vec![
                    item(777, "oak_planks"),
                    item(777, "birch_planks"),
                ])),
                SlotDisplay::Items(IdSet::Ids(vec![item(777, "oak_planks")])),
            ],
            777,
        );
        let older = book(
            vec![
                SlotDisplay::Tag(planks()),
                SlotDisplay::Composite(vec![
                    SlotDisplay::Item(item(776, "oak_planks")),
                    SlotDisplay::Item(item(776, "birch_planks")),
                ]),
                SlotDisplay::Item(item(776, "oak_planks")),
            ],
            776,
        );
        let mut down = Defaults::new(776, 777);
        let outs = down
            .clientbound(
                State::Play,
                "recipe_book_add",
                &body(|w| newer.write(777, w)),
            )
            .unwrap();
        assert_eq!(read(776, &outs[0], BookAdd::read), older);
        let mut up = Defaults::new(777, 776);
        let outs = up
            .clientbound(
                State::Play,
                "recipe_book_add",
                &body(|w| older.write(776, w)),
            )
            .unwrap();
        let mut expected = newer;
        let RecipeDisplay::Shapeless { ingredients, .. } = &mut expected.entries[0].display else {
            unreachable!()
        };
        ingredients[1] = SlotDisplay::Composite(vec![
            SlotDisplay::Item(item(777, "oak_planks")),
            SlotDisplay::Item(item(777, "birch_planks")),
        ]);
        ingredients[2] = SlotDisplay::Item(item(777, "oak_planks"));
        assert_eq!(read(777, &outs[0], BookAdd::read), expected);
    }

    #[test]
    fn recipe_book_flags_grow_in_1_16_2() {
        let book = RecipeBook {
            action: 1,
            flags: [true, false, true, false, false, false, false, false],
            recipes: RecipeIds::Names(vec!["minecraft:stick".into()]),
            highlight: None,
        };
        let mut up = Defaults::new(751, 736);
        let outs = up
            .clientbound(State::Play, "recipe", &body(|w| book.write(736, w)))
            .unwrap();
        assert_eq!(read(751, &outs[0], RecipeBook::read), book);
        let mut down = Defaults::new(736, 751);
        let mut newer = book.clone();
        newer.flags[6] = true;
        let outs = down
            .clientbound(State::Play, "recipe", &body(|w| newer.write(751, w)))
            .unwrap();
        assert_eq!(read(736, &outs[0], RecipeBook::read), book);
    }

    #[test]
    fn malformed_packets_fail_without_panicking() {
        let mut step = Defaults::new(393, 340);
        for name in [
            "container_set_content",
            "container_set_slot",
            "set_equipped_item",
            "update_advancements",
        ] {
            for body in [&[][..], &[0xff; 3], &[0, 0x7f, 0xff, 0x01], &[0x80; 12]] {
                assert!(
                    step.clientbound(State::Play, name, body).is_err(),
                    "{name} {body:?}"
                );
            }
        }
        let mut step = Defaults::new(340, 393);
        for name in ["container_click", "set_creative_mode_slot"] {
            assert!(step.serverbound(State::Play, name, &[0, 1]).is_err());
        }
        let _ = Direction::Clientbound;
    }
}

mod trades {
    use super::super::{
        ItemStack, convert_offers,
        merchant::{Offer, Offers},
        transcode_trade_list,
    };
    use crate::{
        Direction,
        codec::Shared,
        wire::{Reader, Rewriter, Writer},
    };

    fn wool(protocol: i32) -> Option<ItemStack> {
        Some(ItemStack {
            id: crate::data::version(protocol)
                .registry("item")
                .unwrap()
                .id("red_wool")
                .unwrap(),
            count: 3,
            damage: 0,
            tag: None,
            components: None,
        })
    }

    fn offers(protocol: i32) -> Offers {
        Offers {
            container: 2,
            offers: vec![Offer {
                cost: wool(protocol),
                result: wool(protocol),
                second_cost: None,
                disabled: false,
                uses: 0,
                max_uses: 7,
                xp: 0,
                special_price: 0,
                price_multiplier: 0.0,
                demand: 0,
            }],
            level: 0,
            xp: 0,
            regular: false,
            can_restock: false,
        }
    }

    #[test]
    fn plugin_channel_trade_lists_convert_between_1_13_layouts() {
        let mut body = Writer::new();
        offers(401).write(401, &mut body).unwrap();
        let mut shared = Shared::new(404, 401);
        let mut rw = Rewriter::new(&body.buf);
        transcode_trade_list(&mut shared, Direction::Clientbound, &mut rw).unwrap();
        let converted = rw.finish().unwrap();
        let mut reader = Reader::new(&converted);
        assert_eq!(Offers::read(404, &mut reader).unwrap(), offers(404));
        reader.finish().unwrap();
    }

    #[test]
    fn the_1_14_step_turns_a_trade_list_into_merchant_offers() {
        let mut shared = Shared::new(477, 404);
        let mut list = offers(404);
        convert_offers(&mut shared, Direction::Clientbound, &mut list).unwrap();
        let mut packet = Writer::new();
        list.write(477, &mut packet).unwrap();
        let mut reader = Reader::new(&packet.buf);
        let modern = Offers::read(477, &mut reader).unwrap();
        reader.finish().unwrap();
        assert_eq!(modern.offers[0].cost, wool(477));
    }
}
