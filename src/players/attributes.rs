//! The part of presence that is broadcast instead of agreed on: what a
//! connected player looks like (server, display name, skin, visibility, …).
//!
//! Only the peer that holds a player changes its attributes, so they need no
//! consensus. The holding peer sends every change as an [`AttributeUpdate`]
//! with its own sequence number; a receiver applies updates in sequence and
//! asks for a full resync ([`UpdateBody::Resync`]) when one is missing, after
//! a reconnect, or when the sender started a new incarnation. See
//! [`super::PlayerRegistry::apply_remote`] and
//! [`super::PlayerRegistry::full_resync`].
use super::Player;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::sync::Arc;

/// One signed profile property (the skin), as attributes carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Property {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub signature: Option<String>,
}

/// Everything other peers know about one player of the sending peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attributes {
    pub uuid: [u8; 16],
    pub name: String,
    pub server: String,
    pub connection: u64,
    #[serde(default)]
    pub properties: Vec<Property>,
    #[serde(default)]
    pub visible_to: Option<String>,
    #[serde(default)]
    pub display: Option<Json>,
    #[serde(default)]
    pub order: i32,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub protocol: i32,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    /// When the player joined, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub since: u64,
}

impl Attributes {
    /// The attributes of a local player.
    pub fn of(player: &Player) -> Self {
        Self {
            uuid: player.uuid,
            name: player.name.clone(),
            server: player.server.clone(),
            connection: player.connection,
            properties: player
                .properties
                .iter()
                .map(|property| Property {
                    name: property.name.clone(),
                    value: property.value.clone(),
                    signature: property.signature.clone(),
                })
                .collect(),
            visible_to: player.visible_to.as_deref().map(str::to_owned),
            display: player.display.as_deref().cloned(),
            order: player.order,
            ip: player.ip.to_string(),
            port: player.port,
            protocol: player.protocol,
            locale: player.locale.as_deref().map(str::to_owned),
            language: player.language.as_deref().map(str::to_owned),
            since: player.since,
        }
    }

    /// The roster record of a player on `peer`.
    pub fn into_player(self, peer: u16) -> Player {
        Player {
            uuid: self.uuid,
            name: self.name,
            server: self.server,
            properties: self
                .properties
                .into_iter()
                .map(|property| crate::auth::ProfileProperty {
                    name: property.name,
                    value: property.value,
                    signature: property.signature,
                })
                .collect(),
            visible_to: self.visible_to.map(Arc::from),
            display: self.display.map(Arc::new),
            order: self.order,
            peer,
            connection: self.connection,
            ip: Arc::from(self.ip),
            port: self.port,
            protocol: self.protocol,
            locale: self.locale.map(Arc::from),
            language: self.language.map(Arc::from),
            since: self.since,
        }
    }
}

/// One change to the sending peer's players.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttributeChange {
    /// A player joined or changed.
    Upsert(Attributes),
    /// A player left.
    Remove([u8; 16]),
}

/// What an update carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateBody {
    /// Changes since the previous sequence number.
    Changes(Vec<AttributeChange>),
    /// Every player of the sending peer, replacing what the receiver had;
    /// later changes continue from this sequence number.
    Resync(Vec<Attributes>),
}

/// A message on the `presence` stream from one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributeUpdate {
    pub peer: u16,
    pub incarnation: u64,
    pub sequence: u64,
    pub body: UpdateBody,
}

/// What [`super::PlayerRegistry::apply_remote`] did with an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    /// From this peer itself or from an older incarnation of the sender.
    Ignored,
    /// A sequence number is missing: ask the sender for a full resync.
    NeedResync,
}

/// Where a registry sends the changes to its own players. A cluster sends
/// them to every peer; a single Rift has none. `send` is called while the
/// registry is locked, so that sequence numbers follow the order of the
/// changes: it must not block or call back into the registry.
pub trait AttributeSink: Send + Sync + 'static {
    fn send(&self, update: AttributeUpdate);
}
