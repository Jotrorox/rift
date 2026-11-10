//! Packet conversions at each protocol boundary, with hand-built packets.
use super::convert_forced;
use crate::{
    Direction::{self, Clientbound, Serverbound},
    State::{self, Configuration, Play},
    codec::{Shared, testing::Defaults},
    wire::Writer,
};

/// Convert `body` (packet `name` of `from`) for `to`, as one step would.
fn convert(
    from: i32,
    to: i32,
    direction: Direction,
    state: State,
    name: &str,
    body: &[u8],
) -> Vec<(&'static str, Vec<u8>)> {
    let mut shared = match direction {
        Clientbound => Shared::new(to, from),
        Serverbound => Shared::new(from, to),
    };
    convert_forced(&mut shared, direction, state, name, body)
        .unwrap_or_else(|| panic!("{name} {from}->{to} has no rule"))
        .unwrap_or_else(|error| panic!("{name} {from}->{to}: {error}"))
}

fn play(from: i32, to: i32, name: &str, body: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    convert(from, to, Clientbound, Play, name, body)
}

fn json(text: &str) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.string(text);
    writer.into_inner()
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn titles_split_and_merge_across_1_17() {
    let title = json(r#"{"text":"T"}"#);
    let out = play(754, 755, "set_titles", &concat(&[&[0], &title]));
    assert_eq!(out, [("set_title_text", title.clone())]);
    let out = play(
        754,
        755,
        "set_titles",
        &[3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3],
    );
    assert_eq!(
        out,
        [(
            "set_titles_animation",
            vec![0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]
        )]
    );
    assert_eq!(
        play(754, 755, "set_titles", &[5]),
        [("clear_titles", vec![1])]
    );
    assert_eq!(
        play(754, 755, "set_titles", &[4]),
        [("clear_titles", vec![0])]
    );
    // And back.
    assert_eq!(
        play(755, 754, "set_subtitle_text", &title),
        [("set_titles", concat(&[&[1], &title]))]
    );
    assert_eq!(
        play(755, 754, "set_action_bar_text", &title),
        [("set_titles", concat(&[&[2], &title]))]
    );
    assert_eq!(
        play(755, 754, "clear_titles", &[1]),
        [("set_titles", vec![5])]
    );
    assert_eq!(
        play(
            755,
            754,
            "set_titles_animation",
            &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]
        ),
        [("set_titles", vec![3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3])]
    );
}

