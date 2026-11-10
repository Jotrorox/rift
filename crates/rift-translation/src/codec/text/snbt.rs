//! Stringified NBT, as `/give` arguments, item tags in hover events and
//! `nbt` text components write it.
//!
//! The parser follows vanilla's `TagParser`: before 1.21.5 a strict grammar
//! (unquoted strings, `b`/`s`/`l`/`f`/`d` suffixes, typed arrays,
//! homogeneous lists); from 1.21.5 (protocol 770) the richer grammar that
//! also takes underscores in numbers, hexadecimal and binary literals,
//! signedness suffixes, more string escapes, `bool(..)`/`uuid(..)` and lists
//! of mixed types (stored wrapped, as vanilla does). The printer writes the
//! format of the given protocol: strings in double quotes before 1.14, the
//! quote that needs no escaping from then on.
use super::java_order;
use crate::{
    nbt::{Compound, Nbt},
    wire::{MAX_NBT_DEPTH, invalid},
};
use std::io;

/// Protocol 1.21.5 introduced the extended SNBT grammar.
const EXTENDED: i32 = 770;
/// Protocol 1.14 started choosing the quote character.
const SMART_QUOTES: i32 = 477;

/// Parse one SNBT value as `protocol`'s `TagParser` would; trailing input
/// other than whitespace is an error.
pub fn parse(protocol: i32, text: &str) -> io::Result<Nbt> {
    let mut parser = Parser {
        text,
        position: 0,
        extended: protocol >= EXTENDED,
    };
    let value = parser.value(0)?;
    parser.skip();
    if parser.position != text.len() {
        return Err(parser.error("trailing data"));
    }
    Ok(value)
}

/// Parse an SNBT compound, as item tags and entity hover data are.
pub fn parse_compound(protocol: i32, text: &str) -> io::Result<Compound> {
    match parse(protocol, text)? {
        Nbt::Compound(compound) => Ok(compound),
        _ => Err(invalid("SNBT is not a compound")),
    }
}

/// Print `value` the way `protocol`'s `Tag.toString` does. Compound keys
/// come out in the order Java's `HashMap` iterates them.
pub fn print(protocol: i32, value: &Nbt) -> String {
    let mut out = String::new();
    write_value(protocol, value, &mut out);
    out
}

fn write_value(protocol: i32, value: &Nbt, out: &mut String) {
    match value {
        Nbt::Byte(value) => {
            out.push_str(&value.to_string());
            out.push('b');
        }
        Nbt::Short(value) => {
            out.push_str(&value.to_string());
            out.push('s');
        }
        Nbt::Int(value) => out.push_str(&value.to_string()),
        Nbt::Long(value) => {
            out.push_str(&value.to_string());
            out.push('L');
        }
        Nbt::Float(value) => {
            out.push_str(&java_float(f64::from(*value), true));
            out.push('f');
        }
        Nbt::Double(value) => {
            out.push_str(&java_float(*value, false));
            out.push('d');
        }
        Nbt::String(value) => quote(protocol, value, out),
        Nbt::ByteArray(values) => {
            out.push_str("[B;");
            join(out, values, |out, value| {
                out.push_str(&(*value as i8).to_string());
                out.push('B');
            });
            out.push(']');
        }
        Nbt::IntArray(values) => {
            out.push_str("[I;");
            join(out, values, |out, value| out.push_str(&value.to_string()));
            out.push(']');
        }
        Nbt::LongArray(values) => {
            out.push_str("[L;");
            join(out, values, |out, value| {
                out.push_str(&value.to_string());
                out.push('L');
            });
            out.push(']');
        }
        Nbt::List(values) => {
            out.push('[');
            join(out, values, |out, value| write_value(protocol, value, out));
            out.push(']');
        }
        Nbt::Compound(compound) => {
            out.push('{');
            let keys: Vec<&str> = compound.0.iter().map(|(key, _)| key.as_str()).collect();
            let order = java_order(&keys);
            for (position, index) in order.into_iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                let (key, value) = &compound.0[index];
                if is_simple(key) {
                    out.push_str(key);
                } else {
                    quote(protocol, key, out);
                }
                out.push(':');
                write_value(protocol, value, out);
            }
            out.push('}');
        }
    }
}

