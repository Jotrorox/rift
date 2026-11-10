use super::{NEWER, OLDER, down, up};
use crate::{
    Direction, State,
    codec::testing::corpus,
    nbt::Nbt,
    steps::v1_20_2_to_1_20_3::support::{Harness, hex, names, truncations},
    wire::{Reader, Writer},
};

fn up() -> Harness {
    Harness::new(up::STEP.unwrap(), NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(down::STEP.unwrap(), OLDER, NEWER)
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

/// The head of both servers' `login` for the same session, up to the
/// dimension type: entity 1, three worlds, 20 players, distance 2.
const LOGIN_HEAD: &str = "00000001 00 03 136d696e6563726166743a6f766572776f726c64 \
     146d696e6563726166743a7468655f6e6574686572 116d696e6563726166743a7468655f656e64 \
     14 02 02 00 01 00";
const OVERWORLD: &str = "136d696e6563726166743a6f766572776f726c64";
/// World, seed, creative, no previous mode, not debug, flat, no death
/// location, no portal cooldown.
const SPAWN_TAIL: &str =
    "136d696e6563726166743a6f766572776f726c64 1b42ab42bb6be15d 01 ff 00 01 00 00";

fn older_login() -> Vec<u8> {
    hex(&format!("{LOGIN_HEAD} {OVERWORLD} {SPAWN_TAIL}"))
}

fn newer_login(secure_chat: bool) -> Vec<u8> {
    hex(&format!(
        "{LOGIN_HEAD} 00 {SPAWN_TAIL} 0{}",
        u8::from(secure_chat)
    ))
}

#[test]
fn login_names_or_numbers_the_dimension_type_and_moves_secure_chat() {
    let mut up = up();
    let out = up
        .clientbound(State::Play, "login", &older_login())
        .unwrap();
    assert_eq!(names(&out), ["login", "update_attributes"]);
    assert_eq!(out[0].data, newer_login(false));
    // Creative: vanilla's reach with the creative modifiers.
    let mut reach = Writer::new();
    reach.varint(1).varint(2);
    reach
        .varint(6)
        .f64(4.5)
        .varint(1)
        .uuid(0x736565d2_e1a7_403d_a3f8_1aeb3e302542);
    reach.f64(0.5).varint(0);
    reach
        .varint(7)
        .f64(3.0)
        .varint(1)
        .uuid(0x98491ef6_97b1_4584_ae82_71a8cc85cf73);
    reach.f64(2.0).varint(0);
    assert_eq!(out[1].data, reach.buf);

    // Survival from a game event: the plain base values.
    let out = up
        .clientbound(State::Play, "game_event", &hex("0300000000"))
        .unwrap();
    assert_eq!(names(&out), ["game_event", "update_attributes"]);
    let mut reach = Writer::new();
    reach
        .varint(1)
        .varint(2)
        .varint(6)
        .f64(4.5)
        .varint(0)
        .varint(7)
        .f64(3.0)
        .varint(0);
    assert_eq!(out[1].data, reach.buf);
    let out = up
        .clientbound(State::Play, "game_event", &hex("073c23d70a"))
        .unwrap();
    assert_eq!(names(&out), ["game_event"]);

    // `server_data` tells the backend enforces secure chat; the next login
    // says so.
    let out = up
        .clientbound(State::Play, "server_data", &hex("08 0001 41 00 01"))
        .unwrap();
    assert_eq!(out[0].data, hex("08 0001 41 00"));
    let out = up
        .clientbound(State::Play, "login", &older_login())
        .unwrap();
    assert_eq!(out[0].data, newer_login(true));

    let mut down = down();
    let out = down
        .clientbound(State::Play, "login", &newer_login(true))
        .unwrap();
    assert_eq!(names(&out), ["login"]);
    assert_eq!(out[0].data, older_login());
    let out = down
        .clientbound(State::Play, "server_data", &hex("08 0001 41 01 02 abcd"))
        .unwrap();
    assert_eq!(out[0].data, hex("08 0001 41 01 02 abcd 01"));

    for body in truncations(&older_login()) {
        assert!(self::up().clientbound(State::Play, "login", body).is_err());
    }
    for body in truncations(&newer_login(false)) {
        assert!(
            self::down()
                .clientbound(State::Play, "login", body)
                .is_err()
        );
    }
}

/// The dimension types the client was sent decide the numbering.
#[test]
fn dimension_types_follow_registry_data() {
    let element = |name: &str, id: i32| {
        let mut entry = crate::nbt::Compound::new();
        entry.insert("name", name);
        entry.insert("id", id);
        entry.insert("element", Nbt::Compound(crate::nbt::Compound::new()));
        Nbt::Compound(entry)
    };
    let mut registry = crate::nbt::Compound::new();
    registry.insert("type", "minecraft:dimension_type");
    registry.insert(
        "value",
        Nbt::List(vec![
            element("rift:void", 1),
            element("minecraft:overworld", 0),
        ]),
    );
    let mut codec = crate::nbt::Compound::new();
    codec.insert("minecraft:dimension_type", Nbt::Compound(registry));
    let codec = Nbt::Compound(codec).to_bytes(false);

    let mut up = up();
    up.clientbound(State::Configuration, "registry_data", &codec)
        .unwrap();
    let mut respawn = Writer::new();
    respawn.string("rift:void");
    let tail = hex(&format!("{SPAWN_TAIL} 03"));
    respawn.bytes(&tail);
    let out = up
        .clientbound(State::Play, "respawn", &respawn.buf)
        .unwrap();
    let mut expected = vec![1];
    expected.extend(&tail);
    assert_eq!(out[0].data, expected);
    // A respawn before any login sends no reach: the player is unknown.
    assert_eq!(names(&out), ["respawn"]);
    let mut unknown = Writer::new();
    unknown.string("minecraft:the_end").bytes(&tail);
    assert!(
        up.clientbound(State::Play, "respawn", &unknown.buf)
            .is_err()
    );

    let mut down = down();
    let mut registry = Writer::new();
    registry.string("minecraft:dimension_type").varint(2);
    registry.string("minecraft:overworld").bool(false);
    registry.string("rift:void").bool(true).bytes(&hex("0a00"));
    down.clientbound(State::Configuration, "registry_data", &registry.buf)
        .unwrap();
    let out = down.clientbound(State::Play, "respawn", &expected).unwrap();
    assert_eq!(out[0].data, respawn.buf);
    let mut out_of_range = vec![2];
    out_of_range.extend(&tail);
    assert!(
        down.clientbound(State::Play, "respawn", &out_of_range)
            .is_err()
    );
}

#[test]
fn login_phase_flags_are_added_and_removed() {
    // The same profile both servers sent; 1.20.5 adds strict error handling.
    let older = hex("888b6130823b387bb012d6dc24e47d59 06436f7270757300");
    let out = up()
        .clientbound(State::Login, "login_finished", &older)
        .unwrap();
    assert_eq!(
        out[0].data,
        hex("888b6130823b387bb012d6dc24e47d59 06436f7270757300 00")
    );
    let newer = hex("888b6130823b387bb012d6dc24e47d59 06436f7270757300 01");
    let out = down()
        .clientbound(State::Login, "login_finished", &newer)
        .unwrap();
    assert_eq!(out[0].data, older);

    let request = hex("00 03 010203 02 0405");
    let out = up().clientbound(State::Login, "hello", &request).unwrap();
    assert_eq!(out[0].data, hex("00 03 010203 02 0405 01"));
    let out = down()
        .clientbound(State::Login, "hello", &hex("00 03 010203 02 0405 00"))
        .unwrap();
    assert_eq!(out[0].data, request);
    for body in truncations(&newer) {
        assert!(
            down()
                .clientbound(State::Login, "login_finished", body)
                .is_err()
        );
    }
}

#[test]
fn cookies_are_kept_for_the_older_client() {
    let mut down = down();
    let key = |key: &str| {
        let mut body = Writer::new();
        body.string(key);
        body.buf
    };
    // Nothing stored yet, and in login nothing can be.
    for state in [State::Login, State::Configuration] {
        let out = down
            .clientbound(state, "cookie_request", &key("rift:a"))
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].direction, out[0].state),
            (Direction::Serverbound, state)
        );
        assert_eq!(out[0].name, "cookie_response");
        let mut expected = key("rift:a");
        expected.push(0);
        assert_eq!(out[0].data, expected);
    }
    let mut store = key("rift:a");
    store.extend([2, 7, 8]);
    assert!(
        down.clientbound(State::Configuration, "store_cookie", &store)
            .unwrap()
            .is_empty()
    );
    let out = down
        .clientbound(State::Play, "cookie_request", &key("rift:a"))
        .unwrap();
    let mut expected = key("rift:a");
    expected.extend([1, 2, 7, 8]);
    assert_eq!(out[0].data, expected);

    let mut huge = key("rift:b");
    let mut payload = Writer::new();
    payload.byte_array(&[0; 5121]);
    huge.extend(payload.buf);
    assert!(
        down.clientbound(State::Play, "store_cookie", &huge)
            .is_err()
    );
}

