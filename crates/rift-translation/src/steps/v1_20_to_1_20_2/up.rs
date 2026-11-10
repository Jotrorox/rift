//! A 1.20.2 client on a 1.20 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
//!
//! The backend is in play from Login Success on; the client first goes
//! through configuration. The step bridges that gap:
//!
//! 1. Login Success is passed on and the client acknowledges it; until then
//!    every backend packet is held back.
//! 2. In configuration, the backend's Join Game becomes `registry_data`, its
//!    feature flags, brand, keep-alives, pings and tags go out as their
//!    configuration forms, and every other play packet is held back. The
//!    configuration is finished as soon as the backend sends its tags or a
//!    packet that is not part of the join preamble vanilla sends before
//!    them (a vanilla 1.20.1 server's tags stand in when tags never came).
//! 3. When the client acknowledges the finish, the held packets go out in
//!    order as play packets, starting with the Join Game in its 1.20.2
//!    layout.
//!
//! A second Join Game in play (a proxy behind the backend switching servers)
//! sends the client back into configuration with `start_configuration`,
//! exactly like a 1.20.2 server resetting the world, and runs step 2 again
//! with the last tags and feature flags if the backend does not repeat them.
//! Client packets sent in configuration reach the backend as their play
//! forms, at once: the backend is already in play.
use super::{Held, NEWER, OLDER, convert, layout};
use crate::{
    Direction, Packet, State,
    codec::{
        self, HasShared, Shared,
        registries::{self, Side, form, login},
    },
    data,
    step::{Cx, RouteBuilder, Routes, Step},
    steps::Make,
    wire::{Reader, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Before Login Success.
    Login,
    /// Login Success sent; waiting for the client's acknowledgement.
    AwaitLoginAck,
    /// `start_configuration` sent; waiting for the client's acknowledgement.
    AwaitConfigurationAck,
    /// The client is configuring and the step builds its configuration.
    Configuring,
    /// `finish_configuration` sent; waiting for the client's acknowledgement.
    Finishing,
    Play,
}

/// Packet IDs the bridge looks for, on either side.
struct Ids {
    // 1.20.1 play, clientbound.
    login: i32,
    features: i32,
    custom_payload: i32,
    keep_alive: i32,
    ping: i32,
    disconnect: i32,
    tags: i32,
    chunk: i32,
    resource_pack: i32,
    /// What vanilla sends between Join Game and its tags.
    preamble: [i32; 4],
    // 1.20.2, clientbound.
    registry_data: i32,
    configuration_features: i32,
    configuration_tags: i32,
    finish_configuration: i32,
    start_configuration: i32,
    batch_start: i32,
    batch_finished: i32,
    pong_response: i32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        let (older, newer) = (data::version(OLDER), data::version(NEWER));
        let backend = |name| older.expect_packet(State::Play, Direction::Clientbound, name);
        let configuration =
            |name| newer.expect_packet(State::Configuration, Direction::Clientbound, name);
        let play = |name| newer.expect_packet(State::Play, Direction::Clientbound, name);
        Ids {
            login: backend("login"),
            features: backend("update_enabled_features"),
            custom_payload: backend("custom_payload"),
            keep_alive: backend("keep_alive"),
            ping: backend("ping"),
            disconnect: backend("disconnect"),
            tags: backend("update_tags"),
            chunk: backend("level_chunk_with_light"),
            resource_pack: backend("resource_pack_push"),
            preamble: [
                backend("change_difficulty"),
                backend("player_abilities"),
                backend("set_held_slot"),
                backend("update_recipes"),
            ],
            registry_data: configuration("registry_data"),
            configuration_features: configuration("update_enabled_features"),
            configuration_tags: configuration("update_tags"),
            finish_configuration: configuration("finish_configuration"),
            start_configuration: play("start_configuration"),
            batch_start: play("chunk_batch_start"),
            batch_finished: play("chunk_batch_finished"),
            pong_response: play("pong_response"),
        }
    })
}