fn join<T>(out: &mut String, values: &[T], mut each: impl FnMut(&mut String, &T)) {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        each(out, value);
    }
}

/// Keys that print without quotes: `[0-9A-Za-z_.+-]+`.
fn is_simple(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
}

fn quote(protocol: i32, text: &str, out: &mut String) {
    if protocol < SMART_QUOTES {
        out.push('"');
        for character in text.chars() {
            if character == '"' || character == '\\' {
                out.push('\\');
            }
            out.push(character);
        }
        out.push('"');
        return;
    }
    // `StringTag.quoteAndEscape`: the first quote character found picks the
    // other one, so that it needs no escaping.
    let mut quote = None;
    let mut body = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => body.push('\\'),
            '"' | '\'' => {
                let chosen = *quote.get_or_insert(if character == '"' { '\'' } else { '"' });
                if chosen == character {
                    body.push('\\');
                }
            }
            '\n' if protocol >= EXTENDED => {
                body.push_str("\\n");
                continue;
            }
            '\t' if protocol >= EXTENDED => {
                body.push_str("\\t");
                continue;
            }
            _ => {}
        }
        body.push(character);
    }
    let quote = quote.unwrap_or('"');
    out.push(quote);
    out.push_str(&body);
    out.push(quote);
}

/// Java's `Float.toString`/`Double.toString`: the shortest digits that
/// round-trip, in plain notation from 10^-3 to 10^7 and computerized
/// scientific notation (`1.0E-5`) outside.
pub(super) fn java_float(value: f64, single: bool) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .into();
    }
    // Shortest round-trip digits and exponent from Rust's formatter.
    let scientific = if single {
        format!("{:e}", value as f32)
    } else {
        format!("{value:e}")
    };
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let magnitude = value.abs();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if (1e-3..1e7).contains(&magnitude) {
        if exponent >= 0 {
            let split = (exponent + 1) as usize;
            if digits.len() <= split {
                out.push_str(&digits);
                out.push_str(&"0".repeat(split - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..split]);
                out.push('.');
                out.push_str(&digits[split..]);
            }
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exponent - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        out.push_str(if digits.len() > 1 { &digits[1..] } else { "0" });
        out.push('E');
        out.push_str(&exponent.to_string());
    }
    out
}

struct Parser<'a> {
    text: &'a str,
    position: usize,
    extended: bool,
}

fn is_unquoted(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+')
}

