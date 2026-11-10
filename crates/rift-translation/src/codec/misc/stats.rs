//! `award_stats`: the player's statistics, sent when the statistics screen
//! asks for them.
//!
//! 1.12 names each statistic with a string (`stat.mineBlock.minecraft.stone`,
//! `stat.killEntity.Zombie`, `stat.walkOneCm`); from 1.13 a statistic is a
//! `stat_type` ID and an ID in that type's registry (block, item, entity
//! type or custom statistic). Both IDs are mapped; a statistic the receiver
//! cannot name is dropped.
use super::registry;
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data,
    step::Cx,
    wire::{Reader, Writer, invalid},
};
use serde_json::Value;
use std::io;

/// One statistic and its value, in the sender's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stat {
    /// 1.12: the statistic's name.
    Named(String, i32),
    /// 1.13+: `stat_type` ID, ID in that type's registry, value.
    Typed(i32, i32, i32),
}

pub fn read(protocol: i32, data: &[u8]) -> io::Result<Vec<Stat>> {
    let mut reader = Reader::new(data);
    let count = reader.count()?;
    let mut stats = Vec::with_capacity(count);
    for _ in 0..count {
        stats.push(if protocol < 393 {
            Stat::Named(reader.str()?.to_owned(), reader.varint()?)
        } else {
            Stat::Typed(reader.varint()?, reader.varint()?, reader.varint()?)
        });
    }
    reader.finish()?;
    Ok(stats)
}

pub fn write(protocol: i32, stats: &[Stat]) -> io::Result<Vec<u8>> {
    let mut writer = Writer::new();
    writer.varint(stats.len() as i32);
    for stat in stats {
        match (stat, protocol < 393) {
            (Stat::Named(name, value), true) => {
                writer.string(name).varint(*value);
            }
            (Stat::Typed(kind, id, value), false) => {
                writer.varint(*kind).varint(*id).varint(*value);
            }
            _ => return Err(invalid("statistic in the wrong form for the protocol")),
        }
    }
    Ok(writer.into_inner())
}

/// The registry a statistic type's values come from.
fn value_registry(stat_type: &str) -> Option<&'static str> {
    Some(match stat_type {
        "mined" => "block",
        "crafted" | "used" | "broken" | "picked_up" | "dropped" => "item",
        "killed" | "killed_by" => "entity_type",
        "custom" => "custom_stat",
        _ => return None,
    })
}

/// 1.12's prefixes for the statistics that name a block, item or entity,
/// with the 1.13 statistic type each became.
const LEGACY_PREFIXES: &[(&str, &str)] = &[
    ("stat.mineBlock.", "mined"),
    ("stat.craftItem.", "crafted"),
    ("stat.useItem.", "used"),
    ("stat.breakItem.", "broken"),
    ("stat.pickup.", "picked_up"),
    ("stat.drop.", "dropped"),
    ("stat.killEntity.", "killed"),
    ("stat.entityKilledBy.", "killed_by"),
];

