//! Last-seen bookkeeping for a 1.20.5 client on a 1.20.3 backend.
//!
//! A 1.20.3 server wants every command with the client's last-seen update:
//! how far the client's window of the 20 latest signed messages moved
//! (`offset`) and which messages in it the client saw (a 20-bit set). It
//! rejects an update that drops a message the client acknowledged before or
//! acknowledges one it never sent. A 1.20.5 client sends commands without
//! signed arguments with no update at all, so the step makes one up from the
//! client's last real update: the same set with offset 0 says nothing new and
//! is valid as long as the server's window has not moved since.
//!
//! The window moves with every `chat_ack`, so those are held back and their
//! offsets added to the client's next real update. Holding back too much
//! would let the server's pending list grow without bound, so once more than
//! two windows' worth is held, all but one window is passed on: the server's
//! window then lies entirely past the messages the last real update
//! acknowledged, and an empty set is valid for it.
use std::time::{SystemTime, UNIX_EPOCH};

/// Messages in a last-seen window.
const WINDOW: i32 = 20;

#[derive(Default)]
pub struct LastSeen {
    /// The acknowledged set of the client's last update the server got.
    acknowledged: [u8; 3],
    /// Offsets of `chat_ack`s held back from the server.
    held: i32,
    /// The latest timestamp the client sent: the server rejects messages
    /// older than the previous one.
    timestamp: i64,
}

impl LastSeen {
    /// The client sent a real update with a chat message or signed command:
    /// the offset to pass on, including what was held back.
    pub fn update(&mut self, offset: i32, acknowledged: [u8; 3], timestamp: i64) -> i32 {
        self.acknowledged = acknowledged;
        self.timestamp = self.timestamp.max(timestamp);
        offset.saturating_add(std::mem::take(&mut self.held))
    }

    /// The client acknowledged `offset` more messages: the offset of a
    /// `chat_ack` to pass on, if any.
    pub fn acknowledge(&mut self, offset: i32) -> Option<i32> {
        self.held = self.held.saturating_add(offset.max(0));
        let excess = self.held - WINDOW;
        (excess >= WINDOW).then(|| {
            self.acknowledged = [0; 3];
            self.held = WINDOW;
            excess
        })
    }

    /// An update for a command the client sent without one, and the
    /// timestamp to send it with.
    pub fn spoof(&mut self) -> (i32, [u8; 3], i64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        self.timestamp = self.timestamp.max(now);
        (0, self.acknowledged, self.timestamp)
    }

    /// The server starts a new window when the player (re)joins.
    pub fn reset(&mut self) {
        *self = Self {
            timestamp: self.timestamp,
            ..Self::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 1.20.3 server's check, reduced to what it tracks per position:
    /// `None` for no message, `Some(pending)` for one.
    struct Server {
        tracked: Vec<Option<bool>>,
    }

    impl Server {
        fn new() -> Self {
            Self {
                tracked: vec![None; WINDOW as usize],
            }
        }

        fn message(&mut self) {
            self.tracked.push(Some(true));
        }

        fn offset(&mut self, offset: i32) -> bool {
            let movable = self.tracked.len() as i32 - WINDOW;
            if !(0..=movable).contains(&offset) {
                return false;
            }
            self.tracked.drain(..offset as usize);
            true
        }

        fn update(&mut self, offset: i32, acknowledged: [u8; 3]) -> bool {
            if !self.offset(offset) {
                return false;
            }
            for index in 0..WINDOW as usize {
                let seen = acknowledged[index / 8] & (1 << (index % 8)) != 0;
                match (seen, self.tracked[index]) {
                    (true, None) | (false, Some(false)) => return false,
                    (true, Some(_)) => self.tracked[index] = Some(false),
                    (false, _) => self.tracked[index] = None,
                }
            }
            true
        }
    }

    /// The 1.20.5 client: its own window and what it acknowledges.
    struct Client {
        seen: Vec<bool>,
        pending: i32,
    }

    impl Client {
        fn update(&mut self) -> (i32, [u8; 3]) {
            let mut acknowledged = [0; 3];
            let start = self.seen.len().saturating_sub(WINDOW as usize);
            let window = &self.seen[start..];
            let skip = WINDOW as usize - window.len();
            for (index, seen) in window.iter().enumerate() {
                if *seen {
                    acknowledged[(index + skip) / 8] |= 1 << ((index + skip) % 8);
                }
            }
            (std::mem::take(&mut self.pending), acknowledged)
        }
    }

    #[test]
    fn spoofed_updates_stay_valid_through_held_acknowledgements() {
        let (mut server, mut seen) = (Server::new(), LastSeen::default());
        let mut client = Client {
            seen: Vec::new(),
            pending: 0,
        };
        for round in 0..200 {
            server.message();
            client.seen.push(round % 3 != 0);
            client.pending += 1;
            match round % 7 {
                // A signed chat message.
                0 => {
                    let (offset, acknowledged) = client.update();
                    let offset = seen.update(offset, acknowledged, round);
                    assert!(server.update(offset, acknowledged), "round {round}");
                }
                // An acknowledgement on its own.
                1 | 2 | 4 | 5 => {
                    let offset = std::mem::take(&mut client.pending);
                    if let Some(offset) = seen.acknowledge(offset) {
                        assert!(server.offset(offset), "round {round}");
                    }
                }
                // A command without signed arguments.
                _ => {
                    let (offset, acknowledged, _) = seen.spoof();
                    assert!(server.update(offset, acknowledged), "round {round}");
                }
            }
            assert!(server.tracked.len() <= 4 * WINDOW as usize, "round {round}");
        }
    }

    #[test]
    fn timestamps_never_go_back() {
        let mut seen = LastSeen::default();
        let future = i64::MAX / 2;
        seen.update(0, [0; 3], future);
        assert_eq!(seen.spoof().2, future);
        seen.reset();
        assert_eq!(seen.spoof().2, future);
    }
}