impl Parser<'_> {
    fn error(&self, message: &str) -> io::Error {
        invalid(&format!("invalid SNBT at {}: {message}", self.position))
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.position).copied()
    }

    fn skip(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> io::Result<()> {
        self.skip();
        if self.peek() == Some(byte) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected '{}'", byte as char)))
        }
    }

    fn value(&mut self, depth: usize) -> io::Result<Nbt> {
        if depth > MAX_NBT_DEPTH {
            return Err(self.error("nesting limit exceeded"));
        }
        self.skip();
        match self.peek() {
            Some(b'{') => self.compound(depth).map(Nbt::Compound),
            Some(b'[') => self.list(depth),
            Some(quote @ (b'"' | b'\'')) => self.quoted(quote).map(Nbt::String),
            Some(_) => self.unquoted_value(depth),
            None => Err(self.error("expected a value")),
        }
    }

    fn key(&mut self) -> io::Result<String> {
        self.skip();
        match self.peek() {
            Some(quote @ (b'"' | b'\'')) => self.quoted(quote),
            _ => {
                let word = self.word();
                if word.is_empty() {
                    Err(self.error("expected a key"))
                } else {
                    Ok(word.to_owned())
                }
            }
        }
    }

    fn word(&mut self) -> &str {
        let start = self.position;
        while self.peek().is_some_and(is_unquoted) {
            self.position += 1;
        }
        &self.text[start..self.position]
    }

    fn compound(&mut self, depth: usize) -> io::Result<Compound> {
        self.position += 1;
        let mut compound = Compound::new();
        self.skip();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Ok(compound);
        }
        loop {
            let key = self.key()?;
            self.expect(b':')?;
            let value = self.value(depth + 1)?;
            compound.insert(key, value);
            self.skip();
            match self.peek() {
                Some(b',') => {
                    self.position += 1;
                    self.skip();
                    // The extended grammar allows a trailing separator.
                    if self.extended && self.peek() == Some(b'}') {
                        self.position += 1;
                        return Ok(compound);
                    }
                }
                Some(b'}') => {
                    self.position += 1;
                    return Ok(compound);
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn list(&mut self, depth: usize) -> io::Result<Nbt> {
        let bytes = self.text.as_bytes();
        let typed = bytes.get(self.position + 2) == Some(&b';')
            && matches!(bytes.get(self.position + 1), Some(b'B' | b'I' | b'L'));
        if typed {
            let kind = bytes[self.position + 1];
            self.position += 3;
            return self.array(kind, depth);
        }
        self.position += 1;
        let mut values = Vec::new();
        self.skip();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Ok(Nbt::List(values));
        }
        loop {
            let value = self.value(depth + 1)?;
            values.push(value);
            self.skip();
            match self.peek() {
                Some(b',') => {
                    self.position += 1;
                    self.skip();
                    if self.extended && self.peek() == Some(b']') {
                        self.position += 1;
                        break;
                    }
                }
                Some(b']') => {
                    self.position += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
        let kind = values[0].kind();
        if values.iter().all(|value| value.kind() == kind) {
            return Ok(Nbt::List(values));
        }
        if !self.extended {
            return Err(self.error("list elements of different types"));
        }
        // A mixed list is stored as compounds, non-compounds wrapped under "".
        Ok(Nbt::List(values.into_iter().map(wrap_element).collect()))
    }

    fn array(&mut self, kind: u8, depth: usize) -> io::Result<Nbt> {
        let mut numbers = Vec::new();
        self.skip();
        if self.peek() != Some(b']') {
            loop {
                let value = self.value(depth + 1)?;
                let number = match (kind, &value) {
                    (b'B', Nbt::Byte(value)) => i64::from(*value),
                    (b'I', Nbt::Int(value)) => i64::from(*value),
                    (b'L', Nbt::Long(value)) => *value,
                    // The extended grammar takes any integer that fits.
                    (_, value) if self.extended && value.kind() <= 4 => {
                        value.as_i64().unwrap_or_default()
                    }
                    _ => return Err(self.error("invalid array element")),
                };
                numbers.push(number);
                self.skip();
                match self.peek() {
                    Some(b',') => {
                        self.position += 1;
                        self.skip();
                        if self.extended && self.peek() == Some(b']') {
                            break;
                        }
                    }
                    Some(b']') => break,
                    _ => return Err(self.error("expected ',' or ']'")),
                }
            }
        }
        self.position += 1;
        let fits = |low: i64, high: i64| numbers.iter().all(|value| (low..=high).contains(value));
        Ok(match kind {
            b'B' if fits(-128, 255) => {
                Nbt::ByteArray(numbers.iter().map(|value| *value as u8).collect())
            }
            b'I' if fits(i64::from(i32::MIN), i64::from(u32::MAX)) => {
                Nbt::IntArray(numbers.iter().map(|value| *value as i32).collect())
            }
            b'L' => Nbt::LongArray(numbers),
            _ => return Err(self.error("array element out of range")),
        })
    }

    fn quoted(&mut self, quote: u8) -> io::Result<String> {
        self.position += 1;
        let mut out = String::new();
        loop {
            let start = self.position;
            while let Some(byte) = self.peek() {
                if byte == quote || byte == b'\\' {
                    break;
                }
                self.position += 1;
            }
            out.push_str(&self.text[start..self.position]);
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'\\') => {
                    self.position += 1;
                    let escape = self.peek().ok_or_else(|| self.error("bad escape"))?;
                    self.position += 1;
                    match escape {
                        b'\\' | b'"' | b'\'' => out.push(escape as char),
                        _ if !self.extended => return Err(self.error("invalid escape sequence")),
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b's' => out.push(' '),
                        b'x' => out.push(self.hex_escape(2)?),
                        b'u' => out.push(self.hex_escape(4)?),
                        b'U' => out.push(self.hex_escape(8)?),
                        _ => return Err(self.error("invalid escape sequence")),
                    }
                }
                Some(_) => {
                    self.position += 1;
                    return Ok(out);
                }
            }
        }
    }

    fn hex_escape(&mut self, digits: usize) -> io::Result<char> {
        let text = self
            .text
            .get(self.position..self.position + digits)
            .ok_or_else(|| self.error("short escape"))?;
        let code = u32::from_str_radix(text, 16).map_err(|_| self.error("bad escape"))?;
        self.position += digits;
        char::from_u32(code).ok_or_else(|| self.error("invalid code point"))
    }

    fn unquoted_value(&mut self, depth: usize) -> io::Result<Nbt> {
        let start = self.position;
        let word = self.word().to_owned();
        if word.is_empty() {
            return Err(self.error("expected a value"));
        }
        if self.extended {
            self.skip();
            if self.peek() == Some(b'(') {
                return self.operation(&word, depth);
            }
            if let Some(value) = extended_number(&word) {
                return value.map_err(|message| {
                    self.position = start;
                    self.error(message)
                });
            }
            return Ok(match word.as_str() {
                "true" => Nbt::Byte(1),
                "false" => Nbt::Byte(0),
                _ => Nbt::String(word),
            });
        }
        Ok(legacy_typed(&word))
    }

    /// `bool(..)` and `uuid(..)`, the extended grammar's operations.
    fn operation(&mut self, name: &str, depth: usize) -> io::Result<Nbt> {
        self.position += 1;
        let argument = self.value(depth + 1)?;
        self.expect(b')')?;
        match (name, argument) {
            ("bool", Nbt::Byte(value)) => Ok(Nbt::Byte(i8::from(value != 0))),
            ("bool", value) if value.as_f64().is_some() => {
                Ok(Nbt::Byte(i8::from(value.as_f64() != Some(0.0))))
            }
            ("uuid", Nbt::String(text)) => {
                let uuid = super::parse_uuid(&text).ok_or_else(|| self.error("invalid UUID"))?;
                Ok(Nbt::IntArray(super::uuid_ints(uuid).to_vec()))
            }
            _ => Err(self.error("unknown operation")),
        }
    }
}

fn wrap_element(value: Nbt) -> Nbt {
    match value {
        Nbt::Compound(compound) if !(compound.len() == 1 && compound.contains("")) => {
            Nbt::Compound(compound)
        }
        other => Nbt::Compound(Compound(vec![(String::new(), other)])),
    }
}

/// Pre-1.21.5 `TagParser.type`: numbers by suffix, `true`/`false`, else a
/// string (also for numbers that overflow their type).
fn legacy_typed(word: &str) -> Nbt {
    let lower = word.to_ascii_lowercase();
    let (body, suffix) = match lower.as_bytes().last() {
        Some(b'b' | b's' | b'l' | b'f' | b'd') => {
            (&word[..word.len() - 1], lower.as_bytes()[lower.len() - 1])
        }
        _ => (word, 0),
    };
    let integer = |text: &str| {
        let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
        !digits.is_empty()
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'))
    };
    let decimal = |text: &str, dot_required: bool| {
        let text = text.strip_prefix(['-', '+']).unwrap_or(text);
        let (mantissa, exponent) = match text.find(['e', 'E']) {
            Some(index) => (&text[..index], Some(&text[index + 1..])),
            None => (text, None),
        };
        let valid_mantissa = match mantissa.split_once('.') {
            Some((whole, fraction)) => {
                whole.bytes().all(|byte| byte.is_ascii_digit())
                    && fraction.bytes().all(|byte| byte.is_ascii_digit())
                    && !(whole.is_empty() && fraction.is_empty())
            }
            None => {
                !dot_required
                    && !mantissa.is_empty()
                    && mantissa.bytes().all(|byte| byte.is_ascii_digit())
            }
        };
        let valid_exponent = exponent.is_none_or(|exponent| {
            let digits = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        });
        valid_mantissa && valid_exponent
    };
    let parsed = match suffix {
        b'b' if integer(body) => body.parse::<i8>().ok().map(Nbt::Byte),
        b's' if integer(body) => body.parse::<i16>().ok().map(Nbt::Short),
        b'l' if integer(body) => body.parse::<i64>().ok().map(Nbt::Long),
        b'f' if decimal(body, false) => body
            .parse::<f32>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Nbt::Float),
        b'd' if decimal(body, false) => body
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Nbt::Double),
        _ if integer(word) => word.parse::<i32>().ok().map(Nbt::Int),
        _ if decimal(word, true) => word
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Nbt::Double),
        _ => None,
    };
    parsed.unwrap_or_else(|| match lower.as_str() {
        "true" => Nbt::Byte(1),
        "false" => Nbt::Byte(0),
        _ => Nbt::String(word.to_owned()),
    })
}

