//! Tab completion for a 1.12.2 client on a 1.13 backend.
//!
//! A 1.12 client asks with the whole input and gets back words that each
//! replace the last word before the cursor (the text after the last space).
//! A 1.13 server answers a numbered request with the range of the input to
//! replace and its replacements. Chat that is not a command completes
//! player names, which a 1.13 server leaves to its clients: the step
//! answers those itself from the tab list.
use super::Down;
use crate::{
    Direction, Packet, State,
    step::Cx,
    wire::{Reader, Writer},
};
use std::io;

/// The longest command a 1.13 server accepts in a suggestion request.
const MAX_COMMAND: usize = 32500;

/// A forwarded request and the input it was for.
pub(super) struct Pending {
    id: i32,
    /// The input in UTF-16 units, the unit 1.13's ranges count in.
    text: Vec<u16>,
}

impl Down {
    pub(super) fn command_suggestion(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let text = reader.string(32767)?;
        let assume_command = reader.bool()?;
        if reader.bool()? {
            // The block the player looks at, which 1.13 clients complete
            // coordinates from themselves.
            reader.i64()?;
        }
        reader.finish()?;
        if !assume_command && !text.starts_with('/') {
            let word = last_word(text).to_lowercase();
            let mut names: Vec<&str> = self
                .players
                .values()
                .map(String::as_str)
                .filter(|name| name.to_lowercase().starts_with(&word))
                .collect();
            names.sort_unstable();
            names.dedup();
            cx.to_client(state, answer(&names));
            return Ok(());
        }
        if text.encode_utf16().count() > MAX_COMMAND {
            cx.to_client(state, answer(&[]));
            return Ok(());
        }
        let id = self.next_suggestion;
        self.next_suggestion = self.next_suggestion.wrapping_add(1);
        self.suggestion = Some(Pending {
            id,
            text: text.encode_utf16().collect(),
        });
        // A 1.13 server skips a leading slash itself, and a command block's
        // input (`assume_command`) has none.
        let mut out = Writer::new();
        out.varint(id).string(text);
        cx.to_backend(state, Packet::new(packet.id, out.into_inner()));
        Ok(())
    }

    pub(super) fn command_suggestions(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let id = reader.varint()?;
        let (start, length) = (reader.varint()?, reader.varint()?);
        let mut matches = Vec::new();
        for _ in 0..reader.count()? {
            matches.push(reader.string(32767)?);
            if reader.bool()? {
                reader.string(262144)?; // The tooltip, which 1.12 cannot show.
            }
        }
        reader.finish()?;
        let Some(pending) = self.suggestion.take_if(|pending| pending.id == id) else {
            // An answer to a request the client has moved on from.
            return Ok(());
        };
        let words = replacements(&pending.text, start, length, &matches);
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        cx.to_client(state, answer(&words));
        Ok(())
    }
}

/// The text after the last space, which a 1.12 client replaces.
fn last_word(text: &str) -> &str {
    text.rsplit(' ').next().unwrap_or(text)
}

/// A 1.12 tab-complete answer.
fn answer(words: &[&str]) -> Packet {
    let mut out = Writer::new();
    out.varint(words.len() as i32);
    for word in words {
        out.string(word);
    }
    Packet::new(
        Down::client_id(State::Play, Direction::Clientbound, "command_suggestions"),
        out.into_inner(),
    )
}

/// 1.13 suggestions, each replacing `length` units of `text` from `start`,
/// as the 1.12 replacements of the last word. A suggestion that changes
/// text before the last word cannot be expressed and is left out.
fn replacements(text: &[u16], start: i32, length: i32, matches: &[&str]) -> Vec<String> {
    let (Ok(start), Ok(length)) = (usize::try_from(start), usize::try_from(length)) else {
        return Vec::new();
    };
    let Some(end) = start.checked_add(length).filter(|end| *end <= text.len()) else {
        return Vec::new();
    };
    let space = u16::from(b' ');
    let word = text
        .iter()
        .rposition(|unit| *unit == space)
        .map_or(0, |index| index + 1);
    matches
        .iter()
        .filter_map(|suggestion| {
            let mut full = text[..start].to_vec();
            full.extend(suggestion.encode_utf16());
            full.extend_from_slice(&text[end..]);
            if full.len() < word || full[..word] != text[..word] {
                return None;
            }
            String::from_utf16(&full[word..]).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn suggestions_become_replacements_of_the_last_word() {
        // The command name after the slash keeps its slash.
        assert_eq!(
            replacements(&utf16("/gam"), 1, 3, &["gamemode", "gamerule"]),
            ["/gamemode", "/gamerule"]
        );
        // An argument after a space.
        assert_eq!(
            replacements(&utf16("/gamemode s"), 10, 1, &["spectator", "survival"]),
            ["spectator", "survival"]
        );
        // Nothing typed yet after the space.
        assert_eq!(
            replacements(&utf16("/gamemode "), 10, 0, &["adventure"]),
            ["adventure"]
        );
        // A command block's input has no slash.
        assert_eq!(replacements(&utf16("sa"), 0, 2, &["say"]), ["say"]);
        // A suggestion inside the last word keeps what precedes it.
        assert_eq!(
            replacements(
                &utf16("/give @p minecraft:sto"),
                9,
                13,
                &["minecraft:stone"]
            ),
            ["minecraft:stone"]
        );
        // Suggestions reaching back over a space cannot be expressed, ranges
        // past the input are ignored.
        assert!(replacements(&utf16("/tp a b"), 4, 3, &["x y"]).is_empty());
        assert!(replacements(&utf16("/tp"), 2, 5, &["x"]).is_empty());
        assert!(replacements(&utf16("/tp"), -1, 0, &["x"]).is_empty());
        // UTF-16 ranges count a character outside the BMP twice.
        assert_eq!(replacements(&utf16("/say 😀a"), 7, 1, &["ab"]), ["😀ab"]);
    }

    #[test]
    fn the_last_word_follows_the_last_space() {
        assert_eq!(last_word("hello wor"), "wor");
        assert_eq!(last_word("hello "), "");
        assert_eq!(last_word("hey"), "hey");
    }
}
