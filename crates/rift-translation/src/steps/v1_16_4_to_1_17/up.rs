//! A 1.17 client on a 1.16.4 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    BORDER, NEWER, OLDER, check_border,
    common::{self, EMPTY_ITEM, raw_item},
    menus::Menus,
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

pub struct Up {
    shared: Shared,
    menus: Menus,
    /// The action number of the next click sent to the backend.
    next_action: i16,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// Pings standing in for rejected transactions carry this in their top
/// byte, then the window and the action number.
const TRANSACTION: i32 = 0x52 << 24;

/// 1.16 click modes whose result the backend compares with the stack in the
/// clicked slot: pick up (0) and quick move (1), with either button.
fn picks_up(mode: i32, button: i8) -> bool {
    matches!(mode, 0 | 1) && matches!(button, 0 | 1)
}

impl Up {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            menus: Menus::default(),
            next_action: 1,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::Play;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Play, "login", common::login::<Self>)
                .clientbound(Play, "respawn", common::respawn::<Self>)
                .clientbound(Play, "player_position", Self::player_position)
                .clientbound(Play, "set_default_spawn_position", Self::spawn_position)
                .clientbound(Play, "container_ack", Self::container_ack)
                .clientbound(Play, "set_border", Self::set_border)
                // A no-op movement packet that 1.17 finally removed.
                .drop_clientbound(Play, "move_entity")
                .serverbound(Play, "pong", Self::pong)
                .serverbound(Play, "client_information", Self::client_information)
                .serverbound(Play, "container_click", Self::container_click)
                .tap(
                    Clientbound,
                    Play,
                    "container_set_content",
                    |step, _, packet| step.menus.set_content(&packet.data),
                )
                .tap(
                    Clientbound,
                    Play,
                    "container_set_slot",
                    |step, _, packet| step.menus.set_slot(&packet.data),
                )
                .tap(Clientbound, Play, "container_close", |step, _, packet| {
                    step.menus.close(Reader::new(&packet.data).u8()?);
                    Ok(())
                })
                .tap(Serverbound, Play, "container_close", |step, _, packet| {
                    step.menus.close(Reader::new(&packet.data).u8()?);
                    Ok(())
                })
                .build()
        })
    }

    /// 1.17 tells the client whether a teleport dismounts it; 1.16 servers
    /// dismount players with their own packets.
    fn player_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        for _ in 0..3 {
            rw.f64()?;
        }
        rw.f32()?;
        rw.f32()?;
        rw.i8()?;
        rw.varint()?;
        rw.w.bool(false);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// 1.17 added the angle the player respawns facing.
    fn spawn_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i64()?;
        rw.w.f32(0.0);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// A 1.16 server confirms or rejects each click; after a rejection it
    /// ignores the window's clicks until the client acknowledges. 1.17
    /// clients have no transactions, so the acknowledgement rides on a ping
    /// and goes back to the backend when the client answers it, keeping the
    /// round trip that anticheats measure with these packets.
    fn container_ack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let window = r.u8()?;
        let action = r.i16()?;
        let accepted = r.bool()?;
        r.finish()?;
        if !accepted {
            let id = TRANSACTION | i32::from(window) << 16 | i32::from(action as u16);
            let mut w = Writer::new();
            w.i32(id);
            common::to_client(cx, NEWER, state, "ping", w.into_inner());
        }
        Ok(())
    }

    fn pong(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let id = r.i32()?;
        r.finish()?;
        // Every ping a 1.16 backend's client gets comes from this step.
        if id & !0xff_ffff == TRANSACTION {
            let mut w = Writer::new();
            w.u8((id >> 16) as u8).i16(id as i16).bool(true);
            common::to_backend(cx, OLDER, state, "container_ack", w.into_inner());
        }
        Ok(())
    }

    fn set_border(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let action = r.varint()?;
        let (action, name) = usize::try_from(action)
            .ok()
            .and_then(|action| Some((action, *BORDER.get(action)?)))
            .ok_or_else(|| invalid("unknown world border action"))?;
        let body = r.rest();
        check_border(action, body)?;
        common::to_client(cx, NEWER, state, name, body.to_vec());
        Ok(())
    }

    /// 1.17 added text filtering to the client settings.
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
        rw.r.bool()?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// A 1.17 click names the slots it changed and the stack left on the
    /// cursor; a 1.16 click names the stack picked up and an action number
    /// for the backend's confirmation. The picked-up stack is what the
    /// clicked slot held, as far as the step followed it; the client's
    /// changes then update that record.
    fn container_click(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let mut out = Writer::with_capacity(packet.data.len());
        let window = rw.r.u8()?;
        let slot = rw.r.i16()?;
        let button = rw.r.i8()?;
        let mode = rw.r.varint()?;
        let clicked = if slot >= 0 && picks_up(mode, button) {
            self.menus.get(window, slot).to_vec()
        } else {
            EMPTY_ITEM.to_vec()
        };
        let tracked = self.menus.knows(window);
        for _ in 0..rw.r.count()? {
            let changed = rw.r.i16()?;
            if tracked {
                common::item(&mut self.shared, Direction::Serverbound, &mut rw)?;
                let item = std::mem::take(&mut rw.w.buf);
                if let Ok(changed) = usize::try_from(changed) {
                    self.menus.set(window, changed, &item);
                }
            } else {
                raw_item(&mut rw.r)?;
            }
        }
        // The carried stack: 1.16 servers work it out themselves.
        raw_item(&mut rw.r)?;
        rw.finish()?;
        let action = self.next_action;
        self.next_action = action.checked_add(1).unwrap_or(1);
        out.u8(window)
            .i16(slot)
            .i8(button)
            .i16(action)
            .varint(mode)
            .bytes(&clicked);
        cx.to_backend(state, Packet::new(packet.id, out.into_inner()));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
