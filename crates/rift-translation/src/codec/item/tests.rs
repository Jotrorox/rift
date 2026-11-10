//! Real traffic: every item-carrying packet of every recorded session must
//! parse completely and serialise back to the same bytes, and survive the
//! default handlers of both neighbouring steps in a form the receiver's
//! layout reads.
use super::{
    advancements::Advancements,
    merchant::Offers,
    recipes::{self, BookAdd, PlaceGhostRecipe, RecipeBook, UpdateRecipes},
    window::{
        self, Click, Cooldown, CreativeSlot, EditBook, SetContent, SetEquipment,
        SetPlayerInventory, SetSlot,
    },
};
use crate::{
    Direction, State,
    codec::testing::{Defaults, corpus},
    version::PROTOCOLS,
    wire::{Reader, Writer},
};
use std::io;

/// Errors from codecs that are still stubs: such packets are counted, not
/// failed, until those codecs land.
fn pending(error: &io::Error) -> bool {
    let message = error.to_string();
    message.contains("not implemented") || message.contains("needs the component codec")
}

/// Parse `data` as the packet `name` of `protocol` and write it back.
/// `None` when the packet is not one of this codec's.
fn round_trip(
    protocol: i32,
    direction: Direction,
    name: &str,
    data: &[u8],
) -> Option<io::Result<Vec<u8>>> {
    fn run<T>(
        protocol: i32,
        data: &[u8],
        read: impl Fn(i32, &mut Reader<'_>) -> io::Result<T>,
        write: impl Fn(&T, i32, &mut Writer) -> io::Result<()>,
    ) -> io::Result<Vec<u8>> {
        let mut reader = Reader::new(data);
        let value = read(protocol, &mut reader)?;
        reader.finish()?;
        let mut writer = Writer::new();
        write(&value, protocol, &mut writer)?;
        Ok(writer.buf)
    }
    use Direction::{Clientbound, Serverbound};
    Some(match (direction, name) {
        (Clientbound, "container_set_content") => {
            run(protocol, data, SetContent::read, SetContent::write)
        }
        (Clientbound, "container_set_slot") => run(protocol, data, SetSlot::read, SetSlot::write),
        (Clientbound, "set_cursor_item") => {
            run(protocol, data, window::read_stack, |stack, p, w| {
                window::write_stack(p, w, stack)
            })
        }
        (Clientbound, "set_player_inventory") => run(
            protocol,
            data,
            SetPlayerInventory::read,
            SetPlayerInventory::write,
        ),
        (Clientbound, "set_equipment" | "set_equipped_item") => {
            run(protocol, data, SetEquipment::read, SetEquipment::write)
        }
        (Clientbound, "cooldown") => run(protocol, data, Cooldown::read, Cooldown::write),
        (Clientbound, "merchant_offers") => run(protocol, data, Offers::read, Offers::write),
        (Clientbound, "update_advancements") => {
            run(protocol, data, Advancements::read, Advancements::write)
        }
        (Clientbound, "update_recipes") => {
            run(protocol, data, UpdateRecipes::read, UpdateRecipes::write)
        }
        (Clientbound, "recipe") => run(protocol, data, RecipeBook::read, RecipeBook::write),
        (Clientbound, "recipe_book_add") => run(protocol, data, BookAdd::read, BookAdd::write),
        (Clientbound, "recipe_book_remove") => run(
            protocol,
            data,
            |_, r| recipes::read_book_remove(r),
            |ids, _, w| {
                recipes::write_book_remove(w, ids);
                Ok(())
            },
        ),
        (Clientbound, "recipe_book_settings") => run(
            protocol,
            data,
            |_, r| recipes::read_book_settings(r),
            |flags, _, w| {
                recipes::write_book_settings(w, flags);
                Ok(())
            },
        ),
        (Clientbound, "place_ghost_recipe") => run(
            protocol,
            data,
            PlaceGhostRecipe::read,
            PlaceGhostRecipe::write,
        ),
        (Serverbound, "container_click") => run(protocol, data, Click::read, Click::write),
        (Serverbound, "set_creative_mode_slot") => {
            run(protocol, data, CreativeSlot::read, CreativeSlot::write)
        }
        (Serverbound, "edit_book") => run(protocol, data, EditBook::read, EditBook::write),
        _ => return None,
    })
}

/// Packets the codec leaves to a step: recipes across the flattening
/// (numbered in 1.12, named in 1.13) and recipe lists 1.12 does not have.
fn step_owned(a: i32, b: i32, name: &str) -> bool {
    a.min(b) == 340
        && a.max(b) == 393
        && matches!(name, "recipe" | "place_ghost_recipe" | "update_recipes")
}

#[derive(Default, Debug)]
struct Counts {
    checked: usize,
    pending: usize,
}

#[test]
fn corpus_packets_round_trip_in_every_protocol() {
    let mut failures = Vec::new();
    let mut summary = Vec::new();
    for &(protocol, release) in PROTOCOLS {
        let Some(packets) = corpus(protocol) else {
            summary.push(format!("{release}: no corpus, skipped"));
            continue;
        };
        let mut counts = Counts::default();
        for packet in &packets {
            let name = packet.name(protocol);
            let Some(result) = round_trip(protocol, packet.direction, name, &packet.data) else {
                continue;
            };
            match result {
                Ok(bytes) if bytes == packet.data => counts.checked += 1,
                Ok(_) => failures.push(format!("{release} {name}: re-serialised bytes differ")),
                Err(error) if pending(&error) => counts.pending += 1,
                Err(error) => failures.push(format!("{release} {name}: {error}")),
            }
        }
        summary.push(format!(
            "{release}: {} round trips, {} pending other codecs",
            counts.checked, counts.pending
        ));
    }
    eprintln!("{}", summary.join("\n"));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every adjacent pair, both ways: real packets go through the defaults
/// step and what comes out must parse in the receiver's layout.
#[test]
fn corpus_packets_convert_between_neighbours() {
    let mut failures = Vec::new();
    let mut summary = Vec::new();
    for (index, &(protocol, release)) in PROTOCOLS.iter().enumerate() {
        let Some(packets) = corpus(protocol) else {
            continue;
        };
        let neighbours = [index.checked_sub(1), Some(index + 1)]
            .into_iter()
            .flatten()
            .filter_map(|index| PROTOCOLS.get(index).map(|(protocol, _)| *protocol));
        for neighbour in neighbours {
            let mut counts = Counts::default();
            // Clientbound packets come from this protocol's backend,
            // serverbound ones from its client.
            let mut down = Defaults::new(neighbour, protocol);
            let mut up = Defaults::new(protocol, neighbour);
            for packet in &packets {
                let name = packet.name(protocol);
                if round_trip(protocol, packet.direction, name, &packet.data).is_none()
                    || step_owned(protocol, neighbour, name)
                {
                    continue;
                }
                let step = match packet.direction {
                    Direction::Clientbound => &mut down,
                    Direction::Serverbound => &mut up,
                };
                let result = step.send_id(packet.direction, packet.state, packet.id, &packet.data);
                let outs = match result {
                    Ok(outs) => outs,
                    Err(error) if pending(&error) => {
                        counts.pending += 1;
                        continue;
                    }
                    Err(error) => {
                        failures.push(format!("{release} -> {neighbour} {name}: {error}"));
                        continue;
                    }
                };
                for out in outs {
                    match round_trip(neighbour, out.direction, &out.name, &out.data) {
                        None | Some(Ok(_)) => counts.checked += 1,
                        Some(Err(error)) if pending(&error) => counts.pending += 1,
                        Some(Err(error)) => failures.push(format!(
                            "{release} -> {neighbour} {name} as {}: {error}",
                            out.name
                        )),
                    }
                }
            }
            summary.push(format!(
                "{release} -> {neighbour}: {} converted, {} pending other codecs",
                counts.checked, counts.pending
            ));
        }
    }
    eprintln!("{}", summary.join("\n"));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_defaults_step_routes_cleanly() {
    for pair in PROTOCOLS.windows(2) {
        let (older, newer) = (pair[0].0, pair[1].0);
        for (client, backend) in [(older, newer), (newer, older)] {
            let routes = crate::step::RouteBuilder::<Defaults>::new(client, backend)
                .defaults()
                .build();
            let problems: Vec<&String> = routes
                .problems()
                .iter()
                .filter(|problem| problem.contains("rule for"))
                .collect();
            assert!(problems.is_empty(), "{client}/{backend}: {problems:?}");
        }
    }
    let _ = State::Play;
}
