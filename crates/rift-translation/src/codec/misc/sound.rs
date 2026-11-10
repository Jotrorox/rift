//! Sound packets: `sound` at a position, `sound_entity` following an
//! entity (1.14+), the named `custom_sound` (until 1.19.2) and `stop_sound`
//! (1.13+).
//!
//! Until 1.19.2 a sound is a `sound_event` registry ID, and a sound outside
//! the registry is sent by name in `custom_sound`. From 1.19.3 both are one
//! holder: the ID plus one, or zero followed by the name and an optional
//! fixed range. 1.19 added a random seed; 1.21.6 added the `ui` category.
//! A sound the receiver's registry lacks is sent by name where the
//! receiver's protocol can (resource packs may still have it) and dropped
//! otherwise; across the 1.13 flattening, where sound names changed
//! wholesale, it is dropped.
use super::registry;
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data::{self, strip},
    step::Cx,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// Sounds carry a seed from 1.19.
const SEEDED: i32 = 759;
/// Sounds are holders from 1.19.3; `custom_sound` is gone.
const HOLDERS: i32 = 761;
/// The `ui` category exists from 1.21.6.
const UI: i32 = 771;

/// Category names of 1.12's `MC|StopSound` payload, by category ID.
const CATEGORIES: [&str; 10] = [
    "master", "music", "record", "weather", "block", "hostile", "neutral", "player", "ambient",
    "voice",
];

/// A sound event: a registry ID, or a name with an optional fixed range.
#[derive(Clone, Debug, PartialEq)]
pub enum Holder {
    Id(i32),
    Named { name: String, range: Option<f32> },
}

/// The fields every sound packet shares besides its holder and target.
#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    pub holder: Holder,
    pub category: i32,
    pub volume: f32,
    pub pitch: f32,
    pub seed: Option<i64>,
}

/// Where a sound plays: a fixed-point position (eighths of a block), or an
/// entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emitter {
    Position([i32; 3]),
    Entity(i32),
}

fn categories(protocol: i32) -> i32 {
    if protocol >= UI { 11 } else { 10 }
}

fn read_category(protocol: i32, reader: &mut Reader<'_>) -> io::Result<i32> {
    let category = reader.varint()?;
    if !(0..categories(protocol)).contains(&category) {
        return Err(invalid("unknown sound category"));
    }
    Ok(category)
}

fn read_holder(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Holder> {
    let id = reader.varint()?;
    if protocol < HOLDERS {
        return Ok(Holder::Id(id));
    }
    if id != 0 {
        return Ok(Holder::Id(id - 1));
    }
    let name = reader.str()?.to_owned();
    let range = if reader.bool()? {
        Some(reader.f32()?)
    } else {
        None
    };
    Ok(Holder::Named { name, range })
}

fn write_holder(protocol: i32, holder: &Holder, writer: &mut Writer) -> io::Result<()> {
    match holder {
        Holder::Id(id) if protocol < HOLDERS => {
            writer.varint(*id);
        }
        Holder::Id(id) => {
            writer.varint(id + 1);
        }
        Holder::Named { .. } if protocol < HOLDERS => {
            return Err(invalid("named sounds need custom_sound before 1.19.3"));
        }
        Holder::Named { name, range } => {
            writer.varint(0).string(name).bool(range.is_some());
            if let Some(range) = range {
                writer.f32(*range);
            }
        }
    }
    Ok(())
}

/// Which sound packet a body belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sound,
    CustomSound,
    SoundEntity,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Sound => "sound",
            Kind::CustomSound => "custom_sound",
            Kind::SoundEntity => "sound_entity",
        }
    }
}

/// Parse a sound packet of `protocol`.
pub fn read(protocol: i32, kind: Kind, data: &[u8]) -> io::Result<(Sound, Emitter)> {
    let mut reader = Reader::new(data);
    let holder = match kind {
        Kind::CustomSound => Holder::Named {
            name: reader.str()?.to_owned(),
            range: None,
        },
        _ => read_holder(protocol, &mut reader)?,
    };
    let category = read_category(protocol, &mut reader)?;
    let emitter = match kind {
        Kind::SoundEntity => Emitter::Entity(reader.varint()?),
        _ => Emitter::Position([reader.i32()?, reader.i32()?, reader.i32()?]),
    };
    let volume = reader.f32()?;
    let pitch = reader.f32()?;
    let seed = if protocol >= SEEDED {
        Some(reader.i64()?)
    } else {
        None
    };
    reader.finish()?;
    Ok((
        Sound {
            holder,
            category,
            volume,
            pitch,
            seed,
        },
        emitter,
    ))
}