#[test]
fn transfers_disconnect_the_older_client() {
    let mut body = Writer::new();
    body.string("example.com").varint(25565);
    for state in [State::Configuration, State::Play] {
        let out = down().clientbound(state, "transfer", &body.buf).unwrap();
        assert_eq!(names(&out), ["disconnect"]);
        let reason = Nbt::from_bytes(&out[0].data, false).unwrap().unwrap();
        assert!(reason.as_str().unwrap().contains("example.com:25565"));
    }
}

#[test]
fn known_packs_are_answered_for_the_older_client() {
    let offer = hex("01 096d696e656372616674 04636f7265 06312e32302e36");
    let out = down()
        .clientbound(State::Configuration, "select_known_packs", &offer)
        .unwrap();
    assert_eq!(out.len(), 1);
    // What a 1.20.5 client without the pack answers.
    assert_eq!(
        (out[0].direction, out[0].name.as_str()),
        (Direction::Serverbound, "select_known_packs")
    );
    assert_eq!(out[0].data, [0]);
    assert!(
        down()
            .clientbound(State::Configuration, "reset_chat", &[])
            .unwrap()
            .is_empty()
    );
    assert!(
        down()
            .clientbound(State::Play, "debug_sample", &hex("00 00"))
            .unwrap()
            .is_empty()
    );
    let power = hex("05 0000000000000000 0000000000000000 0000000000000000");
    assert!(
        down()
            .clientbound(State::Play, "projectile_power", &power)
            .unwrap()
            .is_empty()
    );
    for (state, name) in [
        (State::Login, "cookie_response"),
        (State::Configuration, "cookie_response"),
        (State::Configuration, "select_known_packs"),
        (State::Play, "cookie_response"),
        (State::Play, "debug_sample_subscription"),
    ] {
        assert!(
            up().serverbound(state, name, &[0]).unwrap().is_empty(),
            "{name}"
        );
    }
}

