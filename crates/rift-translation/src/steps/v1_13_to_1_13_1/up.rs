//! A 1.13.1 client on a 1.13 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    NEWER, OLDER,
    packets::{self, TEXT},
};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer},
};
use std::{collections::VecDeque, io, sync::OnceLock};

pub struct Up {
    shared: Shared,
    suggestions: Suggestions,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            suggestions: Suggestions::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::Play;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .serverbound(Play, "command_suggestion", Self::command_suggestion)
                .clientbound(Play, "command_suggestions", Self::command_suggestions)
                .clientbound(Play, "boss_event", packets::boss_event::<Self>)
                .clientbound(Play, "level_event", packets::level_event::<Self>)
                .clientbound(Play, "custom_payload", packets::custom_payload::<Self>)
                .serverbound(Play, "edit_book", packets::edit_book::<Self>)
                .build()
        })
    }

    /// A 1.13 server would parse the slash as part of the command and
    /// suggest nothing.
    fn command_suggestion(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let transaction = reader.varint()?;
        let command = reader.string(COMMAND)?;
        reader.finish()?;
        // Command blocks send their command as typed, usually without one.
        let (command, shift) = match command.strip_prefix('/') {
            Some(command) => (command, 1),
            None => (command, 0),
        };
        self.suggestions.requested(transaction, shift);
        let mut body = Writer::with_capacity(packet.data.len());
        body.varint(transaction).string(command);
        cx.to_backend(state, Packet::new(packet.id, body.into_inner()));
        Ok(())
    }

    /// The backend's range starts within the command it saw; the client's
    /// counts the slash that was taken off. Owned here instead of by the
    /// text codec because of that; the tooltips are JSON text both versions
    /// read alike.
    fn command_suggestions(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let transaction = rw.varint()?;
        let start = rw.r.varint()?;
        let shift = self.suggestions.answered(transaction);
        rw.w.varint(start.saturating_add(shift));
        rw.varint()?; // Length.
        rw.list(|rw| {
            rw.str()?; // Suggestion.
            rw.optional(|rw| rw.string(TEXT).map(drop)) // Tooltip.
        })?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

/// The longest command 1.13 reads for tab completion.
const COMMAND: usize = 32_500;

/// Tab completion requests on their way to the backend: the transaction ID
/// and how much to add to the start of the answer's range, oldest first.
#[derive(Default)]
struct Suggestions {
    pending: VecDeque<(i32, i32)>,
}

/// The client asks again on every key stroke and the server answers each
/// at once; more pending requests than this are abandoned.
const PENDING: usize = 8;

impl Suggestions {
    fn requested(&mut self, transaction: i32, shift: i32) {
        self.pending.retain(|(pending, _)| *pending != transaction);
        if self.pending.len() == PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back((transaction, shift));
    }

    /// An answer to a request this step did not see came, most likely, from
    /// the chat box, whose commands start with a slash.
    fn answered(&mut self, transaction: i32) -> i32 {
        self.pending
            .iter()
            .position(|(pending, _)| *pending == transaction)
            .and_then(|index| self.pending.remove(index))
            .map_or(1, |(_, shift)| shift)
    }
}
