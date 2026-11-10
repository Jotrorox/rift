//! A 1.21 client on a 1.21.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    NEWER, OLDER, RELATIVE_DELTA, RELATIVE_POSITION, RELATIVE_X, RELATIVE_X_ROT, RELATIVE_Y,
    RELATIVE_Y_ROT, RELATIVE_Z, pack_degrees, packet_id, recipes::DownBook, recipes::DownOut,
    registry_id, stacks, window_to_byte, window_to_varint,
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

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

/// How far ahead of the player `player_look_at` aims when it stands in for
/// `player_rotation`: far enough that not knowing the eye height exactly
/// changes the angle by a negligible amount.
const LOOK_DISTANCE: f64 = 1.0e5;

/// A player's eye height while standing.
pub(super) const EYE_HEIGHT: f64 = 1.62;

pub struct Down {
    shared: Shared,
    player: Option<i32>,
    /// The player's position and rotation as the client last reported them.
    position: [f64; 3],
    rotation: [f32; 2],
    /// The entity the player rides and where the client put it last.
    vehicle: Option<i32>,
    vehicle_position: [f64; 3],
    vehicle_rotation: [f32; 2],
    sprinting: bool,
    /// The window the backend opened over the inventory, if any.
    window: Option<i32>,
    state_id: i32,
    /// The next slot of a creative "destroy all" sweep (see
    /// `set_creative_mode_slot`).
    cleared: i32,
    book: DownBook,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// IDs of packets this step creates.
struct Ids {
    teleport_entity: i32,
    move_vehicle: i32,
    player_look_at: i32,
    set_entity_motion: i32,
    explode: i32,
    container_set_slot: i32,
    set_equipment: i32,
    update_recipes: i32,
    recipe: i32,
    client_tick_end: i32,
    explosion_particle: i32,
    empty_sound: i32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        let client = |name| packet_id(OLDER, State::Play, Direction::Clientbound, name);
        Ids {
            teleport_entity: client("teleport_entity"),
            move_vehicle: client("move_vehicle"),
            player_look_at: client("player_look_at"),
            set_entity_motion: client("set_entity_motion"),
            explode: client("explode"),
            container_set_slot: client("container_set_slot"),
            set_equipment: client("set_equipment"),
            update_recipes: client("update_recipes"),
            recipe: client("recipe"),
            client_tick_end: packet_id(
                NEWER,
                State::Play,
                Direction::Serverbound,
                "client_tick_end",
            ),
            explosion_particle: registry_id(OLDER, "particle_type", "explosion"),
            empty_sound: registry_id(OLDER, "sound_event", "intentionally_empty"),
        }
    })
}

/// A velocity as `set_entity_motion` packs it.
fn pack_velocity(velocity: f64) -> i16 {
    (velocity.clamp(-3.9, 3.9) * 8000.0) as i16
}