/// Serialize a sound in `protocol`, choosing the packet that can carry it:
/// a named sound before 1.19.3 needs `custom_sound`, which cannot follow an
/// entity. `None` when the protocol cannot express it at all.
pub fn write(
    protocol: i32,
    sound: &Sound,
    emitter: Emitter,
) -> io::Result<Option<(Kind, Vec<u8>)>> {
    let mut writer = Writer::new();
    let kind = match (&sound.holder, emitter) {
        (Holder::Named { name, .. }, Emitter::Position(_)) if protocol < HOLDERS => {
            writer.string(name);
            Kind::CustomSound
        }
        (Holder::Named { .. }, Emitter::Entity(_)) if protocol < HOLDERS => return Ok(None),
        (holder, emitter) => {
            write_holder(protocol, holder, &mut writer)?;
            match emitter {
                Emitter::Position(_) => Kind::Sound,
                Emitter::Entity(_) => Kind::SoundEntity,
            }
        }
    };
    if !(0..categories(protocol)).contains(&sound.category) {
        return Err(invalid("sound category out of range"));
    }
    writer.varint(sound.category);
    match emitter {
        Emitter::Position([x, y, z]) => {
            writer.i32(x).i32(y).i32(z);
        }
        Emitter::Entity(entity) => {
            writer.varint(entity);
        }
    }
    writer.f32(sound.volume).f32(sound.pitch);
    if protocol >= SEEDED {
        writer.i64(sound.seed.unwrap_or(0));
    }
    Ok(Some((kind, writer.into_inner())))
}

/// Whether sounds missing from the receiver's registry may go by name: not
/// across the flattening, where every name changed.
fn names_allowed(source: i32, target: i32) -> bool {
    source.min(target) >= 393
}

/// The receiver's name for a registry sound named `name` in the sender's
/// protocol: renamed when the mapping renames it, unchanged otherwise.
fn rename(shared: &mut Shared, direction: Direction, name: &str) -> String {
    let source = shared.source_data(direction).expect_registry("sound_event");
    let target = shared.target_data(direction).expect_registry("sound_event");
    let path = match name.split_once(':') {
        Some(("minecraft", path)) => path,
        Some(_) => return name.to_owned(),
        None => name,
    };
    let mapping = shared.mapping(direction, "sound_event");
    match source
        .id(path)
        .and_then(|id| mapping.get(id))
        .and_then(|id| target.name(id))
    {
        Some(renamed) if renamed != path => format!("minecraft:{renamed}"),
        _ => name.to_owned(),
    }
}

/// Convert a sound's holder to the receiver's terms. `None` drops the packet.
fn convert_holder(
    shared: &mut Shared,
    direction: Direction,
    holder: Holder,
) -> io::Result<Option<Holder>> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    match holder {
        Holder::Id(id) => {
            if let Some(mapped) = shared.mapping(direction, "sound_event").get(id) {
                return Ok(Some(Holder::Id(mapped)));
            }
            let name = shared
                .source_data(direction)
                .expect_registry("sound_event")
                .name(id)
                .ok_or_else(|| invalid("sound event ID out of range"))?;
            Ok(names_allowed(source, target).then(|| Holder::Named {
                name: format!("minecraft:{name}"),
                range: None,
            }))
        }
        Holder::Named { name, range } => Ok(Some(Holder::Named {
            name: rename(shared, direction, &name),
            range,
        })),
    }
}

/// A named sound as a registry ID of the receiver, where it has one.
fn resolve(shared: &Shared, direction: Direction, name: &str) -> Option<i32> {
    let path = match name.split_once(':') {
        Some(("minecraft", path)) => path,
        Some(_) => return None,
        None => name,
    };
    shared
        .target_data(direction)
        .expect_registry("sound_event")
        .id(path)
}

fn convert_category(target: i32, category: i32) -> i32 {
    // Older clients play their interface sounds in the master category.
    if category >= categories(target) {
        0
    } else {
        category
    }
}