/// What one configuration phase has sent the client so far.
#[derive(Default)]
struct Configuration {
    /// The backend's Join Game has been seen and its registries sent.
    joined: bool,
    tags: bool,
    features: bool,
    /// The last resource pack was sent again; the client's answers to it
    /// are not the backend's business.
    pack_resent: bool,
}

pub struct Up {
    shared: Shared,
    phase: Phase,
    /// Backend packets (1.20.1 play) waiting for the client to reach play.
    held: Held,
    configuration: Configuration,
    /// Whether the client has been in play before: a later configuration
    /// is a re-configuration.
    played: bool,
    /// Whether the client has had the Join Game of its current world.
    in_world: bool,
    /// Acknowledgements that arrived before the step asked for them; a
    /// replayed or hurried client may send them early.
    early_login_ack: bool,
    early_finish: bool,
    /// Client configuration packets sent before the backend left login.
    early: Vec<Packet>,
    /// Chunks in the chunk batch the client has been told about.
    batch: Option<i32>,
    /// The last tags (1.20.2 body), feature flags and resource pack (raw
    /// backend packet), for a re-configuration the backend does not repeat
    /// them in.
    last_tags: Option<Vec<u8>>,
    last_features: Option<Vec<u8>>,
    last_pack: Option<Packet>,
    /// The registries the client was sent in the last configuration.
    registries: Vec<registries::Registry>,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            phase: Phase::Login,
            held: Held::default(),
            configuration: Configuration::default(),
            played: false,
            in_world: false,
            early_login_ack: false,
            early_finish: false,
            early: Vec::new(),
            batch: None,
            last_tags: None,
            last_features: None,
            last_pack: None,
            registries: Vec::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Login, "login_finished", Self::login_finished)
                .serverbound(Login, "hello", Self::hello)
                .serverbound(Login, "login_acknowledged", Self::login_acknowledged)
                .serverbound(Configuration, "client_information", Self::send_as_play)
                .serverbound(Configuration, "custom_payload", Self::send_as_play)
                .serverbound(Configuration, "keep_alive", Self::send_as_play)
                .serverbound(Configuration, "pong", Self::send_as_play)
                .serverbound(Configuration, "resource_pack", Self::resource_pack)
                .serverbound(
                    Configuration,
                    "finish_configuration",
                    Self::finish_configuration,
                )
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "custom_payload", Self::custom_payload)
                .clientbound(Play, "add_player", Self::add_player)
                .clientbound(Play, "set_display_objective", Self::display_objective)
                .clientbound(Play, "tag_query", Self::tag_query)
                // Play has no feature flags from 1.20.2 on; the bridge sends
                // them in configuration.
                .drop_clientbound(Play, "update_enabled_features")
                .serverbound(
                    Play,
                    "configuration_acknowledged",
                    Self::configuration_acknowledged,
                )
                .drop_serverbound(Play, "chunk_batch_received")
                .serverbound(Play, "ping_request", Self::ping_request)
                .serverbound(Play, "set_beacon", Self::set_beacon)
                .build()
        })
    }

    /// A backend packet in play while the client is somewhere else.
    fn backend_packet(&mut self, cx: &mut Cx<'_>, packet: Packet) -> io::Result<()> {
        let ids = ids();
        match self.phase {
            Phase::Login => {
                Self::routes().dispatch(self, cx, Direction::Clientbound, State::Play, packet)
            }
            Phase::AwaitLoginAck | Phase::AwaitConfigurationAck => {
                if packet.id == ids.disconnect {
                    self.send_configured(cx, packet)
                } else {
                    self.held.push(packet)
                }
            }
            Phase::Configuring => self.configure(cx, packet),
            Phase::Finishing => {
                if packet.id == ids.disconnect {
                    // The client reads play from the finish on; the reason
                    // matters more than what was held.
                    self.play(cx, packet)
                } else if packet.id == ids.features {
                    self.last_features = Some(packet.data);
                    Ok(())
                } else {
                    self.held.push(packet)
                }
            }
            Phase::Play => self.play(cx, packet),
        }
    }

    /// A backend packet while the client configures.
    fn configure(&mut self, cx: &mut Cx<'_>, packet: Packet) -> io::Result<()> {
        let ids = ids();
        let joined = self.configuration.joined;
        let id = packet.id;
        if id == ids.login {
            if joined {
                // Another world before this one was entered: finish, and
                // re-configure for it from play.
                self.held.push(packet)?;
                return self.finish(cx);
            }
            let sent = login::read_login(OLDER, &mut Reader::new(&packet.data))?
                .codec
                .ok_or_else(|| invalid("Join Game without registries"))?;
            let converted = login::convert_codec(&mut self.shared, Direction::Clientbound, &sent)?;
            cx.to_client(
                State::Configuration,
                Packet::new(
                    ids.registry_data,
                    form::write_codec_packet(NEWER, &converted)?,
                ),
            );
            self.registries = converted;
            self.configuration.joined = true;
            self.held.push(packet)
        } else if id == ids.features {
            cx.to_client(
                State::Configuration,
                Packet::new(ids.configuration_features, packet.data.clone()),
            );
            self.configuration.features = true;
            self.last_features = Some(packet.data);
            Ok(())
        } else if id == ids.tags {
            self.configuration.tags = true;
            self.send_configured(cx, packet)?;
            if joined { self.finish(cx) } else { Ok(()) }
        } else if id == ids.custom_payload
            || id == ids.ping
            || id == ids.disconnect
            || (id == ids.keep_alive && !joined)
        {
            self.send_configured(cx, packet)
        } else if !joined || ids.preamble.contains(&id) {
            self.held.push(packet)
        } else {
            self.held.push(packet)?;
            self.finish(cx)
        }
    }

    /// Convert a backend play packet and send it to the client in
    /// configuration.
    fn send_configured(&mut self, cx: &mut Cx<'_>, packet: Packet) -> io::Result<()> {
        let client = data::version(NEWER);
        let routes = Self::routes();
        for emitted in convert(
            routes,
            self,
            cx,
            Direction::Clientbound,
            State::Play,
            packet,
        )? {
            if emitted.direction == Direction::Serverbound {
                cx.to_backend(emitted.state, emitted.packet);
                continue;
            }
            let name = client
                .packet_name(emitted.state, Direction::Clientbound, emitted.packet.id)
                .unwrap_or("unknown");
            let id = client
                .packet_id(State::Configuration, Direction::Clientbound, name)
                .ok_or_else(|| invalid("a configuration packet became a play-only packet"))?;
            if id == ids().configuration_tags {
                self.last_tags = Some(emitted.packet.data.clone());
            }
            cx.to_client(State::Configuration, Packet::new(id, emitted.packet.data));
        }
        Ok(())
    }

    /// Send what the configuration still lacks, then finish it.
    fn finish(&mut self, cx: &mut Cx<'_>) -> io::Result<()> {
        let ids = ids();
        if !self.configuration.features
            && let Some(features) = &self.last_features
        {
            cx.to_client(
                State::Configuration,
                Packet::new(ids.configuration_features, features.clone()),
            );
        }
        if !self.configuration.tags {
            match self.last_tags.clone() {
                Some(tags) => cx.to_client(
                    State::Configuration,
                    Packet::new(ids.configuration_tags, tags),
                ),
                // A client leaves configuration with the tags it was sent,
                // and its game logic needs vanilla's: those of a vanilla
                // 1.20.1 server stand in.
                None => {
                    let tags = codec::misc::tags::vanilla_update_tags(OLDER)?;
                    self.send_configured(cx, Packet::new(ids.tags, tags))?;
                }
            }
        }
        // A 1.20.2 client forgets the server resource pack when it
        // re-enters configuration; later clients keep it.
        if self.played
            && cx.client_protocol == NEWER
            && let Some(pack) = self.last_pack.clone()
        {
            self.configuration.pack_resent = true;
            self.send_configured(cx, pack)?;
        }
        cx.to_client(
            State::Configuration,
            Packet::empty(ids.finish_configuration),
        );
        self.phase = Phase::Finishing;
        if std::mem::take(&mut self.early_finish) {
            self.release(cx)?;
        }
        Ok(())
    }

    /// The client is in play: let the held packets go.
    fn release(&mut self, cx: &mut Cx<'_>) -> io::Result<()> {
        self.phase = Phase::Play;
        self.played = true;
        self.configuration = Configuration::default();
        for packet in self.held.take() {
            self.backend_packet(cx, packet)?;
        }
        Ok(())
    }

    /// Start a configuration phase: the backend side is in play throughout,
    /// so packets held so far are replayed into it.
    fn configure_held(&mut self, cx: &mut Cx<'_>) -> io::Result<()> {
        self.phase = Phase::Configuring;
        self.configuration = Configuration::default();
        for packet in self.held.take() {
            self.backend_packet(cx, packet)?;
        }
        Ok(())
    }

    /// A backend packet with the client in play.
    fn play(&mut self, cx: &mut Cx<'_>, packet: Packet) -> io::Result<()> {
        let ids = ids();
        let id = packet.id;
        if id == ids.login {
            if self.in_world {
                self.close_batch(cx);
                cx.to_client(State::Play, Packet::empty(ids.start_configuration));
                self.phase = Phase::AwaitConfigurationAck;
                self.in_world = false;
                return self.held.push(packet);
            }
            self.in_world = true;
        }
        if id == ids.chunk {
            if self.batch.is_none() {
                cx.to_client(State::Play, Packet::empty(ids.batch_start));
            }
            *self.batch.get_or_insert(0) += 1;
        } else {
            self.close_batch(cx);
        }
        if id == ids.features {
            self.last_features = Some(packet.data);
            return Ok(());
        }
        if id == ids.resource_pack {
            self.last_pack = Some(packet.clone());
        }
        if id != ids.tags {
            return Self::routes().dispatch(self, cx, Direction::Clientbound, State::Play, packet);
        }
        let routes = Self::routes();
        for emitted in convert(
            routes,
            self,
            cx,
            Direction::Clientbound,
            State::Play,
            packet,
        )? {
            if emitted.direction == Direction::Clientbound {
                self.last_tags = Some(emitted.packet.data.clone());
                cx.to_client(emitted.state, emitted.packet);
            } else {
                cx.to_backend(emitted.state, emitted.packet);
            }
        }
        Ok(())
    }

    /// End the chunk batch the client was told about, if any. A 1.20.1
    /// backend sends chunks without batches; the client times its chunk
    /// processing by them, and its answer stays here.
    fn close_batch(&mut self, cx: &mut Cx<'_>) {
        if let Some(count) = self.batch.take() {
            let mut body = Writer::new();
            body.varint(count);
            cx.to_client(State::Play, Packet::new(ids().batch_finished, body.buf));
        }
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_client(state, packet);
        self.phase = Phase::AwaitLoginAck;
        if std::mem::take(&mut self.early_login_ack) {
            self.configure_held(cx)?;
            for packet in std::mem::take(&mut self.early) {
                Self::routes().dispatch(
                    self,
                    cx,
                    Direction::Serverbound,
                    State::Configuration,
                    packet,
                )?;
            }
        }
        Ok(())
    }

    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_backend(
            state,
            Packet::new(packet.id, layout::hello(&packet.data, false)?),
        );
        Ok(())
    }

    /// The client entered configuration; the backend never hears of it.
    fn login_acknowledged(&mut self, cx: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        if !packet.data.is_empty() {
            return Err(invalid("login acknowledgement with a payload"));
        }
        match self.phase {
            Phase::AwaitLoginAck => self.configure_held(cx),
            Phase::Login => {
                self.early_login_ack = true;
                Ok(())
            }
            _ => Err(invalid("unexpected login acknowledgement")),
        }
    }

    fn configuration_acknowledged(
        &mut self,
        cx: &mut Cx<'_>,
        _: State,
        packet: Packet,
    ) -> io::Result<()> {
        if !packet.data.is_empty() || self.phase != Phase::AwaitConfigurationAck {
            return Err(invalid("unexpected configuration acknowledgement"));
        }
        self.configure_held(cx)
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
        match self.phase {
            Phase::Finishing => self.release(cx),
            Phase::Login | Phase::AwaitLoginAck | Phase::Configuring => {
                self.early_finish = true;
                Ok(())
            }
            _ => Err(invalid("unexpected configuration finish")),
        }
    }

    /// A client configuration packet whose play form has the same layout.
    fn send_as_play(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let name = data::version(NEWER)
            .packet_name(state, Direction::Serverbound, packet.id)
            .ok_or_else(|| invalid("unknown configuration packet"))?;
        let id = data::version(OLDER).expect_packet(State::Play, Direction::Serverbound, name);
        cx.to_backend(State::Play, Packet::new(id, packet.data));
        Ok(())
    }

    fn resource_pack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if self.configuration.pack_resent {
            return Ok(());
        }
        self.send_as_play(cx, state, packet)
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (join, _) = layout::Join::read_older(&packet.data)?;
        // The codec's tap recorded the backend's registries for the client
        // too; the client has the converted ones from configuration.
        for registry in &self.registries {
            registries::record(&mut self.shared, Side::Client, registry);
        }
        cx.to_client(state, Packet::new(packet.id, join.write_newer()));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_client(
            state,
            Packet::new(packet.id, layout::respawn(&packet.data, true)?),
        );
        Ok(())
    }

    fn custom_payload(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = layout::custom_payload_to_newer(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// Spawn Player became Add Entity.
    // INTEGRATION: `codec::entity` owns Add Entity; once it maps entity
    // types and data, this only has to produce the 1.20.2 layout.
    fn add_player(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let id = data::version(NEWER).expect_packet(state, Direction::Clientbound, "add_entity");
        cx.to_client(state, Packet::new(id, layout::add_player(&packet.data)?));
        Ok(())
    }

    fn display_objective(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        if let Some(body) = layout::display_objective(&packet.data, true)? {
            cx.to_client(state, Packet::new(packet.id, body));
        }
        Ok(())
    }

    fn tag_query(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_client(
            state,
            Packet::new(packet.id, layout::tag_query(&packet.data, true)?),
        );
        Ok(())
    }

    /// The 1.20.1 backend has no play ping; the answer comes from here.
    fn ping_request(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if packet.data.len() != 8 {
            return Err(invalid("invalid ping request"));
        }
        cx.to_client(state, Packet::new(ids().pong_response, packet.data));
        Ok(())
    }

    fn set_beacon(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let effects = self.shared.mapping(Direction::Serverbound, "mob_effect");
        let body = layout::set_beacon(&packet.data, |id| effects.get(id))?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn new_for_test() -> Self {
        Self::new()
    }
}

impl Step for Up {
    fn clientbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        match state {
            State::Play => self.backend_packet(cx, packet),
            _ => Self::routes().dispatch(self, cx, Direction::Clientbound, state, packet),
        }
    }

    fn serverbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if state == State::Configuration && self.phase == Phase::Login {
            if self.early.len() >= 64 {
                return Err(invalid("too many configuration packets before login"));
            }
            self.early.push(packet);
            return Ok(());
        }
        Self::routes().dispatch(self, cx, Direction::Serverbound, state, packet)
    }
}
