//! Bus federation between the peers of a cluster (`docs/clustering.md`,
//! "Federation"). The broker speaks [`FederationFrame`]s through a
//! [`Federation`], which the cluster implements on top of its peer links;
//! inbound frames go to [`Broker::handle_federation_frame`] and link changes
//! to [`Broker::peer_up`] and [`Broker::peer_down`].
//!
//! [`Broker::handle_federation_frame`]: super::Broker::handle_federation_frame
//! [`Broker::peer_up`]: super::Broker::peer_up
//! [`Broker::peer_down`]: super::Broker::peer_down

use std::sync::Arc;

use bytes::{BufMut, Bytes, BytesMut};
use rift_messaging::frame::{
    exhausted, invalid, put_string, take_string, take_u8, take_u16, take_u32, take_u64,
};
use serde::{Deserialize, Serialize};

/// A peer's name: unique in the cluster and a valid subject token (no `.`,
/// `*`, `>` or whitespace), since inboxes carry it (`_INBOX.<peer>.<id>`).
pub type PeerId = Arc<str>;

/// The broker's view of the cluster's peer links.
///
/// Implementations must deliver the frames sent to one peer in the order
/// they were sent: an interest update must arrive before a message
/// published after it, so a reply finds the inbox a remote client just
/// subscribed to. [`Federation::send`] is called while the broker holds
/// internal locks, so it must not block and must not call back into the
/// broker; queue the frame instead. A frame for a peer whose link is down
/// may be dropped: the broker re-sends its whole interest from
/// [`Broker::peer_up`](super::Broker::peer_up).
pub trait Federation: Send + Sync + 'static {
    /// This peer's name.
    fn local_peer(&self) -> PeerId;
    /// The other peers whose links are up.
    fn peers(&self) -> Vec<PeerId>;
    /// Queue `frame` for `peer`.
    fn send(&self, peer: &PeerId, frame: FederationFrame);
}

/// Routes [`Scope::Owner`](super::Scope::Owner) subjects: `rift.node.<n>.cmd`
/// to the peer holding node `n`'s session, `rift.node.<n>.state` to the
/// leader. Without one, those subjects stay on the peer that publishes them.
pub trait OwnerRouter: Send + Sync + 'static {
    /// The peer that should receive `subject`, or `None` for this one.
    fn route(&self, subject: &str) -> Option<PeerId>;
}

/// What one subscription pattern contributes to a peer's interest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InterestKey {
    pub pattern: String,
    /// The queue group, or `None` for subscribers that receive every message.
    pub queue: Option<String>,
    /// Whether the subscribers are remote (QUIC) clients, which also hear
    /// other peers' [`Scope::Origin`](super::Scope::Origin) events.
    pub clients: bool,
}

/// The number of local subscriptions behind one [`InterestKey`]; zero
/// withdraws it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterestEntry {
    pub key: InterestKey,
    pub count: u32,
}

/// A peer's interest. Sequence numbers count every update a peer sends; a
/// receiver that sees a gap asks for a [`FederationFrame::Resync`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterestUpdate {
    pub sequence: u64,
    /// `true`: `entries` is the whole interest and replaces what the
    /// receiver knew. `false`: `entries` carries new counts for some keys.
    pub full: bool,
    pub entries: Vec<InterestEntry>,
}

/// A message published on another peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardedMessage {
    pub subject: String,
    pub reply: Option<String>,
    #[serde(with = "payload")]
    pub payload: Bytes,
    /// The authenticated principal that published it, `None` for Rift's own
    /// in-process publishers; see [`rift_messaging::Message::sender`].
    pub sender: Option<String>,
    /// The peer where it was published.
    pub origin: String,
    /// The queue groups the receiver should deliver it to (one member
    /// each). Subscribers outside queue groups always receive it.
    pub queues: Vec<String>,
}

/// One unit of bus federation traffic, on the cluster's `bus` stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FederationFrame {
    Interest(InterestUpdate),
    Message(ForwardedMessage),
    /// The receiver had no subscriber for a forwarded message with this
    /// reply subject, so a request waiting on it should not wait for it.
    NoResponders {
        reply: String,
    },
    /// The sender missed interest updates; answer with a full update.
    Resync,
}

const INTEREST: u8 = 1;
const MESSAGE: u8 = 2;
const NO_RESPONDERS: u8 = 3;
const RESYNC: u8 = 4;

impl FederationFrame {
    /// The compact binary form, in the string encoding of
    /// `docs/messaging-protocol.md` (`u16` length and UTF-8).
    pub fn encode(&self) -> std::io::Result<Bytes> {
        let mut out = BytesMut::new();
        match self {
            Self::Interest(update) => {
                out.put_u8(INTEREST);
                out.put_u64(update.sequence);
                out.put_u8(u8::from(update.full));
                out.put_u32(
                    u32::try_from(update.entries.len())
                        .map_err(|_| invalid("too many interest entries"))?,
                );
                for entry in &update.entries {
                    put_string(&mut out, &entry.key.pattern)?;
                    put_string(&mut out, entry.key.queue.as_deref().unwrap_or_default())?;
                    out.put_u8(u8::from(entry.key.clients));
                    out.put_u32(entry.count);
                }
            }
            Self::Message(message) => {
                out.put_u8(MESSAGE);
                put_string(&mut out, &message.subject)?;
                put_string(&mut out, message.reply.as_deref().unwrap_or_default())?;
                put_string(&mut out, message.sender.as_deref().unwrap_or_default())?;
                put_string(&mut out, &message.origin)?;
                out.put_u16(
                    u16::try_from(message.queues.len())
                        .map_err(|_| invalid("too many queue groups"))?,
                );
                for queue in &message.queues {
                    put_string(&mut out, queue)?;
                }
                out.extend_from_slice(&message.payload);
            }
            Self::NoResponders { reply } => {
                out.put_u8(NO_RESPONDERS);
                put_string(&mut out, reply)?;
            }
            Self::Resync => out.put_u8(RESYNC),
        }
        Ok(out.freeze())
    }

