//! A 1.13.2 client on a 1.14 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER, delegate, entity_data, layout, trades};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Position, Reader, Rewriter, Writer},
};
use std::{
    collections::{HashMap, HashSet},
    io,
    sync::OnceLock,
};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

/// Entities the step remembers positions of; past this a misbehaving
/// server's new entities are not tracked (their sounds are dropped).
const MAX_TRACKED: usize = 16_384;

/// Equipment slots of 1.13: hands, then feet to head.
const EQUIPMENT_SLOTS: i32 = 6;

pub struct Down {
    shared: Shared,
    /// The player's own entity.
    player: Option<i32>,
    /// The difficulty `login` and `respawn` carried in 1.13; 1.14 sends it
    /// in `change_difficulty` only, after them.
    difficulty: u8,
    /// Other players' entities.
    players: HashSet<i32>,
    /// Entity positions (the player's own included), to place entity sounds.
    positions: HashMap<i32, [f64; 3]>,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// IDs of packets the step emits under another name than it received.
struct Ids {
    custom_payload: i32,
    open_screen: i32,
    sound: i32,
    player_sleep: i32,
    set_equipped_item: i32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        let client = Down::routes().client();
        let play = |name| client.expect_packet(State::Play, Direction::Clientbound, name);
        Ids {
            custom_payload: play("custom_payload"),
            open_screen: play("open_screen"),
            sound: play("sound"),
            player_sleep: play("player_sleep"),
            set_equipped_item: play("set_equipped_item"),
        }
    })
}

