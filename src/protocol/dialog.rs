//! Dialogs (Minecraft 1.21.6 and newer): screens with text, inputs and
//! buttons that Rift shows on behalf of plugins, and the button clicks the
//! client reports back.
//!
//! Packet IDs come from the official server artifacts' packet reports for
//! every protocol from 771 (1.21.6) to 777 (26.3). The layouts are the same in
//! all of them (checked against Mojang's 1.21.6 mappings and the unobfuscated
//! 26.3 server):
//!
//! - Show Dialog (play): a `Holder<Dialog>`, here always inline: VarInt 0,
//!   then the dialog as anonymous network NBT.
//! - Show Dialog (configuration): the dialog as anonymous network NBT.
//! - Clear Dialog: no fields.
//! - Custom Click Action (serverbound): an identifier, then a VarInt length
//!   of at most 65536 and an optional anonymous NBT tag (`0` for none).
use super::{
    Packet, ProtocolVersion, State, invalid, packets::take, read_string, read_varint, write_varint,
};
use serde_json::Value;
use std::io;

/// Dialog packet IDs of one protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ids {
    play_show: i32,
    play_clear: i32,
    play_click: i32,
    config_show: i32,
    config_clear: i32,
    config_click: i32,
}

fn ids(version: ProtocolVersion) -> Option<Ids> {
    let (play_show, play_clear, play_click) = match version.number() {
        771 | 772 => (0x85, 0x84, 0x41),
        773 | 774 => (0x8a, 0x89, 0x41),
        775 | 776 => (0x8c, 0x8b, 0x44),
        777 => (0x8f, 0x8e, 0x44),
        _ => return None,
    };
    let (config_show, config_clear) = if version.number() == 777 {
        (0x13, 0x12)
    } else {
        (0x12, 0x11)
    };
    Some(Ids {
        play_show,
        play_clear,
        play_click,
        config_show,
        config_clear,
        config_click: 0x08,
    })
}

/// Whether a player on `protocol` can be shown dialogs.
pub fn supported(protocol: i32) -> bool {
    ProtocolVersion::new(protocol).ok().and_then(ids).is_some()
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "dialogs need Minecraft 1.21.6 or newer",
    )
}

/// Show Dialog for a client in play, carrying `dialog` inline. The caller
/// validated the dialog (see `crate::dialog`), so encoding cannot fail on
/// its shape, only on NBT limits.
pub(crate) fn show(version: ProtocolVersion, dialog: &Value) -> io::Result<Packet> {
    let ids = ids(version).ok_or_else(unsupported)?;
    let mut data = Vec::new();
    write_varint(0, &mut data);
    super::nbt::component(dialog, &mut data)?;
    Ok(Packet::new(ids.play_show, data))
}

/// Whether `value` has an NBT form (no `null` outside objects, no strings
/// over 65535 modified UTF-8 bytes).
pub(crate) fn encodable(value: &Value) -> io::Result<()> {
    super::nbt::component(value, &mut Vec::new())
}

/// Clear Dialog for a client in play.
pub(crate) fn clear(version: ProtocolVersion) -> io::Result<Packet> {
    let ids = ids(version).ok_or_else(unsupported)?;
    Ok(Packet::empty(ids.play_clear))
}

/// What a backend did to the client's dialog screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackendDialog {
    /// The backend showed a dialog of its own, replacing any other.
    Shown,
    /// The backend closed whatever dialog was open.
    Cleared,
}

/// Whether a clientbound backend packet shows or clears a dialog.
pub(crate) fn backend(
    version: ProtocolVersion,
    state: State,
    packet: &Packet,
) -> Option<BackendDialog> {
    let ids = ids(version)?;
    let (show, clear) = match state {
        State::Play => (ids.play_show, ids.play_clear),
        State::Configuration => (ids.config_show, ids.config_clear),
        _ => return None,
    };
    if packet.id == show {
        Some(BackendDialog::Shown)
    } else if packet.id == clear {
        Some(BackendDialog::Cleared)
    } else {
        None
    }
}

/// A Custom Click Action from the client.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Click<'a> {
    pub id: String,
    /// The undecoded payload tag; empty when the client sent none.
    payload: &'a [u8],
}

