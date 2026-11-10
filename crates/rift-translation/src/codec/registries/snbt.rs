//! The SNBT subset `scripts/translation_registries.py` writes the vanilla
//! registry contents in: typed number suffixes (`b`, `s`, `L`, `f`, `d`,
//! plain ints), double-quoted strings with `\\` and `\"` escapes, keys
//! quoted unless they are plain, and typed arrays `[B;..]`, `[I;..]`,
//! `[L;..]`. Only the crate's own data is parsed, but a malformed file still
//! fails cleanly.
use crate::{
    nbt::{Compound, Nbt},
    wire::{MAX_NBT_DEPTH, invalid},
};
use std::{fmt::Write as _, io};

pub fn parse(text: &str) -> io::Result<Nbt> {
    let mut parser = Parser {
        text: text.as_bytes(),
        at: 0,
    };
    let value = parser.value(0)?;
    if parser.at != parser.text.len() {
        return Err(invalid("trailing characters after SNBT value"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn expect(&mut self, byte: u8) -> io::Result<()> {
        if self.peek() != Some(byte) {
            return Err(invalid("malformed SNBT"));
        }
        self.at += 1;
        Ok(())
    }

    fn value(&mut self, depth: usize) -> io::Result<Nbt> {
        if depth > MAX_NBT_DEPTH {
            return Err(invalid("SNBT nesting limit exceeded"));
        }
        match self.peek() {
            Some(b'{') => self.compound(depth),
            Some(b'[') => self.list(depth),
            Some(b'"') => Ok(Nbt::String(self.string()?)),
            Some(_) => self.number(),
            None => Err(invalid("truncated SNBT")),
        }
    }

    fn compound(&mut self, depth: usize) -> io::Result<Nbt> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Nbt::Compound(Compound(entries)));
        }
        loop {
            let key = if self.peek() == Some(b'"') {
                self.string()?
            } else {
                let start = self.at;
                while self
                    .peek()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
                {
                    self.at += 1;
                }
                if start == self.at {
                    return Err(invalid("malformed SNBT key"));
                }
                String::from_utf8_lossy(&self.text[start..self.at]).into_owned()
            };
            self.expect(b':')?;
            entries.push((key, self.value(depth + 1)?));
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Nbt::Compound(Compound(entries)));
                }
                _ => return Err(invalid("malformed SNBT compound")),
            }
        }
    }

    fn list(&mut self, depth: usize) -> io::Result<Nbt> {
        self.expect(b'[')?;
        let typed = match self.text.get(self.at..self.at + 2) {
            Some([kind @ (b'B' | b'I' | b'L'), b';']) => Some(*kind),
            _ => None,
        };
        if typed.is_some() {
            self.at += 2;
        }
        let mut values = Vec::new();
        if self.peek() == Some(b']') {
            self.at += 1;
        } else {
            loop {
                values.push(self.value(depth + 1)?);
                match self.peek() {
                    Some(b',') => self.at += 1,
                    Some(b']') => {
                        self.at += 1;
                        break;
                    }
                    _ => return Err(invalid("malformed SNBT list")),
                }
            }
        }
        let wrong = || invalid("mixed SNBT array");
        Ok(match typed {
            Some(b'B') => Nbt::ByteArray(
                values
                    .iter()
                    .map(|value| match value {
                        Nbt::Byte(byte) => Ok(*byte as u8),
                        _ => Err(wrong()),
                    })
                    .collect::<io::Result<_>>()?,
            ),
            Some(b'I') => Nbt::IntArray(
                values
                    .iter()
                    .map(|value| match value {
                        Nbt::Int(int) => Ok(*int),
                        _ => Err(wrong()),
                    })
                    .collect::<io::Result<_>>()?,
            ),
            Some(_) => Nbt::LongArray(
                values
                    .iter()
                    .map(|value| match value {
                        Nbt::Long(long) => Ok(*long),
                        _ => Err(wrong()),
                    })
                    .collect::<io::Result<_>>()?,
            ),
            None => {
                if let Some(first) = values.first()
                    && values.iter().any(|value| value.kind() != first.kind())
                {
                    return Err(invalid("mixed SNBT list"));
                }
                Nbt::List(values)
            }
        })
    }

    fn string(&mut self) -> io::Result<String> {
        self.expect(b'"')?;
        let mut bytes = Vec::new();
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.at += 1;
                    break;
                }
                Some(b'\\') => {
                    self.at += 1;
                    match self.peek() {
                        Some(escaped @ (b'"' | b'\\')) => bytes.push(escaped),
                        _ => return Err(invalid("malformed SNBT escape")),
                    }
                    self.at += 1;
                }
                Some(byte) => {
                    bytes.push(byte);
                    self.at += 1;
                }
                None => return Err(invalid("unterminated SNBT string")),
            }
        }
        String::from_utf8(bytes).map_err(|_| invalid("SNBT string is not UTF-8"))
    }

    fn number(&mut self) -> io::Result<Nbt> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|byte| byte.is_ascii_digit() || b"+-.eE".contains(&byte))
        {
            self.at += 1;
        }
        let digits = std::str::from_utf8(&self.text[start..self.at])
            .map_err(|_| invalid("malformed SNBT number"))?;
        let bad = |_| invalid("malformed SNBT number");
        let suffix = self.peek();
        let value = match suffix {
            Some(b'b') => Nbt::Byte(digits.parse().map_err(bad)?),
            Some(b's') => Nbt::Short(digits.parse().map_err(bad)?),
            Some(b'L') => Nbt::Long(digits.parse().map_err(bad)?),
            Some(b'f') => Nbt::Float(
                digits
                    .parse()
                    .map_err(|_| invalid("malformed SNBT float"))?,
            ),
            Some(b'd') => Nbt::Double(
                digits
                    .parse()
                    .map_err(|_| invalid("malformed SNBT double"))?,
            ),
            _ => return Ok(Nbt::Int(digits.parse().map_err(bad)?)),
        };
        self.at += 1;
        Ok(value)
    }
}

