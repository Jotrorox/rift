//! A 1.19 client on a 1.18.2 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    NEWER, OLDER,
    blocks::{self, Sequences},
    chat::{GAME_INFO_1_19, SYSTEM_1_19},
    entities, id,
    login::{Finished, Hello, Key},
    messages::{MAX_MESSAGE, Placement, Sent, SystemChat},
    world::{self, DimensionTypes},
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data,
    step::{Cx, RouteBuilder, Routes, Step},
    steps::Make,
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

/// The loom's menu type, the same in both versions.
const LOOM: &str = "loom";
/// Banner patterns a loom offers without a pattern item: IDs 1 to 34 in
/// both versions, which 1.19 selects by their index in this tag.
const SELECTABLE_PATTERNS: i32 = 34;

pub(super) struct Up {
    shared: Shared,
    dimension_types: DimensionTypes,
    sequences: Sequences,
    /// The open loom's container ID.
    loom: Option<i32>,
    /// Packets for the client once the current backend packet has gone.
    after: Vec<Packet>,
    /// Sound seeds: 1.18.2 lets the client pick sound variants itself.
    seed: u64,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| elapsed.as_nanos() as u64);
        Self {
            shared: Shared::new(NEWER, OLDER),
            dimension_types: DimensionTypes::default(),
            sequences: Sequences::default(),
            loom: None,
            after: Vec::new(),
            seed: seed | 1,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .serverbound(Login, "hello", Self::hello)
                .serverbound(Login, "key", Self::key)
                .clientbound(Login, "login_finished", Self::login_finished)
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "add_entity", Self::add_entity)
                .clientbound(Play, "add_mob", Self::add_mob)
                .clientbound(Play, "add_painting", Self::add_painting)
                .clientbound(Play, "add_vibration_signal", Self::add_vibration_signal)
                .clientbound(Play, "block_break_ack", Self::block_break_ack)
                .tap(Clientbound, Play, "block_update", Self::block_updated)
                .tap(
                    Clientbound,
                    Play,
                    "section_blocks_update",
                    Self::section_updated,
                )
                .clientbound(Play, "chat", Self::chat_clientbound)
                .clientbound(Play, "custom_sound", Self::custom_sound)
                .tap(Clientbound, Play, "update_tags", Self::tags_updated)
                .tap(Clientbound, Play, "open_screen", Self::screen_opened)
                .tap(Clientbound, Play, "container_close", Self::screen_closed)
                .tap(Serverbound, Play, "container_close", Self::screen_closed)
                .clientbound(Play, "container_set_data", Self::container_set_data)
                .serverbound(Play, "container_button_click", Self::container_button_click)
                .serverbound(Play, "chat", Self::chat)
                .serverbound(Play, "chat_command", Self::chat_command)
                // Chat previews stay off, so a client asking is answered by
                // nothing it would show.
                .drop_serverbound(Play, "chat_preview")
                .serverbound(Play, "player_action", Self::player_action)
                .serverbound(Play, "use_item_on", Self::use_item_on)
                .serverbound(Play, "use_item", Self::use_item)
                .serverbound(Play, "set_beacon", Self::set_beacon)
                .build()
        })
    }

    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let hello = Hello::read(NEWER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, hello.write(OLDER)));
        Ok(())
    }

    fn key(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let key = Key::read(NEWER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, key.write(OLDER)?));
        Ok(())
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let finished = Finished::read(OLDER, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, finished.write(NEWER)));
        Ok(())
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = world::login_up(&packet.data, &mut self.dimension_types)?;
        self.sequences = Sequences::default();
        self.loom = None;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = world::respawn_up(&packet.data, &self.dimension_types)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    // The entity codec converts entity data and follows entity types through
    // its own taps; these handlers own the spawn packets because 1.19 merged
    // them.
    fn add_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let entities = self.shared.mapping(Direction::Clientbound, "entity_type");
        let blocks = self.shared.mapping(Direction::Clientbound, "block_state");
        if let Some(data) = entities::add_entity_up(&packet.data, entities, blocks)? {
            cx.to_client(state, Packet::new(packet.id, data));
        }
        Ok(())
    }

    fn add_mob(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let entities = self.shared.mapping(Direction::Clientbound, "entity_type");
        if let Some(data) = entities::add_mob_up(&packet.data, entities)? {
            let id = id(NEWER, state, Direction::Clientbound, "add_entity");
            cx.to_client(state, Packet::new(id, data));
        }
        Ok(())
    }

    fn add_painting(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let variants = self
            .shared
            .mapping(Direction::Clientbound, "painting_variant");
        let (spawn, entity_data) = entities::add_painting_up(&packet.data, variants)?;
        let add = id(NEWER, state, Direction::Clientbound, "add_entity");
        let set = id(NEWER, state, Direction::Clientbound, "set_entity_data");
        cx.to_client(state, Packet::new(add, spawn));
        cx.to_client(state, Packet::new(set, entity_data));
        Ok(())
    }

    fn add_vibration_signal(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let id = id(NEWER, state, Direction::Clientbound, "level_particles");
        cx.to_client(
            state,
            Packet::new(id, entities::vibration_up(&packet.data)?),
        );
        Ok(())
    }

    /// The backend's answer to digging: the block as it is now, then the
    /// acknowledgement that lets the client apply it.
    fn block_break_ack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (position, block, ack) = self.sequences.block_break_ack(&packet.data)?;
        let blocks = self.shared.mapping(Direction::Clientbound, "block_state");
        let mut update = Writer::new();
        update.position(position).varint(blocks.get_or(block, 0));
        let update_id = id(NEWER, state, Direction::Clientbound, "block_update");
        cx.to_client(state, Packet::new(update_id, update.buf));
        self.acknowledge_now(cx, state, ack);
        Ok(())
    }

    fn block_updated(&mut self, state: State, packet: &Packet) -> io::Result<()> {
        let (position, _) = blocks::block_update(&packet.data)?;
        if let Some(ack) = self.sequences.block_updated(position) {
            let id = id(NEWER, state, Direction::Clientbound, "block_changed_ack");
            self.after.push(Packet::new(id, ack));
        }
        Ok(())
    }

    fn section_updated(&mut self, state: State, packet: &Packet) -> io::Result<()> {
        for (position, _) in blocks::section_blocks_update(&packet.data)? {
            if let Some(ack) = self.sequences.block_updated(position) {
                let id = id(NEWER, state, Direction::Clientbound, "block_changed_ack");
                self.after.push(Packet::new(id, ack));
            }
        }
        Ok(())
    }

    /// Every 1.18.2 message is shown as system chat: player messages arrive
    /// already decorated.
    fn chat_clientbound(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut message = SystemChat::read(OLDER, &packet.data)?;
        if let Placement::Position(position) = message.placement {
            let chat_type = if position == 2 {
                GAME_INFO_1_19
            } else {
                SYSTEM_1_19
            };
            message.placement = Placement::ChatType(chat_type);
        }
        let id = id(NEWER, state, Direction::Clientbound, "system_chat");
        cx.to_client(state, Packet::new(id, message.write(NEWER)?));
        Ok(())
    }

    fn custom_sound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        // A 64-bit linear congruential generator is plenty for sound variety.
        self.seed = self
            .seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let mut r = Reader::new(&packet.data);
        let mut w = Writer::with_capacity(packet.data.len() + 8);
        w.string(r.str()?)
            .varint(r.varint()?)
            .bytes(r.take(3 * 4 + 2 * 4)?);
        r.finish()?;
        w.i64(self.seed as i64);
        cx.to_client(state, Packet::new(packet.id, w.buf));
        Ok(())
    }

    /// 1.18.2 has no banner pattern registry, so its tags never name the
    /// patterns a 1.19 loom offers. Add them in a tags packet of their own,
    /// which leaves the other registries' tags alone.
    fn tags_updated(&mut self, state: State, _: &Packet) -> io::Result<()> {
        let id = id(NEWER, state, Direction::Clientbound, "update_tags");
        self.after.push(Packet::new(id, banner_pattern_tags()));
        Ok(())
    }

    fn screen_opened(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let container = r.varint()?;
        let menu = r.varint()?;
        let loom = data::version(OLDER).expect_registry("menu").id(LOOM);
        self.loom = (Some(menu) == loom).then_some(container);
        Ok(())
    }

    fn screen_closed(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.loom = None;
        Ok(())
    }

    /// The loom's selected pattern: 1.18.2 sends its ID, 1.19 an index into
    /// the patterns on offer (-1 for none, 0 for a pattern item's only one).
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
            let index = match i32::from(value) {
                0 => -1,
                pattern @ 1..=SELECTABLE_PATTERNS => pattern - 1,
                _ => 0,
            };
            data[3..5].copy_from_slice(&(index as i16).to_be_bytes());
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
        if self.loom == Some(i32::from(container))
            && (0..SELECTABLE_PATTERNS as i8).contains(&button)
        {
            data[1] = (button + 1) as u8;
        }
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = Sent::read_chat(NEWER, &packet.data)?;
        cx.to_backend(
            state,
            Packet::new(packet.id, message.write_chat(OLDER, 0, 0)),
        );
        Ok(())
    }

    /// Commands were chat messages starting with a slash; 1.18.2 refuses
    /// messages over 256 characters, which the slash may tip over.
    fn chat_command(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut command = Sent::read_command(NEWER, &packet.data)?;
        command.text.insert(0, '/');
        while command.text.encode_utf16().count() > MAX_MESSAGE {
            command.text.pop();
        }
        let id = id(OLDER, state, Direction::Serverbound, "chat");
        cx.to_backend(state, Packet::new(id, command.write_chat(OLDER, 0, 0)));
        Ok(())
    }

    fn player_action(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (data, ack) = self.sequences.player_action(&packet.data)?;
        self.acknowledge_now(cx, state, ack);
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn use_item_on(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (data, ack) = self.sequences.use_item_on(&packet.data)?;
        self.acknowledge_now(cx, state, ack);
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn use_item(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (data, ack) = self.sequences.use_item(&packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, data));
        self.acknowledge_now(cx, state, ack);
        Ok(())
    }

    fn acknowledge_now(&mut self, cx: &mut Cx<'_>, state: State, ack: Option<Vec<u8>>) {
        if let Some(ack) = ack {
            let id = id(NEWER, state, Direction::Clientbound, "block_changed_ack");
            cx.to_client(state, Packet::new(id, ack));
        }
    }

    /// 1.19 sends optional effects, 1.18.2 -1 for none.
    fn set_beacon(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let mut w = Writer::new();
        for _ in 0..2 {
            let effect = if r.bool()? { r.varint()? } else { -1 };
            w.varint(effect);
        }
        r.finish()?;
        cx.to_backend(state, Packet::new(packet.id, w.buf));
        Ok(())
    }
}

impl Step for Up {
    fn clientbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
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

/// A 1.19 `update_tags` with only the vanilla banner pattern tags.
fn banner_pattern_tags() -> Vec<u8> {
    let patterns = data::version(NEWER).expect_registry("banner_pattern");
    let item_patterns = ["globe", "creeper", "skull", "flower", "mojang", "piglin"];
    let mut w = Writer::new();
    w.varint(1)
        .string("minecraft:banner_pattern")
        .varint(1 + item_patterns.len() as i32);
    w.string("minecraft:no_item_required")
        .varint(SELECTABLE_PATTERNS);
    for pattern in 1..=SELECTABLE_PATTERNS {
        w.varint(pattern);
    }
    for name in item_patterns {
        w.string(&format!("minecraft:pattern_item/{name}"))
            .varint(1)
            .varint(patterns.id(name).expect("vanilla banner pattern"));
    }
    w.buf
}

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