/// A number in the 1.21.5 grammar, `None` when `word` is not one, or an
/// error when it looks like one but is out of range or malformed.
fn extended_number(word: &str) -> Option<Result<Nbt, &'static str>> {
    let first = word.bytes().next()?;
    if !(first.is_ascii_digit() || matches!(first, b'-' | b'+' | b'.')) {
        return None;
    }
    let (sign, body) = match word.strip_prefix('-') {
        Some(rest) => (-1i128, rest),
        None => (1, word.strip_prefix('+').unwrap_or(word)),
    };
    if body.is_empty() || !(body.as_bytes()[0].is_ascii_digit() || body.starts_with('.')) {
        return None;
    }
    let lower = body.to_ascii_lowercase();
    let (radix, digits) = if let Some(rest) = lower.strip_prefix("0x") {
        (16, rest)
    } else if let Some(rest) = lower.strip_prefix("0b").filter(|rest| {
        rest.bytes()
            .next()
            .is_some_and(|byte| byte == b'0' || byte == b'1')
    }) {
        (2, rest)
    } else {
        (10, lower.as_str())
    };
    // Integer suffixes, optionally with signedness: `b`, `ub`, `sb`, ...
    let integer_suffix = [
        "ub", "us", "ui", "ul", "sb", "ss", "si", "sl", "b", "s", "i", "l",
    ]
    .into_iter()
    .find(|suffix| digits.ends_with(suffix) && (radix != 16 || !matches!(*suffix, "b")));
    let float_suffix = (radix == 10)
        .then(|| {
            ["f", "d"]
                .into_iter()
                .find(|suffix| digits.ends_with(suffix))
        })
        .flatten();
    let (number, suffix) = match (integer_suffix, float_suffix) {
        (Some(suffix), _) => (&digits[..digits.len() - suffix.len()], suffix),
        (None, Some(suffix)) => (&digits[..digits.len() - 1], suffix),
        (None, None) => (digits, ""),
    };
    let clean: String = number
        .chars()
        .filter(|character| *character != '_')
        .collect();
    if clean.is_empty() || number.starts_with('_') || number.ends_with('_') {
        return Some(Err("invalid number"));
    }
    let is_float = radix == 10
        && (matches!(suffix, "f" | "d")
            || (suffix.is_empty() && (clean.contains('.') || clean.contains('e'))));
    if is_float {
        let value: f64 = match clean.parse() {
            Ok(value) => value,
            Err(_) => return Some(Err("invalid number")),
        };
        let value = value * sign as f64;
        return Some(Ok(match suffix {
            "f" => Nbt::Float(value as f32),
            _ => Nbt::Double(value),
        }));
    }
    let magnitude = match u128::from_str_radix(&clean, radix) {
        Ok(value) => value as i128,
        Err(_) => return Some(Err("invalid number")),
    };
    let value = sign * magnitude;
    let unsigned = suffix.starts_with('u');
    let explicit_signed = suffix.starts_with('s') && suffix.len() == 2;
    // Hexadecimal and binary literals without a sign prefix may use the
    // whole unsigned range of their type (`0xff` as a byte is -1).
    let wraps = !explicit_signed && radix != 10 && sign == 1;
    let fit = |bits: u32| -> Result<i64, &'static str> {
        let (low, high) = if unsigned {
            (0, (1i128 << bits) - 1)
        } else if wraps {
            (-(1i128 << (bits - 1)), (1i128 << bits) - 1)
        } else {
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        };
        if value < low || value > high {
            return Err("number out of range");
        }
        let truncated = value as i64;
        Ok(if bits < 64 {
            (truncated << (64 - bits)) >> (64 - bits)
        } else {
            truncated
        })
    };
    let kind = suffix.trim_start_matches(['u', 's']);
    let kind = if suffix.len() == 1 { suffix } else { kind };
    Some(match kind {
        "b" => fit(8).map(|value| Nbt::Byte(value as i8)),
        "s" => fit(16).map(|value| Nbt::Short(value as i16)),
        "l" => fit(64).map(Nbt::Long),
        _ => fit(32).map(|value| Nbt::Int(value as i32)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compound(entries: &[(&str, Nbt)]) -> Nbt {
        Nbt::Compound(Compound(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        ))
    }

    #[test]
    fn legacy_values_parse_by_suffix() {
        let value = parse(
            340,
            r#"{id:"minecraft:stone",Count:2b,Damage:3s,big:5L,f:1.5f,d:2.0,dd:3d,i:-7,s:hello,t:true,q:'it\'s',n:1e3}"#,
        )
        .unwrap();
        let value = value.as_compound().unwrap();
        assert_eq!(value.get("Count"), Some(&Nbt::Byte(2)));
        assert_eq!(value.get("Damage"), Some(&Nbt::Short(3)));
        assert_eq!(value.get("big"), Some(&Nbt::Long(5)));
        assert_eq!(value.get("f"), Some(&Nbt::Float(1.5)));
        assert_eq!(value.get("d"), Some(&Nbt::Double(2.0)));
        assert_eq!(value.get("dd"), Some(&Nbt::Double(3.0)));
        assert_eq!(value.get("i"), Some(&Nbt::Int(-7)));
        assert_eq!(value.get("s"), Some(&Nbt::String("hello".into())));
        assert_eq!(value.get("t"), Some(&Nbt::Byte(1)));
        assert_eq!(value.get("q"), Some(&Nbt::String("it's".into())));
        // An exponent without a dot is not a number before 1.21.5.
        assert_eq!(value.get("n"), Some(&Nbt::String("1e3".into())));
        // Overflowing numbers become strings, as in TagParser.
        assert_eq!(parse(340, "300b").unwrap(), Nbt::String("300b".into()));
        assert_eq!(parse(340, "007").unwrap(), Nbt::String("007".into()));
    }

    #[test]
    fn arrays_lists_and_errors() {
        assert_eq!(
            parse(404, "[B;1b,-2B]").unwrap(),
            Nbt::ByteArray(vec![1, 254])
        );
        assert_eq!(parse(404, "[I; 1, 2]").unwrap(), Nbt::IntArray(vec![1, 2]));
        assert_eq!(parse(404, "[L;1l]").unwrap(), Nbt::LongArray(vec![1]));
        assert_eq!(parse(404, "[]").unwrap(), Nbt::List(vec![]));
        assert!(parse(404, "[I;1b]").is_err());
        assert!(parse(404, "[1,a]").is_err());
        assert!(parse(404, "{a:1,}").is_err());
        assert!(parse(404, "{a:1} x").is_err());
        assert!(parse(404, r#""\n""#).is_err());
        assert!(parse(404, "").is_err());
        assert!(parse(404, &"[".repeat(2000)).is_err());
    }

    #[test]
    fn extended_grammar() {
        assert!(
            parse(770, "0x1ffub")
                .unwrap_err()
                .to_string()
                .contains("range")
        );
        assert_eq!(parse(770, "0xffb").unwrap(), Nbt::Int(0xffb));
        assert!(parse(770, "12abc").is_err());
        assert_eq!(parse(770, "0xfeub").unwrap(), Nbt::Byte(-2));
        assert_eq!(parse(770, "1_000").unwrap(), Nbt::Int(1000));
        assert_eq!(parse(770, "0b101").unwrap(), Nbt::Int(5));
        assert_eq!(parse(770, "1e3").unwrap(), Nbt::Double(1000.0));
        assert_eq!(parse(770, "-5sb").unwrap(), Nbt::Byte(-5));
        assert_eq!(
            parse(770, r#""a\nb\x41""#).unwrap(),
            Nbt::String("a\nbA".into())
        );
        assert_eq!(parse(770, "bool(5)").unwrap(), Nbt::Byte(1));
        assert_eq!(
            parse(770, "[1,a,{x:1}]").unwrap(),
            Nbt::List(vec![
                compound(&[("", Nbt::Int(1))]),
                compound(&[("", Nbt::String("a".into()))]),
                compound(&[("x", Nbt::Int(1))]),
            ])
        );
        assert_eq!(
            parse(770, "[1,2,]").unwrap(),
            Nbt::List(vec![Nbt::Int(1), Nbt::Int(2)])
        );
        assert_eq!(
            parse(770, r#"uuid("00000001-0002-0003-0004-000000000005")"#).unwrap(),
            Nbt::IntArray(vec![1, 0x0002_0003, 0x0004_0000, 5])
        );
    }

    #[test]
    fn printing_follows_each_era() {
        let value = compound(&[
            ("id", Nbt::String("minecraft:stone".into())),
            ("Count", Nbt::Byte(1)),
            ("odd key", Nbt::String("say \"hi\"".into())),
            ("f", Nbt::Float(0.1)),
            ("d", Nbt::Double(1e-5)),
            ("a", Nbt::ByteArray(vec![1, 255])),
        ]);
        let old = print(340, &value);
        let new = print(477, &value);
        assert!(old.contains(r#""odd key":"say \"hi\"""#), "{old}");
        assert!(new.contains(r#""odd key":'say "hi"'"#), "{new}");
        assert!(old.contains("f:0.1f") && old.contains("d:1.0E-5d"), "{old}");
        assert!(old.contains("a:[B;1B,-1B]"), "{old}");
        for protocol in [340, 477, 770] {
            assert_eq!(parse(protocol, &print(protocol, &value)).unwrap(), {
                // Printing does not keep key order; compare by content.
                let parsed = parse(protocol, &print(protocol, &value)).unwrap();
                let mut entries = parsed.as_compound().unwrap().0.clone();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                let mut expected = value.as_compound().unwrap().0.clone();
                expected.sort_by(|a, b| a.0.cmp(&b.0));
                assert_eq!(entries, expected);
                parsed
            });
        }
    }

    #[test]
    fn java_float_formatting() {
        assert_eq!(java_float(1.0, false), "1.0");
        assert_eq!(java_float(1234567.0, false), "1234567.0");
        assert_eq!(java_float(12345678.0, false), "1.2345678E7");
        assert_eq!(java_float(0.001, false), "0.001");
        assert_eq!(java_float(0.0001, false), "1.0E-4");
        assert_eq!(java_float(f64::from(0.1f32), true), "0.1");
        assert_eq!(java_float(-2.5, false), "-2.5");
        assert_eq!(java_float(100.0, false), "100.0");
    }
}