/// 1.12's custom statistics (without `stat.`) and their 1.13 names.
const LEGACY_CUSTOM: &[(&str, &str)] = &[
    ("leaveGame", "leave_game"),
    ("playOneMinute", "play_one_minute"),
    ("timeSinceDeath", "time_since_death"),
    ("sneakTime", "sneak_time"),
    ("walkOneCm", "walk_one_cm"),
    ("crouchOneCm", "crouch_one_cm"),
    ("sprintOneCm", "sprint_one_cm"),
    ("swimOneCm", "swim_one_cm"),
    ("fallOneCm", "fall_one_cm"),
    ("climbOneCm", "climb_one_cm"),
    ("flyOneCm", "fly_one_cm"),
    ("diveOneCm", "walk_under_water_one_cm"),
    ("minecartOneCm", "minecart_one_cm"),
    ("boatOneCm", "boat_one_cm"),
    ("pigOneCm", "pig_one_cm"),
    ("horseOneCm", "horse_one_cm"),
    ("aviateOneCm", "aviate_one_cm"),
    ("jump", "jump"),
    ("drop", "drop"),
    ("damageDealt", "damage_dealt"),
    ("damageTaken", "damage_taken"),
    ("deaths", "deaths"),
    ("mobKills", "mob_kills"),
    ("animalsBred", "animals_bred"),
    ("playerKills", "player_kills"),
    ("fishCaught", "fish_caught"),
    ("talkedToVillager", "talked_to_villager"),
    ("tradedWithVillager", "traded_with_villager"),
    ("cakeSlicesEaten", "eat_cake_slice"),
    ("cauldronFilled", "fill_cauldron"),
    ("cauldronUsed", "use_cauldron"),
    ("armorCleaned", "clean_armor"),
    ("bannerCleaned", "clean_banner"),
    ("brewingstandInteraction", "interact_with_brewingstand"),
    ("beaconInteraction", "interact_with_beacon"),
    ("dropperInspected", "inspect_dropper"),
    ("hopperInspected", "inspect_hopper"),
    ("dispenserInspected", "inspect_dispenser"),
    ("noteblockPlayed", "play_noteblock"),
    ("noteblockTuned", "tune_noteblock"),
    ("flowerPotted", "pot_flower"),
    ("trappedChestTriggered", "trigger_trapped_chest"),
    ("enderchestOpened", "open_enderchest"),
    ("itemEnchanted", "enchant_item"),
    ("recordPlayed", "play_record"),
    ("furnaceInteraction", "interact_with_furnace"),
    ("craftingTableInteraction", "interact_with_crafting_table"),
    ("chestOpened", "open_chest"),
    ("sleepInBed", "sleep_in_bed"),
    ("shulkerBoxOpened", "open_shulker_box"),
];

/// 1.12 names entities in statistics by their legacy names (`PigZombie`).
fn legacy_entities() -> &'static serde_json::Map<String, Value> {
    data::file("tags/misc")
        .and_then(|misc| misc["legacy_entity"].as_object())
        .expect("data/tags/misc.json lists 1.12 entity names")
}

/// A 1.12 statistic name as a 1.13 statistic type and value name.
fn parse_legacy(name: &str) -> Option<(&'static str, String)> {
    let rest = name.strip_prefix("stat.")?;
    if let Some((_, modern)) = LEGACY_CUSTOM.iter().find(|(legacy, _)| *legacy == rest) {
        return Some(("custom", (*modern).to_owned()));
    }
    let (prefix, kind) = LEGACY_PREFIXES
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix))?;
    let value = &name[prefix.len()..];
    if value_registry(kind) == Some("entity_type") {
        let (entity, _) = legacy_entities()
            .iter()
            .find(|(_, legacy)| legacy.as_str() == Some(value))?;
        return Some((kind, entity.clone()));
    }
    // Items and blocks are `namespace.path`.
    let path = value.strip_prefix("minecraft.")?;
    Some((kind, path.to_owned()))
}

/// The 1.12 name of a statistic type and value name.
fn legacy_name(kind: &str, value: &str) -> Option<String> {
    if kind == "custom" {
        let (legacy, _) = LEGACY_CUSTOM.iter().find(|(_, modern)| *modern == value)?;
        return Some(format!("stat.{legacy}"));
    }
    let (prefix, _) = LEGACY_PREFIXES.iter().find(|(_, known)| *known == kind)?;
    if kind == "killed" || kind == "killed_by" {
        let legacy = legacy_entities().get(value)?.as_str()?;
        return Some(format!("{prefix}{legacy}"));
    }
    Some(format!("{prefix}minecraft.{value}"))
}

