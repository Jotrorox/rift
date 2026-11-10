//! A 1.14 client on a 1.13.2 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER, chunk, delegate, entity_data, layout, trades};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data::strip,
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{collections::HashSet, io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

/// The chunk cache radius the 1.14 client is told. A 1.13 server never says
/// how far it sends chunks (at most 32 around the player); a radius well
/// beyond that keeps every chunk it sends even while the centre the step
/// derives from the player's movement lags behind the server's.
pub(super) const CHUNK_CACHE_RADIUS: i32 = 64;

/// Entities a misbehaving server could make the step remember as asleep.
const MAX_SLEEPING: usize = 1024;

pub struct Up {
    shared: Shared,
    /// The player's own entity.
    player: Option<i32>,
    /// Entities put to bed by `player_sleep`, to wake with entity data.
    sleeping: HashSet<i32>,
    /// The player's last known position.
    position: Option<[f64; 3]>,
    /// The chunk cache centre the client was last given.
    center: Option<(i32, i32)>,
    /// The window of the last trade list, for resynchronising after a
    /// trade selection.
    trade_window: Option<i32>,
    /// Action numbers of the clicks the step makes up, counting down so
    /// they stay clear of the client's own.
    action: i16,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// IDs of packets the step emits under another name than it received.
struct Ids {
    set_entity_data: i32,
    change_difficulty: i32,
    set_chunk_cache_center: i32,
    merchant_offers: i32,
    open_book: i32,
    mount_screen_open: i32,
    container_click: i32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        let (client, backend) = (Up::routes().client(), Up::routes().backend());
        let play = |name| client.expect_packet(State::Play, Direction::Clientbound, name);
        Ids {
            set_entity_data: play("set_entity_data"),
            change_difficulty: play("change_difficulty"),
            set_chunk_cache_center: play("set_chunk_cache_center"),
            merchant_offers: play("merchant_offers"),
            open_book: play("open_book"),
            mount_screen_open: play("mount_screen_open"),
            container_click: backend.expect_packet(
                State::Play,
                Direction::Serverbound,
                "container_click",
            ),
        }
    })
}

