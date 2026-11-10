//! A 1.20.5 client on a 1.21 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    data, routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

pub struct Down {
    shared: Shared,
    /// The rotation the client last sent; 1.21 wants it with `use_item`.
    rotation: (f32, f32),
    /// The entity and motion of the last `set_entity_motion`: 1.21 bundles
    /// a projectile's `projectile_power` right after its motion.
    motion: Option<(i32, [f64; 3])>,
    /// The menu type of the window the backend last opened.
    menu: Option<i32>,
    /// The backend's synchronised enchantment registry, in network order.
    enchantments: Option<Vec<String>>,
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
            rotation: (0.0, 0.0),
            motion: None,
            menu: None,
            enchantments: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use crate::Direction::*;
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .drop_clientbound(Configuration, "custom_report_details")
                .drop_clientbound(Configuration, "server_links")
                .drop_clientbound(Play, "custom_report_details")
                .drop_clientbound(Play, "server_links")
                .clientbound(Configuration, "update_enabled_features", Self::features)
                .tap(
                    Clientbound,
                    Configuration,
                    "registry_data",
                    Self::registry_data,
                )
                .clientbound(Play, "projectile_power", Self::projectile_power)
                .tap(Clientbound, Play, "set_entity_motion", Self::entity_motion)
                .clientbound(Play, "game_event", Self::game_event)
                .clientbound(Play, "mount_screen_open", Self::mount_screen_open)
                .clientbound(Play, "container_set_data", Self::container_set_data)
                .tap(Clientbound, Play, "open_screen", Self::open_screen)
                .tap(Clientbound, Play, "container_close", Self::close_screen)
                .tap(Serverbound, Play, "container_close", Self::close_screen)
                .serverbound(Play, "use_item", Self::use_item)
                .tap(
                    Serverbound,
                    Play,
                    "move_player_pos_rot",
                    Self::moved_pos_rot,
                )
                .tap(Serverbound, Play, "move_player_rot", Self::moved_rot)
                .build()
        })
    }

    /// 1.21 has the 1.20.5 `update_1_21` content by default.
    fn features(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::rewrite_features(&packet.data, OLDER, &["update_1_21"])?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// Remember the order of the backend's enchantment registry; the
    /// registries codec converts the packet itself.
    fn registry_data(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        if crate::data::strip(reader.str()?) != "enchantment" {
            return Ok(());
        }
        let mut names = Vec::new();
        for _ in 0..reader.count()? {
            names.push(crate::data::strip(reader.str()?).to_owned());
            if reader.bool()? {
                reader.nbt_bytes(false)?;
            }
        }
        reader.finish()?;
        self.enchantments = Some(names);
        Ok(())
    }

    fn entity_motion(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let motion = [reader.i16()?, reader.i16()?, reader.i16()?].map(|v| f64::from(v) / 8000.0);
        reader.finish()?;
        self.motion = Some((entity, motion));
        Ok(())
    }

    /// 1.21 sends only the acceleration's length and directs it along the
    /// projectile's motion, which it sends just before. Without that motion
    /// the 1.20.5 vector cannot be rebuilt and the packet is dropped: the
    /// client then keeps the projectile's previous acceleration.
    fn projectile_power(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let power = reader.f64()?;
        reader.finish()?;
        let Some((_, motion)) = self.motion.filter(|(id, _)| *id == entity) else {
            return Ok(());
        };
        let length = motion.iter().map(|v| v * v).sum::<f64>().sqrt();
        if length < 1.0e-7 || !power.is_finite() {
            return Ok(());
        }
        let mut writer = Writer::with_capacity(29);
        writer.varint(entity);
        for v in motion {
            writer.f64(v / length * power);
        }
        cx.to_client(state, Packet::new(packet.id, writer.into_inner()));
        Ok(())
    }

    /// 1.21 clients always roll the credits; tell a 1.20.5 client to do the
    /// same rather than skip them and respawn at once.
    fn game_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let event = reader.u8()?;
        let value = reader.f32()?;
        reader.finish()?;
        if event != 4 || value >= 0.5 {
            cx.to_client(state, packet);
            return Ok(());
        }
        let mut writer = Writer::with_capacity(5);
        writer.u8(event).f32(1.0);
        cx.to_client(state, Packet::new(packet.id, writer.into_inner()));
        Ok(())
    }

    /// Chest columns back to the container size 1.20.5 expects.
    fn mount_screen_open(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let window = reader.u8()?;
        let columns = reader.varint()?;
        let entity = reader.i32()?;
        reader.finish()?;
        let size = columns.clamp(0, (i32::MAX - 1) / 3) * 3 + 1;
        let mut writer = Writer::with_capacity(10);
        writer.u8(window).varint(size).i32(entity);
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

    /// An ID of the backend's synchronised enchantment registry as a 1.20.5
    /// built-in ID; a custom enchantment has none.
    fn enchantment(&self, id: i32) -> Option<i32> {
        let index = usize::try_from(id).ok()?;
        // INTEGRATION: prefer `shared.registries` once the registries codec
        // records the order of every synchronised registry it passes on.
        let name = match &self.enchantments {
            Some(names) => names.get(index)?.as_str(),
            None => super::vanilla_synced_enchantments().get(index)?.as_str(),
        };
        data::version(OLDER).expect_registry("enchantment").id(name)
    }

    fn moved_pos_rot(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        if let (Some(rotation), _) = super::movement(&packet.data, true, true)? {
            self.rotation = rotation;
        }
        Ok(())
    }

    fn moved_rot(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        if let (Some(rotation), _) = super::movement(&packet.data, false, true)? {
            self.rotation = rotation;
        }
        Ok(())
    }

    /// 1.21 wants the rotation the item is used with: the last one sent.
    fn use_item(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let hand = reader.varint()?;
        let sequence = reader.varint()?;
        reader.finish()?;
        let mut writer = Writer::with_capacity(18);
        writer
            .varint(hand)
            .varint(sequence)
            .f32(self.rotation.0)
            .f32(self.rotation.1);
        cx.to_backend(state, Packet::new(packet.id, writer.into_inner()));
        Ok(())
    }
}

routed_step!(Down);
