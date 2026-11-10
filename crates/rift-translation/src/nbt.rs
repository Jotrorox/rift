//! An NBT value model for packets whose tags must be changed, not just copied.
//!
//! Compounds keep their entries in wire order so an untouched tag serializes
//! back to the same bytes. Strings are Java modified UTF-8 on the wire.
use crate::wire::{MAX_NBT_DEPTH, Reader, Writer, invalid};
use std::io;

#[derive(Clone, Debug, PartialEq)]
pub enum Nbt {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    List(Vec<Nbt>),
    Compound(Compound),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

/// An ordered compound. Lookups are linear: compounds on the wire are small.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compound(pub Vec<(String, Nbt)>);

impl Compound {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&Nbt> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Nbt> {
        self.0
            .iter_mut()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Replace `key` in place, or append it.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Nbt>) -> Option<Nbt> {
        let (key, value) = (key.into(), value.into());
        match self.0.iter_mut().find(|(name, _)| *name == key) {
            Some((_, slot)) => Some(std::mem::replace(slot, value)),
            None => {
                self.0.push((key, value));
                None
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Nbt> {
        let index = self.0.iter().position(|(name, _)| name == key)?;
        Some(self.0.remove(index).1)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    /// Any integral or floating tag, as vanilla's `getInt` family reads it.
    pub fn int(&self, key: &str) -> Option<i32> {
        self.get(key)?.as_i64().map(|value| value as i32)
    }

    pub fn compound(&self, key: &str) -> Option<&Compound> {
        self.get(key)?.as_compound()
    }

    pub fn compound_mut(&mut self, key: &str) -> Option<&mut Compound> {
        self.get_mut(key)?.as_compound_mut()
    }

    pub fn list(&self, key: &str) -> Option<&[Nbt]> {
        self.get(key)?.as_list()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Nbt)> {
        self.0.iter().map(|(name, value)| (name.as_str(), value))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, Nbt)> for Compound {
    fn from_iter<T: IntoIterator<Item = (String, Nbt)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

macro_rules! from {
    ($($ty:ty => $variant:ident),* $(,)?) => {$(
        impl From<$ty> for Nbt {
            fn from(value: $ty) -> Self {
                Nbt::$variant(value)
            }
        }
    )*};
}
from!(i8 => Byte, i16 => Short, i32 => Int, i64 => Long, f32 => Float, f64 => Double);
from!(String => String, Compound => Compound, Vec<Nbt> => List);

impl From<&str> for Nbt {
    fn from(value: &str) -> Self {
        Nbt::String(value.to_owned())
    }
}

impl From<bool> for Nbt {
    fn from(value: bool) -> Self {
        Nbt::Byte(i8::from(value))
    }
}

impl Nbt {
    pub fn kind(&self) -> u8 {
        match self {
            Nbt::Byte(_) => 1,
            Nbt::Short(_) => 2,
            Nbt::Int(_) => 3,
            Nbt::Long(_) => 4,
            Nbt::Float(_) => 5,
            Nbt::Double(_) => 6,
            Nbt::ByteArray(_) => 7,
            Nbt::String(_) => 8,
            Nbt::List(_) => 9,
            Nbt::Compound(_) => 10,
            Nbt::IntArray(_) => 11,
            Nbt::LongArray(_) => 12,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Nbt::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        Some(match self {
            Nbt::Byte(value) => i64::from(*value),
            Nbt::Short(value) => i64::from(*value),
            Nbt::Int(value) => i64::from(*value),
            Nbt::Long(value) => *value,
            Nbt::Float(value) => *value as i64,
            Nbt::Double(value) => *value as i64,
            _ => return None,
        })
    }

    pub fn as_f64(&self) -> Option<f64> {
        Some(match self {
            Nbt::Float(value) => f64::from(*value),
            Nbt::Double(value) => *value,
            other => other.as_i64()? as f64,
        })
    }

    pub fn as_compound(&self) -> Option<&Compound> {
        match self {
            Nbt::Compound(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_compound_mut(&mut self) -> Option<&mut Compound> {
        match self {
            Nbt::Compound(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Nbt]> {
        match self {
            Nbt::List(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_list_mut(&mut self) -> Option<&mut Vec<Nbt>> {
        match self {
            Nbt::List(value) => Some(value),
            _ => None,
        }
    }

    /// Read one tag; `None` is a lone TAG_End (an absent tag). With `named`
    /// the root carries a name (before 1.20.2), which is discarded.
    pub fn read(reader: &mut Reader, named: bool) -> io::Result<Option<Nbt>> {
        let kind = reader.u8()?;
        if kind == 0 {
            return Ok(None);
        }
        if named {
            let size = reader.u16()?;
            reader.take(usize::from(size))?;
        }
        payload(reader, kind, 0).map(Some)
    }

    /// Parse a complete tag from `bytes`, failing on trailing data.
    pub fn from_bytes(bytes: &[u8], named: bool) -> io::Result<Option<Nbt>> {
        let mut reader = Reader::new(bytes);
        let value = Self::read(&mut reader, named)?;
        reader.finish()?;
        Ok(value)
    }

    /// Write one tag; a named root gets an empty name, as vanilla sends it.
    pub fn write(&self, writer: &mut Writer, named: bool) {
        writer.u8(self.kind());
        if named {
            writer.u16(0);
        }
        self.write_payload(writer);
    }

    /// Write an optional tag: `None` is a lone TAG_End.
    pub fn write_optional(value: Option<&Nbt>, writer: &mut Writer, named: bool) {
        match value {
            Some(value) => value.write(writer, named),
            None => {
                writer.u8(0);
            }
        }
    }

    pub fn to_bytes(&self, named: bool) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write(&mut writer, named);
        writer.buf
    }

    fn write_payload(&self, writer: &mut Writer) {
        match self {
            Nbt::Byte(value) => drop(writer.i8(*value)),
            Nbt::Short(value) => drop(writer.i16(*value)),
            Nbt::Int(value) => drop(writer.i32(*value)),
            Nbt::Long(value) => drop(writer.i64(*value)),
            Nbt::Float(value) => drop(writer.f32(*value)),
            Nbt::Double(value) => drop(writer.f64(*value)),
            Nbt::ByteArray(value) => {
                writer.i32(value.len() as i32).bytes(value);
            }
            Nbt::String(value) => write_string(value, writer),
            Nbt::List(values) => {
                // A list has one element type; mixed lists are a caller bug
                // that would corrupt the stream, so the first element decides.
                let kind = values.first().map_or(0, Nbt::kind);
                debug_assert!(values.iter().all(|value| value.kind() == kind));
                writer.u8(kind).i32(values.len() as i32);
                for value in values {
                    value.write_payload(writer);
                }
            }
            Nbt::Compound(compound) => {
                for (name, value) in &compound.0 {
                    writer.u8(value.kind());
                    write_string(name, writer);
                    value.write_payload(writer);
                }
                writer.u8(0);
            }
            Nbt::IntArray(values) => {
                writer.i32(values.len() as i32);
                for value in values {
                    writer.i32(*value);
                }
            }
            Nbt::LongArray(values) => {
                writer.i32(values.len() as i32);
                for value in values {
                    writer.i64(*value);
                }
            }
        }
    }
}

fn payload(reader: &mut Reader, kind: u8, depth: usize) -> io::Result<Nbt> {
    if depth > MAX_NBT_DEPTH {
        return Err(invalid("NBT nesting limit exceeded"));
    }
    let length = |reader: &mut Reader, width: usize| -> io::Result<usize> {
        let count = reader.i32()?;
        let count = usize::try_from(count).map_err(|_| invalid("invalid NBT array length"))?;
        if count
            .checked_mul(width)
            .is_none_or(|size| size > reader.remaining())
        {
            return Err(invalid("invalid NBT array length"));
        }
        Ok(count)
    };
    Ok(match kind {
        1 => Nbt::Byte(reader.i8()?),
        2 => Nbt::Short(reader.i16()?),
        3 => Nbt::Int(reader.i32()?),
        4 => Nbt::Long(reader.i64()?),
        5 => Nbt::Float(reader.f32()?),
        6 => Nbt::Double(reader.f64()?),
        7 => {
            let count = length(reader, 1)?;
            Nbt::ByteArray(reader.take(count)?.to_vec())
        }
        8 => Nbt::String(read_string(reader)?),
        9 => {
            let element = reader.u8()?;
            let count = reader.i32()?;
            if count < 0 || (count > 0 && element == 0) {
                return Err(invalid("invalid NBT list"));
            }
            // Every element except TAG_End payloads takes at least one byte.
            if count as usize > reader.remaining() {
                return Err(invalid("invalid NBT list"));
            }
            let mut values = Vec::with_capacity(count as usize);
            for _ in 0..count {
                values.push(payload(reader, element, depth + 1)?);
            }
            Nbt::List(values)
        }
        10 => {
            let mut entries = Vec::new();
            loop {
                let element = reader.u8()?;
                if element == 0 {
                    break;
                }
                let name = read_string(reader)?;
                entries.push((name, payload(reader, element, depth + 1)?));
            }
            Nbt::Compound(Compound(entries))
        }
        11 => {
            let count = length(reader, 4)?;
            Nbt::IntArray(
                (0..count)
                    .map(|_| reader.i32())
                    .collect::<io::Result<_>>()?,
            )
        }
        12 => {
            let count = length(reader, 8)?;
            Nbt::LongArray(
                (0..count)
                    .map(|_| reader.i64())
                    .collect::<io::Result<_>>()?,
            )
        }
        _ => return Err(invalid("invalid NBT tag")),
    })
}

/// Decode Java modified UTF-8: NUL is `C0 80` and supplementary characters
/// are surrogate pairs of three bytes each. Unpaired surrogates become U+FFFD.
fn read_string(reader: &mut Reader) -> io::Result<String> {
    let size = reader.u16()?;
    let bytes = reader.take(usize::from(size))?;
    if bytes.is_ascii() {
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    let mut units = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let bad = || invalid("invalid modified UTF-8 in NBT string");
    while index < bytes.len() {
        let first = bytes[index];
        let (unit, width) = match first {
            0..=0x7f => (u16::from(first), 1),
            0xc0..=0xdf => {
                let second = *bytes.get(index + 1).ok_or_else(bad)?;
                ((u16::from(first & 0x1f) << 6) | u16::from(second & 0x3f), 2)
            }
            0xe0..=0xef => {
                let second = *bytes.get(index + 1).ok_or_else(bad)?;
                let third = *bytes.get(index + 2).ok_or_else(bad)?;
                (
                    (u16::from(first & 0x0f) << 12)
                        | (u16::from(second & 0x3f) << 6)
                        | u16::from(third & 0x3f),
                    3,
                )
            }
            _ => return Err(bad()),
        };
        units.push(unit);
        index += width;
    }
    Ok(String::from_utf16_lossy(&units))
}

fn write_string(value: &str, writer: &mut Writer) {
    if value.is_ascii() && !value.contains('\0') && value.len() <= usize::from(u16::MAX) {
        writer.u16(value.len() as u16).bytes(value.as_bytes());
        return;
    }
    let mut bytes = Vec::with_capacity(value.len() + 8);
    for unit in value.encode_utf16() {
        match unit {
            1..=0x7f => bytes.push(unit as u8),
            0..=0x7ff => bytes.extend([0xc0 | (unit >> 6) as u8, 0x80 | (unit & 63) as u8]),
            _ => bytes.extend([
                0xe0 | (unit >> 12) as u8,
                0x80 | ((unit >> 6) & 63) as u8,
                0x80 | (unit & 63) as u8,
            ]),
        }
    }
    // An NBT string holds at most 65535 bytes; cut on a character boundary.
    let mut end = bytes.len().min(usize::from(u16::MAX));
    while end < bytes.len() && end > 0 && bytes[end] & 0xc0 == 0x80 {
        end -= 1;
    }
    writer.u16(end as u16).bytes(&bytes[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Nbt {
        let mut inner = Compound::new();
        inner.insert("name", "Stöne \u{1F600}\0");
        inner.insert("count", 3i8);
        let mut root = Compound::new();
        root.insert("id", 7i32);
        root.insert("big", i64::MIN);
        root.insert("f", 1.5f32);
        root.insert("d", -2.25f64);
        root.insert("s", 12i16);
        root.insert("bytes", Nbt::ByteArray(vec![1, 2, 255]));
        root.insert("ints", Nbt::IntArray(vec![-1, 0, 1]));
        root.insert("longs", Nbt::LongArray(vec![i64::MAX]));
        root.insert(
            "list",
            vec![Nbt::Compound(inner.clone()), Nbt::Compound(inner)],
        );
        root.insert("empty", Vec::<Nbt>::new());
        Nbt::Compound(root)
    }

    #[test]
    fn values_round_trip_named_and_nameless() {
        for named in [false, true] {
            let bytes = sample().to_bytes(named);
            assert_eq!(Nbt::from_bytes(&bytes, named).unwrap(), Some(sample()));
            // The raw skipper agrees with the parser on the tag's extent.
            let mut reader = Reader::new(&bytes);
            assert_eq!(reader.nbt_bytes(named).unwrap(), bytes);
        }
        assert_eq!(Nbt::from_bytes(&[0], true).unwrap(), None);
    }

    #[test]
    fn compounds_keep_order_and_replace_in_place() {
        let mut compound = Compound::new();
        compound.insert("a", 1i32);
        compound.insert("b", 2i32);
        assert_eq!(compound.insert("a", 3i32), Some(Nbt::Int(1)));
        assert_eq!(compound.0[0], ("a".to_owned(), Nbt::Int(3)));
        assert_eq!(compound.int("b"), Some(2));
        assert_eq!(compound.remove("a"), Some(Nbt::Int(3)));
        assert!(!compound.contains("a"));
    }

    #[test]
    fn malformed_input_fails() {
        assert!(Nbt::from_bytes(&[13], false).is_err());
        assert!(Nbt::from_bytes(&[9, 3, 0x7f, 0xff, 0xff, 0xff], false).is_err());
        assert!(Nbt::from_bytes(&[7, 0x7f, 0xff, 0xff, 0xff], false).is_err());
        assert!(Nbt::from_bytes(&[1, 0, 0], false).is_err());
        let mut deep = vec![9u8];
        for _ in 0..600 {
            deep.extend([9, 0, 0, 0, 1]);
        }
        assert!(Nbt::from_bytes(&deep, false).is_err());
    }
}
