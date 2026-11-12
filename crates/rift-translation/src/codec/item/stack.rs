//! The wire layouts of one item stack:
//!
//! - 1.12.x: short ID (negative is empty), byte count, short damage, named
//!   NBT or TAG_End;
//! - 1.13 and 1.13.1: short ID (-1 is empty), byte count, named NBT;
//! - 1.13.2 to 1.20.4: present flag, VarInt ID, byte count, NBT (nameless
//!   root from 1.20.2);
//! - 1.20.5 on: VarInt count (0 is empty), VarInt ID, data component patch.
use super::{ItemStack, components};
use crate::{
    nbt::Nbt,
    version,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// The VarInt-ID present-flag layout starts with 1.13.2.
pub(super) fn present_flag(protocol: i32) -> bool {
    protocol >= 404
}

pub(super) fn read(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Option<ItemStack>> {
    if version::item_components(protocol) {
        let count = reader.varint()?;
        if count == 0 {
            return Ok(None);
        }
        if count < 0 {
            return Err(invalid("negative item count"));
        }
        let id = reader.varint()?;
        if id < 0 {
            return Err(invalid("negative item ID"));
        }
        let components = components::read(protocol, reader)?;
        return Ok(Some(ItemStack {
            id,
            count,
            damage: 0,
            tag: None,
            components: Some(components),
        }));
    }
    let id = if present_flag(protocol) {
        if !reader.bool()? {
            return Ok(None);
        }
        let id = reader.varint()?;
        if id < 0 {
            return Err(invalid("negative item ID"));
        }
        id
    } else {
        // Vanilla writes -1 for the empty stack and reads any negative
        // value as one; only -1 survives a round trip.
        match reader.i16()? {
            -1 => return Ok(None),
            id if id < 0 => return Err(invalid("negative item ID")),
            id => i32::from(id),
        }
    };
    let count = i32::from(reader.i8()?);
    let damage = if version::flattened(protocol) {
        0
    } else {
        reader.i16()?
    };
    let tag = Nbt::read(reader, !version::nameless_nbt(protocol))?;
    Ok(Some(ItemStack {
        id,
        count,
        damage,
        tag,
        components: None,
    }))
}

pub(super) fn write(
    protocol: i32,
    stack: Option<&ItemStack>,
    writer: &mut Writer,
) -> io::Result<()> {
    if version::item_components(protocol) {
        let Some(stack) = stack else {
            writer.varint(0);
            return Ok(());
        };
        if stack.tag.is_some() {
            return Err(invalid(
                "an NBT item tag in a protocol with data components",
            ));
        }
        let components = stack.components.clone().unwrap_or_default();
        writer.varint(stack.count.max(1)).varint(stack.id);
        return components::write(protocol, &components, writer);
    }
    if stack.is_some_and(|stack| stack.components.is_some()) {
        return Err(invalid("data components in a protocol with NBT item tags"));
    }
    match stack {
        None if present_flag(protocol) => {
            writer.bool(false);
        }
        None => {
            writer.i16(-1);
        }
        Some(stack) => {
            if present_flag(protocol) {
                writer.bool(true).varint(stack.id);
            } else {
                let id = i16::try_from(stack.id).map_err(|_| invalid("item ID out of range"))?;
                writer.i16(id);
            }
            // Counts above a byte only exist with components (up to 99).
            writer.i8(stack.count.clamp(i32::from(i8::MIN), i32::from(i8::MAX)) as i8);
            if !version::flattened(protocol) {
                writer.i16(stack.damage);
            }
            Nbt::write_optional(stack.tag.as_ref(), writer, !version::nameless_nbt(protocol));
        }
    }
    Ok(())
}

/// A non-empty stack template (26.1 on): item ID, count, component patch.
pub(super) fn read_template(protocol: i32, reader: &mut Reader<'_>) -> io::Result<ItemStack> {
    let id = reader.varint()?;
    let count = reader.varint()?;
    if id < 0 || count <= 0 {
        return Err(invalid("invalid item stack template"));
    }
    Ok(ItemStack {
        id,
        count,
        damage: 0,
        tag: None,
        components: Some(components::read(protocol, reader)?),
    })
}

pub(super) fn write_template(
    protocol: i32,
    writer: &mut Writer,
    stack: &ItemStack,
) -> io::Result<()> {
    if stack.tag.is_some() {
        return Err(invalid(
            "an NBT item tag in a protocol with data components",
        ));
    }
    writer.varint(stack.id).varint(stack.count.max(1));
    components::write(
        protocol,
        &stack.components.clone().unwrap_or_default(),
        writer,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbt::Compound;

    fn round_trip(protocol: i32, bytes: &[u8]) -> Option<ItemStack> {
        let mut reader = Reader::new(bytes);
        let stack = read(protocol, &mut reader).unwrap();
        reader.finish().unwrap();
        let mut writer = Writer::new();
        write(protocol, stack.as_ref(), &mut writer).unwrap();
        assert_eq!(writer.buf, bytes, "protocol {protocol}");
        stack
    }

    fn tag() -> Nbt {
        let mut tag = Compound::new();
        tag.insert("Damage", 3i32);
        Nbt::Compound(tag)
    }

    #[test]
    fn every_pre_component_layout_round_trips() {
        // 1.12: ID 276 (diamond sword), count 1, damage 5, named {Damage: 3}.
        let mut legacy = vec![0x01, 0x14, 1, 0, 5];
        legacy.extend(tag().to_bytes(true));
        let stack = round_trip(340, &legacy).unwrap();
        assert_eq!((stack.id, stack.count, stack.damage), (276, 1, 5));
        assert_eq!(stack.tag, Some(tag()));
        assert_eq!(round_trip(340, &[0xff, 0xff]), None);
        assert_eq!(round_trip(335, &[0, 1, 64, 0, 0, 0]).unwrap().count, 64);

        // 1.13: no damage.
        let mut flat = vec![0x02, 0x70, 2];
        flat.extend(tag().to_bytes(true));
        assert_eq!(round_trip(393, &flat).unwrap().id, 0x270);
        assert_eq!(round_trip(401, &[0xff, 0xff]), None);

        // 1.13.2: present flag and VarInt ID; 1.20.2 drops the root name.
        let mut present = vec![1, 0xf5, 0x04, 1];
        present.extend(tag().to_bytes(true));
        assert_eq!(round_trip(404, &present).unwrap().id, 0x275);
        assert_eq!(round_trip(763, &present).unwrap().id, 0x275);
        let mut nameless = vec![1, 0xf5, 0x04, 1];
        nameless.extend(tag().to_bytes(false));
        assert_eq!(round_trip(764, &nameless).unwrap().tag, Some(tag()));
        assert_eq!(round_trip(765, &[0]), None);
        assert_eq!(round_trip(477, &[1, 1, 1, 0]).unwrap().tag, None);
    }

    #[test]
    fn component_layout_empty_stacks_round_trip() {
        assert_eq!(round_trip(766, &[0]), None);
        assert_eq!(round_trip(777, &[0]), None);
    }

    #[test]
    fn malformed_stacks_fail() {
        for (protocol, bytes) in [
            (340, &[0xff, 0xfe][..]),
            (340, &[0, 1, 1, 0]),
            (393, &[0, 1, 1, 9]),
            (404, &[2]),
            (404, &[1, 0xff, 0xff, 0xff, 0xff, 0x0f, 1, 0]),
            (764, &[1, 1, 1, 10]),
            (766, &[0xff, 0xff, 0xff, 0xff, 0x0f]),
        ] {
            assert!(
                read(protocol, &mut Reader::new(bytes)).is_err(),
                "{protocol} {bytes:?}"
            );
        }
    }

    #[test]
    fn stacks_that_do_not_fit_the_protocol_fail_to_write() {
        let stack = ItemStack {
            id: 70_000,
            count: 1,
            damage: 0,
            tag: None,
            components: None,
        };
        assert!(write(340, Some(&stack), &mut Writer::new()).is_err());
        let with_tag = ItemStack {
            tag: Some(tag()),
            id: 1,
            ..stack.clone()
        };
        assert!(write(766, Some(&with_tag), &mut Writer::new()).is_err());
        let with_components = ItemStack {
            components: Some(Default::default()),
            id: 1,
            ..stack
        };
        assert!(write(765, Some(&with_components), &mut Writer::new()).is_err());
    }
}
