//! A 1.14.3 client on a 1.14.4 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| {
    Box::new(Down {
        shared: Shared::new(OLDER, NEWER),
    })
});

pub struct Down {
    shared: Shared,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(State::Play, "block_break_ack", Self::block_break_ack)
                .build()
        })
    }

    /// A confirmed digging action needs nothing; a refused one tells the
    /// client what the block really is, as a block update.
    fn block_break_ack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = r.position()?;
        let block = r.varint()?;
        r.varint()?; // The digging action.
        let accepted = r.bool()?;
        r.finish()?;
        if accepted {
            return Ok(());
        }
        let Some(block) = self
            .shared
            .mapping(Direction::Clientbound, "block_state")
            .get(block)
        else {
            return Ok(());
        };
        static BLOCK_UPDATE: OnceLock<i32> = OnceLock::new();
        let id = *BLOCK_UPDATE.get_or_init(|| {
            Self::routes().client().expect_packet(
                State::Play,
                Direction::Clientbound,
                "block_update",
            )
        });
        let mut w = Writer::with_capacity(13);
        w.position(position).varint(block);
        cx.to_client(state, Packet::new(id, w.buf));
        Ok(())
    }
}
routed_step!(Down);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{step::Step, wire::Position};
    use std::collections::VecDeque;

    #[test]
    fn every_packet_has_a_route() {
        assert_eq!(Down::routes().problems(), &[] as &[String]);
    }

    fn ack(accepted: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.position(Position { x: 3, y: 70, z: -9 })
            .varint(1)
            .varint(2)
            .bool(accepted);
        w.buf
    }

    fn send(data: &[u8]) -> io::Result<Vec<(i32, Vec<u8>)>> {
        let id = crate::data::version(NEWER).expect_packet(
            State::Play,
            Direction::Clientbound,
            "block_break_ack",
        );
        let mut step = Down {
            shared: Shared::new(OLDER, NEWER),
        };
        let mut emitted = VecDeque::new();
        let mut cx = Cx {
            emitted: &mut emitted,
            client_protocol: OLDER,
            backend_protocol: NEWER,
        };
        step.clientbound(&mut cx, State::Play, Packet::new(id, data.to_vec()))?;
        Ok(emitted
            .into_iter()
            .map(|emitted| (emitted.packet.id, emitted.packet.data))
            .collect())
    }

    #[test]
    fn digging_acknowledgements_become_block_updates_when_refused() {
        assert_eq!(send(&ack(true)).unwrap(), []);
        let update = crate::data::version(OLDER).expect_packet(
            State::Play,
            Direction::Clientbound,
            "block_update",
        );
        let mut expected = Writer::new();
        expected.position(Position { x: 3, y: 70, z: -9 }).varint(1);
        assert_eq!(send(&ack(false)).unwrap(), [(update, expected.buf)]);
        assert!(send(&ack(false)[..9]).is_err());
        assert!(send(&[ack(false), vec![0]].concat()).is_err());
    }
}
