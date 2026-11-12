//! Replay a recorded real session through the translator, offline:
//!
//! ```sh
//! cargo run -p rift-translation --example replay -- --backend 754 --client 755 > out.jsonl
//! ```
//!
//! reads the session `tests/translation_corpus.py` recorded from the official
//! `--backend` server (`~/.cache/rift-via/corpus/<protocol>.jsonl`, or
//! `$RIFT_TRANSLATION_CORPUS`), feeds its clientbound packets through a
//! translator for a `--client` player and, interleaved by protocol phase, the
//! serverbound packets a real `--client` session sent (from that protocol's
//! own recording). Every translated packet is written as a JSON line in the
//! corpus format plus `"to"` (`"client"` or `"backend"`), `"name"` and `"from"`
//! (the index and name of the packet that produced it); a packet the
//! translator rejects becomes a line with `"error"`. `--summary` prints
//! counts per packet name instead. The exit status is 1 when any packet
//! failed, so a step can be checked without starting a server.
//! `tests/translation_replay.py` decodes the output with the receiving
//! version's minecraft-data schema.
use rift_translation::{Direction, Output, Packet, Sent, State, Translator, data};
use std::{collections::BTreeMap, io::Write, path::PathBuf, process::ExitCode};

struct Recorded {
    direction: Direction,
    state: State,
    packet: Packet,
}

fn corpus(protocol: i32) -> Result<Vec<Recorded>, String> {
    let directory = std::env::var_os("RIFT_TRANSLATION_CORPUS")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".cache/rift-via/corpus")))
        .ok_or("no corpus directory")?;
    let path = directory.join(format!("{protocol}.jsonl"));
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).map_err(|error| error.to_string())?;
            let hex = value["hex"].as_str().ok_or("line without hex")?;
            let data = (0..hex.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(&hex[index..index + 2], 16))
                .collect::<Result<_, _>>()
                .map_err(|error| error.to_string())?;
            Ok(Recorded {
                direction: match value["d"].as_str() {
                    Some("c") => Direction::Clientbound,
                    _ => Direction::Serverbound,
                },
                state: match value["s"].as_str() {
                    Some("login") => State::Login,
                    Some("configuration") => State::Configuration,
                    _ => State::Play,
                },
                packet: Packet::new(value["id"].as_i64().ok_or("line without id")? as i32, data),
            })
        })
        .collect()
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Handshake => "handshake",
        State::Status => "status",
        State::Login => "login",
        State::Configuration => "configuration",
        State::Play => "play",
    }
}

fn name(protocol: i32, state: State, direction: Direction, id: i32) -> &'static str {
    data::version(protocol)
        .packet_name(state, direction, id)
        .unwrap_or("?")
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn main() -> ExitCode {
    let (mut backend, mut client, mut summary) = (None, None, false);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" => backend = args.next().and_then(|value| value.parse::<i32>().ok()),
            "--client" => client = args.next().and_then(|value| value.parse::<i32>().ok()),
            "--summary" => summary = true,
            _ => {
                eprintln!("usage: replay --backend PROTOCOL --client PROTOCOL [--summary]");
                return ExitCode::from(2);
            }
        }
    }
    let (Some(backend), Some(client)) = (backend, client) else {
        eprintln!("usage: replay --backend PROTOCOL --client PROTOCOL [--summary]");
        return ExitCode::from(2);
    };
    let mut translator = match Translator::new(client, backend) {
        Ok(Some(translator)) => translator,
        Ok(None) => {
            eprintln!("both sides speak protocol {client}: nothing to translate");
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let (from_backend, from_client) = match (corpus(backend), corpus(client)) {
        (Ok(backend), Ok(client)) => (backend, client),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let mut clientbound = from_backend
        .into_iter()
        .filter(|recorded| recorded.direction == Direction::Clientbound)
        .enumerate()
        .peekable();
    let mut serverbound = from_client
        .into_iter()
        .filter(|recorded| recorded.direction == Direction::Serverbound)
        .enumerate()
        .peekable();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut counts: BTreeMap<(&'static str, String, bool), usize> = BTreeMap::new();
    let mut failures = 0usize;
    loop {
        // A client packet goes in as soon as the translated client side has
        // reached the phase it was recorded in; backend packets drive phases.
        let from_client = serverbound
            .peek()
            .is_some_and(|(_, recorded)| recorded.state == translator.client_phase());
        let (index, recorded) = if from_client {
            serverbound.next().unwrap()
        } else if let Some(next) = clientbound.next() {
            next
        } else if let Some((index, recorded)) = serverbound.next() {
            // The session ended in another phase than the client recorded.
            eprintln!(
                "skipping client packet {index} recorded in {}",
                state_name(recorded.state)
            );
            continue;
        } else {
            break;
        };
        let (source, phase) = match recorded.direction {
            Direction::Clientbound => (backend, translator.backend_phase()),
            Direction::Serverbound => (client, translator.client_phase()),
        };
        let source_name = name(source, phase, recorded.direction, recorded.packet.id);
        let side = match recorded.direction {
            Direction::Clientbound => "backend",
            Direction::Serverbound => "client",
        };
        let mut out = Output::default();
        let result = match recorded.direction {
            Direction::Clientbound => translator.clientbound(recorded.packet, &mut out),
            Direction::Serverbound => translator.serverbound(recorded.packet, &mut out),
        };
        let key = (side, source_name.to_owned(), result.is_ok());
        *counts.entry(key).or_default() += 1;
        if let Err(error) = &result {
            failures += 1;
            if !summary {
                let line = serde_json::json!({
                    "error": error.to_string(),
                    "from": {"side": side, "index": index, "name": source_name,
                             "s": state_name(phase)},
                });
                if writeln!(stdout, "{line}").is_err() {
                    return ExitCode::from(1);
                }
            }
        }
        if summary {
            continue;
        }
        let lines = out
            .client
            .into_iter()
            .map(|sent| ("client", client, Direction::Clientbound, sent))
            .chain(
                out.backend
                    .into_iter()
                    .map(|sent| ("backend", backend, Direction::Serverbound, sent)),
            );
        for (to, protocol, direction, Sent { state, packet }) in lines {
            let line = serde_json::json!({
                "to": to,
                "d": if direction == Direction::Clientbound { "c" } else { "s" },
                "s": state_name(state),
                "id": packet.id,
                "name": name(protocol, state, direction, packet.id),
                "hex": hex(&packet.data),
                "from": {"side": side, "index": index, "name": source_name},
            });
            // A reader that stopped early (`| head`) ends the replay.
            if writeln!(stdout, "{line}").is_err() {
                return ExitCode::from(1);
            }
        }
    }
    if summary {
        for ((side, packet, ok), count) in &counts {
            let status = if *ok { "ok" } else { "FAILED" };
            let _ = writeln!(stdout, "{side:8} {packet:40} {count:6} {status}");
        }
    }
    eprintln!("{failures} packets failed");
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
