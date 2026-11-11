//! Bounded readers and writers for Minecraft's wire types.
//!
//! [`Reader`] consumes a packet body, [`Writer`] builds one, and [`Rewriter`]
//! pairs them for the common case of copying a packet while changing a few
//! fields. Every read is bounded by the input, so a malformed packet fails
//! instead of allocating or recursing without limit.
//!
//! Naming on [`Rewriter`]: `x()` copies a value through and returns it,
//! `read_x()` consumes it without writing, `write_x()` writes without reading.
use std::io;

pub fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Nested values share this depth budget. Vanilla's own limits are lower.
pub const MAX_DEPTH: usize = 64;
pub const MAX_NBT_DEPTH: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl Position {
    /// 1.8–1.13.2: x (26 bits), y (12 bits), z (26 bits).
    pub fn from_legacy(value: i64) -> Self {
        Self {
            x: (value >> 38) as i32,
            y: ((value << 26) >> 52) as i32,
            z: ((value << 38) >> 38) as i32,
        }
    }

    pub fn to_legacy(self) -> i64 {
        ((self.x as i64 & 0x3ff_ffff) << 38)
            | ((self.y as i64 & 0xfff) << 26)
            | (self.z as i64 & 0x3ff_ffff)
    }

    /// 1.14+: x (26 bits), z (26 bits), y (12 bits).
    pub fn from_modern(value: i64) -> Self {
        Self {
            x: (value >> 38) as i32,
            y: ((value << 52) >> 52) as i32,
            z: ((value << 26) >> 38) as i32,
        }
    }

    pub fn to_modern(self) -> i64 {
        ((self.x as i64 & 0x3ff_ffff) << 38)
            | ((self.z as i64 & 0x3ff_ffff) << 12)
            | (self.y as i64 & 0xfff)
    }
}

#[derive(Clone, Copy)]
pub struct Reader<'a> {
    input: &'a [u8],
}

macro_rules! read_number {
    ($($name:ident: $ty:ty),* $(,)?) => {$(
        pub fn $name(&mut self) -> io::Result<$ty> {
            const SIZE: usize = std::mem::size_of::<$ty>();
            Ok(<$ty>::from_be_bytes(self.take(SIZE)?.try_into().unwrap()))
        }
    )*};
}