/// Convert one sound packet; `None` when the receiver cannot play it.
pub fn convert(
    shared: &mut Shared,
    direction: Direction,
    kind: Kind,
    data: &[u8],
) -> io::Result<Option<(Kind, Vec<u8>)>> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let (mut sound, emitter) = read(source, kind, data)?;
    let Some(holder) = convert_holder(shared, direction, sound.holder)? else {
        return Ok(None);
    };
    sound.holder = match holder {
        // Before 1.19.3 only `custom_sound` carries names; an entity's sound
        // needs the receiver's registry.
        Holder::Named { name, .. } if target < HOLDERS && matches!(emitter, Emitter::Entity(_)) => {
            match resolve(shared, direction, &name) {
                Some(id) => Holder::Id(id),
                None => return Ok(None),
            }
        }
        holder => holder,
    };
    sound.category = convert_category(target, sound.category);
    if target >= SEEDED && sound.seed.is_none() {
        sound.seed = Some(shared.misc.next_seed());
    }
    write(target, &sound, emitter)
}

fn handle<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
    kind: Kind,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let Some((kind, body)) = convert(shared, direction, kind, &packet.data)? else {
        return Ok(());
    };
    let id = shared
        .target_data(direction)
        .packet_id(state, direction, kind.name())
        .ok_or_else(|| invalid("the receiver has no packet for this sound"))?;
    cx.to_client(state, Packet::new(id, body));
    Ok(())
}

pub fn sound<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    handle(step, cx, state, packet, Kind::Sound)
}

pub fn custom_sound<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    handle(step, cx, state, packet, Kind::CustomSound)
}

pub fn sound_entity<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    if step.shared().client < 477 {
        // INTEGRATION: 1.13 has no entity sounds. With the entity codec's
        // tracked positions this could become a `sound` at the entity.
        return Ok(());
    }
    handle(step, cx, state, packet, Kind::SoundEntity)
}

/// The contents of a `stop_sound` packet (1.13+).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopSound {
    pub category: Option<i32>,
    pub name: Option<String>,
}

pub fn read_stop(protocol: i32, data: &[u8]) -> io::Result<StopSound> {
    let mut reader = Reader::new(data);
    let flags = reader.u8()?;
    if flags & !3 != 0 {
        return Err(invalid("unknown stop_sound flags"));
    }
    let category = if flags & 1 != 0 {
        Some(read_category(protocol, &mut reader)?)
    } else {
        None
    };
    let name = if flags & 2 != 0 {
        Some(reader.str()?.to_owned())
    } else {
        None
    };
    reader.finish()?;
    Ok(StopSound { category, name })
}

pub fn write_stop(stop: &StopSound) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.u8(u8::from(stop.category.is_some()) | u8::from(stop.name.is_some()) << 1);
    if let Some(category) = stop.category {
        writer.varint(category);
    }
    if let Some(name) = &stop.name {
        writer.string(name);
    }
    writer.into_inner()
}

/// 1.12 stops sounds with the `MC|StopSound` plugin message: a category name
/// and a sound name, either empty for "any".
fn legacy_stop(stop: &StopSound) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.string("MC|StopSound");
    writer.string(
        stop.category
            .map_or("", |category| CATEGORIES[category as usize]),
    );
    writer.string(stop.name.as_deref().map_or("", strip));
    writer.into_inner()
}

/// The `stop_sound` body for 1.13 equivalent to the data of a 1.12
/// `MC|StopSound` plugin message (after the channel name), for steps that
/// translate plugin messages.
pub fn stop_sound_from_legacy(
    shared: &mut Shared,
    direction: Direction,
    data: &[u8],
) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(data);
    let category = reader.str()?;
    let name = reader.string(256)?;
    reader.finish()?;
    let category = match category {
        "" => None,
        name => Some(
            CATEGORIES
                .iter()
                .position(|known| *known == name)
                .ok_or_else(|| invalid("unknown sound category"))? as i32,
        ),
    };
    let name = (!name.is_empty()).then(|| {
        let name = if name.contains(':') {
            name.to_owned()
        } else {
            format!("minecraft:{name}")
        };
        rename(shared, direction, &name)
    });
    Ok(write_stop(&StopSound { category, name }))
}