fn last_seen(offset: i32, acknowledged: [u8; 3]) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(offset).bytes(&acknowledged);
    body.buf
}

fn signed_command(command: &str, timestamp: i64, offset: i32, acknowledged: [u8; 3]) -> Vec<u8> {
    let mut body = Writer::new();
    body.string(command)
        .i64(timestamp)
        .i64(42)
        .varint(1)
        .string("message");
    body.bytes(&[7; 256]);
    body.bytes(&last_seen(offset, acknowledged));
    body.buf
}

#[test]
fn commands_get_the_last_seen_update_the_older_backend_wants() {
    let mut up = up();
    // A signed command passes as the backend's command, signatures intact.
    let signed = signed_command("msg a hi", 1000, 2, [3, 0, 0]);
    let out = up
        .serverbound(State::Play, "chat_command_signed", &signed)
        .unwrap();
    assert_eq!(names(&out), ["chat_command"]);
    assert_eq!(out[0].data, signed);
    // Acknowledgements are held back...
    let out = up.serverbound(State::Play, "chat_ack", &[5]).unwrap();
    assert!(out.is_empty());
    // ...so an unsigned command can repeat the last update.
    let mut command = Writer::new();
    command.string("time set day");
    let out = up
        .serverbound(State::Play, "chat_command", &command.buf)
        .unwrap();
    let mut reader = Reader::new(&out[0].data);
    assert_eq!(reader.str().unwrap(), "time set day");
    assert!(reader.i64().unwrap() >= 1000);
    assert_eq!(reader.i64().unwrap(), 0); // Salt.
    assert_eq!(reader.varint().unwrap(), 0); // Signatures.
    assert_eq!(reader.rest(), last_seen(0, [3, 0, 0]));
    // A chat message carries what was held back.
    let mut chat = Writer::new();
    chat.string("hi")
        .i64(2000)
        .i64(1)
        .bool(false)
        .bytes(&last_seen(1, [1, 0, 0]));
    let out = up.serverbound(State::Play, "chat", &chat.buf).unwrap();
    let mut expected = Writer::new();
    expected
        .string("hi")
        .i64(2000)
        .i64(1)
        .bool(false)
        .bytes(&last_seen(6, [1, 0, 0]));
    assert_eq!(out[0].data, expected.buf);
    // Too much held back: all but one window goes on.
    assert!(
        up.serverbound(State::Play, "chat_ack", &[39])
            .unwrap()
            .is_empty()
    );
    let out = up.serverbound(State::Play, "chat_ack", &[1]).unwrap();
    assert_eq!(out[0].data, [20]);

    // The older client's commands go to the newer backend as signed ones.
    let out = down()
        .serverbound(State::Play, "chat_command", &signed)
        .unwrap();
    assert_eq!(
        (names(&out), &out[0].data),
        (vec!["chat_command_signed"], &signed)
    );

    for body in truncations(&signed) {
        assert!(
            self::up()
                .serverbound(State::Play, "chat_command_signed", body)
                .is_err()
        );
    }
    let mut too_many = Writer::new();
    too_many.string("x").i64(0).i64(0).varint(9);
    assert!(
        self::up()
            .serverbound(State::Play, "chat_command_signed", &too_many.buf)
            .is_err()
    );
}

