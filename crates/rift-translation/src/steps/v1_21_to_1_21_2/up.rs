//! A 1.21.2 client on a 1.21 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    NEWER, OLDER, RELATIVE_DELTA_X, RELATIVE_DELTA_Y, RELATIVE_DELTA_Z, RELATIVE_X, RELATIVE_Y,
    RELATIVE_Z, packet_id,
    recipes::{BookChange, UpBook},
    stacks, unpack_degrees, window_to_byte, window_to_varint,
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

/// How far 1.21 clients push their vehicle per input key.
const IMPULSE: f32 = 0.98;

/// Ping IDs this step sends to find the client's answer to a teleport;
/// the low byte counts.
const PING_BASE: i32 = 0x5269_6600;

pub struct Up {
    shared: Shared,
    player: Option<i32>,
    /// The entity the player rides, from `set_passengers`.
    vehicle: Option<i32>,
    /// The client's keys, as 1.21's vehicle input: sideways, forward, flags.
    input: (f32, f32, u8),
    on_ground: bool,
    /// Whether clientbound packets are inside a bundle.
    bundling: bool,
    pings: Vec<i32>,
    next_ping: i32,
    /// A teleport's ping was answered: the next position-and-rotation is the
    /// client's answer to it and must follow the acceptance.
    capture: bool,
    held: Option<Vec<u8>>,
    book: UpBook,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// IDs of packets this step creates.
struct Ids {
    entity_position_sync: i32,
    set_cursor_item: i32,
    set_player_inventory: i32,
    recipe_book_settings: i32,
    recipe_book_add: i32,
    recipe_book_remove: i32,
    ping: i32,
    bundle_delimiter: i32,
    player_input: i32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        let client = |name| packet_id(NEWER, State::Play, Direction::Clientbound, name);
        Ids {
            entity_position_sync: client("entity_position_sync"),
            set_cursor_item: client("set_cursor_item"),
            set_player_inventory: client("set_player_inventory"),
            recipe_book_settings: client("recipe_book_settings"),
            recipe_book_add: client("recipe_book_add"),
            recipe_book_remove: client("recipe_book_remove"),
            ping: client("ping"),
            bundle_delimiter: client("bundle_delimiter"),
            player_input: packet_id(OLDER, State::Play, Direction::Serverbound, "player_input"),
        }
    })
}