pub fn stop_sound<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut stop = read_stop(source, &packet.data)?;
    if let Some(name) = &stop.name {
        stop.name = Some(rename(shared, direction, name));
    }
    stop.category = stop
        .category
        .map(|category| convert_category(target, category));
    if target < 393 {
        let id = shared
            .target_data(direction)
            .expect_packet(state, direction, "custom_payload");
        cx.to_client(state, Packet::new(id, legacy_stop(&stop)));
    } else {
        cx.to_client(state, Packet::new(packet.id, write_stop(&stop)));
    }
    Ok(())
}

/// Whether a registry rename rule changes any sound's name between the two.
fn renames(source: i32, target: i32) -> bool {
    let (from, to) = (
        data::version(source).expect_registry("sound_event"),
        data::version(target).expect_registry("sound_event"),
    );
    let mapping = crate::mapping::mapping(source, target, "sound_event");
    from.names().any(|(id, name)| {
        mapping
            .get(id)
            .and_then(|id| to.name(id))
            .is_some_and(|other| other != name)
    })
}

fn layout(protocol: i32) -> (bool, bool, bool) {
    (protocol >= SEEDED, protocol >= HOLDERS, protocol >= UI)
}

/// Whether `kind` (in `source`'s protocol) needs converting for `target`.
pub fn needed(kind: Kind, source: i32, target: i32) -> bool {
    let ids = !registry::unchanged(source, target, "sound_event");
    match kind {
        Kind::Sound => ids || layout(source) != layout(target),
        // 1.13 has no entity sounds.
        Kind::SoundEntity => ids || layout(source) != layout(target) || target < 477,
        Kind::CustomSound => {
            target >= HOLDERS || layout(source) != layout(target) || renames(source, target)
        }
    }
}

