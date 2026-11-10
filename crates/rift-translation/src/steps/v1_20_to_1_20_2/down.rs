//! A 1.20 client on a 1.20.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
//!
//! The client goes from Login Success straight to play and waits for Join
//! Game; the backend first runs a configuration phase. The step
//! acknowledges Login Success on the client's behalf and runs that phase
//! itself: it keeps the registries for the Join Game, keeps feature flags,
//! tags, plugin messages and resource packs to deliver right after it,
//! answers keep-alives, pings and resource packs (accepted and loaded: the
//! client gets the pack after joining and answers for real then), and
//! acknowledges the finish. Nothing reaches the client in between except a
//! disconnect, as from a 1.20.1 server that is still preparing the join.
//!
//! When the backend re-enters configuration from play (a proxy behind it
//! switching servers), the step acknowledges and runs it the same way while
//! the client stays in its old world, then turns the next Join Game into a
//! Join Game plus a Respawn into the same dimension: the reset Rift itself
//! performs for pre-1.20.2 clients when switching servers. Client packets
//! during configuration are dropped, apart from settings and plugin
//! messages, which go to the backend in their configuration forms.
//!
//! Chunk batches are acknowledged here at once, asking for vanilla's
//! maximum rate: a 1.20.1 client takes chunks as fast as they come.
use super::{Held, NEWER, OLDER, convert, layout};
use crate::{
    Direction, Packet, State,
    codec::{
        HasShared, Shared, entity,
        registries::{self, form},
    },
    data,
    nbt::Nbt,
    step::{Cx, RouteBuilder, Routes, Step},
    steps::Make,
    wire::{Reader, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

/// The chunks per tick the step asks a 1.20.2 server for: its upper bound.
const CHUNKS_PER_TICK: f32 = 64.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Login,
    /// The backend configures; the client waits in play.
    Configuring,
    /// The backend is in play and about to send Join Game.
    AwaitJoin,
    Play,
}

/// Packet IDs the step sends on its own or looks for.
struct Ids {
    // 1.20.2, serverbound.
    login_acknowledged: i32,
    configuration_keep_alive: i32,
    pong: i32,
    configuration_resource_pack: i32,
    configuration_finish: i32,
    configuration_client_information: i32,
    configuration_custom_payload: i32,
    configuration_acknowledged: i32,
    batch_received: i32,
    // 1.20.2 play, clientbound: conversions of configuration packets.
    play_custom_payload: i32,
    play_disconnect: i32,
    play_tags: i32,
    play_resource_pack: i32,
    // 1.20.1, clientbound.
    respawn: i32,
    features: i32,
    rotate_head: i32,
    entity_motion: i32,
    spawn_player: i32,
    // 1.20.1 play, serverbound.
    client_information: i32,
    custom_payload: i32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        use Direction::*;
        use State::*;
        let (older, newer) = (data::version(OLDER), data::version(NEWER));
        Ids {
            login_acknowledged: newer.expect_packet(Login, Serverbound, "login_acknowledged"),
            configuration_keep_alive: newer.expect_packet(Configuration, Serverbound, "keep_alive"),
            pong: newer.expect_packet(Configuration, Serverbound, "pong"),
            configuration_resource_pack: newer.expect_packet(
                Configuration,
                Serverbound,
                "resource_pack",
            ),
            configuration_finish: newer.expect_packet(
                Configuration,
                Serverbound,
                "finish_configuration",
            ),
            configuration_client_information: newer.expect_packet(
                Configuration,
                Serverbound,
                "client_information",
            ),
            configuration_custom_payload: newer.expect_packet(
                Configuration,
                Serverbound,
                "custom_payload",
            ),
            configuration_acknowledged: newer.expect_packet(
                Play,
                Serverbound,
                "configuration_acknowledged",
            ),
            batch_received: newer.expect_packet(Play, Serverbound, "chunk_batch_received"),
            play_custom_payload: newer.expect_packet(Play, Clientbound, "custom_payload"),
            play_disconnect: newer.expect_packet(Play, Clientbound, "disconnect"),
            play_tags: newer.expect_packet(Play, Clientbound, "update_tags"),
            play_resource_pack: newer.expect_packet(Play, Clientbound, "resource_pack_push"),
            respawn: older.expect_packet(Play, Clientbound, "respawn"),
            features: older.expect_packet(Play, Clientbound, "update_enabled_features"),
            rotate_head: older.expect_packet(Play, Clientbound, "rotate_head"),
            entity_motion: older.expect_packet(Play, Clientbound, "set_entity_motion"),
            spawn_player: older.expect_packet(Play, Clientbound, "add_player"),
            client_information: older.expect_packet(Play, Serverbound, "client_information"),
            custom_payload: older.expect_packet(Play, Serverbound, "custom_payload"),
        }
    })
}