#[test]
fn button_clicks_and_horse_inventories_change_shape() {
    let out = up()
        .serverbound(State::Play, "container_button_click", &hex("03 ac02"))
        .unwrap();
    assert_eq!(out[0].data, hex("03 2c"));
    let out = down()
        .serverbound(State::Play, "container_button_click", &hex("03 02"))
        .unwrap();
    assert_eq!(out[0].data, hex("03 02"));
    // A donkey with a chest: 16 slots on 1.20.5, 17 with the armor slot.
    let out = up()
        .clientbound(State::Play, "mount_screen_open", &hex("01 11 00000009"))
        .unwrap();
    assert_eq!(out[0].data, hex("01 10 00000009"));
    let out = down()
        .clientbound(State::Play, "mount_screen_open", &hex("01 10 00000009"))
        .unwrap();
    assert_eq!(out[0].data, hex("01 11 00000009"));
}

/// The text codec converts item hover events across the pair: the item and
/// its count always arrive, in the receiver's form.
#[test]
fn item_hover_text_is_left_to_the_text_codec() {
    let motd = |contents: crate::nbt::Compound| {
        let mut hover = crate::nbt::Compound::new();
        hover.insert("action", "show_item");
        hover.insert("contents", Nbt::Compound(contents));
        let mut motd = crate::nbt::Compound::new();
        motd.insert("text", "x");
        motd.insert("hoverEvent", Nbt::Compound(hover));
        Nbt::Compound(motd).to_bytes(false)
    };
    let shown = |out: &[crate::codec::testing::Out]| {
        let mut reader = crate::wire::Reader::new(&out[0].data);
        let motd = Nbt::read(&mut reader, false).unwrap().unwrap();
        let contents = motd
            .as_compound()
            .and_then(|motd| motd.compound("hoverEvent"))
            .and_then(|hover| hover.compound("contents"))
            .cloned()
            .unwrap();
        (
            contents.str("id").map(str::to_owned),
            contents.get("count").and_then(Nbt::as_i64),
        )
    };
    let mut older = crate::nbt::Compound::new();
    older.insert("id", "minecraft:diamond_sword");
    older.insert("count", Nbt::Int(1));
    older.insert("tag", "{Damage:5}");
    let mut body = motd(older);
    body.extend([0, 0]);
    let out = up().clientbound(State::Play, "server_data", &body).unwrap();
    assert_eq!(names(&out), ["server_data"]);
    assert_eq!(
        shown(&out),
        (Some("minecraft:diamond_sword".to_owned()), Some(1))
    );

    let mut newer = crate::nbt::Compound::new();
    newer.insert("id", "minecraft:diamond_sword");
    newer.insert("count", Nbt::Int(1));
    let mut body = motd(newer);
    body.push(0);
    let out = down()
        .clientbound(State::Play, "server_data", &body)
        .unwrap();
    // 1.20.4 leaves out a count of one, its default.
    assert_eq!(
        shown(&out),
        (Some("minecraft:diamond_sword".to_owned()), None)
    );
}

