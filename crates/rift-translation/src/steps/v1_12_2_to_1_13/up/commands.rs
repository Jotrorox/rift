//! Commands: 1.12 servers send no command tree and complete commands from
//! the raw chat text; 1.13 clients parse commands with the tree and ask the
//! server only where the tree says so.
//!
//! The client gets a tree whose root has a single greedy string argument
//! that is executable and asks the server for suggestions, so it accepts
//! any command and sends every completion request on. Requests carry a
//! transaction ID and the replacement range in 1.13; 1.12 answers only with
//! the matches for the last word, in request order.
use super::Up;
use crate::{
    Packet, State,
    step::{Cx, RouteBuilder},
    wire::{Reader, Writer},
};
use std::io;

/// Requests the backend has not answered yet that the step keeps; a client
/// only waits for its latest anyway.
const PENDING: usize = 16;

pub fn install(builder: RouteBuilder<Up>) -> RouteBuilder<Up> {
    builder
        .serverbound(State::Play, "command_suggestion", request)
        .clientbound(State::Play, "command_suggestions", suggestions)
}

/// The catch-all tree: root, then `args` (`brigadier:string`, greedy phrase,
/// `minecraft:ask_server` suggestions).
// INTEGRATION: the misc codec plans a shared helper for this tree.
pub fn tree() -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(2); // Nodes.
    writer.u8(0).varint(1).varint(1); // Root with one child, node 1.
    // Argument (2), executable (4), with a suggestions type (0x10).
    writer.u8(0x02 | 0x04 | 0x10).varint(0);
    writer
        .string("args")
        .string("brigadier:string")
        .varint(2) // Greedy phrase.
        .string("minecraft:ask_server");
    writer.varint(0); // Root index.
    writer.buf
}

/// 1.13: transaction ID and text. 1.12: text, "assume command", optional
/// looked-at block. The 1.13 client sends the whole input; 1.12 completes
/// commands only when the text starts with a slash.
fn request(step: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let transaction = reader.varint()?;
    let text = reader.string(32500)?;
    reader.finish()?;
    let command = if text.starts_with('/') {
        text.to_owned()
    } else {
        format!("/{text}")
    };
    if step.suggestions.len() == PENDING {
        step.suggestions.pop_front();
    }
    step.suggestions.push_back((transaction, text.to_owned()));
    let mut writer = Writer::with_capacity(command.len() + 4);
    writer.string(&command).bool(false).bool(false);
    cx.to_backend(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// The UTF-16 length of `text`: suggestion ranges count Java characters.
fn units(text: &str) -> i32 {
    text.encode_utf16().count() as i32
}

/// 1.12: the matches. 1.13: transaction ID, the range they replace, and
/// each match with an optional tooltip.
fn suggestions(step: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let count = reader.count()?;
    let mut matches = Vec::with_capacity(count);
    for _ in 0..count {
        matches.push(reader.str()?);
    }
    reader.finish()?;
    let Some((transaction, text)) = step.suggestions.pop_front() else {
        // Nothing asked: a 1.13 client would not know what to replace.
        return Ok(());
    };
    // 1.12 completes the last word: everything after the last space, or the
    // command name after the slash.
    let word = text
        .rfind(' ')
        .map_or_else(|| usize::from(text.starts_with('/')), |space| space + 1);
    let first_word = !text.contains(' ');
    let mut writer = Writer::with_capacity(packet.data.len() + 8);
    writer
        .varint(transaction)
        .varint(units(&text[..word]))
        .varint(units(&text[word..]))
        .varint(matches.len() as i32);
    for found in matches {
        let found = if first_word {
            found.strip_prefix('/').unwrap_or(found)
        } else {
            found
        };
        writer.string(found).bool(false);
    }
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}
