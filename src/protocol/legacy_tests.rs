use super::*;

#[test]
fn proxy_messages_use_system_position_until_the_1_19_1_overlay_flag() {
    for number in [47, 340, 404, 735, 758, 759, 760, 761, 764] {
        let version = ProtocolVersion::new(number).unwrap();
        let packet = version
            .switching()
            .unwrap()
            .system_message(version, "proxy")
            .unwrap();
        let mut data = packet.data.as_slice();
        assert_eq!(
            read_string(&mut data, 32767).unwrap(),
            "{\"text\":\"proxy\"}"
        );
        let mut expected = vec![u8::from(number <= 759)];
        if (735..759).contains(&number) {
            expected.extend([0; 16]);
        }
        assert_eq!(data, expected);
    }
}

// Small independent protocol fixtures cover the boundaries hidden by modern
// configuration. Real servers supply full registries in network_minecraft.py.
fn join(number: i32) -> (Packet, Vec<u8>) {
    let mut data = 123_i32.to_be_bytes().to_vec();
    let mut respawn = Vec::new();
    if number < 735 {
        data.push(9); // Creative + hardcore; Respawn carries only the game mode.
        if number < 108 {
            data.push(0);
        } else {
            data.extend(0_i32.to_be_bytes());
        }
        respawn.extend(0_i32.to_be_bytes());
        if number < 477 {
            data.push(2);
            respawn.push(2);
        }
        if number >= 573 {
            data.extend(12345_i64.to_be_bytes());
            respawn.extend(12345_i64.to_be_bytes());
        }
        data.push(20);
        write_string("flat", &mut data);
        respawn.push(1);
        write_string("flat", &mut respawn);
        if number >= 477 {
            data.push(2);
        }
        data.push(0);
        if number >= 573 {
            data.push(1);
        }
    } else {
        if number >= 751 {
            data.push(0);
        }
        data.extend([1, 255, 1]);
        write_string("minecraft:overworld", &mut data);
        data.extend([10, 0, 0, 0]); // Named, empty registry compound.
        let mut spawn = Vec::new();
        if (751..=758).contains(&number) {
            spawn.extend([10, 0, 0, 1, 0, 1, b'x', 1, 0]);
        } else {
            write_string("minecraft:overworld", &mut spawn);
        }
        write_string("minecraft:overworld", &mut spawn);
        spawn.extend(12345_i64.to_be_bytes());
        data.extend(&spawn);
        respawn.extend(spawn);
        respawn.extend([1, 255, 0, 1, 0]);
        data.extend([20, 2]);
        if number >= 757 {
            data.push(2);
        }
        data.extend([0, 1, 0, 1]);
        if number >= 759 {
            let mut death = vec![1];
            write_string("minecraft:the_nether", &mut death);
            death.extend(456_i64.to_be_bytes());
            data.extend(&death);
            respawn.extend(death);
        }
        if number >= 763 {
            data.push(10);
            respawn.push(10);
        }
    }
    let version = ProtocolVersion::new(number).unwrap();
    (
        Packet::new(version.switching().unwrap().join_game, data),
        respawn,
    )
}

#[test]
fn every_legacy_world_reset_preserves_destination_fields_and_rejects_truncation() {
    for number in (0..764).filter(|n| ids(*n).is_some()) {
        let (packet, expected) = join(number);
        let (reset, respawn) = world(number, &packet).unwrap();
        assert_eq!(respawn.id, ids(number).unwrap().respawn);
        assert_eq!(respawn.data, expected, "protocol {number}");
        let mut expected_join = packet.clone();
        if number < 108 {
            expected_join.data[5] = 255;
        } else if number < 735 {
            expected_join.data[5..9].fill(255);
        }
        assert_eq!(reset, expected_join);
        for cut in 0..packet.data.len() {
            assert!(
                world(number, &Packet::new(packet.id, packet.data[..cut].to_vec())).is_err(),
                "protocol {number}, cut {cut}"
            );
        }
        let mut trailing = packet;
        trailing.data.push(0);
        assert!(world(number, &trailing).is_err());
    }
}