/// Convert one statistic; `None` when the receiver cannot name it.
fn convert_one(shared: &mut Shared, direction: Direction, stat: &Stat) -> io::Result<Option<Stat>> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    // Name the statistic in the sender's protocol: type name, registry, ID.
    let (kind, registry, id, value) = match stat {
        Stat::Named(name, value) => {
            let Some((kind, entry)) = parse_legacy(name) else {
                return Ok(None);
            };
            let registry = value_registry(kind).expect("known type");
            if target < 393 {
                // Both sides name statistics: pass on what the receiver knows.
                return Ok(Some(stat.clone()));
            }
            let id = if registry == "custom_stat" {
                // 1.12 has no custom statistic registry: name it in the receiver's.
                let Some((_, stats)) = registry::registry(target, registry) else {
                    return Ok(None);
                };
                let Some(id) = stats.id(&entry) else {
                    return Ok(None);
                };
                return Ok(Some(Stat::Typed(stat_type_id(target, kind)?, id, *value)));
            } else {
                match data::version(source).expect_registry(registry).id(&entry) {
                    Some(id) => id,
                    None => return Ok(None),
                }
            };
            (kind.to_owned(), registry, id, *value)
        }
        Stat::Typed(kind, id, value) => {
            let (_, types) = registry::registry(source, "stat_type")
                .ok_or_else(|| invalid("no statistic types"))?;
            let kind = types
                .name(*kind)
                .ok_or_else(|| invalid("unknown statistic type"))?;
            let registry = value_registry(kind).ok_or_else(|| invalid("unknown statistic type"))?;
            (kind.to_owned(), registry, *id, *value)
        }
    };
    if target < 393 {
        // A 1.13 statistic for 1.12: name the mapped entry.
        let entry = if registry == "custom_stat" {
            let (_, stats) = registry::registry(source, registry)
                .ok_or_else(|| invalid("no custom statistics"))?;
            stats
                .name(id)
                .ok_or_else(|| invalid("unknown custom statistic"))?
                .to_owned()
        } else {
            let Some(mapped) = shared.mapping(direction, registry).get(id) else {
                return Ok(None);
            };
            match data::version(target).expect_registry(registry).name(mapped) {
                Some(name) => name.to_owned(),
                None => return Ok(None),
            }
        };
        return Ok(legacy_name(&kind, &entry).map(|name| Stat::Named(name, value)));
    }
    let Some(mapping) = registry::mapping(shared, direction, registry) else {
        return Ok(None);
    };
    let Some(mapped) = mapping.get(id) else {
        return Ok(None);
    };
    Ok(Some(Stat::Typed(
        stat_type_id(target, &kind)?,
        mapped,
        value,
    )))
}

fn stat_type_id(protocol: i32, kind: &str) -> io::Result<i32> {
    registry::registry(protocol, "stat_type")
        .and_then(|(_, types)| types.id(kind))
        .ok_or_else(|| invalid("the receiver lacks a statistic type"))
}

/// Convert a statistics list from the sender's protocol to the receiver's.
pub fn convert(shared: &mut Shared, direction: Direction, stats: &[Stat]) -> io::Result<Vec<Stat>> {
    let mut converted = Vec::with_capacity(stats.len());
    for stat in stats {
        if let Some(stat) = convert_one(shared, direction, stat)? {
            converted.push(stat);
        }
    }
    Ok(converted)
}

pub fn award_stats<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let stats = read(shared.source(direction), &packet.data)?;
    let stats = convert(shared, direction, &stats)?;
    let body = write(shared.target(direction), &stats)?;
    cx.to_client(state, Packet::new(packet.id, body));
    Ok(())
}