impl<'a> Reader<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self { input }
    }

    pub fn remaining(&self) -> usize {
        self.input.len()
    }

    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// The unread input, without consuming it.
    pub fn peek(&self) -> &'a [u8] {
        self.input
    }

    /// Fails when input is left over: the layout did not match.
    pub fn finish(self) -> io::Result<()> {
        if self.input.is_empty() {
            Ok(())
        } else {
            Err(invalid("trailing data after packet"))
        }
    }

    pub fn take(&mut self, size: usize) -> io::Result<&'a [u8]> {
        let value = self
            .input
            .get(..size)
            .ok_or_else(|| invalid("short packet"))?;
        self.input = &self.input[size..];
        Ok(value)
    }

    /// Everything that is left.
    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.input)
    }

    read_number!(u8: u8, i8: i8, i16: i16, u16: u16, i32: i32, i64: i64, f32: f32, f64: f64);

    pub fn bool(&mut self) -> io::Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid("invalid boolean")),
        }
    }

    pub fn varint(&mut self) -> io::Result<i32> {
        let mut value = 0u32;
        for shift in (0..35).step_by(7) {
            let byte = self.u8()?;
            value |= u32::from(byte & 127) << shift;
            if byte & 128 == 0 {
                return Ok(value as i32);
            }
        }
        Err(invalid("invalid VarInt"))
    }

    pub fn varlong(&mut self) -> io::Result<i64> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = self.u8()?;
            value |= u64::from(byte & 127) << shift;
            if byte & 128 == 0 {
                return Ok(value as i64);
            }
        }
        Err(invalid("invalid VarLong"))
    }

    /// A UTF-8 string with a VarInt byte length, at most `max` UTF-16 units.
    pub fn string(&mut self, max: usize) -> io::Result<&'a str> {
        let length = self.varint()?;
        if length < 0 || length as usize > max.saturating_mul(4) {
            return Err(invalid("invalid string length"));
        }
        let value = std::str::from_utf8(self.take(length as usize)?)
            .map_err(|_| invalid("string is not UTF-8"))?;
        if value.len() > max && value.encode_utf16().count() > max {
            return Err(invalid("string exceeds character limit"));
        }
        Ok(value)
    }

    /// A string of at most 32767 characters, the protocol's default limit.
    pub fn str(&mut self) -> io::Result<&'a str> {
        self.string(32767)
    }

    pub fn uuid(&mut self) -> io::Result<u128> {
        Ok(u128::from_be_bytes(self.take(16)?.try_into().unwrap()))
    }

    /// A list length: non-negative and never more than the remaining bytes
    /// (every element the caller reads must be at least one byte).
    pub fn count(&mut self) -> io::Result<usize> {
        let count = self.varint()?;
        if count < 0 || count as usize > self.input.len() {
            return Err(invalid("invalid list length"));
        }
        Ok(count as usize)
    }

    /// A byte array with a VarInt length.
    pub fn byte_array(&mut self) -> io::Result<&'a [u8]> {
        let length = self.count()?;
        self.take(length)
    }

    pub fn position_legacy(&mut self) -> io::Result<Position> {
        Ok(Position::from_legacy(self.i64()?))
    }

    pub fn position(&mut self) -> io::Result<Position> {
        Ok(Position::from_modern(self.i64()?))
    }

    /// The raw bytes of one NBT tag. `named` selects the root format: before
    /// 1.20.2 the root carries a name, from 1.20.2 on it does not. A single
    /// TAG_End byte is an absent tag in both.
    pub fn nbt_bytes(&mut self, named: bool) -> io::Result<&'a [u8]> {
        let start = self.input;
        let kind = self.u8()?;
        if kind != 0 {
            if named {
                let size = self.u16()?;
                self.take(usize::from(size))?;
            }
            self.skip_nbt_payload(kind, 0)?;
        }
        Ok(&start[..start.len() - self.input.len()])
    }

    fn skip_nbt_payload(&mut self, kind: u8, depth: usize) -> io::Result<()> {
        if depth > MAX_NBT_DEPTH {
            return Err(invalid("NBT nesting limit exceeded"));
        }
        let array = |this: &mut Self, width: usize| -> io::Result<()> {
            let count = this.i32()?;
            let size = usize::try_from(count)
                .ok()
                .and_then(|count| count.checked_mul(width))
                .ok_or_else(|| invalid("invalid NBT array length"))?;
            this.take(size)?;
            Ok(())
        };
        match kind {
            1 => drop(self.take(1)?),
            2 => drop(self.take(2)?),
            3 | 5 => drop(self.take(4)?),
            4 | 6 => drop(self.take(8)?),
            7 => array(self, 1)?,
            11 => array(self, 4)?,
            12 => array(self, 8)?,
            8 => {
                let size = self.u16()?;
                self.take(usize::from(size))?;
            }
            9 => {
                let element = self.u8()?;
                let count = self.i32()?;
                if count < 0 || count as usize > self.input.len() || (count > 0 && element == 0) {
                    return Err(invalid("invalid NBT list"));
                }
                for _ in 0..count {
                    self.skip_nbt_payload(element, depth + 1)?;
                }
            }
            10 => loop {
                let element = self.u8()?;
                if element == 0 {
                    break;
                }
                let size = self.u16()?;
                self.take(usize::from(size))?;
                self.skip_nbt_payload(element, depth + 1)?;
            },
            _ => return Err(invalid("invalid NBT tag")),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

macro_rules! write_number {
    ($($name:ident: $ty:ty),* $(,)?) => {$(
        pub fn $name(&mut self, value: $ty) -> &mut Self {
            self.buf.extend_from_slice(&value.to_be_bytes());
            self
        }
    )*};
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(value);
        self
    }

    write_number!(u8: u8, i8: i8, i16: i16, u16: u16, i32: i32, i64: i64, f32: f32, f64: f64);

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.buf.push(u8::from(value));
        self
    }

    pub fn varint(&mut self, value: i32) -> &mut Self {
        let mut value = value as u32;
        while value > 127 {
            self.buf.push(value as u8 & 127 | 128);
            value >>= 7;
        }
        self.buf.push(value as u8);
        self
    }

    pub fn varlong(&mut self, value: i64) -> &mut Self {
        let mut value = value as u64;
        while value > 127 {
            self.buf.push(value as u8 & 127 | 128);
            value >>= 7;
        }
        self.buf.push(value as u8);
        self
    }

    pub fn string(&mut self, value: &str) -> &mut Self {
        self.varint(value.len() as i32);
        self.buf.extend_from_slice(value.as_bytes());
        self
    }

    pub fn uuid(&mut self, value: u128) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn byte_array(&mut self, value: &[u8]) -> &mut Self {
        self.varint(value.len() as i32);
        self.buf.extend_from_slice(value);
        self
    }

    pub fn position_legacy(&mut self, value: Position) -> &mut Self {
        self.i64(value.to_legacy())
    }

    pub fn position(&mut self, value: Position) -> &mut Self {
        self.i64(value.to_modern())
    }
}