/// The SNBT of `value`, in the same notation; for messages and tests.
pub fn write(value: &Nbt) -> String {
    let mut out = String::new();
    write_into(value, &mut out);
    out
}

fn write_into(value: &Nbt, out: &mut String) {
    let quote = |text: &str, out: &mut String| {
        out.push('"');
        out.push_str(&text.replace('\\', "\\\\").replace('"', "\\\""));
        out.push('"');
    };
    let list = |values: &mut dyn Iterator<Item = String>, prefix: &str, out: &mut String| {
        out.push('[');
        out.push_str(prefix);
        out.push_str(&values.collect::<Vec<_>>().join(","));
        out.push(']');
    };
    match value {
        Nbt::Byte(value) => drop(write!(out, "{value}b")),
        Nbt::Short(value) => drop(write!(out, "{value}s")),
        Nbt::Int(value) => drop(write!(out, "{value}")),
        Nbt::Long(value) => drop(write!(out, "{value}L")),
        Nbt::Float(value) => drop(write!(out, "{value:?}f")),
        Nbt::Double(value) => drop(write!(out, "{value:?}d")),
        Nbt::String(value) => quote(value, out),
        Nbt::ByteArray(values) => list(
            &mut values.iter().map(|value| format!("{}b", *value as i8)),
            "B;",
            out,
        ),
        Nbt::IntArray(values) => list(&mut values.iter().map(i32::to_string), "I;", out),
        Nbt::LongArray(values) => list(
            &mut values.iter().map(|value| format!("{value}L")),
            "L;",
            out,
        ),
        Nbt::List(values) => {
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_into(value, out);
            }
            out.push(']');
        }
        Nbt::Compound(compound) => {
            out.push('{');
            for (index, (key, value)) in compound.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if !key.is_empty()
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
                {
                    out.push_str(key);
                } else {
                    quote(key, out);
                }
                out.push(':');
                write_into(value, out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips() {
        let text = r#"{a:1b,b:-2s,c:3,d:4L,e:0.1f,f:-2.5d,g:"x\"y\\z","minecraft:k":[1,2],l:[B;1b,-1b],m:[I;7],n:[L;8L],o:[],p:{},q:[{r:1.0e-5f}]}"#;
        let value = parse(text).unwrap();
        let Nbt::Compound(compound) = &value else {
            panic!()
        };
        assert_eq!(compound.get("e"), Some(&Nbt::Float(0.1)));
        assert_eq!(compound.str("g"), Some("x\"y\\z"));
        assert_eq!(compound.get("l"), Some(&Nbt::ByteArray(vec![1, 255])));
        assert_eq!(parse(&write(&value)).unwrap(), value);
    }

    #[test]
    fn malformed_text_fails() {
        for text in [
            "",
            "{",
            "{a:}",
            "{a:1",
            "[1,1b]",
            "[I;1b]",
            "\"open",
            "1x",
            "{a:1}}",
            "{:1}",
            "[B;",
            "{a:\"\\n\"}",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
        let deep = "[".repeat(1000) + &"]".repeat(1000);
        assert!(parse(&deep).is_err());
    }
}