/// Whether `stop_sound` needs converting from `source` to `target`.
pub fn stop_needed(source: i32, target: i32) -> bool {
    target < 393 || (source >= UI) != (target >= UI) || renames(source, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::testing::Defaults;

    fn body(protocol: i32, sound: &Sound, emitter: Emitter) -> Vec<u8> {
        write(protocol, sound, emitter).unwrap().unwrap().1
    }

    fn named(name: &str) -> Holder {
        Holder::Named {
            name: name.into(),
            range: None,
        }
    }

    fn sound(holder: Holder, seed: Option<i64>) -> Sound {
        Sound {
            holder,
            category: 4,
            volume: 1.0,
            pitch: 0.5,
            seed,
        }
    }

    fn registry_id(protocol: i32, name: &str) -> i32 {
        data::version(protocol)
            .expect_registry("sound_event")
            .id(name)
            .unwrap()
    }

    #[test]
    fn layouts_round_trip_across_their_boundaries() {
        let at = Emitter::Position([8, -16, 24]);
        for (protocol, seed) in [
            (340, None),
            (758, None),
            (759, Some(7)),
            (761, Some(-1)),
            (777, Some(3)),
        ] {
            let plain = sound(Holder::Id(5), seed);
            let bytes = body(protocol, &plain, at);
            assert_eq!(
                read(protocol, Kind::Sound, &bytes).unwrap(),
                (plain.clone(), at)
            );
            let custom = sound(named("custom:thing"), seed);
            let (kind, bytes) = write(protocol, &custom, at).unwrap().unwrap();
            let expected = if protocol < HOLDERS {
                Kind::CustomSound
            } else {
                Kind::Sound
            };
            assert_eq!(kind, expected);
            assert_eq!(read(protocol, kind, &bytes).unwrap().0, custom);
        }
        let ranged = sound(
            Holder::Named {
                name: "minecraft:x".into(),
                range: Some(16.0),
            },
            Some(1),
        );
        let bytes = body(761, &ranged, Emitter::Entity(9));
        assert_eq!(
            read(761, Kind::SoundEntity, &bytes).unwrap(),
            (ranged, Emitter::Entity(9))
        );
        // An entity cannot play a named sound before 1.19.3.
        assert_eq!(
            write(760, &sound(named("a"), Some(0)), Emitter::Entity(1)).unwrap(),
            None
        );
        // Strictness: unknown categories, trailing bytes, short input.
        let mut bad = body(770, &sound(Holder::Id(1), Some(0)), at);
        bad[1] = 10;
        assert!(read(770, Kind::Sound, &bad).is_err());
        assert!(read(771, Kind::Sound, &bad).is_ok());
        let mut long = body(340, &sound(Holder::Id(1), None), at);
        long.push(0);
        assert!(read(340, Kind::Sound, &long).is_err());
        assert!(read(340, Kind::Sound, &long[..5]).is_err());
    }

    #[test]
    fn sounds_cross_the_holder_and_seed_boundaries() {
        let at = Emitter::Position([1, 2, 3]);
        // 1.19.2 named sound up to 1.19.3: an inline holder in `sound`.
        let mut up = Defaults::new(761, 760);
        let custom = body(760, &sound(named("minecraft:custom.horn"), Some(4)), at);
        let out = up
            .clientbound(State::Play, "custom_sound", &custom)
            .unwrap();
        assert_eq!(out[0].name, "sound");
        assert_eq!(
            read(761, Kind::Sound, &out[0].data).unwrap().0.holder,
            named("minecraft:custom.horn")
        );
        // And back down: `custom_sound` again.
        let mut down = Defaults::new(760, 761);
        let out = down
            .clientbound(State::Play, "sound", &out[0].data)
            .unwrap();
        assert_eq!(out[0].name, "custom_sound");
        assert_eq!(out[0].data, custom);
        // A registry sound keeps its meaning; 1.19 adds a seed.
        let id = registry_id(758, "entity.pig.ambient");
        let mut up = Defaults::new(759, 758);
        let out = up
            .clientbound(
                State::Play,
                "sound",
                &body(758, &sound(Holder::Id(id), None), at),
            )
            .unwrap();
        let (converted, _) = read(759, Kind::Sound, &out[0].data).unwrap();
        assert_eq!(
            converted.holder,
            Holder::Id(registry_id(759, "entity.pig.ambient"))
        );
        assert!(converted.seed.is_some());
    }

    #[test]
    fn missing_sounds_go_by_name_or_are_dropped() {
        let at = Emitter::Position([0, 0, 0]);
        // 1.21.7 inserted a sound event 1.21.6 lacks.
        let older = data::version(771).expect_registry("sound_event");
        let newer = data::version(772).expect_registry("sound_event");
        let (id, name) = newer
            .names()
            .find(|(_, name)| older.id(name).is_none())
            .unwrap();
        let mut down = Defaults::new(771, 772);
        let out = down
            .clientbound(
                State::Play,
                "sound",
                &body(772, &sound(Holder::Id(id), Some(0)), at),
            )
            .unwrap();
        assert_eq!(
            read(771, Kind::Sound, &out[0].data).unwrap().0.holder,
            named(&format!("minecraft:{name}"))
        );
        let out = down
            .clientbound(
                State::Play,
                "sound_entity",
                &body(772, &sound(Holder::Id(id), Some(0)), Emitter::Entity(5)),
            )
            .unwrap();
        assert_eq!(
            read(771, Kind::SoundEntity, &out[0].data).unwrap().0.holder,
            named(&format!("minecraft:{name}"))
        );
        // Before 1.19.3 an entity's unknown sound cannot be sent at all.
        let older = data::version(759).expect_registry("sound_event");
        let newer = data::version(760).expect_registry("sound_event");
        if let Some((id, _)) = newer.names().find(|(_, name)| older.id(name).is_none()) {
            let mut down = Defaults::new(759, 760);
            let out = down
                .clientbound(
                    State::Play,
                    "sound_entity",
                    &body(760, &sound(Holder::Id(id), Some(0)), Emitter::Entity(5)),
                )
                .unwrap();
            assert!(out.is_empty());
        }
        // The ui category becomes master for older clients.
        let mut down = Defaults::new(770, 771);
        let mut ui = sound(Holder::Id(1), Some(0));
        ui.category = 10;
        let out = down
            .clientbound(State::Play, "sound", &body(771, &ui, at))
            .unwrap();
        assert_eq!(read(770, Kind::Sound, &out[0].data).unwrap().0.category, 0);
        // A sound ID the sender's registry lacks is an error.
        let mut step = Defaults::new(761, 760);
        assert!(
            step.clientbound(
                State::Play,
                "sound",
                &body(760, &sound(Holder::Id(100_000), Some(0)), at)
            )
            .is_err()
        );
    }

    #[test]
    fn stop_sound_round_trips_and_becomes_a_plugin_message_for_1_12() {
        for stop in [
            StopSound {
                category: None,
                name: None,
            },
            StopSound {
                category: Some(3),
                name: None,
            },
            StopSound {
                category: Some(0),
                name: Some("minecraft:music.game".into()),
            },
        ] {
            assert_eq!(read_stop(393, &write_stop(&stop)).unwrap(), stop);
        }
        assert!(read_stop(393, &[4]).is_err());
        assert!(read_stop(393, &[1]).is_err());
        let mut down = Defaults::new(340, 393);
        let stop = StopSound {
            category: Some(1),
            name: Some("minecraft:music.game".into()),
        };
        let out = down
            .clientbound(State::Play, "stop_sound", &write_stop(&stop))
            .unwrap();
        assert_eq!(out[0].name, "custom_payload");
        let mut reader = Reader::new(&out[0].data);
        assert_eq!(reader.str().unwrap(), "MC|StopSound");
        let payload = reader.peek();
        assert_eq!(reader.str().unwrap(), "music");
        assert_eq!(reader.str().unwrap(), "music.game");
        let mut shared = Shared::new(393, 340);
        let back = stop_sound_from_legacy(&mut shared, Direction::Clientbound, payload).unwrap();
        assert_eq!(
            read_stop(393, &back).unwrap(),
            StopSound {
                category: Some(1),
                name: Some("minecraft:music.game".into())
            }
        );
    }

    #[test]
    fn only_pairs_that_differ_convert_sounds() {
        assert!(!needed(Kind::Sound, 754, 753));
        assert!(needed(Kind::Sound, 758, 759));
        assert!(needed(Kind::CustomSound, 760, 761));
        assert!(!needed(Kind::CustomSound, 759, 760) || renames(759, 760));
        assert!(stop_needed(393, 340));
        assert!(stop_needed(771, 770));
    }

    /// Every sound packet of every recorded session parses and re-serialises
    /// to the same bytes, and converts to both neighbours into packets the
    /// neighbour's reader accepts, naming the same sound.
    #[test]
    fn corpus_sounds_round_trip_and_convert() {
        use crate::version::PROTOCOLS;
        let (mut checked, mut converted) = (0, 0);
        for (index, &(protocol, _)) in PROTOCOLS.iter().enumerate() {
            let packets: Vec<_> = crate::codec::misc::sessions(protocol)
                .into_iter()
                .flatten()
                .collect();
            for packet in &packets {
                let kind = match packet.name(protocol) {
                    "sound" => Kind::Sound,
                    "custom_sound" => Kind::CustomSound,
                    "sound_entity" => Kind::SoundEntity,
                    "stop_sound" => {
                        let stop = read_stop(protocol, &packet.data).unwrap();
                        assert_eq!(write_stop(&stop), packet.data);
                        checked += 1;
                        continue;
                    }
                    _ => continue,
                };
                let (sound, emitter) = read(protocol, kind, &packet.data).unwrap();
                assert_eq!(
                    write(protocol, &sound, emitter).unwrap().unwrap(),
                    (kind, packet.data.clone())
                );
                checked += 1;
                let name = |protocol: i32, holder: &Holder| match holder {
                    Holder::Id(id) => data::version(protocol)
                        .expect_registry("sound_event")
                        .name(*id)
                        .map(|name| format!("minecraft:{name}")),
                    Holder::Named { name, .. } => Some(name.clone()),
                };
                let original = name(protocol, &sound.holder);
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
                    for out in out {
                        let kind = match out.name.as_str() {
                            "sound" => Kind::Sound,
                            "custom_sound" => Kind::CustomSound,
                            _ => Kind::SoundEntity,
                        };
                        let (sound, _) = read(other, kind, &out.data).unwrap();
                        // A sound the receiver lacks arrives as the rules'
                        // stand-in for it.
                        let kept = original.as_deref().is_some_and(|original| {
                            data::version(other)
                                .expect_registry("sound_event")
                                .id(crate::data::strip(original))
                                .is_some()
                        });
                        if protocol.min(other) >= 393 && kept {
                            assert_eq!(
                                name(other, &sound.holder),
                                original,
                                "{protocol} -> {other}"
                            );
                        }
                        converted += 1;
                    }
                }
            }
        }
        eprintln!(
            "{checked} recorded sound packets round-tripped, {converted} conversions checked"
        );
    }
}
