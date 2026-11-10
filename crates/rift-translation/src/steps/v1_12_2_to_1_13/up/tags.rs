//! The vanilla 1.13 tags, which 1.13 clients rely on (water and lava
//! physics, tool speeds, ...) and 1.12 servers never send. `tags.json` is
//! generated from the 1.13 datapack by `generate.py`.
use crate::{data, wire::Writer};
use serde::Deserialize;
use std::{collections::BTreeMap, io};

/// 1.13's fluids in registry order; it has no fluid registry report.
const FLUIDS: &[&str] = &["empty", "flowing_water", "water", "flowing_lava", "lava"];

#[derive(Deserialize)]
struct Tags {
    blocks: BTreeMap<String, Vec<String>>,
    items: BTreeMap<String, Vec<String>>,
    fluids: BTreeMap<String, Vec<String>>,
}

/// The body of the 1.13 `update_tags` packet.
// INTEGRATION: the misc codec plans the complete vanilla tag set of every
// protocol; this is the same set for 1.13.
pub fn packet() -> io::Result<Vec<u8>> {
    let tags: Tags = serde_json::from_str(include_str!("tags.json"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let version = data::version(super::super::NEWER);
    let (blocks, items) = (
        version.expect_registry("block"),
        version.expect_registry("item"),
    );
    let mut writer = Writer::with_capacity(2400);
    let mut section = |tags: &BTreeMap<String, Vec<String>>, id: &dyn Fn(&str) -> Option<i32>| {
        writer.varint(tags.len() as i32);
        for (name, entries) in tags {
            let ids: Vec<i32> = entries.iter().filter_map(|entry| id(entry)).collect();
            writer.string(&format!("minecraft:{name}"));
            writer.varint(ids.len() as i32);
            for id in ids {
                writer.varint(id);
            }
        }
    };
    section(&tags.blocks, &|name| blocks.id(name));
    section(&tags.items, &|name| items.id(name));
    section(&tags.fluids, &|name| {
        FLUIDS
            .iter()
            .position(|fluid| *fluid == name)
            .map(|id| id as i32)
    });
    Ok(writer.buf)
}
