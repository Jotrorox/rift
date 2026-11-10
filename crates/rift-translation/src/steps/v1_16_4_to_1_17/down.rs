//! A 1.16.4 client on a 1.17 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{BORDER, NEWER, OLDER, check_border, common};
use crate::{
    Packet, State,
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
                .clientbound(Play, "login", common::login::<Self>)
                .clientbound(Play, "respawn", common::respawn::<Self>)
                .clientbound(Play, "player_position", Self::player_position)
                .clientbound(Play, "set_default_spawn_position", Self::spawn_position)
                .clientbound(Play, "ping", Self::ping)
                // Sculk sensor vibrations: a 1.16 client has nothing to show.
                .drop_clientbound(Play, "add_vibration_signal")
                .serverbound(Play, "container_ack", Self::container_ack)
                .serverbound(Play, "client_information", Self::client_information);
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
            builder.build()
        })
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
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