impl Down {
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            player: None,
            position: [0.0; 3],
            rotation: [0.0; 2],
            vehicle: None,
            vehicle_position: [0.0; 3],
            vehicle_rotation: [0.0; 2],
            sprinting: false,
            window: None,
            state_id: 0,
            cleared: 0,
            book: DownBook::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Login, "login_finished", Self::login_finished)
                .clientbound(Configuration, "update_enabled_features", Self::features)
                .tap(Clientbound, Configuration, "update_tags", Self::update_tags)
                .tap(Clientbound, Play, "update_tags", Self::update_tags)
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
                .clientbound(Play, "player_rotation", Self::player_rotation)
                .clientbound(Play, "teleport_entity", Self::teleport_entity)
                .clientbound(Play, "entity_position_sync", Self::entity_position_sync)
                .clientbound(Play, "move_minecart_along_track", Self::move_minecart)
                .tap(Clientbound, Play, "move_vehicle", Self::vehicle_moved)
                .tap(Clientbound, Play, "set_passengers", Self::passengers)
                .tap(Clientbound, Play, "remove_entities", Self::removed)
                .clientbound(Play, "update_recipes", Self::update_recipes)
                .clientbound(Play, "recipe_book_settings", Self::recipe_book_settings)
                .clientbound(Play, "recipe_book_add", Self::recipe_book_add)
                .clientbound(Play, "recipe_book_remove", Self::recipe_book_remove)
                .clientbound(Play, "place_ghost_recipe", Self::place_ghost_recipe)
                .clientbound(Play, "set_cursor_item", Self::set_cursor_item)
                .clientbound(Play, "set_player_inventory", Self::set_player_inventory)
                .clientbound(Play, "container_set_slot", Self::container_set_slot)
                .tap(
                    Clientbound,
                    Play,
                    "container_set_content",
                    Self::container_content,
                )
                .tap(Clientbound, Play, "open_screen", Self::open_screen)
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
                .tap(Serverbound, Play, "move_vehicle", Self::vehicle_moved)
                .serverbound(Play, "player_input", Self::player_input)
                .tap(Serverbound, Play, "player_command", Self::player_command)
                .serverbound(Play, "container_close", Self::container_close_serverbound)
                .serverbound(Play, "place_recipe", Self::place_recipe)
                .serverbound(
                    Play,
                    "recipe_book_seen_recipe",
                    Self::recipe_book_seen_recipe,
                )
                .serverbound(Play, "use_item_on", Self::use_item_on)
                .serverbound(Play, "set_creative_mode_slot", Self::set_creative_mode_slot)
                .build()
        })
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::login_finished(&packet.data, NEWER, OLDER)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// Bundles are default content on 1.21.2 and a feature on 1.21.
    fn features(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body =
            super::super::v1_20_5_to_1_21::rewrite_features(&packet.data, OLDER, &["bundle"])?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn update_tags(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.book.update_tags(&packet.data)
    }

    fn client_information(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::client_information(&packet.data, OLDER, NEWER)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let (player, body) = super::login(&packet.data, NEWER, OLDER)?;
        self.player = Some(player);
        self.vehicle = None;
        self.window = None;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::respawn(&packet.data, NEWER, OLDER)?;
        self.vehicle = None;
        self.window = None;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn set_time(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::set_time(&packet.data, NEWER, OLDER)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// A 1.21.2 teleport sets or adds to the player's motion; a 1.21 one
    /// keeps the motion along relative axes and stops it along the others.
    /// What 1.21 cannot express follows as a motion packet (setting it) or a
    /// silent explosion out of the world (adding to it).
    fn player_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let teleport = reader.varint()?;
        let position = [reader.f64()?, reader.f64()?, reader.f64()?];
        let delta = [reader.f64()?, reader.f64()?, reader.f64()?];
        let rotation = [reader.f32()?, reader.f32()?];
        let flags = reader.i32()?;
        reader.finish()?;
        let mut writer = Writer::with_capacity(40);
        for value in position {
            writer.f64(value);
        }
        writer.f32(rotation[0]).f32(rotation[1]);
        writer.u8((flags & 0x1f) as u8).varint(teleport);
        cx.to_client(state, Packet::new(packet.id, writer.into_inner()));

        for (axis, bit) in [RELATIVE_X, RELATIVE_Y, RELATIVE_Z].into_iter().enumerate() {
            self.position[axis] = position[axis]
                + if flags & bit != 0 {
                    self.position[axis]
                } else {
                    0.0
                };
        }
        for (axis, bit) in [RELATIVE_Y_ROT, RELATIVE_X_ROT].into_iter().enumerate() {
            self.rotation[axis] = rotation[axis]
                + if flags & bit != 0 {
                    self.rotation[axis]
                } else {
                    0.0
                };
        }

        let relative_position = flags & RELATIVE_POSITION;
        let relative_delta = (flags & RELATIVE_DELTA) >> 5;
        let moving = delta.iter().any(|value| *value != 0.0);
        if relative_delta == relative_position && !moving {
            return Ok(());
        }
        let Some(player) = self.player else {
            return Ok(());
        };
        if relative_delta == RELATIVE_POSITION && relative_position == RELATIVE_POSITION {
            self.push(cx, state, delta);
        } else {
            let mut motion = Writer::with_capacity(11);
            motion.varint(player);
            for value in delta {
                motion.i16(pack_velocity(value));
            }
            cx.to_client(
                state,
                Packet::new(ids().set_entity_motion, motion.into_inner()),
            );
        }
        Ok(())
    }

    /// Add to the player's motion: the knockback of an explosion with no
    /// power, far above the world and silent.
    fn push(&self, cx: &mut Cx<'_>, state: State, delta: [f64; 3]) {
        let ids = ids();
        let mut body = Writer::with_capacity(48);
        body.f64(self.position[0])
            .f64(20_000.0)
            .f64(self.position[2]);
        body.f32(0.0).varint(0);
        for value in delta {
            body.f32(value as f32);
        }
        body.varint(0); // Keep blocks.
        body.varint(ids.explosion_particle)
            .varint(ids.explosion_particle);
        body.varint(ids.empty_sound + 1);
        cx.to_client(state, Packet::new(ids.explode, body.into_inner()));
    }

    /// 1.21 cannot turn the player on its own; looking at a point far away in
    /// the wanted direction does the same.
    fn player_rotation(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let yaw = reader.f32()?;
        let pitch = reader.f32()?;
        reader.finish()?;
        if !yaw.is_finite() || !pitch.is_finite() {
            return Err(invalid("non-finite rotation"));
        }
        self.rotation = [yaw, pitch];
        let (yaw, pitch) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
        let direction = [
            -yaw.sin() * pitch.cos(),
            -pitch.sin(),
            yaw.cos() * pitch.cos(),
        ];
        let eyes = [
            self.position[0],
            self.position[1] + EYE_HEIGHT,
            self.position[2],
        ];
        let mut body = Writer::with_capacity(26);
        body.varint(1); // From the eyes.
        for axis in 0..3 {
            body.f64(eyes[axis] + direction[axis] * LOOK_DISTANCE);
        }
        body.bool(false);
        cx.to_client(state, Packet::new(ids().player_look_at, body.into_inner()));
        Ok(())
    }

    fn write_teleport(
        cx: &mut Cx<'_>,
        state: State,
        entity: i32,
        position: [f64; 3],
        rotation: [f32; 2],
        on_ground: bool,
    ) {
        let mut body = Writer::with_capacity(32);
        body.varint(entity);
        for value in position {
            body.f64(value);
        }
        body.i8(pack_degrees(rotation[0]))
            .i8(pack_degrees(rotation[1]))
            .bool(on_ground);
        cx.to_client(state, Packet::new(ids().teleport_entity, body.into_inner()));
    }

    /// 1.21.2 teleports a player's own vehicle relative to where the client
    /// has it; 1.21 clients ignore teleports of the entity they steer and
    /// take `move_vehicle` instead.
    fn teleport_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let mut position = [reader.f64()?, reader.f64()?, reader.f64()?];
        reader.take(24)?; // Motion: 1.21 teleports keep it.
        let mut rotation = [reader.f32()?, reader.f32()?];
        let flags = reader.i32()?;
        let on_ground = reader.bool()?;
        reader.finish()?;
        let relative = flags & (RELATIVE_POSITION | RELATIVE_Y_ROT | RELATIVE_X_ROT);
        if relative == 0 {
            Self::write_teleport(cx, state, entity, position, rotation, on_ground);
            return Ok(());
        }
        if Some(entity) != self.vehicle {
            // Only a player's vehicle is teleported relatively; anything else
            // cannot be placed without knowing where it is.
            return Ok(());
        }
        for (axis, bit) in [RELATIVE_X, RELATIVE_Y, RELATIVE_Z].into_iter().enumerate() {
            if flags & bit != 0 {
                position[axis] += self.vehicle_position[axis];
            }
        }
        for (axis, bit) in [RELATIVE_Y_ROT, RELATIVE_X_ROT].into_iter().enumerate() {
            if flags & bit != 0 {
                rotation[axis] += self.vehicle_rotation[axis];
            }
        }
        self.vehicle_position = position;
        self.vehicle_rotation = rotation;
        let mut body = Writer::with_capacity(32);
        for value in position {
            body.f64(value);
        }
        body.f32(rotation[0]).f32(rotation[1]);
        cx.to_client(state, Packet::new(ids().move_vehicle, body.into_inner()));
        Ok(())
    }

    /// 1.21.2's periodic position sync is 1.21's entity teleport.
    fn entity_position_sync(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let position = [reader.f64()?, reader.f64()?, reader.f64()?];
        reader.take(24)?;
        let rotation = [reader.f32()?, reader.f32()?];
        let on_ground = reader.bool()?;
        reader.finish()?;
        Self::write_teleport(cx, state, entity, position, rotation, on_ground);
        Ok(())
    }

    /// Experimental minecarts send the steps along the track; a 1.21 client
    /// can only move to the last one and take its motion.
    fn move_minecart(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let mut last = None;
        for _ in 0..reader.count()? {
            let position = [reader.f64()?, reader.f64()?, reader.f64()?];
            let motion = [reader.f64()?, reader.f64()?, reader.f64()?];
            let rotation = [reader.i8()?, reader.i8()?];
            reader.f32()?; // Weight.
            last = Some((position, motion, rotation));
        }
        reader.finish()?;
        let Some((position, motion, rotation)) = last else {
            return Ok(());
        };
        let mut body = Writer::with_capacity(32);
        body.varint(entity);
        for value in position {
            body.f64(value);
        }
        body.i8(rotation[0]).i8(rotation[1]).bool(false);
        cx.to_client(state, Packet::new(ids().teleport_entity, body.into_inner()));
        let mut body = Writer::with_capacity(11);
        body.varint(entity);
        for value in motion {
            body.i16(pack_velocity(value));
        }
        cx.to_client(
            state,
            Packet::new(ids().set_entity_motion, body.into_inner()),
        );
        Ok(())
    }

    fn vehicle_moved(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        self.vehicle_position = [reader.f64()?, reader.f64()?, reader.f64()?];
        self.vehicle_rotation = [reader.f32()?, reader.f32()?];
        reader.finish()
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

    fn send_book(cx: &mut Cx<'_>, state: State, out: DownOut) {
        if let Some(recipes) = out.recipes {
            cx.to_client(state, Packet::new(ids().update_recipes, recipes));
        }
        if let Some(book) = out.book {
            cx.to_client(state, Packet::new(ids().recipe, book));
        }
    }

    fn update_recipes(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let out = self.book.update_recipes(&mut self.shared, &packet.data)?;
        Self::send_book(cx, state, out);
        Ok(())
    }

    fn recipe_book_settings(&mut self, _: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        self.book.settings(&packet.data)
    }

    fn recipe_book_add(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let out = self.book.add(&mut self.shared, &packet.data)?;
        Self::send_book(cx, state, out);
        Ok(())
    }

    fn recipe_book_remove(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = self.book.remove(&packet.data)?;
        cx.to_client(state, Packet::new(ids().recipe, body));
        Ok(())
    }

    fn place_ghost_recipe(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let window = reader.varint()?;
        let window = i8::try_from(window).map_err(|_| invalid("window ID out of range"))?;
        let Some(name) = self.book.ghost(reader.rest()) else {
            return Ok(());
        };
        let mut body = Writer::new();
        body.i8(window).string(&name);
        cx.to_client(state, Packet::new(packet.id, body.into_inner()));
        Ok(())
    }

    /// The cursor is window -1 of `container_set_slot` on 1.21; its state ID
    /// is ignored there, so the last known one does.
    fn set_cursor_item(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.i8(-1).varint(self.state_id).i16(-1);
        stacks::transcode_optional(&mut self.shared, Direction::Clientbound, &mut rw)?;
        cx.to_client(state, Packet::new(ids().container_set_slot, rw.finish()?));
        Ok(())
    }

    /// The player's inventory is window -2 on 1.21, by inventory slot.
    fn set_player_inventory(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let slot = rw.r.varint()?;
        let slot = i16::try_from(slot).map_err(|_| invalid("inventory slot out of range"))?;
        rw.w.i8(-2).varint(0).i16(slot);
        stacks::transcode_optional(&mut self.shared, Direction::Clientbound, &mut rw)?;
        cx.to_client(state, Packet::new(ids().container_set_slot, rw.finish()?));
        Ok(())
    }

    /// 1.21.2 servers update the inventory's armour slots even while another
    /// window is open; 1.21 clients then ignore window 0 but take the same
    /// change as the player's equipment.
    fn container_set_slot(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let window = window_to_byte(&mut rw, true)?;
        self.state_id = rw.varint()?;
        let slot = rw.i16()?;
        let start = rw.w.len();
        stacks::transcode_optional(&mut self.shared, Direction::Clientbound, &mut rw)?;
        let body = rw.finish()?;
        let stack = body[start..].to_vec();
        cx.to_client(state, Packet::new(packet.id, body));
        if let (0, 5..=8, Some(_), Some(player)) = (window, slot, self.window, self.player) {
            let mut equipment = Writer::new();
            equipment.varint(player).i8(10 - slot as i8).bytes(&stack);
            cx.to_client(
                state,
                Packet::new(ids().set_equipment, equipment.into_inner()),
            );
        }
        Ok(())
    }

    fn container_content(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        reader.varint()?;
        self.state_id = reader.varint()?;
        Ok(())
    }

    fn open_screen(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.window = Some(Reader::new(&packet.data).varint()?);
        Ok(())
    }

    fn container_close(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        window_to_byte(&mut rw, false)?;
        self.window = None;
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
        window_to_byte(&mut rw, false)?;
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
        let window = window_to_byte(&mut rw, false)?;
        rw.varint()?;
        rw.i32()?;
        self.window = Some(window);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// Copy a movement body up to its on-ground flag, which 1.21.2 widens to
    /// flags (horizontal collision unknown, so clear), then end the client's
    /// tick: 1.21 clients send one movement packet per tick, at its end.
    fn movement(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
        prefix: usize,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.copy(prefix)?;
        let on_ground = rw.r.bool()?;
        rw.w.u8(u8::from(on_ground));
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        cx.to_backend(state, Packet::empty(ids().client_tick_end));
        Ok(())
    }

    fn track(&mut self, body: &[u8], position: bool, rotation: bool) -> io::Result<()> {
        let mut reader = Reader::new(body);
        if position {
            self.position = [reader.f64()?, reader.f64()?, reader.f64()?];
        }
        if rotation {
            self.rotation = [reader.f32()?, reader.f32()?];
        }
        Ok(())
    }

    fn move_player_pos(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        self.track(&packet.data, true, false)?;
        self.movement(cx, state, packet, 24)
    }

    fn move_player_pos_rot(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        self.track(&packet.data, true, true)?;
        self.movement(cx, state, packet, 32)
    }

    fn move_player_rot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        self.track(&packet.data, false, true)?;
        self.movement(cx, state, packet, 8)
    }

    fn move_player_status_only(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        self.movement(cx, state, packet, 0)
    }

    fn player_command(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        reader.varint()?;
        match reader.varint()? {
            3 => self.sprinting = true,
            4 => self.sprinting = false,
            _ => {}
        }
        Ok(())
    }

    /// 1.21's vehicle input as 1.21.2's key set.
    fn player_input(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let sideways = reader.f32()?;
        let forward = reader.f32()?;
        let flags = reader.u8()?;
        reader.finish()?;
        let mut keys = 0u8;
        if forward > 0.0 {
            keys |= 0x01;
        } else if forward < 0.0 {
            keys |= 0x02;
        }
        if sideways > 0.0 {
            keys |= 0x04;
        } else if sideways < 0.0 {
            keys |= 0x08;
        }
        if flags & 1 != 0 {
            keys |= 0x10;
        }
        if flags & 2 != 0 {
            keys |= 0x20;
        }
        if self.sprinting {
            keys |= 0x40;
        }
        cx.to_backend(state, Packet::new(packet.id, vec![keys]));
        Ok(())
    }

    fn container_close_serverbound(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        window_to_varint(&mut rw, true)?;
        self.window = None;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn place_recipe(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let window = reader.i8()?;
        let name = reader.string(32767)?;
        let all = reader.bool()?;
        reader.finish()?;
        let Some(display) = DownBook::display_id(name) else {
            return Ok(());
        };
        let mut body = Writer::with_capacity(8);
        body.varint(i32::from(window)).varint(display).bool(all);
        cx.to_backend(state, Packet::new(packet.id, body.into_inner()));
        Ok(())
    }

    fn recipe_book_seen_recipe(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let name = reader.string(32767)?;
        reader.finish()?;
        let Some(display) = DownBook::display_id(name) else {
            return Ok(());
        };
        let mut body = Writer::with_capacity(5);
        body.varint(display);
        cx.to_backend(state, Packet::new(packet.id, body.into_inner()));
        Ok(())
    }

    fn use_item_on(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::use_item_on(&packet.data, OLDER, NEWER)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// A 1.21 client's creative "destroy all" clears slots 0 to 45 in order
    /// without clearing them locally, counting on the server's echo, which
    /// 1.21.2 servers no longer send. Once the sweep completes, echo it.
    fn set_creative_mode_slot(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let slot = rw.i16()?;
        let empty = matches!(stacks::peek(&rw.r)?.0, stacks::Peeked::Empty);
        stacks::transcode_optional(&mut self.shared, Direction::Serverbound, &mut rw)?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        if empty && i32::from(slot) == self.cleared {
            self.cleared += 1;
            if self.cleared > 45 {
                self.cleared = 0;
                for slot in 1..=45i16 {
                    let mut body = Writer::with_capacity(6);
                    body.i8(0).varint(self.state_id).i16(slot).varint(0);
                    cx.to_client(
                        state,
                        Packet::new(ids().container_set_slot, body.into_inner()),
                    );
                }
            }
        } else {
            self.cleared = i32::from(empty && slot == 0);
        }
        Ok(())
    }
}

routed_step!(Down);
