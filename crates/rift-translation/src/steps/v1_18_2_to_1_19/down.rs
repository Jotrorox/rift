//! A 1.18.2 client on a 1.19 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    NEWER, OLDER,
    blocks::{self, Digging},
    chat::{ChatTypes, Clock},
    entities::{self, Painting, Spawned},
    id, join,
    login::{Finished, Hello, Key},
    messages::{Placement, PlayerChat, Sent, SystemChat},
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared, entity, registries::Side},
    data,
    step::{Cx, RouteBuilder, Routes, Step},
    steps::Make,
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

const LOOM: &str = "loom";

pub(super) struct Down {
    shared: Shared,
    /// The backend's chat types, from its `login`.
    chat_types: ChatTypes,
    digging: Digging,
    /// The open loom's container ID.
    loom: Option<i32>,
    /// A painting waiting for the entity data that names its variant.
    painting: Option<Painting>,
    /// Packets for the client once the current backend packet has gone.
    after: Vec<Packet>,
    clock: Clock,
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
            chat_types: ChatTypes::default(),
            digging: Digging::default(),
            loom: None,
            painting: None,
            after: Vec::new(),
            clock: Clock::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .serverbound(Login, "hello", Self::hello)
                .serverbound(Login, "key", Self::key)
                .clientbound(Login, "login_finished", Self::login_finished)
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "add_entity", Self::add_entity)
                .clientbound(Play, "player_chat", Self::player_chat)
                .clientbound(Play, "system_chat", Self::system_chat)
                .clientbound(Play, "custom_sound", Self::custom_sound)
                // 1.18.2 has no chat previews and no server status in play.
                .drop_clientbound(Play, "chat_preview")
                .drop_clientbound(Play, "set_display_chat_preview")
                .drop_clientbound(Play, "server_data")
                // Digging is acknowledged from the block updates instead.
                .drop_clientbound(Play, "block_changed_ack")
                .tap(Clientbound, Play, "block_update", Self::block_updated)
                .tap(
                    Clientbound,
                    Play,
                    "section_blocks_update",
                    Self::section_updated,
                )
                .tap(Clientbound, Play, "open_screen", Self::screen_opened)
                .tap(Clientbound, Play, "container_close", Self::screen_closed)
                .tap(Serverbound, Play, "container_close", Self::screen_closed)
                .clientbound(Play, "container_set_data", Self::container_set_data)
                .serverbound(Play, "container_button_click", Self::container_button_click)
                .serverbound(Play, "chat", Self::chat)
                .serverbound(Play, "player_action", Self::player_action)
                .serverbound(Play, "use_item_on", Self::use_item_on)
                .serverbound(Play, "use_item", Self::use_item)
                .serverbound(Play, "set_beacon", Self::set_beacon)
                .build()
        })
    }

    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let hello = Hello::read(OLDER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, hello.write(NEWER)));
        Ok(())
    }

    fn key(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let key = Key::read(OLDER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, key.write(NEWER)?));
        Ok(())
    }

    /// 1.18.2 learns the profile's properties from the player list.
    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let finished = Finished::read(NEWER, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, finished.write(OLDER)));
        Ok(())
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_login(&mut self.shared, Direction::Clientbound, &packet.data)?;
        self.chat_types = ChatTypes::from_shared(&self.shared, Side::Backend);
        self.digging = Digging::default();
        self.loom = None;
        self.painting = None;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_respawn(&mut self.shared, Direction::Clientbound, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// Mobs spawn with `add_mob`, paintings with `add_painting` once their
    /// variant is known; the rest stays `add_entity`.
    fn add_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        entity::track_spawn_packet(&mut self.shared, &packet.data)?;
        let entities = self.shared.mapping(Direction::Clientbound, "entity_type");
        let blocks = self.shared.mapping(Direction::Clientbound, "block_state");
        match entities::add_entity_down(&packet.data, entities, blocks)? {
            Spawned::Packet(name, data) => {
                let id = id(OLDER, state, Direction::Clientbound, name);
                cx.to_client(state, Packet::new(id, data));
            }
            Spawned::Painting(painting) => self.painting = Some(painting),
            Spawned::Dropped => {}
        }
        Ok(())
    }

    /// Spawn the waiting painting before `packet`, with the variant `packet`
    /// names if it is the painting's entity data, else the default one.
    fn spawn_painting(&mut self, cx: &mut Cx<'_>, state: State, packet: &Packet) {
        let Some(painting) = self.painting.take() else {
            return;
        };
        let backend = data::version(NEWER);
        let variant = (backend.packet_name(state, Direction::Clientbound, packet.id)
            == Some("set_entity_data"))
        .then(|| entities::painting_variant(&packet.data, painting.entity))
        .flatten()
        .unwrap_or(0);
        let variants = self
            .shared
            .mapping(Direction::Clientbound, "painting_variant");
        let id = id(OLDER, state, Direction::Clientbound, "add_painting");
        cx.to_client(
            state,
            Packet::new(id, painting.write(variants.get_or(variant, 0))),
        );
    }

    fn player_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = PlayerChat::read(NEWER, &packet.data)?;
        let Some(display) = self.chat_types.display(
            message.chat_type,
            &message.sender_name,
            message.target_name.as_ref(),
            &message.content,
        ) else {
            return Ok(());
        };
        let overlay = self.chat_types.is_overlay(message.chat_type);
        let chat = SystemChat {
            json: display.to_string(),
            placement: Placement::Position(if overlay { 2 } else { 0 }),
            sender: message.sender,
        };
        let id = id(OLDER, state, Direction::Clientbound, "chat");
        cx.to_client(state, Packet::new(id, chat.write(OLDER)?));
        Ok(())
    }

    fn system_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut message = SystemChat::read(NEWER, &packet.data)?;
        if let Placement::ChatType(chat_type) = message.placement {
            let overlay = self.chat_types.is_overlay(chat_type);
            message.placement = Placement::Position(if overlay { 2 } else { 1 });
        }
        let id = id(OLDER, state, Direction::Clientbound, "chat");
        cx.to_client(state, Packet::new(id, message.write(OLDER)?));
        Ok(())
    }

    /// 1.18.2 picks sound variants itself: the seed goes.
    fn custom_sound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let mut w = Writer::with_capacity(packet.data.len());
        w.string(r.str()?)
            .varint(r.varint()?)
            .bytes(r.take(3 * 4 + 2 * 4)?);
        r.i64()?;
        r.finish()?;
        cx.to_client(state, Packet::new(packet.id, w.buf));
        Ok(())
    }

    fn block_updated(&mut self, state: State, packet: &Packet) -> io::Result<()> {
        if !self.digging.is_waiting() {
            return Ok(());
        }
        let update = blocks::block_update(&packet.data)?;
        self.acknowledge_digging(state, &[update]);
        Ok(())
    }

    fn section_updated(&mut self, state: State, packet: &Packet) -> io::Result<()> {
        if !self.digging.is_waiting() {
            return Ok(());
        }
        let updates = blocks::section_blocks_update(&packet.data)?;
        self.acknowledge_digging(state, &updates);
        Ok(())
    }

    fn acknowledge_digging(&mut self, state: State, updates: &[(crate::wire::Position, i32)]) {
        let blocks = self.shared.mapping(Direction::Clientbound, "block_state");
        let mut acks = Vec::new();
        for (position, block) in updates {
            self.digging
                .block_updated(*position, blocks.get_or(*block, 0), &mut acks);
        }
        let id = id(OLDER, state, Direction::Clientbound, "block_break_ack");
        self.after
            .extend(acks.into_iter().map(|ack| Packet::new(id, ack)));
    }

    fn screen_opened(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let container = r.varint()?;
        let menu = r.varint()?;
        let loom = data::version(NEWER).expect_registry("menu").id(LOOM);
        self.loom = (Some(menu) == loom).then_some(container);
        Ok(())
    }

    fn screen_closed(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.loom = None;
        Ok(())
    }

    /// The loom's selected pattern: 1.19 sends an index into the patterns on
    /// offer, 1.18.2 the pattern's ID. Without a pattern item the offer is
    /// patterns 1 to 34 in order; with one, 1.18.2 hides the buttons anyway.
    fn container_set_data(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let (container, property, value) = (r.u8()?, r.i16()?, r.i16()?);
        r.finish()?;
        let mut data = packet.data;
        if self.loom == Some(i32::from(container)) && property == 0 {
            let pattern = if value < 0 {
                0
            } else {
                value.saturating_add(1)
            };
            data[3..5].copy_from_slice(&pattern.to_be_bytes());
        }
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn container_button_click(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let (container, button) = (r.i8()?, r.i8()?);
        r.finish()?;
        let mut data = packet.data;
        if self.loom == Some(i32::from(container)) && button > 0 {
            data[1] = (button - 1) as u8;
        }
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// A message starting with a slash is a command from 1.19 on.
    fn chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut message = Sent::read_chat(OLDER, &packet.data)?;
        let timestamp = self.clock.now();
        if let Some(command) = message.text.strip_prefix('/') {
            message.text = command.to_owned();
            let id = id(NEWER, state, Direction::Serverbound, "chat_command");
            cx.to_backend(
                state,
                Packet::new(id, message.write_command(NEWER, timestamp, 0)),
            );
        } else {
            cx.to_backend(
                state,
                Packet::new(packet.id, message.write_chat(NEWER, timestamp, 0)),
            );
        }
        Ok(())
    }

    fn player_action(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (data, ack) = self.digging.player_action(&packet.data)?;
        if let Some(ack) = ack {
            let id = id(OLDER, state, Direction::Clientbound, "block_break_ack");
            cx.to_client(state, Packet::new(id, ack));
        }
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// 1.19 backends only echo sequence numbers back; 0 asks for none.
    fn use_item_on(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        r.varint()?; // Hand.
        r.i64()?; // Position.
        r.varint()?; // Face.
        r.take(3 * 4)?; // Cursor position.
        r.bool()?; // Inside a block.
        r.finish()?;
        let mut data = packet.data;
        data.push(0);
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn use_item(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let hand = r.varint()?;
        r.finish()?;
        let mut w = Writer::new();
        w.varint(hand).varint(0);
        cx.to_backend(state, Packet::new(packet.id, w.buf));
        Ok(())
    }

    /// 1.18.2 sends -1 for no effect, 1.19 an absent one.
    fn set_beacon(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let mut w = Writer::new();
        for _ in 0..2 {
            let effect = r.varint()?;
            w.bool(effect > 0);
            if effect > 0 {
                w.varint(effect);
            }
        }
        r.finish()?;
        cx.to_backend(state, Packet::new(packet.id, w.buf));
        Ok(())
    }
}

impl Step for Down {
    fn clientbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        self.spawn_painting(cx, state, &packet);
        Self::routes().dispatch(self, cx, Direction::Clientbound, state, packet)?;
        for packet in self.after.drain(..) {
            cx.to_client(state, packet);
        }
        Ok(())
    }

    fn serverbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::routes().dispatch(self, cx, Direction::Serverbound, state, packet)
    }
}

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
