//! A 1.16.4 client on a 1.17 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    BORDER, COMBAT, HIDE_TITLES, NEWER, OLDER, RESET_TITLES, TITLES, check_border,
    common::{self, EMPTY_ITEM, Login, Respawn},
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, Handler, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer},
};
use std::{collections::VecDeque, io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    /// Backend pings relayed to the client as transactions, oldest first:
    /// the action number used and the ping's ID.
    pings: VecDeque<(i16, i32)>,
    next_action: i16,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// Pings waiting for the client at most; past this the oldest is answered
/// on the client's behalf, so a client that stops answering costs nothing.
pub(super) const MAX_PINGS: usize = 256;

impl Down {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            pings: VecDeque::new(),
            next_action: i16::MIN,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::Play;
            let mut builder = RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "player_position", Self::player_position)
                .clientbound(Play, "set_default_spawn_position", Self::spawn_position)
                .clientbound(Play, "ping", Self::ping)
                .clientbound(Play, "remove_entity", Self::remove_entity)
                .clientbound(Play, "clear_titles", Self::clear_titles)
                .clientbound(Play, "set_titles_animation", Self::titles_animation)
                // Sculk sensor vibrations: a 1.16 client has nothing to show.
                .drop_clientbound(Play, "add_vibration_signal")
                .serverbound(Play, "container_ack", Self::container_ack)
                .serverbound(Play, "client_information", Self::client_information)
                .serverbound(Play, "container_click", Self::container_click);
            let borders: [Handler<Self>; 6] = [
                |step, cx, state, packet| step.border(cx, state, packet, 0),
                |step, cx, state, packet| step.border(cx, state, packet, 1),
                |step, cx, state, packet| step.border(cx, state, packet, 2),
                |step, cx, state, packet| step.border(cx, state, packet, 3),
                |step, cx, state, packet| step.border(cx, state, packet, 4),
                |step, cx, state, packet| step.border(cx, state, packet, 5),
            ];
            for (name, handler) in BORDER.into_iter().zip(borders) {
                builder = builder.clientbound(Play, name, handler);
            }
            let combat: [Handler<Self>; 3] = [
                |step, cx, state, packet| step.combat(cx, state, packet, 0),
                |step, cx, state, packet| step.combat(cx, state, packet, 1),
                |step, cx, state, packet| step.combat(cx, state, packet, 2),
            ];
            for (name, handler) in COMBAT.into_iter().zip(combat) {
                builder = builder.clientbound(Play, name, handler);
            }
            let titles: [Handler<Self>; 3] = [
                |step, cx, state, packet| step.title(cx, state, packet, 0),
                |step, cx, state, packet| step.title(cx, state, packet, 1),
                |step, cx, state, packet| step.title(cx, state, packet, 2),
            ];
            for (name, handler) in TITLES.into_iter().zip(titles) {
                builder = builder.clientbound(Play, name, handler);
            }
            builder.build()
        })
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut login = Login::read(NEWER, &packet.data)?;
        // INTEGRATION: the registries codec's conversion of the dimension codec.
        login.edit_dimension_types(common::remove_world_height)?;
        login.edit_biomes(|biome| common::known_category(biome, common::CATEGORIES_1_16))?;
        cx.to_client(state, Packet::new(packet.id, login.write(OLDER)));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut respawn = Respawn::read(&packet.data)?;
        // INTEGRATION: the registries codec's conversion of the dimension type.
        common::remove_world_height(respawn.dimension_type()?)?;
        cx.to_client(state, Packet::new(packet.id, respawn.write()));
        Ok(())
    }

    fn player_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        for _ in 0..3 {
            rw.f64()?;
        }
        rw.f32()?;
        rw.f32()?;
        rw.i8()?;
        rw.varint()?;
        // Dismounting: the backend also sends the passenger change.
        rw.r.bool()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn spawn_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i64()?;
        rw.r.f32()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// A 1.16 client has no ping, but answers every rejected transaction on
    /// its inventory: relaying pings that way keeps the round trip plugins
    /// measure with them.
    fn ping(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let id = r.i32()?;
        r.finish()?;
        if self.pings.len() >= MAX_PINGS
            && let Some((_, oldest)) = self.pings.pop_front()
        {
            self.pong(cx, state, oldest);
        }
        let action = self.next_action;
        self.next_action = action.wrapping_add(1);
        self.pings.push_back((action, id));
        let mut w = Writer::new();
        w.u8(0).i16(action).bool(false);
        common::to_client(cx, OLDER, state, "container_ack", w.into_inner());
        Ok(())
    }

    fn pong(&mut self, cx: &mut Cx<'_>, state: State, id: i32) {
        let mut w = Writer::new();
        w.i32(id);
        common::to_backend(cx, NEWER, state, "pong", w.into_inner());
    }

    /// The client's answer to a relayed ping. Earlier pings it skipped are
    /// answered too, in order; anything else is the client confirming a
    /// transaction the backend never sent.
    fn container_ack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let window = r.u8()?;
        let action = r.i16()?;
        r.bool()?;
        r.finish()?;
        if window != 0 {
            return Ok(());
        }
        if let Some(index) = self.pings.iter().position(|(sent, _)| *sent == action) {
            for (_, id) in self.pings.drain(..=index).collect::<Vec<_>>() {
                self.pong(cx, state, id);
            }
        }
        Ok(())
    }

    fn remove_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.varint(1);
        rw.varint()?;
        common::to_client(cx, OLDER, state, "remove_entities", rw.finish()?);
        Ok(())
    }

    fn border(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
        action: usize,
    ) -> io::Result<()> {
        check_border(action, &packet.data)?;
        let mut w = Writer::with_capacity(packet.data.len() + 1);
        w.varint(action as i32).bytes(&packet.data);
        common::to_client(cx, OLDER, state, "set_border", w.into_inner());
        Ok(())
    }

    fn combat(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
        action: i32,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.varint(action);
        match action {
            1 => {
                rw.varint()?;
                rw.i32()?;
            }
            2 => {
                rw.varint()?;
                rw.i32()?;
                common::text(&mut self.shared, Direction::Clientbound, &mut rw)?;
            }
            _ => {}
        }
        common::to_client(cx, OLDER, state, "player_combat", rw.finish()?);
        Ok(())
    }

    fn title(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
        action: i32,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.varint(action);
        common::text(&mut self.shared, Direction::Clientbound, &mut rw)?;
        common::to_client(cx, OLDER, state, "set_titles", rw.finish()?);
        Ok(())
    }

    fn titles_animation(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.varint(3);
        for _ in 0..3 {
            rw.i32()?;
        }
        common::to_client(cx, OLDER, state, "set_titles", rw.finish()?);
        Ok(())
    }

    fn clear_titles(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let reset = r.bool()?;
        r.finish()?;
        let mut w = Writer::new();
        w.varint(if reset { RESET_TITLES } else { HIDE_TITLES });
        common::to_client(cx, OLDER, state, "set_titles", w.into_inner());
        Ok(())
    }

    fn client_information(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.string(16)?;
        rw.i8()?;
        rw.varint()?;
        rw.bool()?;
        rw.u8()?;
        rw.varint()?;
        // A 1.16 client cannot ask for its chat to be filtered.
        rw.w.bool(false);
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// A 1.16 click names the stack it picked up; a 1.17 click names what
    /// the client now sees in the slots it changed and on the cursor, and
    /// the backend corrects every slot where that differs from the truth.
    /// Without replaying the click the step reports the clicked slot as
    /// emptied and the picked-up stack as carried, which is exactly right
    /// for a plain pick-up; for any other outcome the backend's corrections
    /// bring the client in line.
    fn container_click(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.u8()?;
        let slot = rw.i16()?;
        rw.i8()?;
        rw.r.i16()?;
        rw.varint()?;
        if slot >= 0 {
            rw.w.varint(1).i16(slot).bytes(EMPTY_ITEM);
        } else {
            rw.w.varint(0);
        }
        common::item(&mut self.shared, Direction::Serverbound, &mut rw)?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