/// Copies a packet body from `r` to `w`, changing fields on the way.
pub struct Rewriter<'a> {
    pub r: Reader<'a>,
    pub w: Writer,
    depth: usize,
}

macro_rules! copy_number {
    ($($name:ident: $ty:ty),* $(,)?) => {$(
        pub fn $name(&mut self) -> io::Result<$ty> {
            let value = self.r.$name()?;
            self.w.$name(value);
            Ok(value)
        }
    )*};
}

impl<'a> Rewriter<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self {
            r: Reader::new(input),
            w: Writer::with_capacity(input.len() + 8),
            depth: 0,
        }
    }

    /// The rewritten body. Unconsumed input means the layout did not match.
    pub fn finish(self) -> io::Result<Vec<u8>> {
        self.r.finish()?;
        Ok(self.w.buf)
    }

    pub fn remaining(&self) -> usize {
        self.r.remaining()
    }

    pub fn copy(&mut self, size: usize) -> io::Result<&'a [u8]> {
        let value = self.r.take(size)?;
        self.w.bytes(value);
        Ok(value)
    }

    /// Copy everything that is left; used once no later field needs a rewrite.
    pub fn copy_rest(&mut self) {
        let rest = self.r.rest();
        self.w.bytes(rest);
    }

    copy_number!(u8: u8, i8: i8, i16: i16, u16: u16, i32: i32, i64: i64, f32: f32, f64: f64);
    copy_number!(bool: bool, varint: i32, varlong: i64, uuid: u128);

    pub fn string(&mut self, max: usize) -> io::Result<&'a str> {
        let value = self.r.string(max)?;
        self.w.string(value);
        Ok(value)
    }

    pub fn str(&mut self) -> io::Result<&'a str> {
        self.string(32767)
    }

    pub fn byte_array(&mut self) -> io::Result<&'a [u8]> {
        let value = self.r.byte_array()?;
        self.w.byte_array(value);
        Ok(value)
    }

    /// Copy a list length; see [`Reader::count`].
    pub fn count(&mut self) -> io::Result<usize> {
        let count = self.r.count()?;
        self.w.varint(count as i32);
        Ok(count)
    }

    pub fn list(&mut self, mut element: impl FnMut(&mut Self) -> io::Result<()>) -> io::Result<()> {
        for _ in 0..self.count()? {
            element(self)?;
        }
        Ok(())
    }

    /// A boolean, then the value when it is true.
    pub fn optional(&mut self, value: impl FnOnce(&mut Self) -> io::Result<()>) -> io::Result<()> {
        if self.bool()? {
            value(self)?;
        }
        Ok(())
    }

    /// Run `value` one nesting level deeper, failing past [`MAX_DEPTH`].
    pub fn nested<T>(&mut self, value: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        if self.depth >= MAX_DEPTH {
            return Err(invalid("translated value nesting limit exceeded"));
        }
        self.depth += 1;
        let result = value(self);
        self.depth -= 1;
        result
    }

    /// Run `value` on a VarInt length-prefixed region and prefix the rewritten
    /// bytes with their new length.
    pub fn length_prefixed(
        &mut self,
        value: impl FnOnce(&mut Rewriter<'a>) -> io::Result<()>,
    ) -> io::Result<()> {
        let region = self.r.byte_array()?;
        let mut inner = Rewriter::new(region);
        inner.depth = self.depth;
        value(&mut inner)?;
        let bytes = inner.finish()?;
        self.w.byte_array(&bytes);
        Ok(())
    }

    /// Copy one NBT tag unchanged; see [`Reader::nbt_bytes`].
    pub fn nbt(&mut self, named: bool) -> io::Result<&'a [u8]> {
        let value = self.r.nbt_bytes(named)?;
        self.w.bytes(value);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip_in_both_layouts() {
        for position in [
            Position { x: 0, y: 0, z: 0 },
            Position {
                x: -1,
                y: -1,
                z: -1,
            },
            Position {
                x: 33_554_431,
                y: 2047,
                z: -33_554_432,
            },
            Position {
                x: -123_456,
                y: -64,
                z: 654_321,
            },
        ] {
            assert_eq!(Position::from_legacy(position.to_legacy()), position);
            assert_eq!(Position::from_modern(position.to_modern()), position);
        }
    }

    #[test]
    fn varints_round_trip_and_reject_overlong_input() {
        for value in [0, 1, 127, 128, 255, 25565, i32::MAX, -1, i32::MIN] {
            let mut writer = Writer::new();
            writer.varint(value);
            let mut reader = Reader::new(&writer.buf);
            assert_eq!(reader.varint().unwrap(), value);
            reader.finish().unwrap();
        }
        assert!(Reader::new(&[0x80; 6]).varint().is_err());
        for value in [0, -1, i64::MAX, i64::MIN, 1 << 40] {
            let mut writer = Writer::new();
            writer.varlong(value);
            assert_eq!(Reader::new(&writer.buf).varlong().unwrap(), value);
        }
    }

    #[test]
    fn rewriter_copies_and_rejects_trailing_data() {
        let mut writer = Writer::new();
        writer.varint(300).string("hello").bool(true).i64(-5);
        let mut rewriter = Rewriter::new(&writer.buf);
        assert_eq!(rewriter.varint().unwrap(), 300);
        assert_eq!(rewriter.str().unwrap(), "hello");
        assert!(rewriter.bool().unwrap());
        assert!(rewriter.r.finish().is_err());
        assert_eq!(rewriter.i64().unwrap(), -5);
        assert_eq!(rewriter.finish().unwrap(), writer.buf);
    }

    #[test]
    fn nbt_bytes_cover_named_and_nameless_roots() {
        // {a: 1b, l: [I; 1]} as a nameless compound.
        let nameless = [
            10, 1, 0, 1, b'a', 1, 11, 0, 1, b'l', 0, 0, 0, 1, 0, 0, 0, 1, 0,
        ];
        let mut reader = Reader::new(&nameless);
        assert_eq!(reader.nbt_bytes(false).unwrap().len(), nameless.len());
        reader.finish().unwrap();
        let mut named = vec![10, 0, 0];
        named.extend_from_slice(&nameless[1..]);
        let mut reader = Reader::new(&named);
        assert_eq!(reader.nbt_bytes(true).unwrap().len(), named.len());
        reader.finish().unwrap();
        assert_eq!(Reader::new(&[0, 9]).nbt_bytes(true).unwrap(), &[0]);
        assert!(Reader::new(&[13]).nbt_bytes(false).is_err());
    }
}