#[test]
fn switching_removes_only_live_tab_entries_and_bossbars_and_resets_titles() {
    for number in [47, 107, 315, 735, 755, 759, 760, 761, 763] {
        let version = ProtocolVersion::new(number).unwrap();
        let ids = ids(number).unwrap();
        let mut state = LegacyState::default();
        let mut add = vec![if number >= 761 { 63 } else { 0 }, 2];
        for value in [1, 2] {
            add.extend([value; 16]);
            write_string("Player", &mut add);
            add.push(0); // No properties.
            if number >= 761 {
                add.push(0); // No chat session.
                add.extend([1, 1, 0, 0]); // Mode, listed, latency, display.
            } else {
                add.extend([1, 0, 0]);
                if number >= 759 {
                    add.push(0); // No profile key.
                }
            }
        }
        state
            .observe(version, &Packet::new(ids.players, add))
            .unwrap();
        let mut remove = if number >= 761 { vec![1] } else { vec![4, 1] };
        remove.extend([2; 16]);
        state
            .observe(
                version,
                &Packet::new(
                    if number >= 761 {
                        ids.remove_players
                    } else {
                        ids.players
                    },
                    remove,
                ),
            )
            .unwrap();
        if ids.boss >= 0 {
            for (uuid, action) in [(3, 0), (4, 0), (4, 1)] {
                let mut boss = vec![uuid; 16];
                boss.push(action);
                state
                    .observe(version, &Packet::new(ids.boss, boss))
                    .unwrap();
            }
        }
        let reset = state.reset(version).unwrap();
        let mut expected = if number >= 761 { vec![1] } else { vec![4, 1] };
        expected.extend([1; 16]);
        assert_eq!(reset[0].data, expected);
        if ids.boss >= 0 {
            let mut expected = vec![3; 16];
            expected.push(1);
            assert_eq!(reset[1], Packet::new(ids.boss, expected));
        }
        let title = if number >= 755 {
            1
        } else if number >= 315 {
            5
        } else {
            4
        };
        assert_eq!(reset.last().unwrap(), &Packet::new(ids.title, vec![title]));
        assert_eq!(
            state.reset(version).unwrap().len(),
            2,
            "state survives reset"
        );
    }
}

#[test]
fn malformed_nbt_is_bounded_and_never_panics() {
    for bytes in [
        vec![10, 0, 0, 7, 0, 0, 255, 255, 255, 255], // Negative array.
        vec![10, 0, 0, 9, 0, 0, 0, 0, 0, 0, 1, 0],   // Nonempty end-tag list.
        vec![10, 0, 0, 13, 0, 0, 0],                 // Unknown type.
        vec![0],
        [[10, 0, 0].repeat(66), vec![0; 66]].concat(),
    ] {
        assert!(super::super::nbt::named(&mut bytes.as_slice()).is_err());
    }
}

#[test]
fn legacy_commands_keep_signed_arguments_and_preserve_last_seen_acknowledgements() {
    for (number, id) in [(47, 1), (340, 2), (404, 2), (754, 3), (758, 3)] {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let mut chat = Vec::new();
        write_string("/server lobby", &mut chat);
        assert_eq!(
            caps.proxy_command_with(&Packet::new(id, chat), &[])
                .unwrap()
                .unwrap()
                .0,
            "server lobby"
        );
        let mut chat = Vec::new();
        write_string("server lobby", &mut chat);
        assert!(
            caps.proxy_command_with(&Packet::new(id, chat), &[])
                .unwrap()
                .is_none()
        );
    }
    for (number, id, ack_id) in [(759, 3, -1), (760, 4, 3)] {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        for signed in [false, true] {
            let mut data = Vec::new();
            write_string("server lobby", &mut data);
            data.extend([0; 16]);
            data.push(u8::from(signed));
            if signed {
                write_string("name", &mut data);
                data.extend([3, 1, 2, 3]); // VarInt signature length, not fixed 256 bytes.
            }
            data.push(0); // Signed preview.
            let mut update = vec![1];
            update.extend([7; 16]);
            update.extend([2, 8, 9, 0]); // Signature and no rejected message.
            if number == 760 {
                data.extend(&update);
            }
            let packet = Packet::new(id, data);
            let result = caps.proxy_command_with(&packet, &[]).unwrap();
            if signed {
                assert!(result.is_none());
            } else {
                let (command, acknowledgement) = result.unwrap();
                assert_eq!(command, "server lobby");
                assert_eq!(
                    acknowledgement,
                    if number == 760 {
                        Some(Packet::new(ack_id, update))
                    } else {
                        None
                    }
                );
            }
            for cut in 14..packet.data.len() {
                assert!(
                    caps.proxy_command_with(&Packet::new(id, packet.data[..cut].to_vec()), &[])
                        .is_err()
                );
            }
        }
    }
}

#[test]
fn legacy_login_profile_keys_are_bounded_and_do_not_shift_the_uuid() {
    for number in [759, 760] {
        let version = ProtocolVersion::new(number).unwrap();
        let mut data = Vec::new();
        write_string("Player", &mut data);
        data.push(1);
        data.extend(12345_i64.to_be_bytes());
        data.extend([3, 1, 2, 3, 2, 4, 5]); // Length-prefixed key and signature.
        if number == 760 {
            data.push(1);
            data.extend([9; 16]);
        }
        let packet = Packet::new(0, data);
        let identity = super::super::start_identity(version, &packet).unwrap();
        assert_eq!(identity.name, "Player");
        assert_eq!(
            identity.uuid,
            if number == 760 { Some([9; 16]) } else { None }
        );
        for cut in 0..packet.data.len() {
            assert!(
                super::super::start_identity(version, &Packet::new(0, packet.data[..cut].to_vec()))
                    .is_err()
            );
        }
        let mut invalid = packet;
        // Key length starts after name, presence flag and expiry.
        invalid.data.splice(16..17, [255, 255, 127]);
        assert!(super::super::start_identity(version, &invalid).is_err());
    }
}