    /// Decode [`FederationFrame::encode`]'s form.
    pub fn decode(mut frame: Bytes) -> std::io::Result<Self> {
        let nonempty = |value: String| (!value.is_empty()).then_some(value);
        match take_u8(&mut frame)? {
            INTEREST => {
                let sequence = take_u64(&mut frame)?;
                let full = take_u8(&mut frame)? != 0;
                let count = take_u32(&mut frame)? as usize;
                // Every entry takes at least nine bytes.
                if count > frame.len() / 9 {
                    return Err(invalid("truncated interest update"));
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let pattern = take_string(&mut frame)?;
                    let queue = nonempty(take_string(&mut frame)?);
                    let clients = take_u8(&mut frame)? != 0;
                    let count = take_u32(&mut frame)?;
                    entries.push(InterestEntry {
                        key: InterestKey {
                            pattern,
                            queue,
                            clients,
                        },
                        count,
                    });
                }
                exhausted(&frame)?;
                Ok(Self::Interest(InterestUpdate {
                    sequence,
                    full,
                    entries,
                }))
            }
            MESSAGE => {
                let subject = take_string(&mut frame)?;
                let reply = nonempty(take_string(&mut frame)?);
                let sender = nonempty(take_string(&mut frame)?);
                let origin = take_string(&mut frame)?;
                let count = take_u16(&mut frame)? as usize;
                let mut queues = Vec::with_capacity(count.min(frame.len() / 2));
                for _ in 0..count {
                    queues.push(take_string(&mut frame)?);
                }
                Ok(Self::Message(ForwardedMessage {
                    subject,
                    reply,
                    payload: frame,
                    sender,
                    origin,
                    queues,
                }))
            }
            NO_RESPONDERS => {
                let reply = take_string(&mut frame)?;
                exhausted(&frame)?;
                Ok(Self::NoResponders { reply })
            }
            RESYNC => {
                exhausted(&frame)?;
                Ok(Self::Resync)
            }
            _ => Err(invalid("unknown federation frame")),
        }
    }
}

/// Serde for [`Bytes`] payloads: a byte string, also read from a sequence.
mod payload {
    use bytes::Bytes;
    use serde::{Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(payload: &Bytes, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(payload)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Bytes, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Bytes;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a byte string")
            }

            fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes::copy_from_slice(value))
            }

            fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes::from(value))
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Bytes, A::Error> {
                let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 20));
                while let Some(byte) = seq.next_element::<u8>()? {
                    bytes.push(byte);
                }
                Ok(Bytes::from(bytes))
            }
        }
        deserializer.deserialize_bytes(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_through_the_binary_and_serde_forms() {
        let frames = [
            FederationFrame::Interest(InterestUpdate {
                sequence: 7,
                full: true,
                entries: vec![
                    InterestEntry {
                        key: InterestKey {
                            pattern: "game.>".into(),
                            queue: None,
                            clients: false,
                        },
                        count: 2,
                    },
                    InterestEntry {
                        key: InterestKey {
                            pattern: "jobs.*".into(),
                            queue: Some("workers".into()),
                            clients: true,
                        },
                        count: 0,
                    },
                ],
            }),
            FederationFrame::Message(ForwardedMessage {
                subject: "game.score".into(),
                reply: Some("_INBOX.eu-1.ab".into()),
                payload: Bytes::from_static(b"\0payload"),
                sender: Some("server.lobby-1".into()),
                origin: "eu-1".into(),
                queues: vec!["workers".into()],
            }),
            FederationFrame::Message(ForwardedMessage {
                subject: "game.score".into(),
                reply: None,
                payload: Bytes::new(),
                sender: None,
                origin: "eu-2".into(),
                queues: vec![],
            }),
            FederationFrame::NoResponders {
                reply: "_INBOX.eu-1.ab".into(),
            },
            FederationFrame::Resync,
        ];
        for frame in frames {
            let encoded = frame.encode().unwrap();
            assert_eq!(FederationFrame::decode(encoded).unwrap(), frame);
            let json = serde_json::to_string(&frame).unwrap();
            assert_eq!(
                serde_json::from_str::<FederationFrame>(&json).unwrap(),
                frame
            );
        }
        assert!(FederationFrame::decode(Bytes::from_static(&[9])).is_err());
        assert!(FederationFrame::decode(Bytes::from_static(&[1, 0, 0])).is_err());
        assert!(
            FederationFrame::decode(Bytes::from_static(&[
                1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 255, 255, 255, 255
            ]))
            .is_err()
        );
    }
}