impl Up {
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            player: None,
            vehicle: None,
            input: (0.0, 0.0, 0),
            on_ground: false,
            bundling: false,
            pings: Vec::new(),
            next_ping: 0,
            capture: false,
            held: None,
            book: UpBook::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Login, "login_finished", Self::login_finished)
                .clientbound(Configuration, "update_enabled_features", Self::features)
                .serverbound(
                    Configuration,
                    "client_information",
                    Self::client_information,
                )
                .serverbound(Play, "client_information", Self::client_information)
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "set_time", Self::set_time)
                .clientbound(Play, "player_position", Self::player_position)
                .clientbound(Play, "teleport_entity", Self::teleport_entity)
                .tap(Clientbound, Play, "bundle_delimiter", Self::bundle)
                .tap(Clientbound, Play, "set_passengers", Self::passengers)
                .tap(Clientbound, Play, "remove_entities", Self::removed)
                .clientbound(Play, "update_recipes", Self::update_recipes)
                .clientbound(Play, "recipe", Self::recipe)
                .clientbound(Play, "place_ghost_recipe", Self::place_ghost_recipe)
                .clientbound(Play, "container_set_slot", Self::container_set_slot)
                .clientbound(Play, "container_close", Self::container_close)
                .clientbound(Play, "container_set_data", Self::container_set_data)
                .clientbound(Play, "mount_screen_open", Self::mount_screen_open)
                .serverbound(Play, "move_player_pos", Self::move_player_pos)
                .serverbound(Play, "move_player_pos_rot", Self::move_player_pos_rot)
                .serverbound(Play, "move_player_rot", Self::move_player_rot)
                .serverbound(
                    Play,
                    "move_player_status_only",
                    Self::move_player_status_only,
                )
                .serverbound(Play, "player_input", Self::player_input)
                .serverbound(Play, "accept_teleportation", Self::accept_teleportation)
                .serverbound(Play, "pong", Self::pong)
                .drop_serverbound(Play, "client_tick_end")
                .drop_serverbound(Play, "bundle_item_selected")
                .serverbound(Play, "container_close", Self::container_close_serverbound)
                .serverbound(Play, "place_recipe", Self::place_recipe)
                .serverbound(
                    Play,
                    "recipe_book_seen_recipe",
                    Self::recipe_book_seen_recipe,
                )
                .serverbound(Play, "use_item_on", Self::use_item_on)
                .build()
        })
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::login_finished(&packet.data, OLDER, NEWER)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn features(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::super::v1_20_5_to_1_21::rewrite_features(&packet.data, NEWER, &[])?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn client_information(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::client_information(&packet.data, NEWER, OLDER)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn reset(&mut self) {
        self.vehicle = None;
        self.capture = false;
        self.held = None;
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (player, body) = super::login(&packet.data, OLDER, NEWER)?;
        self.player = Some(player);
        self.reset();
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::respawn(&packet.data, OLDER, NEWER)?;
        self.reset();
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn set_time(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::set_time(&packet.data, OLDER, NEWER)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// 1.21 keeps the player's motion along relative axes and stops it
    /// along absolute ones; 1.21.2 says so with its delta flags.
    fn player_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let position = [reader.f64()?, reader.f64()?, reader.f64()?];
        let rotation = [reader.f32()?, reader.f32()?];
        let mut flags = i32::from(reader.u8()? & 0x1f);
        let teleport = reader.varint()?;
        reader.finish()?;
        for (relative, delta) in [
            (RELATIVE_X, RELATIVE_DELTA_X),
            (RELATIVE_Y, RELATIVE_DELTA_Y),
            (RELATIVE_Z, RELATIVE_DELTA_Z),
        ] {
            if flags & relative != 0 {
                flags |= delta;
            }
        }
        let mut writer = Writer::with_capacity(70);
        writer.varint(teleport);
        for value in position {
            writer.f64(value);
        }
        writer.f64(0.0).f64(0.0).f64(0.0);
        writer.f32(rotation[0]).f32(rotation[1]).i32(flags);
        let position = Packet::new(packet.id, writer.into_inner());
        // 1.21.2 and 1.21.3 clients answer a teleport with their position
        // before the acceptance, which a 1.21 server ignores while it waits.
        // A ping just before the teleport marks which position is the answer.
        if cx.client_protocol != NEWER {
            cx.to_client(state, position);
            return Ok(());
        }
        let ping = PING_BASE | (self.next_ping & 0xff);
        self.next_ping = self.next_ping.wrapping_add(1);
        if self.pings.len() >= 16 {
            self.pings.remove(0);
        }
        self.pings.push(ping);
        let bundle = !self.bundling;
        if bundle {
            cx.to_client(state, Packet::empty(ids().bundle_delimiter));
        }
        let mut body = Writer::with_capacity(4);
        body.i32(ping);
        cx.to_client(state, Packet::new(ids().ping, body.into_inner()));
        cx.to_client(state, position);
        if bundle {
            cx.to_client(state, Packet::empty(ids().bundle_delimiter));
        }
        Ok(())
    }

    fn bundle(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.bundling = !self.bundling;
        Ok(())
    }

    /// 1.21's periodic entity teleport is 1.21.2's position sync.
    fn teleport_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let position = [reader.f64()?, reader.f64()?, reader.f64()?];
        let yaw = unpack_degrees(reader.i8()?);
        let pitch = unpack_degrees(reader.i8()?);
        let on_ground = reader.bool()?;
        reader.finish()?;
        let mut writer = Writer::with_capacity(66);
        writer.varint(entity);
        for value in position {
            writer.f64(value);
        }
        writer
            .f64(0.0)
            .f64(0.0)
            .f64(0.0)
            .f32(yaw)
            .f32(pitch)
            .bool(on_ground);
        cx.to_client(
            state,
            Packet::new(ids().entity_position_sync, writer.into_inner()),
        );
        Ok(())
    }

    fn passengers(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let vehicle = reader.varint()?;
        let mut riding = false;
        for _ in 0..reader.count()? {
            riding |= Some(reader.varint()?) == self.player;
        }
        reader.finish()?;
        if riding {
            self.vehicle = Some(vehicle);
        } else if self.vehicle == Some(vehicle) {
            self.vehicle = None;
        }
        Ok(())
    }

    fn removed(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        for _ in 0..reader.count()? {
            if Some(reader.varint()?) == self.vehicle {
                self.vehicle = None;
            }
        }
        reader.finish()
    }

    fn update_recipes(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = self.book.update_recipes(&mut self.shared, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn recipe(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (settings, list) = self.book.recipe(&packet.data)?;
        cx.to_client(state, Packet::new(ids().recipe_book_settings, settings));
        match list {
            BookChange::Add(body) => cx.to_client(state, Packet::new(ids().recipe_book_add, body)),
            BookChange::Remove(body) => {
                cx.to_client(state, Packet::new(ids().recipe_book_remove, body))
            }
        }
        Ok(())
    }

    fn place_ghost_recipe(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let window = reader.i8()?;
        let name = reader.string(32767)?;
        reader.finish()?;
        let Some(display) = self.book.display_of(name) else {
            return Ok(());
        };
        let mut writer = Writer::new();
        writer.varint(i32::from(window)).bytes(display);
        cx.to_client(state, Packet::new(packet.id, writer.into_inner()));
        Ok(())
    }

    /// Windows -1 (the cursor) and -2 (the inventory) became packets of
    /// their own in 1.21.2.
    fn container_set_slot(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let window = rw.r.i8()?;
        let state_id = rw.r.varint()?;
        let slot = rw.r.i16()?;
        let id = match window {
            -1 => ids().set_cursor_item,
            -2 => {
                rw.w.varint(i32::from(slot));
                ids().set_player_inventory
            }
            _ => {
                rw.w.varint(i32::from(window)).varint(state_id).i16(slot);
                packet.id
            }
        };
        stacks::transcode_optional(&mut self.shared, Direction::Clientbound, &mut rw)?;
        cx.to_client(state, Packet::new(id, rw.finish()?));
        Ok(())
    }

    fn container_close(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        window_to_varint(&mut rw, false)?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn container_set_data(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        window_to_varint(&mut rw, false)?;
        rw.i16()?;
        rw.i16()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn mount_screen_open(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        window_to_varint(&mut rw, false)?;
        rw.varint()?;
        rw.i32()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// Copy a movement body up to its flags and turn them into 1.21's
    /// on-ground boolean.
    fn movement(&mut self, body: &[u8], prefix: usize) -> io::Result<(Vec<u8>, bool)> {
        let mut rw = Rewriter::new(body);
        rw.copy(prefix)?;
        let flags = rw.r.u8()?;
        let on_ground = flags & 1 != 0;
        rw.w.bool(on_ground);
        Ok((rw.finish()?, on_ground))
    }

    fn move_player_pos(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (body, on_ground) = self.movement(&packet.data, 24)?;
        self.on_ground = on_ground;
        self.capture = false;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn move_player_pos_rot(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let (body, on_ground) = self.movement(&packet.data, 32)?;
        self.on_ground = on_ground;
        if std::mem::take(&mut self.capture) {
            self.held = Some(body);
            return Ok(());
        }
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// While riding, 1.21 servers expect the vehicle input with every turn;
    /// 1.21.2 clients send keys only when they change.
    fn move_player_rot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (body, on_ground) = self.movement(&packet.data, 8)?;
        self.on_ground = on_ground;
        self.capture = false;
        cx.to_backend(state, Packet::new(packet.id, body));
        if self.vehicle.is_some() {
            self.send_input(cx, state);
        }
        Ok(())
    }

    /// 1.21.2 clients also report horizontal collisions here; a 1.21 server
    /// only needs to hear when the player lands or leaves the ground.
    fn move_player_status_only(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let (body, on_ground) = self.movement(&packet.data, 0)?;
        self.capture = false;
        if on_ground == self.on_ground {
            return Ok(());
        }
        self.on_ground = on_ground;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn send_input(&self, cx: &mut Cx<'_>, state: State) {
        let (sideways, forward, flags) = self.input;
        let mut body = Writer::with_capacity(9);
        body.f32(sideways).f32(forward).u8(flags);
        cx.to_backend(state, Packet::new(ids().player_input, body.into_inner()));
    }

    /// 1.21.2's key set as 1.21's vehicle input. 1.21 servers only use it
    /// while the player rides, so it is sent then.
    fn player_input(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let keys = reader.u8()?;
        reader.finish()?;
        let axis = |positive: u8, negative: u8| match (keys & positive != 0, keys & negative != 0) {
            (true, false) => IMPULSE,
            (false, true) => -IMPULSE,
            _ => 0.0,
        };
        let flags = u8::from(keys & 0x10 != 0) | (u8::from(keys & 0x20 != 0) << 1);
        self.input = (axis(0x04, 0x08), axis(0x01, 0x02), flags);
        if self.vehicle.is_some() {
            self.send_input(cx, state);
        }
        Ok(())
    }

    fn accept_teleportation(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        self.capture = false;
        cx.to_backend(state, packet);
        if let Some(held) = self.held.take() {
            let id = packet_id(
                OLDER,
                State::Play,
                Direction::Serverbound,
                "move_player_pos_rot",
            );
            cx.to_backend(state, Packet::new(id, held));
        }
        Ok(())
    }

    fn pong(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let id = reader.i32()?;
        reader.finish()?;
        if let Some(index) = self.pings.iter().position(|ping| *ping == id) {
            self.pings.remove(index);
            self.capture = true;
            self.held = None;
            return Ok(());
        }
        cx.to_backend(state, packet);
        Ok(())
    }

    fn container_close_serverbound(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        window_to_byte(&mut rw, true)?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn place_recipe(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let window = reader.varint()?;
        let display = reader.varint()?;
        let all = reader.bool()?;
        reader.finish()?;
        let window = i8::try_from(window).map_err(|_| invalid("window ID out of range"))?;
        let Some(name) = self.book.name_of(display) else {
            return Ok(());
        };
        let mut writer = Writer::new();
        writer.i8(window).string(name).bool(all);
        cx.to_backend(state, Packet::new(packet.id, writer.into_inner()));
        Ok(())
    }

    fn recipe_book_seen_recipe(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let display = reader.varint()?;
        reader.finish()?;
        let Some(name) = self.book.name_of(display) else {
            return Ok(());
        };
        let mut writer = Writer::new();
        writer.string(name);
        cx.to_backend(state, Packet::new(packet.id, writer.into_inner()));
        Ok(())
    }

    fn use_item_on(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::use_item_on(&packet.data, NEWER, OLDER)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }
}

routed_step!(Up);