/// Every packet of both recorded sessions goes through both steps.
#[test]
fn recorded_sessions_go_through_both_steps() {
    for (make, client, backend) in [
        (up::STEP.unwrap(), NEWER, OLDER),
        (down::STEP.unwrap(), OLDER, NEWER),
    ] {
        let (Some(from_backend), Some(from_client)) = (corpus(backend), corpus(client)) else {
            eprintln!("corpus for {backend} or {client} not captured: skipped");
            continue;
        };
        let mut harness = Harness::new(make, client, backend);
        let packets = from_backend
            .iter()
            .filter(|packet| packet.direction == Direction::Clientbound)
            .map(|packet| (packet, backend))
            .chain(
                from_client
                    .iter()
                    .filter(|packet| packet.direction == Direction::Serverbound)
                    .map(|packet| (packet, client)),
            );
        for (packet, protocol) in packets {
            harness
                .send_id(packet.direction, packet.state, packet.id, &packet.data)
                .unwrap_or_else(|error| {
                    panic!(
                        "{protocol} {:?} {}: {error}",
                        packet.state,
                        packet.name(protocol)
                    )
                });
        }
    }
}

/// What each server sent for the same events, converted by the step, is
/// what the other server sent for them.
#[test]
fn converted_logins_match_what_the_other_server_sends() {
    let (Some(older), Some(newer)) = (corpus(OLDER), corpus(NEWER)) else {
        eprintln!("corpus not captured: skipped");
        return;
    };
    let find = |packets: &[crate::codec::testing::Recorded], protocol, name| {
        packets
            .iter()
            .find(|packet| {
                packet.direction == Direction::Clientbound && packet.name(protocol) == name
            })
            .unwrap()
            .data
            .clone()
    };
    let (older_login, newer_login) = (find(&older, OLDER, "login"), find(&newer, NEWER, "login"));
    // The seeds differ between the two worlds: compare around them.
    let out = up()
        .clientbound(State::Play, "login", &older_login)
        .unwrap();
    let seed = |data: &[u8], at: usize| data[..at].to_vec();
    let at = newer_login.len() - 15;
    assert_eq!(seed(&out[0].data, at), seed(&newer_login, at));
    assert_eq!(out[0].data[at + 8..], newer_login[at + 8..]);
}

