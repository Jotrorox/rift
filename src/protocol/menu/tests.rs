use super::*;
use crate::menu::{Click as Kind, Kind as MenuKind};
use rift_translation::codec::misc::screen::{MenuType, menu_type};

/// One protocol of each family the menu packets distinguish: 1.12, 1.13,
/// 1.13.2, 1.14, 1.16.5, 1.17, 1.17.1, 1.20.3, 1.20.5, 1.21.2, 1.21.5, 26.3.
const FAMILIES: [i32; 12] = [335, 393, 404, 477, 754, 755, 756, 765, 766, 768, 770, 777];

fn version(protocol: i32) -> ProtocolVersion {
    ProtocolVersion::new(protocol).unwrap()
}

fn ids(protocol: i32) -> Ids {
    Ids::of(protocol).unwrap()
}

fn packet_id(protocol: i32, direction: Direction, name: &str) -> i32 {
    data::version(protocol)
        .packet_id(State::Play, direction, name)
        .unwrap()
}

#[test]
fn menus_need_1_12() {
    assert!(!supported(316));
    assert!(!supported(334));
    for protocol in [335, 338, 340, 754, 755, 777] {
        assert!(supported(protocol), "{protocol}");
        assert!(Ids::of(protocol).is_some());
    }
    assert!(!supported(778));
    assert!(open_screen(version(316), 101, MenuKind::Hopper, None).is_err());
}

