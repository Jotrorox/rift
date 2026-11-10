//! A 1.21 client on a 1.20.5 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data, routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

pub struct Up {
    shared: Shared,
    /// The rotation the backend last heard from the client.
    rotation: Option<(f32, f32)>,
    on_ground: bool,
    /// The menu type of the window the backend last opened.
    menu: Option<i32>,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// Backend-side IDs of packets this step creates.
struct Ids {
    client_command: i32,
    move_player_rot: i32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        let backend = data::version(OLDER);
        Ids {
            client_command: backend.expect_packet(
                State::Play,
                Direction::Serverbound,
                "client_command",
            ),
            move_player_rot: backend.expect_packet(
                State::Play,
                Direction::Serverbound,
                "move_player_rot",
            ),
        }
    })
}

impl Up {
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            rotation: None,
            on_ground: false,
            menu: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Configuration, "update_enabled_features", Self::features)
                .clientbound(Play, "projectile_power", Self::projectile_power)
                .clientbound(Play, "game_event", Self::game_event)
                .clientbound(Play, "mount_screen_open", Self::mount_screen_open)
                .clientbound(Play, "container_set_data", Self::container_set_data)
                .tap(Clientbound, Play, "open_screen", Self::open_screen)
                .tap(Clientbound, Play, "container_close", Self::close_screen)
                .tap(Serverbound, Play, "container_close", Self::close_screen)
                .serverbound(Play, "use_item", Self::use_item)
                .tap(Serverbound, Play, "move_player_pos", Self::moved_pos)
                .tap(
                    Serverbound,
                    Play,
                    "move_player_pos_rot",
                    Self::moved_pos_rot,
                )
                .tap(Serverbound, Play, "move_player_rot", Self::moved_rot)
                .tap(
                    Serverbound,
                    Play,
                    "move_player_status_only",
                    Self::moved_status,
                )
                .build()
        })
    }

    fn features(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::rewrite_features(&packet.data, NEWER, &[])?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// 1.20.5 sends the acceleration as a vector, 1.21 only its length: the
    /// client takes the direction from the projectile's motion.
    fn projectile_power(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let (x, y, z) = (reader.f64()?, reader.f64()?, reader.f64()?);
        reader.finish()?;
        let mut writer = Writer::with_capacity(13);
        writer.varint(entity).f64((x * x + y * y + z * z).sqrt());
        cx.to_client(state, Packet::new(packet.id, writer.into_inner()));
        Ok(())
    }

    /// A 1.20.5 server sends "win game" with value 0 to a player who has
    /// seen the credits, so the client skips them and asks to respawn at
    /// once. A 1.21 client would roll them regardless: answer for it.
    fn game_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let event = reader.u8()?;
        let value = reader.f32()?;
        reader.finish()?;
        if event == 4 && value < 0.5 {
            let mut body = Writer::with_capacity(1);
            body.varint(0); // Perform respawn.
            cx.to_backend(state, Packet::new(ids().client_command, body.into_inner()));
            return Ok(());
        }
        cx.to_client(state, packet);
        Ok(())
    }

    /// 1.20.5 sends the mount's container size (a saddle slot plus three
    /// slots per chest column), 1.21 the number of chest columns.
    fn mount_screen_open(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let window = reader.u8()?;
        let size = reader.varint()?;
        let entity = reader.i32()?;
        reader.finish()?;
        let mut writer = Writer::with_capacity(10);
        writer.u8(window).varint((size.max(1) - 1) / 3).i32(entity);
        cx.to_client(state, Packet::new(packet.id, writer.into_inner()));
        Ok(())
    }

    fn open_screen(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.menu = Some(super::screen_menu(&packet.data)?);
        Ok(())
    }

    fn close_screen(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.menu = None;
        Ok(())
    }

    fn container_set_data(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let property = Reader::new(packet.data.get(1..).unwrap_or_default()).i16()?;
        if self.menu != Some(super::enchantment_menu()) || !super::is_enchantment_hint(property) {
            cx.to_client(state, packet);
            return Ok(());
        }
        let body = super::map_enchantment_hint(&packet.data, |id| self.enchantment(id))?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// A 1.20.5 built-in enchantment ID as an ID of the synchronised
    /// registry the client was given.
    fn enchantment(&mut self, id: i32) -> Option<i32> {
        let name = data::version(OLDER)
            .expect_registry("enchantment")
            .name(id)?;
        // INTEGRATION: the registries codec synthesises 1.21's enchantment
        // registry for this client; once `shared.registries` exposes the
        // order it sent, look `name` up there. Until then, the vanilla order.
        super::vanilla_synced_enchantments()
            .iter()
            .position(|known| known == name)
            .map(|index| index as i32)
    }

    fn moved_pos(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.on_ground = super::movement(&packet.data, true, false)?.1;
        Ok(())
    }

    fn moved_pos_rot(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        (self.rotation, self.on_ground) = super::movement(&packet.data, true, true)?;
        Ok(())
    }

    fn moved_rot(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        (self.rotation, self.on_ground) = super::movement(&packet.data, false, true)?;
        Ok(())
    }

    fn moved_status(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.on_ground = super::movement(&packet.data, false, false)?.1;
        Ok(())
    }

    /// 1.21 sends the rotation an item is used with; 1.20.5 uses the last
    /// rotation it heard, so tell it first when that differs.
    fn use_item(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let hand = reader.varint()?;
        let sequence = reader.varint()?;
        let rotation = (reader.f32()?, reader.f32()?);
        reader.finish()?;
        if !rotation.0.is_finite() || !rotation.1.is_finite() {
            return Err(invalid("use_item with a non-finite rotation"));
        }
        if self.rotation != Some(rotation) {
            self.rotation = Some(rotation);
            let mut body = Writer::with_capacity(9);
            body.f32(rotation.0).f32(rotation.1).bool(self.on_ground);
            cx.to_backend(state, Packet::new(ids().move_player_rot, body.into_inner()));
        }
        let mut body = Writer::with_capacity(10);
        body.varint(hand).varint(sequence);
        cx.to_backend(state, Packet::new(packet.id, body.into_inner()));
        Ok(())
    }
}

routed_step!(Up);
