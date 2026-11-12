//! A 1.12.1 client on a 1.12.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer},
};
use std::{collections::VecDeque, io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    keep_alives: KeepAlives,
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
            keep_alives: KeepAlives::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(State::Play, "keep_alive", Self::keep_alive_request)
                .serverbound(State::Play, "keep_alive", Self::keep_alive_answer)
                .build()
        })
    }

    fn keep_alive_request(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let id = reader.i64()?;
        reader.finish()?;
        let mut body = Writer::new();
        body.varint(self.keep_alives.narrow(id));
        cx.to_client(state, Packet::new(packet.id, body.into_inner()));
        Ok(())
    }

    fn keep_alive_answer(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let id = reader.varint()?;
        reader.finish()?;
        if let Some(id) = self.keep_alives.widen(id) {
            let mut body = Writer::new();
            body.i64(id);
            cx.to_backend(state, Packet::new(packet.id, body.into_inner()));
        }
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

/// Keep-alive IDs for a client that holds only VarInts. Vanilla's IDs are
/// milliseconds of the JVM's monotonic clock, which commonly counts from
/// boot: they fit an `i32` for the first 24.8 days of the host's uptime and
/// pass as they are. Any other ID (beyond `i32::MAX`, or negative) is
/// replaced by a negative stand-in, remembered until the client echoes it.
#[derive(Default)]
struct KeepAlives {
    /// Stand-in and real ID, oldest first.
    pending: VecDeque<(i32, i64)>,
    last: i32,
}

/// A server sends one keep-alive every 15 seconds and gives up after 30;
/// older stand-ins can no longer be answered usefully.
const PENDING: usize = 16;

impl KeepAlives {
    fn narrow(&mut self, id: i64) -> i32 {
        if let Ok(id @ 0..) = i32::try_from(id) {
            return id;
        }
        self.last = self.last.checked_sub(1).unwrap_or(-1);
        if self.pending.len() == PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back((self.last, id));
        self.last
    }

    /// The backend's ID for the client's answer `id`, or `None` for a
    /// stand-in that is unknown or already answered: the backend would
    /// disconnect the player for an ID it did not send.
    fn widen(&mut self, id: i32) -> Option<i64> {
        if id >= 0 {
            return Some(id.into());
        }
        let index = self
            .pending
            .iter()
            .position(|(stand_in, _)| *stand_in == id)?;
        self.pending.remove(index).map(|(_, real)| real)
    }
}