pub struct Down {
    shared: Shared,
    phase: Phase,
    /// The registries of the last configuration as a 1.20.1 codec.
    registries: Option<Nbt>,
    /// Feature flags of the current configuration, for after Join Game.
    features: Option<Vec<u8>>,
    /// Configuration packets turned into 1.20.1 play packets, for after
    /// Join Game.
    held: Held,
    /// Whether the client has had a Join Game already.
    joined: bool,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            phase: Phase::Login,
            registries: None,
            features: None,
            held: Held::default(),
            joined: false,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Login, "login_finished", Self::login_finished)
                .serverbound(Login, "hello", Self::hello)
                .clientbound(Configuration, "update_enabled_features", Self::features)
                .clientbound(Configuration, "update_tags", Self::hold_as_play)
                .clientbound(Configuration, "custom_payload", Self::hold_as_play)
                .clientbound(Configuration, "keep_alive", Self::keep_alive)
                .clientbound(Configuration, "ping", Self::ping)
                .clientbound(Configuration, "resource_pack_push", Self::resource_pack)
                .clientbound(Configuration, "disconnect", Self::disconnect)
                .clientbound(
                    Configuration,
                    "finish_configuration",
                    Self::finish_configuration,
                )
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "start_configuration", Self::start_configuration)
                .clientbound(Play, "add_entity", Self::add_entity)
                .clientbound(Play, "game_event", Self::game_event)
                .clientbound(Play, "set_display_objective", Self::display_objective)
                .clientbound(Play, "tag_query", Self::tag_query)
                .drop_clientbound(Play, "chunk_batch_start")
                .clientbound(Play, "chunk_batch_finished", Self::chunk_batch_finished)
                // Only an answer to a ping the client cannot have sent.
                .drop_clientbound(Play, "pong_response")
                .serverbound(Play, "set_beacon", Self::set_beacon)
                .build()
        })
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_client(state, packet);
        cx.to_backend(State::Login, Packet::empty(ids().login_acknowledged));
        self.enter_configuration();
        Ok(())
    }

    fn enter_configuration(&mut self) {
        self.phase = Phase::Configuring;
        self.features = None;
    }

    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_backend(
            state,
            Packet::new(packet.id, layout::hello(&packet.data, true)?),
        );
        Ok(())
    }

    fn features(&mut self, _: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        for _ in 0..r.count()? {
            r.str()?;
        }
        r.finish()?;
        self.features = Some(packet.data);
        Ok(())
    }

    /// A configuration packet whose play form has the same layout: convert
    /// it as that and keep the result for after Join Game.
    fn hold_as_play(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let ids = ids();
        let name = data::version(NEWER)
            .packet_name(state, Direction::Clientbound, packet.id)
            .unwrap_or("unknown");
        let id = match name {
            "update_tags" => ids.play_tags,
            "custom_payload" => ids.play_custom_payload,
            "resource_pack_push" => ids.play_resource_pack,
            _ => return Err(invalid("no play form for a configuration packet")),
        };
        let routes = Self::routes();
        let converted = convert(
            routes,
            self,
            cx,
            Direction::Clientbound,
            State::Play,
            Packet::new(id, packet.data),
        )?;
        for emitted in converted {
            match emitted.direction {
                Direction::Clientbound => self.held.push(emitted.packet)?,
                Direction::Serverbound => cx.to_backend(emitted.state, emitted.packet),
            }
        }
        Ok(())
    }

    fn keep_alive(&mut self, cx: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        if packet.data.len() != 8 {
            return Err(invalid("invalid keep-alive"));
        }
        cx.to_backend(
            State::Configuration,
            Packet::new(ids().configuration_keep_alive, packet.data),
        );
        Ok(())
    }

    fn ping(&mut self, cx: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        if packet.data.len() != 4 {
            return Err(invalid("invalid ping"));
        }
        cx.to_backend(State::Configuration, Packet::new(ids().pong, packet.data));
        Ok(())
    }

    fn resource_pack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        self.hold_as_play(cx, state, packet)?;
        // The backend's configuration waits for an answer that is not
        // "accepted"; the client answers again when it sees the pack.
        for action in [3, 0] {
            let mut body = Writer::new();
            body.varint(action);
            cx.to_backend(
                State::Configuration,
                Packet::new(ids().configuration_resource_pack, body.buf),
            );
        }
        Ok(())
    }

    fn disconnect(&mut self, cx: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        let routes = Self::routes();
        let packet = Packet::new(ids().play_disconnect, packet.data);
        for emitted in convert(
            routes,
            self,
            cx,
            Direction::Clientbound,
            State::Play,
            packet,
        )? {
            match emitted.direction {
                Direction::Clientbound => cx.to_client(emitted.state, emitted.packet),
                Direction::Serverbound => cx.to_backend(emitted.state, emitted.packet),
            }
        }
        Ok(())
    }

    fn finish_configuration(
        &mut self,
        cx: &mut Cx<'_>,
        _: State,
        packet: Packet,
    ) -> io::Result<()> {
        if !packet.data.is_empty() {
            return Err(invalid("configuration finish with a payload"));
        }
        cx.to_backend(
            State::Configuration,
            Packet::empty(ids().configuration_finish),
        );
        self.phase = Phase::AwaitJoin;
        Ok(())
    }

    /// The registries come from configuration; what the configuration held
    /// back follows the Join Game, as from a 1.20.1 server.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let join = layout::Join::read_newer(&packet.data)?;
        // The registries codec converted this configuration's registry data
        // and holds it for here.
        let pending = registries::take_pending(&mut self.shared);
        if !pending.is_empty() {
            self.registries = Some(form::write_codec(OLDER, &pending)?);
        }
        let registries = self
            .registries
            .as_ref()
            .ok_or_else(|| invalid("Join Game before any registry data"))?;
        cx.to_client(state, Packet::new(packet.id, join.write_older(registries)));
        if self.joined {
            cx.to_client(state, Packet::new(ids().respawn, join.respawn_older()));
        }
        if let Some(features) = self.features.take() {
            cx.to_client(state, Packet::new(ids().features, features));
        }
        for held in self.held.take() {
            cx.to_client(state, held);
        }
        self.joined = true;
        self.phase = Phase::Play;
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_client(
            state,
            Packet::new(packet.id, layout::respawn(&packet.data, false)?),
        );
        Ok(())
    }

    fn start_configuration(&mut self, cx: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        if !packet.data.is_empty() || self.phase != Phase::Play {
            return Err(invalid("unexpected start of configuration"));
        }
        cx.to_backend(State::Play, Packet::empty(ids().configuration_acknowledged));
        self.enter_configuration();
        Ok(())
    }

    /// Add Entity of a player was Spawn Player before 1.20.2, with the head
    /// rotation and velocity in their own packets.
    // INTEGRATION: `codec::entity` owns Add Entity in general; this step owns
    // it for its pair because of the split. Entity types and falling-block
    // states are mapped by name here until the codec offers a helper.
    fn add_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let ids = ids();
        let mut entity = layout::AddEntity::read(&packet.data)?;
        // The entity codec converts this entity's data by its type.
        let names = (
            entity::type_name(NEWER, entity.kind),
            self.shared
                .mapping(Direction::Clientbound, "entity_type")
                .get(entity.kind)
                .and_then(|kind| entity::type_name(OLDER, kind)),
        );
        if let (Some(backend), Some(client)) = names {
            self.shared
                .entity
                .track(entity.id, entity::Tracked { backend, client });
        }
        if entity.kind == layout::player_type(NEWER) {
            cx.to_client(state, Packet::new(ids.spawn_player, entity.spawn_player()));
            let mut head = Writer::new();
            head.varint(entity.id).i8(entity.head_yaw);
            cx.to_client(state, Packet::new(ids.rotate_head, head.buf));
            if entity.velocity != [0; 3] {
                let mut motion = Writer::new();
                motion.varint(entity.id);
                for value in entity.velocity {
                    motion.i16(value);
                }
                cx.to_client(state, Packet::new(ids.entity_motion, motion.buf));
            }
            return Ok(());
        }
        let types = self.shared.mapping(Direction::Clientbound, "entity_type");
        let Some(kind) = types.get(entity.kind) else {
            // The client could not show it; later packets for it are ignored.
            return Ok(());
        };
        let falling_block = data::version(NEWER)
            .expect_registry("entity_type")
            .id("falling_block");
        if Some(entity.kind) == falling_block {
            let states = self.shared.mapping(Direction::Clientbound, "block_state");
            entity.data = states.get(entity.data).unwrap_or(0);
        }
        entity.kind = kind;
        cx.to_client(state, Packet::new(packet.id, entity.write()));
        Ok(())
    }

    /// 1.20.2 added event 12 (limited crafting), which 1.20.1 lacks.
    fn game_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let event = r.u8()?;
        r.f32()?;
        r.finish()?;
        if event <= 11 {
            cx.to_client(state, packet);
        }
        Ok(())
    }

    fn display_objective(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        if let Some(body) = layout::display_objective(&packet.data, false)? {
            cx.to_client(state, Packet::new(packet.id, body));
        }
        Ok(())
    }

    fn tag_query(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_client(
            state,
            Packet::new(packet.id, layout::tag_query(&packet.data, false)?),
        );
        Ok(())
    }

    fn chunk_batch_finished(
        &mut self,
        cx: &mut Cx<'_>,
        _: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        r.varint()?;
        r.finish()?;
        let mut body = Writer::new();
        body.f32(CHUNKS_PER_TICK);
        cx.to_backend(State::Play, Packet::new(ids().batch_received, body.buf));
        Ok(())
    }

    fn set_beacon(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let effects = self.shared.mapping(Direction::Serverbound, "mob_effect");
        let body = layout::set_beacon(&packet.data, |id| effects.get(id))?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// A client packet while the backend configures: settings and plugin
    /// messages have configuration forms, everything else belongs to a world
    /// the backend has left (or not entered yet) and is dropped.
    fn while_configuring(&mut self, cx: &mut Cx<'_>, packet: Packet) {
        let ids = ids();
        let id = if packet.id == ids.client_information {
            ids.configuration_client_information
        } else if packet.id == ids.custom_payload {
            ids.configuration_custom_payload
        } else {
            return;
        };
        cx.to_backend(State::Configuration, Packet::new(id, packet.data));
    }

    #[cfg(test)]
    pub(super) fn new_for_test() -> Self {
        Self::new()
    }
}

impl Step for Down {
    fn clientbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::routes().dispatch(self, cx, Direction::Clientbound, state, packet)
    }

    fn serverbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if state == State::Play && self.phase == Phase::Configuring {
            self.while_configuring(cx, packet);
            return Ok(());
        }
        Self::routes().dispatch(self, cx, Direction::Serverbound, state, packet)
    }
}