impl Click<'_> {
    /// The payload as JSON (`None` without one). Clients are untrusted: a
    /// malformed tag is an error, never a panic.
    pub fn payload(&self) -> io::Result<Option<Value>> {
        if self.payload.is_empty() {
            return Ok(None);
        }
        let mut bytes = self.payload;
        let value = super::nbt::to_json(&mut bytes)?;
        if !bytes.is_empty() {
            return Err(invalid("trailing data after click payload"));
        }
        Ok(Some(value))
    }
}

/// The payload length limit of `ServerboundCustomClickActionPacket`.
const MAX_PAYLOAD: usize = 65536;

/// Parse a serverbound packet received in `state` as a Custom Click Action,
/// or `None` when it is another packet.
pub(crate) fn click<'a>(
    version: ProtocolVersion,
    state: State,
    packet: &'a Packet,
) -> io::Result<Option<Click<'a>>> {
    let Some(ids) = ids(version) else {
        return Ok(None);
    };
    let id = match state {
        State::Play => ids.play_click,
        State::Configuration => ids.config_click,
        _ => return Ok(None),
    };
    if packet.id != id {
        return Ok(None);
    }
    let mut bytes = packet.data.as_slice();
    let id = read_string(&mut bytes, 32767)?.to_owned();
    let length = usize::try_from(read_varint(&mut bytes)?)
        .ok()
        .filter(|length| *length <= MAX_PAYLOAD)
        .ok_or_else(|| invalid("invalid click payload length"))?;
    let tag = take(&mut bytes, length)?;
    if !bytes.is_empty() {
        return Err(invalid("trailing click action data"));
    }
    // An empty optional tag is a single TAG_End.
    let payload = match tag {
        [] | [0] => &[][..],
        tag => tag,
    };
    Ok(Some(Click { id, payload }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::write_string;

    fn version(protocol: i32) -> ProtocolVersion {
        ProtocolVersion::new(protocol).unwrap()
    }

    #[test]
    fn dialogs_need_1_21_6() {
        assert!(!supported(770));
        for protocol in 771..=777 {
            assert!(supported(protocol), "{protocol}");
        }
        assert!(show(version(770), &serde_json::json!({})).is_err());
        assert!(clear(version(766)).is_err());
    }

    #[test]
    fn packet_ids_follow_the_packet_reports() {
        // (protocol, play show, play clear, play click, config show, config clear)
        for (protocol, show_id, clear_id, click_id, config_show, config_clear) in [
            (771, 0x85, 0x84, 0x41, 0x12, 0x11),
            (772, 0x85, 0x84, 0x41, 0x12, 0x11),
            (773, 0x8a, 0x89, 0x41, 0x12, 0x11),
            (774, 0x8a, 0x89, 0x41, 0x12, 0x11),
            (775, 0x8c, 0x8b, 0x44, 0x12, 0x11),
            (776, 0x8c, 0x8b, 0x44, 0x12, 0x11),
            (777, 0x8f, 0x8e, 0x44, 0x13, 0x12),
        ] {
            let v = version(protocol);
            assert_eq!(show(v, &serde_json::json!("x")).unwrap().id, show_id);
            assert_eq!(clear(v).unwrap().id, clear_id);
            let packet = Packet::new(click_id, {
                let mut data = Vec::new();
                write_string("rift:x", &mut data);
                write_varint(1, &mut data);
                data.push(0);
                data
            });
            assert!(click(v, State::Play, &packet).unwrap().is_some());
            let mut config = packet.clone();
            config.id = 0x08;
            assert!(click(v, State::Configuration, &config).unwrap().is_some());
            assert_eq!(
                backend(v, State::Configuration, &Packet::empty(config_show)),
                Some(BackendDialog::Shown)
            );
            assert_eq!(
                backend(v, State::Configuration, &Packet::empty(config_clear)),
                Some(BackendDialog::Cleared)
            );
            assert_eq!(
                backend(v, State::Play, &Packet::empty(show_id)),
                Some(BackendDialog::Shown)
            );
            assert_eq!(backend(v, State::Play, &Packet::empty(0x00)), None);
        }
    }

    #[test]
    fn show_writes_an_inline_holder_with_network_nbt() {
        let packet = show(
            version(772),
            &serde_json::json!({"type": "minecraft:notice", "title": "Hi"}),
        )
        .unwrap();
        // VarInt 0 (inline), then TAG_Compound without a name.
        assert_eq!(packet.data[..2], [0, 10]);
        let mut bytes = &packet.data[1..];
        assert_eq!(
            super::super::nbt::to_json(&mut bytes).unwrap(),
            serde_json::json!({"type": "minecraft:notice", "title": "Hi"})
        );
        assert!(bytes.is_empty());
    }

    fn click_packet(id: &str, payload: Option<&serde_json::Value>) -> Packet {
        let mut tag = Vec::new();
        match payload {
            Some(payload) => super::super::nbt::component(payload, &mut tag).unwrap(),
            None => tag.push(0),
        }
        let mut data = Vec::new();
        write_string(id, &mut data);
        write_varint(tag.len() as i32, &mut data);
        data.extend(tag);
        Packet::new(0x41, data)
    }

    #[test]
    fn clicks_decode_identifiers_and_payloads() {
        let v = version(772);
        let packet = click_packet("rift:d/1/0", None);
        let parsed = click(v, State::Play, &packet).unwrap().unwrap();
        assert_eq!(parsed.id, "rift:d/1/0");
        assert_eq!(parsed.payload().unwrap(), None);

        let values = serde_json::json!({
            "name": "Ünïcødé \u{0}\u{1F600}",
            "agree": true,
            "count": 7,
            "ratio": 0.3,
            "list": ["a", 1, {"b": 2}],
        });
        let packet = click_packet("rift:d/1/0", Some(&values));
        let parsed = click(v, State::Play, &packet).unwrap().unwrap();
        // Booleans travel as bytes; f64 stays f64 through this encoder.
        assert_eq!(
            parsed.payload().unwrap().unwrap(),
            serde_json::json!({
                "name": "Ünïcødé \u{0}\u{1F600}",
                "agree": 1,
                "count": 7,
                "ratio": 0.3,
                "list": ["a", 1, {"b": 2}],
            })
        );
        // Other serverbound packets are not clicks.
        assert!(
            click(v, State::Play, &Packet::empty(0x40))
                .unwrap()
                .is_none()
        );
        assert!(click(version(770), State::Play, &packet).unwrap().is_none());
    }

    #[test]
    fn float_tags_keep_their_shortest_decimal() {
        let mut data = vec![10, 5];
        data.extend(2u16.to_be_bytes());
        data.extend(b"hp");
        data.extend(0.3f32.to_be_bytes());
        data.push(0);
        let mut bytes = data.as_slice();
        assert_eq!(
            super::super::nbt::to_json(&mut bytes).unwrap(),
            serde_json::json!({"hp": 0.3})
        );
    }

    #[test]
    fn malformed_clicks_are_errors() {
        let v = version(772);
        // A payload length beyond the vanilla limit.
        let mut data = Vec::new();
        write_string("rift:x", &mut data);
        write_varint(65537, &mut data);
        assert!(click(v, State::Play, &Packet::new(0x41, data)).is_err());
        // A declared length longer than the packet.
        let mut data = Vec::new();
        write_string("rift:x", &mut data);
        write_varint(4, &mut data);
        data.push(10);
        assert!(click(v, State::Play, &Packet::new(0x41, data)).is_err());
        // Trailing bytes after the tag.
        let mut data = Vec::new();
        write_string("rift:x", &mut data);
        write_varint(1, &mut data);
        data.extend([0, 0]);
        assert!(click(v, State::Play, &Packet::new(0x41, data)).is_err());
        // A tag that does not decode.
        let mut data = Vec::new();
        write_string("rift:x", &mut data);
        write_varint(2, &mut data);
        data.extend([99, 0]);
        let packet = Packet::new(0x41, data);
        assert!(
            click(v, State::Play, &packet)
                .unwrap()
                .unwrap()
                .payload()
                .is_err()
        );
        // Lists nested deeper than any vanilla client sends: a list of one
        // list of one list ..., ending in an empty list.
        let nested = |levels: usize| {
            let mut tag = vec![9];
            for _ in 0..levels {
                tag.extend([9, 0, 0, 0, 1]);
            }
            tag.extend([0, 0, 0, 0, 0]);
            let mut data = Vec::new();
            write_string("rift:x", &mut data);
            write_varint(tag.len() as i32, &mut data);
            data.extend(tag);
            Packet::new(0x41, data)
        };
        let shallow = nested(8);
        assert!(
            click(v, State::Play, &shallow)
                .unwrap()
                .unwrap()
                .payload()
                .is_ok()
        );
        let deep = nested(40);
        assert!(
            click(v, State::Play, &deep)
                .unwrap()
                .unwrap()
                .payload()
                .is_err()
        );
    }
}