impl Up {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            player: None,
            sleeping: HashSet::new(),
            position: None,
            center: None,
            trade_window: None,
            action: -1,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::Play;
            layout::install(RouteBuilder::new(NEWER, OLDER).defaults())
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "change_difficulty", Self::change_difficulty)
                .clientbound(Play, "player_sleep", Self::player_sleep)
                .clientbound(Play, "animate", Self::animate)
                .clientbound(Play, "custom_payload", Self::custom_payload)
                .clientbound(Play, "open_screen", Self::open_screen)
                .clientbound(Play, "level_chunk", Self::level_chunk)
                .clientbound(Play, "player_position", Self::player_position)
                .clientbound(Play, "move_vehicle", Self::vehicle_moved)
                .tap(
                    Direction::Clientbound,
                    Play,
                    "remove_entities",
                    Self::removed,
                )
                .serverbound(Play, "move_player_pos", Self::moved)
                .serverbound(Play, "move_player_pos_rot", Self::moved)
                .serverbound(Play, "move_vehicle", Self::moved)
                .serverbound(Play, "select_trade", Self::select_trade)
                // Only a singleplayer owner can change or lock the
                // difficulty, and 1.13 has no jigsaw blocks to configure.
                .drop_serverbound(Play, "change_difficulty")
                .drop_serverbound(Play, "lock_difficulty")
                .drop_serverbound(Play, "set_jigsaw_block")
                .build()
        })
    }

    /// The shared codecs' routes alone, for packets the step looks at and
    /// then hands on.
    fn defaults() -> &'static Routes<Self> {
        static DEFAULTS: OnceLock<Routes<Up>> = OnceLock::new();
        DEFAULTS.get_or_init(|| RouteBuilder::new(NEWER, OLDER).defaults().build())
    }

    fn change_difficulty_packet(difficulty: u8) -> Packet {
        let mut w = Writer::with_capacity(2);
        w.u8(difficulty).bool(false);
        Packet::new(ids().change_difficulty, w.buf)
    }

    /// A new world: the client starts a fresh chunk cache.
    fn forget_world(&mut self) {
        self.sleeping.clear();
        self.position = None;
        self.center = None;
    }

    /// Join game: the difficulty moves to its own packet and the view
    /// distance is new.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let entity = rw.i32()?;
        rw.u8()?; // Game mode.
        rw.i32()?; // Dimension.
        let difficulty = rw.r.u8()?;
        rw.u8()?; // Maximum players.
        rw.string(layout::LEVEL_TYPE)?;
        rw.w.varint(CHUNK_CACHE_RADIUS);
        rw.bool()?; // Reduced debug info.
        let data = rw.finish()?;
        self.player = Some(entity);
        self.forget_world();
        cx.to_client(state, Packet::new(packet.id, data));
        cx.to_client(state, Self::change_difficulty_packet(difficulty));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Dimension.
        let difficulty = rw.r.u8()?;
        rw.u8()?; // Game mode.
        rw.string(layout::LEVEL_TYPE)?;
        let data = rw.finish()?;
        self.forget_world();
        cx.to_client(state, Packet::new(packet.id, data));
        cx.to_client(state, Self::change_difficulty_packet(difficulty));
        Ok(())
    }

    fn change_difficulty(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let difficulty = r.u8()?;
        r.finish()?;
        cx.to_client(state, Self::change_difficulty_packet(difficulty));
        Ok(())
    }

    /// A player went to bed: 1.14 shows it through entity data, with the
    /// sleeping pose for other players (the client works out its own).
    fn player_sleep(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let bed = r.position_legacy()?;
        r.finish()?;
        if self.sleeping.len() < MAX_SLEEPING {
            self.sleeping.insert(entity);
        }
        let pose = (self.player != Some(entity)).then_some(entity_data::SLEEPING);
        let data = entity_data::sleep_data(entity, Some(bed), pose);
        cx.to_client(state, Packet::new(ids().set_entity_data, data));
        Ok(())
    }

    /// Leaving a bed is still an animation, but the sleeping position and
    /// pose must be cleared as well.
    fn animate(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let animation = r.u8()?;
        r.finish()?;
        cx.to_client(state, Packet::new(packet.id, packet.data));
        if animation == 2 && self.sleeping.remove(&entity) {
            let pose = (self.player != Some(entity)).then_some(entity_data::STANDING);
            let data = entity_data::sleep_data(entity, None, pose);
            cx.to_client(state, Packet::new(ids().set_entity_data, data));
        }
        Ok(())
    }

    fn removed(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        for _ in 0..r.count()? {
            self.sleeping.remove(&r.varint()?);
        }
        Ok(())
    }

    /// The trade list and the open-book signal became packets.
    fn custom_payload(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let channel = r.string(32767)?;
        match strip(channel) {
            "trader_list" => {
                let payload = r.rest();
                let mut window = Reader::new(payload);
                self.trade_window = Some(window.i32()?);
                let data = trades::convert(&mut self.shared, Direction::Clientbound, payload)?;
                cx.to_client(state, Packet::new(ids().merchant_offers, data));
            }
            "book_open" => {
                let hand = r.varint()?;
                r.finish()?;
                let mut w = Writer::with_capacity(1);
                w.varint(hand);
                cx.to_client(state, Packet::new(ids().open_book, w.buf));
            }
            _ => cx.to_client(state, packet),
        }
        Ok(())
    }

    /// A horse's inventory has its own packet in 1.14; every other window
    /// is the shared codecs' business.
    fn open_screen(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let window = r.u8()?;
        if r.string(32)? != "EntityHorse" {
            return delegate(
                Self::defaults(),
                self,
                cx,
                Direction::Clientbound,
                state,
                "open_screen",
                packet,
            );
        }
        r.string(262_144)?; // Title: 1.14 names the window after the horse.
        let slots = r.u8()?;
        let entity = r.i32()?;
        r.finish()?;
        let mut w = Writer::with_capacity(8);
        w.u8(window).varint(i32::from(slots)).i32(entity);
        cx.to_client(state, Packet::new(ids().mount_screen_open, w.buf));
        Ok(())
    }

    fn send_center(&mut self, cx: &mut Cx<'_>, center: (i32, i32)) {
        self.center = Some(center);
        let mut w = Writer::with_capacity(10);
        w.varint(center.0).varint(center.1);
        cx.to_client(
            State::Play,
            Packet::new(ids().set_chunk_cache_center, w.buf),
        );
    }

    /// Move the chunk cache centre along with the player.
    fn follow(&mut self, cx: &mut Cx<'_>, position: [f64; 3]) {
        self.position = Some(position);
        let center = (chunk(position[0]), chunk(position[2]));
        if self.center != Some(center) {
            self.send_center(cx, center);
        }
    }

    /// A chunk the client's cache would ignore moves the centre to it; a
    /// 1.13 server sends the first chunks before the player's position.
    fn level_chunk(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let at = (r.i32()?, r.i32()?);
        let outside = self.center.is_none_or(|(x, z)| {
            (i64::from(at.0) - i64::from(x)).abs() > i64::from(CHUNK_CACHE_RADIUS)
                || (i64::from(at.1) - i64::from(z)).abs() > i64::from(CHUNK_CACHE_RADIUS)
        });
        if outside {
            self.send_center(cx, at);
        }
        delegate(
            Self::defaults(),
            self,
            cx,
            Direction::Clientbound,
            state,
            "level_chunk",
            packet,
        )
    }

    fn player_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let target = [r.f64()?, r.f64()?, r.f64()?];
        r.f32()?;
        r.f32()?;
        let relative = r.u8()?;
        r.varint()?; // Teleport ID.
        r.finish()?;
        // Bits 0 to 2 make x, y and z relative to the current position.
        let absolute = relative & 0b111 == 0;
        let position = match (absolute, self.position) {
            (true, _) => Some(target),
            (false, Some(current)) => Some(std::array::from_fn(|axis| {
                if relative & (1 << axis) != 0 {
                    current[axis] + target[axis]
                } else {
                    target[axis]
                }
            })),
            (false, None) => None,
        };
        if let Some(position) = position {
            self.follow(cx, position);
        }
        cx.to_client(state, packet);
        Ok(())
    }

    fn vehicle_moved(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let position = vehicle_position(&packet.data)?;
        self.follow(cx, position);
        cx.to_client(state, packet);
        Ok(())
    }

    /// The client's own movement: the layouts are unchanged, but the chunk
    /// cache centre has to follow.
    fn moved(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let routes = Self::routes();
        let name = routes
            .backend()
            .packet_name(state, Direction::Serverbound, packet.id)
            .unwrap_or_default();
        let position = if name == "move_vehicle" {
            vehicle_position(&packet.data)?
        } else {
            let mut r = Reader::new(&packet.data);
            let position = [r.f64()?, r.f64()?, r.f64()?];
            if name == "move_player_pos_rot" {
                r.f32()?;
                r.f32()?;
            }
            r.bool()?;
            r.finish()?;
            position
        };
        cx.to_backend(state, packet);
        self.follow(cx, position);
        Ok(())
    }

    /// Selecting a trade makes a 1.14 client move the trade's costs into the
    /// trade slots, which a 1.13 server does not do. A click the server
    /// cannot agree with (ending a drag that never started, claiming a stone
    /// was picked up) changes nothing but makes it send the whole window
    /// again, putting the items back where the server has them.
    fn select_trade(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        r.varint()?;
        r.finish()?;
        cx.to_backend(state, packet);
        let Some(window) = self.trade_window else {
            return Ok(());
        };
        let stone = crate::data::version(OLDER)
            .expect_registry("item")
            .id("stone")
            .ok_or_else(|| invalid("1.13.2 has no stone item"))?;
        let mut w = Writer::with_capacity(12);
        w.u8(window as u8)
            .i16(-999)
            .i8(2)
            .i16(self.action)
            .varint(5)
            .bool(true)
            .varint(stone)
            .i8(1)
            .u8(0);
        self.action = if self.action <= i16::MIN + 1 {
            -1
        } else {
            self.action - 1
        };
        cx.to_backend(state, Packet::new(ids().container_click, w.buf));
        Ok(())
    }
}
routed_step!(Up);

fn vehicle_position(data: &[u8]) -> io::Result<[f64; 3]> {
    let mut r = Reader::new(data);
    let position = [r.f64()?, r.f64()?, r.f64()?];
    r.f32()?;
    r.f32()?;
    r.finish()?;
    Ok(position)
}