#[test]
fn open_screen_names_the_menu_and_title_in_each_protocol() {
    let title = crate::text::Text::Plain("Hi".into());
    for protocol in FAMILIES {
        for kind in [
            MenuKind::Chest { rows: 3 },
            MenuKind::Chest { rows: 6 },
            MenuKind::Hopper,
            MenuKind::Dispenser,
        ] {
            let packet = open_screen(version(protocol), 101, kind, Some(&title)).unwrap();
            assert_eq!(
                packet.id,
                packet_id(protocol, Direction::Clientbound, "open_screen")
            );
            let mut r = Reader::new(&packet.data);
            let menu = menu_type(protocol, kind.menu()).unwrap();
            match menu {
                MenuType::Window { kind, slots } => {
                    assert!(protocol < MENUS);
                    assert_eq!(r.u8().unwrap(), 101);
                    assert_eq!(r.str().unwrap(), kind);
                    assert_eq!(r.str().unwrap(), r#"{"text":"Hi"}"#);
                    assert_eq!(r.u8().unwrap(), slots);
                }
                MenuType::Registry(id) => {
                    assert!(protocol >= MENUS);
                    assert_eq!(r.varint().unwrap(), 101);
                    assert_eq!(r.varint().unwrap(), id);
                    if protocol >= 765 {
                        // An anonymous NBT string.
                        assert_eq!(r.rest(), [8, 0, 2, b'H', b'i']);
                    } else {
                        assert_eq!(r.str().unwrap(), r#"{"text":"Hi"}"#);
                    }
                }
            }
            assert!(r.is_empty(), "{protocol} {kind:?}");
        }
    }
    // 1.12's window names and slot counts.
    let packet = open_screen(version(340), 127, MenuKind::Dispenser, None).unwrap();
    let mut expected = vec![127];
    crate::protocol::write_string("minecraft:dispenser", &mut expected);
    crate::protocol::write_string(r#"{"text":""}"#, &mut expected);
    expected.push(9);
    assert_eq!(packet.data, expected);
}

fn stone(protocol: i32) -> Vec<u8> {
    let stack = ItemStack {
        id: 1,
        count: 3,
        damage: 0,
        tag: None,
        components: rift_translation::version::item_components(protocol).then(Default::default),
    };
    let mut w = Writer::new();
    item::write(protocol, Some(&stack), &mut w).unwrap();
    w.into_inner()
}

#[test]
fn slots_and_the_cursor_read_back_with_the_translation_codec() {
    for protocol in FAMILIES {
        let v = version(protocol);
        let packet = set_slot(v, 101, 7, 4, &stone(protocol)).unwrap();
        assert_eq!(
            packet.id,
            packet_id(protocol, Direction::Clientbound, "container_set_slot")
        );
        let mut r = Reader::new(&packet.data);
        let read = window::SetSlot::read(protocol, &mut r).unwrap();
        r.finish().unwrap();
        assert_eq!(read.container, 101);
        assert_eq!(read.slot, 4);
        assert_eq!(read.state_id, if protocol >= 756 { 7 } else { 0 });
        assert_eq!(
            read.item.as_ref().map(|item| (item.id, item.count)),
            Some((1, 3))
        );

        let cursor = empty_cursor(v, 7).unwrap();
        let mut r = Reader::new(&cursor.data);
        if protocol >= 768 {
            assert_eq!(
                cursor.id,
                packet_id(protocol, Direction::Clientbound, "set_cursor_item")
            );
            assert_eq!(window::read_stack(protocol, &mut r).unwrap(), None);
        } else {
            assert_eq!(cursor.id, packet.id);
            let read = window::SetSlot::read(protocol, &mut r).unwrap();
            assert_eq!((read.container, read.slot, read.item), (-1, -1, None));
        }
        r.finish().unwrap();
    }
}

#[test]
fn closes_name_the_container_as_a_byte_then_a_varint() {
    for protocol in FAMILIES {
        let v = version(protocol);
        let packet = close(v, 120).unwrap();
        assert_eq!(
            packet.id,
            packet_id(protocol, Direction::Clientbound, "container_close")
        );
        assert_eq!(packet.data, [120]);
        let packet = client_close(v, 120).unwrap();
        assert_eq!(
            packet.id,
            packet_id(protocol, Direction::Serverbound, "container_close")
        );
        assert_eq!(packet.data, [120]);
        assert_eq!(
            client_container(&ids(protocol), &packet).unwrap(),
            Some((ClientPacket::Close, 120))
        );
    }
    // A VarInt from 1.21.2: 200 takes two bytes, which a byte cannot hold.
    assert_eq!(close(version(768), 200).unwrap().data, [0xc8, 0x01]);
    assert!(close(version(767), 300).is_err());
}

/// A click as the client of `protocol` writes it, with stacks in every
/// field that has one.
fn click_packet(protocol: i32, container: i32, slot: i16, button: i8, mode: i32) -> Packet {
    let full = |protocol| {
        let bytes = stone(protocol);
        window::read_stack(protocol, &mut Reader::new(&bytes)).unwrap()
    };
    let stack = |protocol| {
        if window::hashed_clicks(protocol) {
            ClickSlot::Hashed(Some(window::HashedStack {
                item: 1,
                count: 3,
                components: vec![(5, 77)],
                removed: vec![6],
            }))
        } else {
            ClickSlot::Full(full(protocol))
        }
    };
    let click = window::Click {
        container,
        state_id: 42,
        slot,
        button,
        action: 9,
        mode,
        clicked: full(protocol),
        changed: vec![(slot, stack(protocol)), (30, stack(protocol))],
        carried: stack(protocol),
    };
    let mut w = Writer::new();
    click.write(protocol, &mut w).unwrap();
    Packet::new(
        packet_id(protocol, Direction::Serverbound, "container_click"),
        w.into_inner(),
    )
}

#[test]
fn clicks_read_the_same_fields_in_every_layout() {
    for protocol in FAMILIES {
        let ids = ids(protocol);
        let packet = click_packet(protocol, 101, 4, 1, 0);
        assert_eq!(
            client_container(&ids, &packet).unwrap(),
            Some((ClientPacket::Click, 101))
        );
        let click = Click::read(&ids, &packet).unwrap();
        assert_eq!(
            (click.container, click.slot, click.button, click.mode),
            (101, 4, 1, 0),
            "{protocol}"
        );
        assert_eq!(click.state_id, (protocol >= 756).then_some(42));
        assert_eq!(click.changed, (protocol >= 755).then(|| vec![4, 30]));
        assert_eq!(click.kind(), Some((Kind::Right, None)));
        // Trailing bytes and short packets are malformed.
        let mut long = packet.clone();
        long.data.push(0);
        assert!(Click::read(&ids, &long).is_err());
        let mut short = packet.clone();
        short.data.pop();
        assert!(Click::read(&ids, &short).is_err());
        // Other packets name no container.
        assert_eq!(
            client_container(&ids, &Packet::empty(ids.creative)).unwrap(),
            None
        );
        assert!(creative(&ids, &Packet::empty(ids.creative)));
    }
}

#[test]
fn click_modes_map_to_the_design_s_click_kinds() {
    let click = |slot, button, mode| Click {
        container: 101,
        slot,
        button,
        mode,
        state_id: None,
        changed: None,
    };
    for ((slot, button, mode), kind) in [
        ((0, 0, 0), Some((Kind::Left, None))),
        ((0, 1, 0), Some((Kind::Right, None))),
        ((0, 0, 1), Some((Kind::ShiftLeft, None))),
        ((0, 1, 1), Some((Kind::ShiftRight, None))),
        ((0, 0, 2), Some((Kind::NumberKey, Some(1)))),
        ((0, 8, 2), Some((Kind::NumberKey, Some(9)))),
        ((0, 40, 2), Some((Kind::Offhand, None))),
        ((0, 9, 2), None),
        ((0, 2, 3), Some((Kind::Middle, None))),
        ((0, 0, 4), Some((Kind::Drop, None))),
        ((0, 1, 4), Some((Kind::DropAll, None))),
        // Outside the window, between slots, drags and collection.
        ((-999, 0, 0), None),
        ((-999, 0, 4), None),
        ((-1, 0, 0), None),
        ((0, 1, 5), None),
        ((0, 0, 6), None),
    ] {
        assert_eq!(
            click(slot, button, mode).kind(),
            kind,
            "{slot} {button} {mode}"
        );
    }
}

#[test]
fn what_a_click_changed_on_the_client() {
    let old = |slot, button, mode| Click {
        container: 101,
        slot,
        button,
        mode,
        state_id: None,
        changed: None,
    };
    // Before 1.17: the clicked slot, or every menu slot for a double click or
    // a drag; the inventory rows for shift, keys, F, spreads and their own
    // slots.
    assert_eq!(old(4, 0, 0).menu_slots(27), [4]);
    assert!(!old(4, 0, 0).touches_inventory(27));
    assert_eq!(old(4, 0, 6).menu_slots(9), (0..9).collect::<Vec<_>>());
    assert_eq!(old(-999, 0, 5).menu_slots(5), (0..5).collect::<Vec<_>>());
    for (button, mode) in [(0, 1), (3, 2), (40, 2), (0, 5), (0, 6)] {
        assert!(
            old(4, button, mode).touches_inventory(27),
            "{button} {mode}"
        );
    }
    assert!(old(30, 0, 0).touches_inventory(27));
    assert!(old(30, 0, 0).menu_slots(27).is_empty());
    assert!(!old(4, 0, 4).touches_inventory(27));
    // From 1.17: the list says, except for the off hand, which is never in
    // it.
    let new = |slot, button, mode, changed: Vec<i16>| Click {
        changed: Some(changed),
        ..old(slot, button, mode)
    };
    assert_eq!(new(4, 0, 1, vec![4, 40]).menu_slots(27), [4]);
    assert!(new(4, 0, 1, vec![4, 40]).touches_inventory(27));
    assert!(!new(4, 0, 1, vec![4]).touches_inventory(27));
    assert_eq!(new(-999, 0, 5, vec![1, 2]).menu_slots(9), [1, 2]);
    assert!(new(4, 40, 2, vec![4]).touches_inventory(27));
    assert!(new(4, 0, 0, vec![4]).on_menu(27));
    assert!(!new(30, 0, 0, vec![]).on_menu(27));
}

#[test]
fn backend_screens_and_inventory_updates_are_recognized() {
    for protocol in FAMILIES {
        let ids = ids(protocol);
        let mut open = Writer::new();
        if protocol >= MENUS {
            open.varint(7).varint(2);
        } else {
            open.u8(7).string("minecraft:chest");
        }
        let packet = Packet::new(ids.open_screen, open.into_inner());
        assert_eq!(
            backend_packet(&ids, &packet),
            Some(BackendPacket::Opened(7))
        );
        for (id, expected) in [
            (ids.sign_editor, BackendPacket::Screen),
            (ids.close, BackendPacket::Closed),
            (ids.respawn, BackendPacket::World),
            (ids.login, BackendPacket::World),
        ] {
            assert_eq!(
                backend_packet(&ids, &Packet::new(id, vec![1])),
                Some(expected),
                "{protocol}"
            );
        }
        if protocol >= 477 {
            let mut mount = Writer::new();
            write_container(protocol, &mut mount, 9).unwrap();
            mount.varint(3).i32(1);
            assert_eq!(
                backend_packet(&ids, &Packet::new(ids.mount_screen, mount.into_inner())),
                Some(BackendPacket::Opened(9))
            );
        }
        for (container, expected) in [(0, Some(BackendPacket::InventorySlot)), (5, None)] {
            let packet = set_slot(version(protocol), container, 1, 36, &stone(protocol)).unwrap();
            assert_eq!(backend_packet(&ids, &packet), expected, "{protocol}");
        }
        let content = window::SetContent {
            container: 0,
            state_id: 3,
            items: vec![None; 46],
            carried: None,
        };
        let mut w = Writer::new();
        content.write(protocol, &mut w).unwrap();
        assert_eq!(
            backend_packet(&ids, &Packet::new(ids.set_content, w.into_inner())),
            Some(BackendPacket::InventoryContent)
        );
        assert_eq!(backend_packet(&ids, &Packet::new(ids.click, vec![])), None);
    }
}

/// The requests of tests/menus_probe.py (`request_transaction`,
/// `request_claim_all`, `request_state_id`), byte for byte.
#[test]
fn inventory_requests_match_the_probe_byte_for_byte() {
    // Container 0, slot -1, button 0, action -1, mode 0, 99 stone.
    let transaction_1_12 = [0, 0xff, 0xff, 0, 0xff, 0xff, 0, 0, 1, 99, 0, 0, 0];
    let transaction_1_13 = [0, 0xff, 0xff, 0, 0xff, 0xff, 0, 0, 1, 99, 0];
    let transaction_1_13_2 = [0, 0xff, 0xff, 0, 0xff, 0xff, 0, 1, 1, 99, 0];
    // Container 0, state ID -1, slot -1, button 0, mode 0, no changed
    // slots, an empty cursor (whatever its layout, a single 0).
    let state_id = [0, 0xff, 0xff, 0xff, 0xff, 0x0f, 0xff, 0xff, 0, 0, 0, 0];
    for (protocol, expected, recipe_) in [
        (335, &transaction_1_12[..], Recipe::Transaction),
        (340, &transaction_1_12[..], Recipe::Transaction),
        (393, &transaction_1_13[..], Recipe::Transaction),
        (401, &transaction_1_13[..], Recipe::Transaction),
        (404, &transaction_1_13_2[..], Recipe::Transaction),
        (578, &transaction_1_13_2[..], Recipe::Transaction),
        (754, &transaction_1_13_2[..], Recipe::Transaction),
        (756, &state_id[..], Recipe::StateId),
        (764, &state_id[..], Recipe::StateId),
        (766, &state_id[..], Recipe::StateId),
        (768, &state_id[..], Recipe::StateId),
        (770, &state_id[..], Recipe::StateId),
        (777, &state_id[..], Recipe::StateId),
    ] {
        assert_eq!(recipe(protocol), recipe_);
        let packet = inventory_request(protocol).unwrap();
        assert_eq!(
            packet.id,
            packet_id(protocol, Direction::Serverbound, "container_click")
        );
        assert_eq!(packet.data, expected, "{protocol}");
    }
    // 1.17: every one of the 46 slots and the cursor claims 99 stone.
    assert_eq!(recipe(755), Recipe::ClaimAll);
    let mut expected = vec![0, 0xff, 0xff, 0, 0, 46];
    for slot in 0..46u8 {
        expected.extend([0, slot, 1, 1, 99, 0]);
    }
    expected.extend([1, 1, 99, 0]);
    assert_eq!(inventory_request(755).unwrap().data, expected);
}

#[test]
fn rejections_are_recognized_and_acknowledged_up_to_1_16_5() {
    for protocol in [335, 340, 393, 404, 477, 578, 735, 754] {
        let rejection = Packet::new(
            packet_id(protocol, Direction::Clientbound, "container_ack"),
            vec![0, 0xff, 0xff, 0],
        );
        assert!(is_rejection(protocol, &rejection), "{protocol}");
        // Accepted, another action, another window, a malformed body.
        for data in [
            vec![0, 0xff, 0xff, 1],
            vec![0, 0, 5, 0],
            vec![3, 0xff, 0xff, 0],
            vec![0, 0xff, 0xff, 0, 0],
            vec![0, 0xff],
        ] {
            assert!(!is_rejection(protocol, &Packet::new(rejection.id, data)));
        }
        let acknowledgement = acknowledgement(protocol).unwrap();
        assert_eq!(
            acknowledgement.id,
            packet_id(protocol, Direction::Serverbound, "container_ack")
        );
        assert_eq!(acknowledgement.data, [0, 0xff, 0xff, 1]);
    }
    assert!(acknowledgement(755).is_err());
    assert!(!is_rejection(
        755,
        &Packet::new(0x11, vec![0, 0xff, 0xff, 0])
    ));
}

#[test]
fn an_anvil_opens_with_0_slots_on_1_12_and_its_menu_from_1_14() {
    let title = crate::text::Text::Plain("Name".into());
    for protocol in FAMILIES {
        let packet = open_screen(version(protocol), 101, MenuKind::Anvil, Some(&title)).unwrap();
        let mut r = Reader::new(&packet.data);
        if protocol >= MENUS {
            assert_eq!(r.varint().unwrap(), 101);
            let anvil = data::version(protocol)
                .registry("menu")
                .unwrap()
                .id("anvil")
                .unwrap();
            assert_eq!(r.varint().unwrap(), anvil, "{protocol}");
        } else {
            assert_eq!(r.u8().unwrap(), 101);
            assert_eq!(r.str().unwrap(), "minecraft:anvil");
            r.str().unwrap();
            // Any other number of slots shows a chest.
            assert_eq!(r.u8().unwrap(), 0);
            assert!(r.is_empty());
        }
    }
}

#[test]
fn the_anvil_cost_is_property_0_with_a_byte_then_a_varint_container() {
    for protocol in FAMILIES {
        let packet = anvil_cost(version(protocol), 127).unwrap();
        assert_eq!(
            packet.id,
            packet_id(protocol, Direction::Clientbound, "container_set_data"),
            "{protocol}"
        );
        // The container, property 0 and the value 0 as shorts.
        assert_eq!(packet.data, [127, 0, 0, 0, 0]);
    }
    assert_eq!(
        anvil_cost(version(767), 200).unwrap().data,
        [200, 0, 0, 0, 0]
    );
    assert_eq!(
        anvil_cost(version(768), 200).unwrap().data,
        [0xc8, 1, 0, 0, 0, 0]
    );
}

#[test]
fn renames_are_read_from_rename_item_and_from_mc_item_name_on_1_12() {
    for protocol in FAMILIES {
        let ids = ids(protocol);
        let packet = if protocol >= 393 {
            let mut w = Writer::new();
            w.string("Grüße");
            Packet::new(
                packet_id(protocol, Direction::Serverbound, "rename_item"),
                w.into_inner(),
            )
        } else {
            let mut w = Writer::new();
            w.string("MC|ItemName").string("Grüße");
            Packet::new(
                packet_id(protocol, Direction::Serverbound, "custom_payload"),
                w.into_inner(),
            )
        };
        assert_eq!(
            rename(&ids, &packet).unwrap().as_deref(),
            Some("Grüße"),
            "{protocol}"
        );
        // Trailing bytes or a broken string close the session.
        let mut trailing = packet.clone();
        trailing.data.push(0);
        assert!(rename(&ids, &trailing).is_err(), "{protocol}");
        let mut broken = packet.clone();
        broken.data.truncate(broken.data.len() - 1);
        assert!(rename(&ids, &broken).is_err(), "{protocol}");
        // Other packets are no renames.
        assert_eq!(rename(&ids, &Packet::new(ids.click, vec![])).unwrap(), None);
    }
    // 1.12: an empty message is an empty text; other channels, and a
    // channel Rift cannot read, are not renames.
    let ids = ids(340);
    let payload = packet_id(340, Direction::Serverbound, "custom_payload");
    let mut empty = Writer::new();
    empty.string("MC|ItemName");
    assert_eq!(
        rename(&ids, &Packet::new(payload, empty.into_inner()))
            .unwrap()
            .as_deref(),
        Some("")
    );
    let mut other = Writer::new();
    other.string("MC|Brand").string("vanilla");
    assert_eq!(
        rename(&ids, &Packet::new(payload, other.into_inner())).unwrap(),
        None
    );
    assert_eq!(
        rename(&ids, &Packet::new(payload, vec![0xff])).unwrap(),
        None
    );
    // A rename_item from 1.13 can be empty.
    let ids = self::ids(754);
    let mut w = Writer::new();
    w.string("");
    let packet = Packet::new(ids.rename, w.into_inner());
    assert_eq!(rename(&ids, &packet).unwrap().as_deref(), Some(""));
}

#[test]
fn anvil_text_limits_are_35_up_to_1_16_5_and_50_from_1_17() {
    for (protocol, limit) in [
        (335, 35),
        (393, 35),
        (477, 35),
        (754, 35),
        (755, 50),
        (777, 50),
    ] {
        assert_eq!(crate::menu::anvil_text_limit(protocol), limit, "{protocol}");
        let fits = "x".repeat(limit);
        assert_eq!(crate::menu::anvil_rename(protocol, &fits), Some(fits));
        assert_eq!(
            crate::menu::anvil_rename(protocol, &"x".repeat(limit + 1)),
            None
        );
    }
}

#[test]
fn set_experience_is_recognized_for_the_anvil() {
    for protocol in FAMILIES {
        let ids = ids(protocol);
        let packet = Packet::new(
            packet_id(protocol, Direction::Clientbound, "set_experience"),
            vec![0, 0, 0, 0, 5, 7],
        );
        assert_eq!(
            backend_packet(&ids, &packet),
            Some(BackendPacket::Experience),
            "{protocol}"
        );
    }
}