impl Down {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            player: None,
            // Normal until the server's `change_difficulty` says otherwise.
            difficulty: 2,
            players: HashSet::new(),
            positions: HashMap::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::{Clientbound, Serverbound};
            use State::Play;
            layout::install(RouteBuilder::new(OLDER, NEWER).defaults())
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "change_difficulty", Self::change_difficulty)
                // 1.13 clients keep every chunk the server sends.
                .drop_clientbound(Play, "set_chunk_cache_center")
                .drop_clientbound(Play, "set_chunk_cache_radius")
                .clientbound(Play, "light_update", Self::light_update)
                .clientbound(Play, "merchant_offers", Self::merchant_offers)
                .clientbound(Play, "open_book", Self::open_book)
                .clientbound(Play, "mount_screen_open", Self::mount_screen_open)
                .clientbound(Play, "sound_entity", Self::sound_entity)
                .clientbound(Play, "entity_event", Self::entity_event)
                .clientbound(Play, "set_entity_data", Self::set_entity_data)
                .clientbound(Play, "add_player", Self::add_player)
                .tap(Clientbound, Play, "add_entity", Self::added_object)
                .tap(Clientbound, Play, "add_mob", Self::added_object)
                .tap(Clientbound, Play, "add_experience_orb", Self::added_orb)
                .tap(Clientbound, Play, "add_global_entity", Self::added_global)
                .tap(Clientbound, Play, "add_painting", Self::added_painting)
                .tap(Clientbound, Play, "teleport_entity", Self::teleported)
                .tap(Clientbound, Play, "move_entity_pos", Self::moved_entity)
                .tap(Clientbound, Play, "move_entity_pos_rot", Self::moved_entity)
                .tap(Clientbound, Play, "remove_entities", Self::removed)
                .tap(
                    Clientbound,
                    Play,
                    "player_position",
                    Self::teleported_player,
                )
                .tap(Serverbound, Play, "move_player_pos", Self::moved_player)
                .tap(Serverbound, Play, "move_player_pos_rot", Self::moved_player)
                .build()
        })
    }

    /// The shared codecs' routes alone, for packets the step looks at and
    /// then hands on.
    fn defaults() -> &'static Routes<Self> {
        static DEFAULTS: OnceLock<Routes<Down>> = OnceLock::new();
        DEFAULTS.get_or_init(|| RouteBuilder::new(OLDER, NEWER).defaults().build())
    }

    fn forget_world(&mut self) {
        self.players.clear();
        self.positions.clear();
    }

    fn track(&mut self, entity: i32, position: [f64; 3]) {
        if self.positions.len() < MAX_TRACKED || self.positions.contains_key(&entity) {
            self.positions.insert(entity, position);
        }
    }

    /// Join game: the difficulty comes back from the last `change_difficulty`
    /// and the view distance goes.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let entity = rw.i32()?;
        rw.u8()?; // Game mode.
        rw.i32()?; // Dimension.
        rw.w.u8(self.difficulty);
        rw.u8()?; // Maximum players.
        rw.string(layout::LEVEL_TYPE)?;
        rw.r.varint()?; // View distance.
        rw.bool()?; // Reduced debug info.
        let data = rw.finish()?;
        self.player = Some(entity);
        self.forget_world();
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Dimension.
        rw.w.u8(self.difficulty);
        rw.u8()?; // Game mode.
        rw.string(layout::LEVEL_TYPE)?;
        let data = rw.finish()?;
        self.forget_world();
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn change_difficulty(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        self.difficulty = r.u8()?;
        r.bool()?; // Locked: 1.13 cannot lock the difficulty.
        r.finish()?;
        cx.to_client(state, Packet::new(packet.id, vec![self.difficulty]));
        Ok(())
    }

    /// Light travels inside chunks for 1.13; the chunk codec merges it.
    fn light_update(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        // INTEGRATION: the chunk codec's default route for `light_update`.
        delegate(
            Self::defaults(),
            self,
            cx,
            Direction::Clientbound,
            state,
            "light_update",
            packet,
        )
    }

    fn plugin_message(channel: &str, payload: &[u8]) -> Packet {
        let mut w = Writer::with_capacity(channel.len() + payload.len() + 2);
        w.string(channel).bytes(payload);
        Packet::new(ids().custom_payload, w.buf)
    }

    fn merchant_offers(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let payload = trades::convert(&mut self.shared, Direction::Clientbound, &packet.data)?;
        cx.to_client(state, Self::plugin_message(trades::TRADER_LIST, &payload));
        Ok(())
    }

    fn open_book(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        r.varint()?; // Hand.
        r.finish()?;
        cx.to_client(
            state,
            Self::plugin_message("minecraft:book_open", &packet.data),
        );
        Ok(())
    }

    /// A horse's inventory opens as a window of the `EntityHorse` type, which
    /// carries a title (1.14 takes the horse's name instead).
    fn mount_screen_open(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let window = r.u8()?;
        let slots = r.varint()?;
        let entity = r.i32()?;
        r.finish()?;
        let mut w = Writer::with_capacity(48);
        w.u8(window)
            .string("EntityHorse")
            .string(r#"{"translate":"entity.minecraft.horse"}"#)
            .u8(slots.clamp(0, 255) as u8)
            .i32(entity);
        cx.to_client(state, Packet::new(ids().open_screen, w.buf));
        Ok(())
    }

    /// A sound following an entity plays where the entity is; one the step
    /// cannot place, or a sound 1.13 has no stand-in for, is dropped.
    fn sound_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let sound = r.varint()?;
        let source = r.varint()?;
        let entity = r.varint()?;
        let volume = r.f32()?;
        let pitch = r.f32()?;
        r.finish()?;
        let mapping = self.shared.mapping(Direction::Clientbound, "sound_event");
        let (Some(sound), Some(position)) = (mapping.get(sound), self.positions.get(&entity))
        else {
            return Ok(());
        };
        let mut w = Writer::with_capacity(24);
        w.varint(sound).varint(source);
        for coordinate in position {
            // Fixed point with three fraction bits.
            w.i32((coordinate * 8.0) as i32);
        }
        w.f32(volume).f32(pitch);
        cx.to_client(state, Packet::new(ids().sound, w.buf));
        Ok(())
    }

    /// 1.14 tells a player of their own death with an entity event too,
    /// which makes a 1.13 client empty its inventory display; the health
    /// update shows the death screen on its own. Other players who die keep
    /// showing their equipment in 1.13 unless it is cleared.
    fn entity_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.i32()?;
        let event = r.i8()?;
        r.finish()?;
        const DEATH: i8 = 3;
        if event == DEATH && self.player == Some(entity) {
            return Ok(());
        }
        cx.to_client(state, packet);
        if event == DEATH && self.players.contains(&entity) {
            for slot in 0..EQUIPMENT_SLOTS {
                let mut w = Writer::with_capacity(4);
                w.varint(entity).varint(slot).bool(false);
                cx.to_client(state, Packet::new(ids().set_equipped_item, w.buf));
            }
        }
        Ok(())
    }

    /// A player in bed: 1.13 shows it with `player_sleep`.
    fn sleep(&self, cx: &mut Cx<'_>, state: State, entity: i32, bed: Option<Option<Position>>) {
        if let Some(Some(bed)) = bed {
            let mut w = Writer::with_capacity(13);
            w.varint(entity).position_legacy(bed);
            cx.to_client(state, Packet::new(ids().player_sleep, w.buf));
        }
    }

    fn is_player(&self, entity: i32) -> bool {
        self.player == Some(entity) || self.players.contains(&entity)
    }

    fn set_entity_data(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let bed = if self.is_player(entity) {
            let bed = entity_data::sleeping(NEWER, &mut r)?;
            r.finish()?;
            bed
        } else {
            None
        };
        delegate(
            Self::defaults(),
            self,
            cx,
            Direction::Clientbound,
            state,
            "set_entity_data",
            packet,
        )?;
        self.sleep(cx, state, entity, bed);
        Ok(())
    }

    /// A player coming into view may already be asleep.
    fn add_player(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        r.uuid()?;
        let position = [r.f64()?, r.f64()?, r.f64()?];
        r.i8()?;
        r.i8()?;
        let bed = entity_data::sleeping(NEWER, &mut r)?;
        r.finish()?;
        self.players.insert(entity);
        self.track(entity, position);
        delegate(
            Self::defaults(),
            self,
            cx,
            Direction::Clientbound,
            state,
            "add_player",
            packet,
        )?;
        self.sleep(cx, state, entity, bed);
        Ok(())
    }

    /// `add_entity` and `add_mob`: entity, UUID, type, then the position.
    fn added_object(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        r.uuid()?;
        r.varint()?;
        let position = [r.f64()?, r.f64()?, r.f64()?];
        self.track(entity, position);
        Ok(())
    }

    fn added_orb(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let position = [r.f64()?, r.f64()?, r.f64()?];
        self.track(entity, position);
        Ok(())
    }

    fn added_global(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        r.i8()?;
        let position = [r.f64()?, r.f64()?, r.f64()?];
        self.track(entity, position);
        Ok(())
    }

    fn added_painting(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        r.uuid()?;
        r.varint()?;
        let block = layout::read_position(&mut r, NEWER)?;
        let center = |coordinate: i32| f64::from(coordinate) + 0.5;
        self.track(entity, [center(block.x), center(block.y), center(block.z)]);
        Ok(())
    }

    fn teleported(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let position = [r.f64()?, r.f64()?, r.f64()?];
        self.track(entity, position);
        Ok(())
    }

    /// Relative moves in 1/4096 of a block.
    fn moved_entity(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let delta = [r.i16()?, r.i16()?, r.i16()?];
        if let Some(position) = self.positions.get_mut(&entity) {
            for (coordinate, delta) in position.iter_mut().zip(delta) {
                *coordinate += f64::from(delta) / 4096.0;
            }
        }
        Ok(())
    }

    fn removed(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        for _ in 0..r.count()? {
            let entity = r.varint()?;
            self.players.remove(&entity);
            self.positions.remove(&entity);
        }
        Ok(())
    }

    fn teleported_player(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let Some(player) = self.player else {
            return Ok(());
        };
        let mut r = Reader::new(&packet.data);
        let target = [r.f64()?, r.f64()?, r.f64()?];
        r.f32()?;
        r.f32()?;
        let relative = r.u8()?;
        let current = self.positions.get(&player).copied();
        let position = match current {
            Some(current) => std::array::from_fn(|axis| {
                if relative & (1 << axis) != 0 {
                    current[axis] + target[axis]
                } else {
                    target[axis]
                }
            }),
            None if relative & 0b111 == 0 => target,
            None => return Ok(()),
        };
        self.track(player, position);
        Ok(())
    }

    fn moved_player(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let Some(player) = self.player else {
            return Ok(());
        };
        let mut r = Reader::new(&packet.data);
        let position = [r.f64()?, r.f64()?, r.f64()?];
        self.track(player, position);
        Ok(())
    }
}
routed_step!(Down);
