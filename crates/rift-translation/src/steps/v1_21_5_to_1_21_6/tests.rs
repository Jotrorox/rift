use super::{NEWER, OLDER, down, up};
use crate::{
    Direction, State,
    nbt::Nbt,
    steps::v1_21_2_to_1_21_4::harness::{Harness, hex, recorded, single},
    wire::{Reader, Writer},
};

fn up() -> Harness {
    Harness::new(super::UP.unwrap()(), NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(super::DOWN.unwrap()(), OLDER, NEWER)
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

#[test]
fn difficulty_is_a_byte_before_1_21_6_and_a_varint_after() {
    // Real `change_difficulty` bodies: peaceful, unlocked, from both servers
    // and hard from the 1.21.6 server.
    for body in ["0000", "0300", "0201"] {
        let out = up()
            .clientbound(State::Play, "change_difficulty", &hex(body))
            .unwrap();
        assert_eq!(single(&out, "change_difficulty"), hex(body));
        let out = down()
            .clientbound(State::Play, "change_difficulty", &hex(body))
            .unwrap();
        assert_eq!(single(&out, "change_difficulty"), hex(body));
    }
    let out = down()
        .serverbound(State::Play, "change_difficulty", &[2])
        .unwrap();
    assert_eq!(single(&out, "change_difficulty"), [2]);
    let out = up()
        .serverbound(State::Play, "change_difficulty", &[3])
        .unwrap();
    assert_eq!(single(&out, "change_difficulty"), [3]);
    assert!(
        up().clientbound(State::Play, "change_difficulty", &hex("0400"))
            .is_err()
    );
    assert!(
        down()
            .clientbound(State::Play, "change_difficulty", &hex("8001"))
            .is_err()
    );
    assert!(
        down()
            .clientbound(State::Play, "change_difficulty", &hex("00"))
            .is_err()
    );
    assert!(
        up().serverbound(State::Play, "change_difficulty", &hex("0100"))
            .is_err()
    );
    assert!(
        down()
            .serverbound(State::Play, "change_difficulty", &[])
            .is_err()
    );
}

#[test]
fn player_commands_shift_around_the_sneak_actions() {
    // Start sprinting: 1 on 1.21.6, 3 on 1.21.5.
    let out = up()
        .serverbound(State::Play, "player_command", &hex("070100"))
        .unwrap();
    assert_eq!(single(&out, "player_command"), hex("070300"));
    let out = down()
        .serverbound(State::Play, "player_command", &hex("070300"))
        .unwrap();
    assert_eq!(single(&out, "player_command"), hex("070100"));
    // Riding jump strength survives.
    let out = down()
        .serverbound(State::Play, "player_command", &hex("07055a"))
        .unwrap();
    assert_eq!(single(&out, "player_command"), hex("07035a"));
    // A 1.21.5 client's shift actions: the server reads shift from input.
    for action in ["070000", "070100"] {
        assert!(
            down()
                .serverbound(State::Play, "player_command", &hex(action))
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        up().serverbound(State::Play, "player_command", &hex("070700"))
            .is_err()
    );
    assert!(
        down()
            .serverbound(State::Play, "player_command", &hex("070900"))
            .is_err()
    );
    assert!(
        down()
            .serverbound(State::Play, "player_command", &hex("07030000"))
            .is_err()
    );
}

#[test]
fn shift_input_becomes_sneak_commands_for_1_21_5() {
    let mut step = up();
    // The real 1.21.6 join names entity 1; take a different one.
    let mut login = Writer::new();
    login.i32(42).bool(false);
    step.clientbound(State::Play, "login", &login.buf).unwrap();
    let out = step
        .serverbound(State::Play, "player_input", &[0x21])
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        (out[0].name.as_str(), out[0].data.clone()),
        ("player_input", vec![0x21])
    );
    assert_eq!(
        (out[1].name.as_str(), out[1].data.clone()),
        ("player_command", hex("2a0000"))
    );
    // Still sneaking: nothing more to say.
    let out = step
        .serverbound(State::Play, "player_input", &[0x20])
        .unwrap();
    assert_eq!(single(&out, "player_input"), [0x20]);
    let out = step
        .serverbound(State::Play, "player_input", &[0x01])
        .unwrap();
    assert_eq!(out[1].data, hex("2a0100"));
    // After a respawn the server has a player that is not sneaking.
    step.serverbound(State::Play, "player_input", &[0x20])
        .unwrap();
    step.clientbound(State::Play, "respawn", &[0]).unwrap();
    let out = step
        .serverbound(State::Play, "player_input", &[0x20])
        .unwrap();
    assert_eq!(out[1].data, hex("2a0000"));
    assert!(
        step.serverbound(State::Play, "player_input", &[0x80])
            .is_err()
    );
    assert!(
        step.serverbound(State::Play, "player_input", &[0, 0])
            .is_err()
    );
}

#[test]
fn the_game_mode_switcher_runs_the_command_1_21_5_clients_sent() {
    let out = up()
        .serverbound(State::Play, "change_game_mode", &[1])
        .unwrap();
    let body = single(&out, "chat_command");
    assert_eq!(Reader::new(body).str().unwrap(), "gamemode creative");
    let out = up()
        .serverbound(State::Play, "change_game_mode", &[3])
        .unwrap();
    assert_eq!(
        Reader::new(&out[0].data).str().unwrap(),
        "gamemode spectator"
    );
    assert!(
        up().serverbound(State::Play, "change_game_mode", &[4])
            .is_err()
    );
    for state in [State::Configuration, State::Play] {
        let out = up()
            .serverbound(state, "custom_click_action", &hex("0461626364"))
            .unwrap();
        assert!(out.is_empty());
    }
}

/// A real 1.21.6 `show_dialog`: a notice titled "Rules" in gold with the
/// messages "Be nice" and "Link", the second opening a URL when clicked.
const NOTICE: &str = "000a090004626f64790a00000002080008636f6e74656e747300074265206e6963650800047479706500176d696e6563726166743a706c61696e5f6d657373616765000a0008636f6e74656e74730a000b636c69636b5f6576656e74080006616374696f6e00086f70656e5f75726c08000375726c001368747470733a2f2f6578616d706c652e636f6d000800047465787400044c696e6b000800047479706500176d696e6563726166743a706c61696e5f6d657373616765000a00057469746c65080005636f6c6f720004676f6c6408000474657874000552756c6573000800047479706500106d696e6563726166743a6e6f7469636500";

#[test]
fn spelled_out_dialogs_show_as_chat_for_1_21_5() {
    let out = down()
        .clientbound(State::Play, "show_dialog", &hex(NOTICE))
        .unwrap();
    let body = single(&out, "system_chat");
    let mut reader = Reader::new(body);
    let message = Nbt::read(&mut reader, false).unwrap().unwrap();
    assert!(!reader.bool().unwrap(), "chat, not the action bar");
    reader.finish().unwrap();
    let extra = message.as_compound().unwrap().list("extra").unwrap();
    let texts: Vec<&str> = extra
        .iter()
        .map(|part| part.as_compound().unwrap().str("text").unwrap())
        .collect();
    assert_eq!(texts, ["Rules", "\n", "Be nice", "\n", "Link"]);
    let title = extra[0].as_compound().unwrap();
    assert_eq!(title.str("color"), Some("gold"));
    assert_eq!(title.get("bold"), Some(&Nbt::Byte(1)));
    // The click event a 1.21.5 client might not read is gone.
    assert!(!extra[4].as_compound().unwrap().contains("click_event"));
    // The server's own dialogs are only an ID: nothing to show.
    assert!(
        down()
            .clientbound(State::Play, "show_dialog", &[3])
            .unwrap()
            .is_empty()
    );
    let mut broken = hex(NOTICE);
    broken.pop();
    assert!(
        down()
            .clientbound(State::Play, "show_dialog", &broken)
            .is_err()
    );
    for (state, name) in [
        (State::Configuration, "show_dialog"),
        (State::Configuration, "clear_dialog"),
        (State::Play, "clear_dialog"),
        (State::Play, "waypoint"),
    ] {
        assert!(
            down()
                .clientbound(state, name, &hex(&NOTICE[2..]))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn dialog_text_keeps_translations_and_drops_what_needs_the_server() {
    use crate::nbt::Compound;
    let mut score = Compound::new();
    score.insert("score", Nbt::Compound(Compound::new()));
    let mut translated = Compound::new();
    translated.insert("translate", "chat.type.text");
    translated.insert("with", Nbt::List(vec![Nbt::Int(7), Nbt::Int(8)]));
    translated.insert("extra", Nbt::List(vec![Nbt::Compound(score)]));
    let plain = super::plain_text(&Nbt::Compound(translated)).unwrap();
    assert_eq!(plain.str("translate"), Some("chat.type.text"));
    let with = plain.list("with").unwrap();
    assert_eq!(with[1].as_compound().unwrap().str("text"), Some("8"));
    assert!(!plain.contains("extra"));
    assert!(super::plain_text(&Nbt::Int(3)).is_some());
    assert!(super::plain_text(&Nbt::LongArray(vec![1])).is_none());
}

/// Recorded difficulty packets convert to what the other server sends.
#[test]
fn recorded_difficulty_converts_to_what_the_other_server_sends() {
    for (protocol, mut step, other) in [(OLDER, up(), NEWER), (NEWER, down(), OLDER)] {
        let (Some(sent), Some(expected)) = (
            recorded(
                protocol,
                Direction::Clientbound,
                State::Play,
                "change_difficulty",
            ),
            recorded(
                other,
                Direction::Clientbound,
                State::Play,
                "change_difficulty",
            ),
        ) else {
            return;
        };
        let out = step.replay(&sent[0]).unwrap();
        assert_eq!(single(&out, "change_difficulty"), expected[0].data);
    }
}

/// Whole recorded sessions of both versions cross the step in both
/// orientations. Packets the shared codecs own pass through unconverted
/// until the codecs convert them; nothing may fail.
#[test]
fn recorded_sessions_cross_the_step_both_ways() {
    use crate::steps::v1_21_2_to_1_21_4::harness::cross_sessions;
    for harness in [up(), down()] {
        if let Some(count) = cross_sessions(harness) {
            assert!(count > 500, "{count} packets");
        }
    }
}