#[test]
fn combat_split_and_killer_removal() {
    let message = json(r#"{"text":"died"}"#);
    let died = concat(&[&[2, 7], &[0, 0, 0, 9], &message]);
    assert_eq!(
        play(754, 755, "player_combat", &died),
        [(
            "player_combat_kill",
            concat(&[&[7], &[0, 0, 0, 9], &message])
        )]
    );
    assert_eq!(
        play(754, 755, "player_combat", &[0]),
        [("player_combat_enter", vec![])]
    );
    assert_eq!(
        play(755, 754, "player_combat_end", &[5, 0, 0, 0, 1]),
        [("player_combat", vec![1, 5, 0, 0, 0, 1])]
    );
    assert_eq!(
        play(
            755,
            754,
            "player_combat_kill",
            &concat(&[&[7], &[0, 0, 0, 9], &message])
        ),
        [("player_combat", died)]
    );
    // 1.20 has no killer; going back the killer is unknown (-1).
    let kill = concat(&[&[7], &[0, 0, 0, 9], &message]);
    assert_eq!(
        play(762, 763, "player_combat_kill", &kill),
        [("player_combat_kill", concat(&[&[7], &message]))]
    );
    assert_eq!(
        play(763, 762, "player_combat_kill", &concat(&[&[7], &message])),
        [(
            "player_combat_kill",
            concat(&[&[7], &[255, 255, 255, 255], &message])
        )]
    );
}

#[test]
fn chat_sender_and_system_overlay() {
    let message = json(r#"{"text":"m"}"#);
    let old = concat(&[&message, &[1]]);
    let new = concat(&[&message, &[1], &[0; 16]]);
    assert_eq!(play(578, 735, "chat", &old), [("chat", new.clone())]);
    assert_eq!(play(735, 578, "chat", &new), [("chat", old)]);
    // 1.19 chat type 2 is the action bar, 1.19.1 sends a flag.
    assert_eq!(
        play(759, 760, "system_chat", &concat(&[&message, &[2]])),
        [("system_chat", concat(&[&message, &[1]]))]
    );
    assert_eq!(
        play(759, 760, "system_chat", &concat(&[&message, &[1]])),
        [("system_chat", concat(&[&message, &[0]]))]
    );
    assert_eq!(
        play(760, 759, "system_chat", &concat(&[&message, &[1]])),
        [("system_chat", concat(&[&message, &[2]]))]
    );
}

#[test]
fn chat_type_holders_from_1_21() {
    // disguised_chat: message, chat type, name, no target (NBT strings).
    let text = |value: &str| {
        let mut writer = Writer::new();
        writer.u8(8).u16(value.len() as u16).bytes(value.as_bytes());
        writer.into_inner()
    };
    let old = concat(&[&text("hi"), &[3], &text("Bob"), &[0]]);
    let new = concat(&[&text("hi"), &[4], &text("Bob"), &[0]]);
    assert_eq!(
        play(766, 767, "disguised_chat", &old),
        [("disguised_chat", new.clone())]
    );
    assert_eq!(
        play(767, 766, "disguised_chat", &new),
        [("disguised_chat", old)]
    );
    // An inline chat type has no ID for an older client: the first type.
    let mut inline = Writer::new();
    inline.varint(0);
    for _ in 0..2 {
        inline
            .string("chat.type.text")
            .varint(2)
            .varint(0)
            .varint(2);
        inline.u8(10).u8(0);
    }
    let packet = concat(&[&text("hi"), &inline.into_inner(), &text("Bob"), &[0]]);
    assert_eq!(
        play(767, 766, "disguised_chat", &packet),
        [(
            "disguised_chat",
            concat(&[&text("hi"), &[0], &text("Bob"), &[0]])
        )]
    );
    assert_eq!(
        play(767, 767, "disguised_chat", &packet),
        [("disguised_chat", packet.clone())]
    );
}

#[test]
fn player_chat_gains_a_global_index_in_1_21_5() {
    let mut body = Writer::new();
    body.uuid(7)
        .varint(0)
        .bool(false)
        .string("hi")
        .i64(1)
        .i64(2)
        .varint(0);
    body.bool(false).varint(0).varint(1);
    body.u8(8).u16(3).bytes(b"Bob").bool(false);
    let old = body.into_inner();
    let mut shared = Shared::new(770, 769);
    for index in 0..3u8 {
        let out = convert_forced(&mut shared, Clientbound, Play, "player_chat", &old)
            .unwrap()
            .unwrap();
        assert_eq!(out[0].1, concat(&[&[index], &old]));
    }
    assert_eq!(
        play(770, 769, "player_chat", &concat(&[&[5], &old])),
        [("player_chat", old)]
    );
}

#[test]
fn player_list_forms_convert_for_1_19_3() {
    let display = json(r#"{"text":"D"}"#);
    // 1.19.2 add: name, no properties, game mode 1, latency 20, display name, no key.
    let mut add = Writer::new();
    add.varint(0)
        .varint(1)
        .uuid(42)
        .string("Bob")
        .varint(0)
        .varint(1)
        .varint(20)
        .bool(true)
        .bytes(&display)
        .bool(false);
    let out = play(760, 761, "player_info", &add.into_inner());
    let mut update = Writer::new();
    update
        .u8(0b111101)
        .varint(1)
        .uuid(42)
        .string("Bob")
        .varint(0)
        .varint(1)
        .bool(true)
        .varint(20)
        .bool(true)
        .bytes(&display);
    assert_eq!(out, [("player_info_update", update.buf.clone())]);
    // Back: one add carrying everything, with no profile key.
    let mut add = Writer::new();
    add.varint(0)
        .varint(1)
        .uuid(42)
        .string("Bob")
        .varint(0)
        .varint(1)
        .varint(20)
        .bool(true)
        .bytes(&display)
        .bool(false);
    assert_eq!(
        play(761, 760, "player_info_update", &update.buf),
        [("player_info", add.into_inner())]
    );
    // Removal is its own packet from 1.19.3.
    let mut remove = Writer::new();
    remove.varint(4).varint(1).uuid(42);
    let mut removed = Writer::new();
    removed.varint(1).uuid(42);
    assert_eq!(
        play(760, 761, "player_info", &remove.buf),
        [("player_info_remove", removed.buf.clone())]
    );
    assert_eq!(
        play(761, 760, "player_info_remove", &removed.buf),
        [("player_info", remove.buf)]
    );
    // Several actions without an add become one packet each.
    let mut update = Writer::new();
    update.u8(0b10100).varint(1).uuid(42).varint(3).varint(0);
    let out = play(761, 760, "player_info_update", &update.buf);
    assert_eq!(
        out.iter()
            .map(|(name, body)| (*name, body[0]))
            .collect::<Vec<_>>(),
        [("player_info", 1), ("player_info", 2)]
    );
    // 1.21.2 list order and 1.21.4 hat are dropped for older clients.
    let mut update = Writer::new();
    update
        .u8(0b1100_0000)
        .varint(1)
        .uuid(42)
        .varint(5)
        .bool(true);
    let mut order_only = Writer::new();
    order_only.u8(0b0100_0000).varint(1).uuid(42).varint(5);
    assert_eq!(
        play(769, 768, "player_info_update", &update.buf),
        [("player_info_update", order_only.buf.clone())]
    );
    assert_eq!(play(768, 767, "player_info_update", &order_only.buf), []);
}

#[test]
fn scores_split_into_reset_in_1_20_3() {
    let mut set = Writer::new();
    set.string("Bob").varint(0).string("kills").varint(7);
    let mut new_set = Writer::new();
    new_set
        .string("Bob")
        .string("kills")
        .varint(7)
        .bool(false)
        .bool(false);
    assert_eq!(
        play(764, 765, "set_score", &set.buf),
        [("set_score", new_set.buf.clone())]
    );
    assert_eq!(
        play(765, 764, "set_score", &new_set.buf),
        [("set_score", set.buf)]
    );
    let mut remove = Writer::new();
    remove.string("Bob").varint(1).string("");
    let mut reset = Writer::new();
    reset.string("Bob").bool(false);
    assert_eq!(
        play(764, 765, "set_score", &remove.buf),
        [("reset_score", reset.buf.clone())]
    );
    assert_eq!(
        play(765, 764, "reset_score", &reset.buf),
        [("set_score", remove.buf)]
    );
    // A display name and a fixed number format are dropped for 1.20.2.
    let mut decorated = Writer::new();
    decorated
        .string("Bob")
        .string("kills")
        .varint(7)
        .bool(true)
        .u8(8)
        .u16(1)
        .bytes(b"B")
        .bool(true)
        .varint(2)
        .u8(8)
        .u16(1)
        .bytes(b"x");
    let mut plain = Writer::new();
    plain.string("Bob").varint(0).string("kills").varint(7);
    assert_eq!(
        play(765, 764, "set_score", &decorated.buf),
        [("set_score", plain.buf)]
    );
    assert_eq!(
        play(764, 763, "set_display_objective", &[1, 1, b'k']),
        [("set_display_objective", vec![1, 1, b'k'])]
    );
}

#[test]
fn objectives_and_teams_before_1_13() {
    let mut old = Writer::new();
    old.string("kills").i8(0).string("§cKills").string("hearts");
    let mut new = Writer::new();
    new.string("kills")
        .i8(0)
        .string(r#"{"extra":[{"color":"red","text":"Kills"}],"text":""}"#)
        .varint(1);
    assert_eq!(
        play(340, 393, "set_objective", &old.buf),
        [("set_objective", new.buf.clone())]
    );
    assert_eq!(
        play(393, 340, "set_objective", &new.buf),
        [("set_objective", old.buf)]
    );
    // 1.12 limits objective names to 32 characters.
    let mut long = Writer::new();
    long.string("kills")
        .i8(2)
        .string(&format!(r#"{{"text":"{}"}}"#, "x".repeat(40)))
        .varint(0);
    let out = play(393, 340, "set_objective", &long.buf);
    let mut expected = Writer::new();
    expected
        .string("kills")
        .i8(2)
        .string(&"x".repeat(32))
        .string("integer");
    assert_eq!(out, [("set_objective", expected.buf)]);
    // A 1.12 team without colour whose prefix ends in a colour code gets
    // that colour in 1.13.
    let mut team = Writer::new();
    team.string("t")
        .i8(2)
        .string("T")
        .string("§a")
        .string("")
        .i8(0)
        .string("never")
        .string("pushOwnTeam")
        .i8(-1);
    let mut expected = Writer::new();
    expected
        .string("t")
        .i8(2)
        .string(r#"{"text":"T"}"#)
        .i8(0)
        .string("never")
        .string("pushOwnTeam")
        .varint(10)
        .string(r#"{"text":""}"#)
        .string(r#"{"text":""}"#);
    assert_eq!(
        play(340, 393, "set_player_team", &team.buf),
        [("set_player_team", expected.buf)]
    );
}

#[test]
fn team_options_and_order_change() {
    let text = |value: &str| {
        let mut writer = Writer::new();
        writer.u8(8).u16(value.len() as u16).bytes(value.as_bytes());
        writer.into_inner()
    };
    let mut strings = Writer::new();
    strings
        .string("t")
        .i8(0)
        .bytes(&text("T"))
        .i8(3)
        .string("hideForOwnTeam")
        .string("pushOtherTeams")
        .varint(12)
        .bytes(&text("<"))
        .bytes(&text(">"))
        .varint(1)
        .string("Bob");
    let mut ids = Writer::new();
    ids.string("t")
        .i8(0)
        .bytes(&text("T"))
        .i8(3)
        .varint(3)
        .varint(2)
        .varint(12)
        .bytes(&text("<"))
        .bytes(&text(">"))
        .varint(1)
        .string("Bob");
    assert_eq!(
        play(769, 770, "set_player_team", &strings.buf),
        [("set_player_team", ids.buf.clone())]
    );
    assert_eq!(
        play(770, 769, "set_player_team", &ids.buf),
        [("set_player_team", strings.buf)]
    );
    // 26.2: names first, then options, an optional colour and the flags.
    let mut reordered = Writer::new();
    reordered
        .string("t")
        .i8(0)
        .bytes(&text("T"))
        .bytes(&text("<"))
        .bytes(&text(">"))
        .varint(3)
        .varint(2)
        .bool(true)
        .varint(12)
        .i8(3)
        .varint(1)
        .string("Bob");
    assert_eq!(
        play(775, 776, "set_player_team", &ids.buf),
        [("set_player_team", reordered.buf.clone())]
    );
    assert_eq!(
        play(776, 775, "set_player_team", &reordered.buf),
        [("set_player_team", ids.buf)]
    );
}

#[test]
fn resource_packs_gain_ids_in_1_20_3() {
    let mut old = Writer::new();
    old.string("https://x/p.zip")
        .string("abc")
        .bool(true)
        .bool(false);
    let out = play(764, 765, "resource_pack_push", &old.buf);
    let id = super::misc::name_uuid(b"https://x/p.zip");
    let mut new = Writer::new();
    new.uuid(id)
        .string("https://x/p.zip")
        .string("abc")
        .bool(true)
        .bool(false);
    assert_eq!(out, [("resource_pack_push", new.buf.clone())]);
    // An older client on a newer backend: answers are matched to the pack.
    let mut shared = Shared::new(764, 765);
    let pushed = convert_forced(
        &mut shared,
        Clientbound,
        Configuration,
        "resource_pack_push",
        &new.buf,
    )
    .unwrap()
    .unwrap();
    assert_eq!(pushed, [("resource_pack_push", old.buf.clone())]);
    let accepted = convert_forced(
        &mut shared,
        Serverbound,
        Configuration,
        "resource_pack",
        &[3],
    )
    .unwrap()
    .unwrap();
    let mut answer = Writer::new();
    answer.uuid(id).varint(3);
    assert_eq!(accepted, [("resource_pack", answer.buf)]);
    let loaded = convert_forced(
        &mut shared,
        Serverbound,
        Configuration,
        "resource_pack",
        &[0],
    )
    .unwrap()
    .unwrap();
    assert_eq!(loaded[0].1[16..], [0]);
    // Nothing pending any more: a stray answer goes nowhere.
    assert_eq!(
        convert_forced(
            &mut shared,
            Serverbound,
            Configuration,
            "resource_pack",
            &[0]
        )
        .unwrap()
        .unwrap(),
        []
    );
    // Removal cannot be expressed; newer results map to older ones.
    assert_eq!(play(765, 764, "resource_pack_pop", &[0]), []);
    let mut downloaded = Writer::new();
    downloaded.uuid(id).varint(4);
    assert_eq!(
        convert(
            765,
            764,
            Serverbound,
            Play,
            "resource_pack",
            &downloaded.buf
        ),
        []
    );
    let mut invalid = Writer::new();
    invalid.uuid(id).varint(5);
    assert_eq!(
        convert(765, 764, Serverbound, Play, "resource_pack", &invalid.buf),
        [("resource_pack", vec![2])]
    );
    // 1.17 added the forced flag and prompt.
    let mut oldest = Writer::new();
    oldest.string("u").string("h");
    let mut prompt = Writer::new();
    prompt.string("u").string("h").bool(false).bool(false);
    assert_eq!(
        play(754, 755, "resource_pack_push", &oldest.buf),
        [("resource_pack_push", prompt.buf.clone())]
    );
    assert_eq!(
        play(755, 754, "resource_pack_push", &prompt.buf),
        [("resource_pack_push", oldest.buf)]
    );
}

#[test]
fn server_data_through_every_layout() {
    let motd = json(r#"{"text":"M"}"#);
    let png = [0x89, b'P', b'N', b'G'];
    let icon = "data:image/png;base64,iVBORw==";
    let mut v759 = Writer::new();
    v759.bool(true)
        .bytes(&motd)
        .bool(true)
        .string(icon)
        .bool(true);
    let mut v760 = Writer::new();
    v760.bool(true)
        .bytes(&motd)
        .bool(true)
        .string(icon)
        .bool(false)
        .bool(false);
    let mut v761 = Writer::new();
    v761.bool(true)
        .bytes(&motd)
        .bool(true)
        .string(icon)
        .bool(false);
    let mut v762 = Writer::new();
    v762.bytes(&motd).bool(true).byte_array(&png).bool(false);
    assert_eq!(
        play(759, 760, "server_data", &v759.buf),
        [("server_data", v760.buf.clone())]
    );
    assert_eq!(
        play(760, 761, "server_data", &v760.buf),
        [("server_data", v761.buf.clone())]
    );
    assert_eq!(
        play(761, 762, "server_data", &v761.buf),
        [("server_data", v762.buf.clone())]
    );
    assert_eq!(
        play(762, 761, "server_data", &v762.buf),
        [("server_data", v761.buf)]
    );
    let mut v766 = Writer::new();
    v766.u8(8).u16(1).bytes(b"M").bool(false);
    let mut v765 = Writer::new();
    v765.u8(8).u16(1).bytes(b"M").bool(false).bool(false);
    assert_eq!(
        play(766, 765, "server_data", &v766.buf),
        [("server_data", v765.buf.clone())]
    );
    assert_eq!(
        play(765, 766, "server_data", &v765.buf),
        [("server_data", v766.buf)]
    );
}

#[test]
fn maps_across_their_layouts() {
    // 1.12: map 3, scale 0, tracking, one icon (type 1 frame, rotation 4), no colours.
    let old = [3, 0, 1, 1, 0x14, 5, 6, 0];
    let mut new = Writer::new();
    new.varint(3)
        .i8(0)
        .bool(true)
        .varint(1)
        .varint(1)
        .i8(5)
        .i8(6)
        .u8(4)
        .bool(false)
        .u8(0);
    assert_eq!(
        play(340, 393, "map_item_data", &old),
        [("map_item_data", new.buf.clone())]
    );
    assert_eq!(
        play(393, 340, "map_item_data", &new.buf),
        [("map_item_data", old.to_vec())]
    );
    // A banner decoration does not exist in 1.12 and is left out.
    let mut banner = Writer::new();
    banner
        .varint(3)
        .i8(0)
        .bool(true)
        .varint(1)
        .varint(12)
        .i8(5)
        .i8(6)
        .u8(4)
        .bool(false)
        .u8(0);
    assert_eq!(
        play(393, 340, "map_item_data", &banner.buf),
        [("map_item_data", vec![3, 0, 1, 0, 0])]
    );
    // 1.17: decorations optional; an older client gets the last ones again.
    let mut shared = Shared::new(754, 755);
    let mut with = Writer::new();
    with.varint(3)
        .i8(0)
        .bool(false)
        .bool(true)
        .varint(1)
        .varint(1)
        .i8(5)
        .i8(6)
        .u8(4)
        .bool(false)
        .u8(0);
    let out = convert_forced(&mut shared, Clientbound, Play, "map_item_data", &with.buf)
        .unwrap()
        .unwrap();
    let mut expected = Writer::new();
    expected
        .varint(3)
        .i8(0)
        .bool(true)
        .bool(false)
        .varint(1)
        .varint(1)
        .i8(5)
        .i8(6)
        .u8(4)
        .bool(false)
        .u8(0);
    assert_eq!(out[0].1, expected.buf);
    let without = [3, 0, 0, 0, 0];
    let out = convert_forced(&mut shared, Clientbound, Play, "map_item_data", &without)
        .unwrap()
        .unwrap();
    assert_eq!(out[0].1, expected.buf);
}

#[test]
fn routes_carry_converted_packets_end_to_end() {
    // A 1.17 client on a 1.16.5 backend, through the pair's default routes.
    let mut step = Defaults::new(755, 754);
    let title = json(r#"{"text":"T"}"#);
    let out = step
        .clientbound(Play, "set_titles", &concat(&[&[0], &title]))
        .unwrap();
    assert_eq!(
        (out[0].name.as_str(), out[0].data.clone()),
        ("set_title_text", title)
    );
    // A 1.20.2 client on a 1.20.4 backend gets JSON for NBT.
    let mut step = Defaults::new(764, 765);
    let out = step
        .clientbound(Play, "disconnect", &[8, 0, 3, b'b', b'y', b'e'])
        .unwrap();
    assert_eq!(out[0].data, json(r#"{"text":"bye"}"#));
    let out = step
        .clientbound(Configuration, "disconnect", &[8, 0, 3, b'b', b'y', b'e'])
        .unwrap();
    assert_eq!(out[0].data, json(r#"{"text":"bye"}"#));
}

#[test]
fn every_pair_routes_cleanly() {
    // Building every pair's defaults must not panic, and every text rule
    // that applies names packets the receiver knows or renames.
    for pair in crate::version::PROTOCOLS.windows(2) {
        let (older, newer) = (pair[0].0, pair[1].0);
        let _ = Defaults::new(older, newer);
        let _ = Defaults::new(newer, older);
    }
}
