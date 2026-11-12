//! A 1.21.2 client on a 1.21.4 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data, routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    /// The hotbar slot the client has selected, for picking blocks.
    selected: i16,
    /// The state ID of the player's inventory menu the server last sent.
    inventory_state: i32,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// `ContainerInput.SWAP`: swap a slot with the hotbar slot `button` names.
const SWAP: i32 = 2;
/// The player's main inventory in both the inventory and the menu numbering;
/// the hotbar a 1.21.2 client selects itself.
const MAIN_INVENTORY: std::ops::RangeInclusive<i32> = 9..=35;

impl Down {
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            selected: 0,
            inventory_state: 0,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        use Direction::*;
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .tap(
                    Clientbound,
                    State::Play,
                    "container_set_content",
                    Self::menu_state,
                )
                .tap(
                    Clientbound,
                    State::Play,
                    "container_set_slot",
                    Self::menu_state,
                )
                .tap(
                    Serverbound,
                    State::Play,
                    "set_carried_item",
                    Self::carried_item,
                )
                .clientbound(State::Play, "login", Self::then_loaded)
                .clientbound(State::Play, "respawn", Self::then_loaded)
                .clientbound(State::Play, "set_held_slot", Self::set_held_slot)
                .clientbound(
                    State::Configuration,
                    "update_enabled_features",
                    Self::enabled_features,
                )
                .serverbound(State::Play, "move_vehicle", Self::move_vehicle)
                .serverbound(State::Play, "pick_item", Self::pick_item)
                .build()
        })
    }

    /// Follow the inventory menu's state ID (`container_set_content` and
    /// `container_set_slot` both start with the container and state IDs).
    fn menu_state(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        if reader.varint()? == 0 {
            self.inventory_state = reader.varint()?;
        }
        Ok(())
    }

    fn carried_item(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.selected = Reader::new(&packet.data).i16()?;
        Ok(())
    }

    /// A 1.21.4 server keeps a joining or respawning player invulnerable and
    /// still until the client says it has loaded the world, or three seconds
    /// have passed. A 1.21.2 client never says so: say it for the client as
    /// soon as it learns of the new world, which is what older servers
    /// assumed.
    fn then_loaded(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_client(state, packet);
        let id = data::version(NEWER).expect_packet(
            State::Play,
            Direction::Serverbound,
            "player_loaded",
        );
        cx.to_backend(State::Play, Packet::empty(id));
        Ok(())
    }

    fn set_held_slot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::held_slot(&packet.data, false)?;
        self.selected = i16::from(body[0] as i8);
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn enabled_features(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::enabled_features(&packet.data, false)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn move_vehicle(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::move_vehicle(&packet.data, true)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// A 1.21.2 client picking a block it carries outside the hotbar asks the
    /// server to swap that inventory slot into the hotbar. A 1.21.4 server
    /// only picks by block or entity, so swap the slot with the selected
    /// hotbar slot through the inventory menu instead; the server answers
    /// with the changed slots. Stale state IDs only make it resend the
    /// whole inventory.
    fn pick_item(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let slot = reader.varint()?;
        reader.finish()?;
        if !MAIN_INVENTORY.contains(&slot) || !(0..9).contains(&self.selected) {
            return Ok(());
        }
        let mut body = Writer::new();
        body.varint(0)
            .varint(self.inventory_state)
            .i16(slot as i16)
            .i8(self.selected as i8)
            .varint(SWAP)
            .varint(0) // No changed slots: the server works them out.
            .varint(0); // An empty cursor.
        let id =
            data::version(NEWER).expect_packet(state, Direction::Serverbound, "container_click");
        cx.to_backend(state, Packet::new(id, body.buf));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