/// Whether statistics need converting from `source` to `target`.
pub fn needed(source: i32, target: i32) -> bool {
    if source < 393 || target < 393 {
        return (source < 393) != (target < 393);
    }
    ["stat_type", "block", "item", "entity_type", "custom_stat"]
        .iter()
        .any(|registry| !registry::unchanged(source, target, registry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::testing::Defaults;

    fn id(protocol: i32, registry: &str, name: &str) -> i32 {
        registry::registry(protocol, registry)
            .unwrap()
            .1
            .id(name)
            .unwrap()
    }

    #[test]
    fn layouts_round_trip_and_reject_malformed_input() {
        let named = vec![Stat::Named("stat.jump".into(), 4)];
        assert_eq!(read(340, &write(340, &named).unwrap()).unwrap(), named);
        let typed = vec![Stat::Typed(8, 3, 100), Stat::Typed(0, 1, -1)];
        assert_eq!(read(393, &write(393, &typed).unwrap()).unwrap(), typed);
        assert!(write(393, &named).is_err());
        assert!(read(393, &[2, 0, 0]).is_err());
        assert!(read(393, &[0, 0]).is_err());
    }

    #[test]
    fn legacy_names_cover_1_12_and_name_1_13_statistics() {
        let misc = data::file("tags/misc").unwrap();
        let custom = registry::registry(393, "custom_stat").unwrap().1;
        for name in misc["legacy_custom_stat"].as_array().unwrap() {
            let (kind, value) = parse_legacy(name.as_str().unwrap()).unwrap();
            assert_eq!(kind, "custom");
            assert!(custom.id(&value).is_some(), "{value}");
            assert_eq!(legacy_name(kind, &value).as_deref(), name.as_str());
        }
        assert_eq!(
            parse_legacy("stat.killEntity.PigZombie"),
            Some(("killed", "zombie_pigman".into()))
        );
        assert_eq!(
            parse_legacy("stat.mineBlock.minecraft.stone"),
            Some(("mined", "stone".into()))
        );
        assert_eq!(parse_legacy("achievement.openInventory"), None);
    }

    #[test]
    fn statistics_cross_the_flattening_both_ways() {
        let legacy = vec![
            Stat::Named("stat.mineBlock.minecraft.stone".into(), 3),
            Stat::Named("stat.walkOneCm".into(), 1000),
            Stat::Named("stat.killEntity.Zombie".into(), 2),
            Stat::Named("stat.useItem.minecraft.no_such_item".into(), 1),
        ];
        let mut up = Defaults::new(393, 340);
        let out = up
            .clientbound(State::Play, "award_stats", &write(340, &legacy).unwrap())
            .unwrap();
        let stats = read(393, &out[0].data).unwrap();
        let mined = id(393, "stat_type", "mined");
        let custom = id(393, "stat_type", "custom");
        let killed = id(393, "stat_type", "killed");
        assert_eq!(
            stats,
            vec![
                Stat::Typed(mined, id(393, "block", "stone"), 3),
                Stat::Typed(custom, id(393, "custom_stat", "walk_one_cm"), 1000),
                Stat::Typed(killed, id(393, "entity_type", "zombie"), 2),
            ]
        );
        let mut down = Defaults::new(340, 393);
        let out = down
            .clientbound(State::Play, "award_stats", &out[0].data)
            .unwrap();
        assert_eq!(read(340, &out[0].data).unwrap(), legacy[..3].to_vec());
    }

    #[test]
    fn modern_statistics_map_their_registries() {
        // 1.14 added custom statistics 1.13.2 lacks.
        let custom = id(477, "stat_type", "custom");
        let stats = vec![
            Stat::Typed(custom, id(477, "custom_stat", "jump"), 7),
            Stat::Typed(custom, id(477, "custom_stat", "bell_ring"), 1),
        ];
        let mut down = Defaults::new(404, 477);
        let out = down
            .clientbound(State::Play, "award_stats", &write(477, &stats).unwrap())
            .unwrap();
        assert_eq!(
            read(404, &out[0].data).unwrap(),
            vec![Stat::Typed(
                id(404, "stat_type", "custom"),
                id(404, "custom_stat", "jump"),
                7
            )]
        );
        let mut step = Defaults::new(404, 477);
        assert!(
            step.clientbound(
                State::Play,
                "award_stats",
                &write(477, &[Stat::Typed(99, 0, 0)]).unwrap()
            )
            .is_err()
        );
        assert!(!needed(498, 490));
        assert!(needed(340, 393));
    }

    #[test]
    fn corpus_statistics_round_trip_and_convert() {
        use crate::version::PROTOCOLS;
        let (mut checked, mut converted) = (0, 0);
        for (index, &(protocol, _)) in PROTOCOLS.iter().enumerate() {
            let packets: Vec<_> = crate::codec::misc::sessions(protocol)
                .into_iter()
                .flatten()
                .collect();
            for packet in packets
                .iter()
                .filter(|packet| packet.name(protocol) == "award_stats")
            {
                let stats = read(protocol, &packet.data).unwrap();
                assert_eq!(write(protocol, &stats).unwrap(), packet.data);
                checked += 1;
                for neighbour in [index.checked_sub(1), Some(index + 1)]
                    .into_iter()
                    .flatten()
                {
                    let Some(&(other, _)) = PROTOCOLS.get(neighbour) else {
                        continue;
                    };
                    let mut step = Defaults::new(other, protocol);
                    let out = step
                        .send_id(
                            Direction::Clientbound,
                            packet.state,
                            packet.id,
                            &packet.data,
                        )
                        .unwrap();
                    let back = read(other, &out[0].data).unwrap();
                    assert!(back.len() <= stats.len());
                    converted += 1;
                }
            }
        }
        eprintln!("{checked} recorded award_stats packets round-tripped, {converted} converted");
    }
}