/// The pair's stand-ins keep a block's shape and waterlogging, and the new
/// attributes stay unmapped so the entity codec drops them rather than
/// setting an unrelated attribute.
#[test]
fn new_registry_entries_stand_in_sensibly() {
    let (newer, older) = (crate::data::version(NEWER), crate::data::version(OLDER));
    let mapping = crate::mapping::mapping(NEWER, OLDER, "block_state");
    for (from, to) in [
        (
            "heavy_core[waterlogged=true]",
            "polished_deepslate_wall[east=none,north=none,south=none,up=true,waterlogged=true,west=none]",
        ),
        (
            "vault[facing=north,ominous=false,vault_state=active]",
            "trial_spawner[trial_spawner_state=inactive]",
        ),
    ] {
        let id = newer
            .block_states
            .parse(from)
            .unwrap_or_else(|| panic!("{from}"));
        let mapped = mapping
            .get(id)
            .unwrap_or_else(|| panic!("{from} is unmapped"));
        assert_eq!(older.block_states.describe(mapped).unwrap(), to);
    }
    let attribute = |name| newer.expect_registry("attribute").id(name).unwrap();
    let attributes = crate::mapping::mapping(NEWER, OLDER, "attribute");
    assert_eq!(attributes.get(attribute("generic.scale")), None);
    let items = crate::mapping::mapping(NEWER, OLDER, "item");
    let item = newer.expect_registry("item").id("armadillo_scute").unwrap();
    let scute = older.expect_registry("item").id("scute").unwrap();
    assert_eq!(items.get(item), Some(scute));
}

