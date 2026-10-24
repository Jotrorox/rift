//! A streaming rewriter: read one packet body, copying unchanged fields
//! verbatim and writing changed ones. Every read is bounded by the input, so a
//! malformed packet fails instead of allocating or recursing without limit.
use crate::protocol::{invalid, read_varint, write_varint};
use std::io;

/// Nested values (items in containers in bundles, slot displays in composite
/// displays, ...) share this depth budget. Vanilla's own limits are far lower.
const MAX_DEPTH: usize = 32;
const MAX_NBT_DEPTH: usize = 512;

/// How `Rewriter::nbt_renaming` replaces a matched string value.
pub(super) enum Rename<'n> {
    String(&'n str),
    /// Inline the sound event by name, for a receiver without it built in.
    SoundEvent,
}

pub(super) struct Rewriter<'a> {
    input: &'a [u8],
    pub(super) output: Vec<u8>,
    depth: usize,
}

impl<'a> Rewriter<'a> {
    pub(super) fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            output: Vec::with_capacity(input.len() + 8),
            depth: 0,
        }
    }

    /// The rewritten body. Unconsumed input means the layout did not match.
    pub(super) fn finish(self) -> io::Result<Vec<u8>> {
        if !self.input.is_empty() {
            return Err(invalid("trailing data after translated packet"));
        }
        Ok(self.output)
    }

    pub(super) fn remaining(&self) -> usize {
        self.input.len()
    }

    pub(super) fn take(&mut self, size: usize) -> io::Result<&'a [u8]> {
        let value = self
            .input
            .get(..size)
            .ok_or_else(|| invalid("short packet"))?;
        self.input = &self.input[size..];
        Ok(value)
    }

    pub(super) fn copy(&mut self, size: usize) -> io::Result<()> {
        let value = self.take(size)?;
        self.output.extend_from_slice(value);
        Ok(())
    }

    /// Copy everything that is left; used once no later field needs a rewrite.
    pub(super) fn copy_rest(&mut self) {
        self.output.extend_from_slice(self.input);
        self.input = &[];
    }

    pub(super) fn read_varint(&mut self) -> io::Result<i32> {
        read_varint(&mut self.input)
    }

    pub(super) fn write_varint(&mut self, value: i32) {
        write_varint(value, &mut self.output);
    }

    /// Copy a VarInt in its original encoding and return its value.
    pub(super) fn varint(&mut self) -> io::Result<i32> {
        let start = self.input;
        let value = self.read_varint()?;
        self.output
            .extend_from_slice(&start[..start.len() - self.input.len()]);
        Ok(value)
    }

    pub(super) fn varlong(&mut self) -> io::Result<()> {
        let size = self
            .input
            .iter()
            .take(10)
            .position(|byte| byte & 0x80 == 0)
            .ok_or_else(|| invalid("invalid VarLong"))?;
        self.copy(size + 1)
    }

    pub(super) fn bool(&mut self) -> io::Result<bool> {
        let value = match self.take(1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(invalid("invalid boolean")),
        };
        self.output.push(u8::from(value));
        Ok(value)
    }

    pub(super) fn byte(&mut self) -> io::Result<u8> {
        let value = self.take(1)?[0];
        self.output.push(value);
        Ok(value)
    }

    pub(super) fn int(&mut self) -> io::Result<i32> {
        let value = i32::from_be_bytes(self.take(4)?.try_into().unwrap());
        self.output.extend_from_slice(&value.to_be_bytes());
        Ok(value)
    }

    /// A UTF-8 string with a VarInt byte length, at most `max` UTF-16 units.
    pub(super) fn string(&mut self, max: usize) -> io::Result<&'a str> {
        let start = self.input;
        let value = crate::protocol::read_string(&mut self.input, max)?;
        self.output
            .extend_from_slice(&start[..start.len() - self.input.len()]);
        Ok(value)
    }

    pub(super) fn identifier(&mut self) -> io::Result<&'a str> {
        self.string(32767)
    }

    /// A list length: non-negative, at most `max`, and never more elements
    /// than remaining bytes (every element this layout reads is non-empty).
    pub(super) fn count(&mut self, max: usize) -> io::Result<usize> {
        let count = self.varint()?;
        if count < 0 || count as usize > max || count as usize > self.input.len() {
            return Err(invalid("invalid list length"));
        }
        Ok(count as usize)
    }

    pub(super) fn list(
        &mut self,
        max: usize,
        mut element: impl FnMut(&mut Self) -> io::Result<()>,
    ) -> io::Result<()> {
        for _ in 0..self.count(max)? {
            element(self)?;
        }
        Ok(())
    }

    /// `ByteBufCodecs.optional`: a boolean, then the value when present.
    pub(super) fn optional(
        &mut self,
        value: impl FnOnce(&mut Self) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.bool()? {
            value(self)?;
        }
        Ok(())
    }

    pub(super) fn nested<T>(
        &mut self,
        value: impl FnOnce(&mut Self) -> io::Result<T>,
    ) -> io::Result<T> {
        if self.depth >= MAX_DEPTH {
            return Err(invalid("translated value nesting limit exceeded"));
        }
        self.depth += 1;
        let result = value(self);
        self.depth -= 1;
        result
    }

    /// Run `value` on a VarInt length-prefixed region (an untrusted creative
    /// component) and re-prefix the rewritten bytes with their new length.
    pub(super) fn length_prefixed(
        &mut self,
        value: impl FnOnce(&mut Rewriter<'a>) -> io::Result<()>,
    ) -> io::Result<()> {
        let length = self.read_varint()?;
        if length < 0 {
            return Err(invalid("invalid length prefix"));
        }
        let mut inner = Rewriter::new(self.take(length as usize)?);
        inner.depth = self.depth;
        value(&mut inner)?;
        let bytes = inner.finish()?;
        self.write_varint(
            i32::try_from(bytes.len()).map_err(|_| invalid("length prefix overflow"))?,
        );
        self.output.extend_from_slice(&bytes);
        Ok(())
    }

    /// One network NBT tag: a type byte and an unnamed payload. TAG_End (an
    /// absent optional tag) has no payload.
    pub(super) fn nbt(&mut self) -> io::Result<()> {
        let start = self.input;
        let kind = self.take(1)?[0];
        if kind != 0 {
            self.nbt_payload(kind, 0)?;
        }
        self.output
            .extend_from_slice(&start[..start.len() - self.input.len()]);
        Ok(())
    }

    /// Copy one network NBT tag, replacing string values `rename` matches.
    /// A string in a compound may become an inline sound event; strings in a
    /// list can only become other strings, as a list has one element type.
    pub(super) fn nbt_renaming<'n>(
        &mut self,
        rename: &dyn Fn(&[u8]) -> Option<Rename<'n>>,
    ) -> io::Result<()> {
        let kind = self.take(1)?[0];
        self.output.push(kind);
        if kind != 0 {
            self.renaming_payload(kind, 0, rename)?;
        }
        Ok(())
    }

    fn renaming_payload<'n>(
        &mut self,
        kind: u8,
        depth: usize,
        rename: &dyn Fn(&[u8]) -> Option<Rename<'n>>,
    ) -> io::Result<()> {
        if depth > MAX_NBT_DEPTH {
            return Err(invalid("NBT nesting limit exceeded"));
        }
        match kind {
            9 => {
                let element = self.byte()?;
                let count = self.int()?;
                if count < 0 || count as usize > self.input.len() || (count > 0 && element == 0) {
                    return Err(invalid("invalid NBT list"));
                }
                for _ in 0..count {
                    if element == 8 {
                        let value = self.nbt_string()?;
                        match rename(value) {
                            Some(Rename::String(name)) => self.write_nbt_string(name)?,
                            _ => self.write_nbt_string_bytes(value),
                        }
                    } else {
                        self.renaming_payload(element, depth + 1, rename)?;
                    }
                }
            }
            10 => loop {
                let element = self.take(1)?[0];
                if element == 0 {
                    self.output.push(0);
                    break;
                }
                let name = self.nbt_string()?;
                if element != 8 {
                    self.output.push(element);
                    self.write_nbt_string_bytes(name);
                    self.renaming_payload(element, depth + 1, rename)?;
                    continue;
                }
                let value = self.nbt_string()?;
                match rename(value) {
                    Some(Rename::String(replacement)) => {
                        self.output.push(8);
                        self.write_nbt_string_bytes(name);
                        self.write_nbt_string(replacement)?;
                    }
                    Some(Rename::SoundEvent) => {
                        // `SoundEvent.DIRECT_CODEC`: `{sound_id: <name>}`.
                        self.output.push(10);
                        self.write_nbt_string_bytes(name);
                        self.output.push(8);
                        self.write_nbt_string("sound_id")?;
                        self.write_nbt_string_bytes(value);
                        self.output.push(0);
                    }
                    None => {
                        self.output.push(8);
                        self.write_nbt_string_bytes(name);
                        self.write_nbt_string_bytes(value);
                    }
                }
            },
            _ => {
                let start = self.input;
                self.nbt_payload(kind, depth)?;
                self.output
                    .extend_from_slice(&start[..start.len() - self.input.len()]);
            }
        }
        Ok(())
    }

    /// An NBT string's modified UTF-8 bytes, without its length.
    fn nbt_string(&mut self) -> io::Result<&'a [u8]> {
        let size = u16::from_be_bytes(self.take(2)?.try_into().unwrap());
        self.take(usize::from(size))
    }

    fn write_nbt_string_bytes(&mut self, value: &[u8]) {
        self.output
            .extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.output.extend_from_slice(value);
    }

    fn write_nbt_string(&mut self, value: &str) -> io::Result<()> {
        crate::protocol::write_modified_utf8(value, &mut self.output)
    }

    fn nbt_payload(&mut self, kind: u8, depth: usize) -> io::Result<()> {
        if depth > MAX_NBT_DEPTH {
            return Err(invalid("NBT nesting limit exceeded"));
        }
        let length = |this: &mut Self, width: usize| -> io::Result<()> {
            let count = i32::from_be_bytes(this.take(4)?.try_into().unwrap());
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
            7 => length(self, 1)?,
            11 => length(self, 4)?,
            12 => length(self, 8)?,
            8 => {
                let size = u16::from_be_bytes(self.take(2)?.try_into().unwrap());
                self.take(usize::from(size))?;
            }
            9 => {
                let element = self.take(1)?[0];
                let count = i32::from_be_bytes(self.take(4)?.try_into().unwrap());
                if count < 0 || count as usize > self.input.len() || (count > 0 && element == 0) {
                    return Err(invalid("invalid NBT list"));
                }
                for _ in 0..count {
                    self.nbt_payload(element, depth + 1)?;
                }
            }
            10 => loop {
                let element = self.take(1)?[0];
                if element == 0 {
                    break;
                }
                let size = u16::from_be_bytes(self.take(2)?.try_into().unwrap());
                self.take(usize::from(size))?;
                self.nbt_payload(element, depth + 1)?;
            },
            _ => return Err(invalid("invalid NBT tag")),
        }
        Ok(())
    }
}