/// Write what the steps of both pairs produce from hand-made inputs to
/// `$RIFT_S10_PRODUCED`, for `schema_check.py` to decode with minecraft-data.
#[test]
#[ignore = "run by schema_check.py"]
fn dump_produced_packets() {
    use crate::steps::v1_20_2_to_1_20_3 as older_pair;
    let path = std::env::var("RIFT_S10_PRODUCED").expect("RIFT_S10_PRODUCED names the output");
    let string = |text: &str| {
        let mut writer = Writer::new();
        writer.string(text);
        writer.buf
    };
    let mut push_old = string("https://example.com/p.zip");
    push_old.extend(string("0123456789abcdef0123456789abcdef01234567"));
    push_old.extend([1, 0]);
    let mut push_new = vec![9; 16];
    push_new.extend(&push_old);
    let mut jigsaw = vec![0; 8];
    for text in [
        "minecraft:a",
        "minecraft:b",
        "minecraft:c",
        "minecraft:stone",
        "rollable",
    ] {
        jigsaw.extend(string(text));
    }
    let mut jigsaw_new = jigsaw.clone();
    jigsaw_new.extend([1, 2]);
    let mut status = vec![9; 16];
    status.push(6);
    let mut transfer = string("example.com");
    transfer.push(1);
    let mut store = string("rift:a");
    store.extend([1, 7]);
    let signed = signed_command("msg a hi", 1, 0, [0; 3]);
    let mut chat = Writer::new();
    chat.string("hi")
        .i64(1)
        .i64(1)
        .bool(false)
        .bytes(&last_seen(0, [0; 3]));
    let survival = older_login();
    let older_up = (
        older_pair::UP.unwrap(),
        older_pair::NEWER,
        older_pair::OLDER,
    );
    let older_down = (
        older_pair::DOWN.unwrap(),
        older_pair::OLDER,
        older_pair::NEWER,
    );
    let newer_up = (up::STEP.unwrap(), NEWER, OLDER);
    let newer_down = (down::STEP.unwrap(), OLDER, NEWER);
    use Direction::{Clientbound as C, Serverbound as S};
    use State::{Configuration, Login, Play};
    let cases: Vec<(_, Direction, State, &str, Vec<u8>)> = vec![
        (
            older_up,
            C,
            Configuration,
            "resource_pack_push",
            push_old.clone(),
        ),
        (older_up, C, Play, "resource_pack_push", push_old),
        (older_up, S, Play, "resource_pack", status.clone()),
        (older_up, S, Configuration, "resource_pack", status),
        (older_up, C, Play, "set_score", hex("01 61 01 01 62")),
        (older_up, C, Play, "set_score", hex("01 61 00 01 62 05")),
        (
            older_up,
            C,
            Play,
            "respawn",
            hex(&format!("{OVERWORLD} {SPAWN_TAIL} 00")),
        ),
        (older_up, S, Play, "set_jigsaw_block", jigsaw_new),
        (older_down, C, Play, "resource_pack_push", push_new),
        (older_down, S, Play, "resource_pack", vec![1]),
        (older_down, C, Play, "reset_score", hex("01 61 01 01 62")),
        (
            older_down,
            C,
            Play,
            "set_score",
            hex("01 61 01 62 05 01 08 0001 78 01 00"),
        ),
        (
            older_down,
            C,
            Play,
            "set_default_spawn_position",
            vec![0; 12],
        ),
        (older_down, C, Play, "game_event", hex("0d 00000000")),
        (older_down, S, Play, "set_jigsaw_block", jigsaw),
        (newer_up, C, Login, "hello", hex("00 01 01 01 02")),
        (
            newer_up,
            C,
            Login,
            "login_finished",
            hex("888b6130823b387bb012d6dc24e47d59 01 61 00"),
        ),
        (newer_up, C, Play, "login", survival),
        (
            newer_up,
            C,
            Play,
            "respawn",
            hex(&format!("{OVERWORLD} {SPAWN_TAIL} 01")),
        ),
        (newer_up, C, Play, "game_event", hex("03 3f800000")),
        (newer_up, C, Play, "server_data", hex("08 0001 41 00 01")),
        (
            newer_up,
            C,
            Play,
            "mount_screen_open",
            hex("01 03 00000001"),
        ),
        (newer_up, S, Play, "chat", chat.buf),
        (newer_up, S, Play, "chat_command_signed", signed.clone()),
        (newer_up, S, Play, "chat_command", string("time set day")),
        (newer_up, S, Play, "container_button_click", hex("01 02")),
        (newer_down, C, Login, "hello", hex("00 01 01 01 02 01")),
        (
            newer_down,
            C,
            Login,
            "login_finished",
            hex("888b6130823b387bb012d6dc24e47d59 01 61 00 01"),
        ),
        (newer_down, C, Login, "cookie_request", string("rift:a")),
        (newer_down, C, Configuration, "store_cookie", store),
        (
            newer_down,
            C,
            Configuration,
            "cookie_request",
            string("rift:a"),
        ),
        (
            newer_down,
            C,
            Configuration,
            "select_known_packs",
            hex("00"),
        ),
        (newer_down, C, Configuration, "transfer", transfer.clone()),
        (newer_down, C, Play, "transfer", transfer),
        (newer_down, C, Play, "login", newer_login(true)),
        (
            newer_down,
            C,
            Play,
            "respawn",
            hex(&format!("00 {SPAWN_TAIL} 01")),
        ),
        (newer_down, C, Play, "server_data", hex("08 0001 41 00")),
        (
            newer_down,
            C,
            Play,
            "mount_screen_open",
            hex("01 02 00000001"),
        ),
        (newer_down, S, Play, "chat_command", signed),
        (newer_down, S, Play, "container_button_click", hex("01 02")),
    ];
    let mut lines = String::new();
    let mut harnesses: Vec<(crate::steps::Make, Harness)> = Vec::new();
    for ((make, client, backend), direction, state, name, body) in cases {
        let index = match harnesses
            .iter()
            .position(|(known, _)| *known as usize == make as usize)
        {
            Some(index) => index,
            None => {
                harnesses.push((make, Harness::new(make, client, backend)));
                harnesses.len() - 1
            }
        };
        let out = harnesses[index]
            .1
            .send(direction, state, name, &body)
            .unwrap();
        for out in out {
            let protocol = match out.direction {
                Direction::Clientbound => client,
                Direction::Serverbound => backend,
            };
            let id =
                crate::data::version(protocol).expect_packet(out.state, out.direction, &out.name);
            let line = serde_json::json!({
                "protocol": protocol,
                "d": if out.direction == Direction::Clientbound { "c" } else { "s" },
                "s": match out.state { Login => "login", Configuration => "configuration", _ => "play" },
                "id": id,
                "name": out.name,
                "hex": out.data.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            });
            lines.push_str(&format!("{line}\n"));
        }
    }
    std::fs::write(path, lines).unwrap();
}
