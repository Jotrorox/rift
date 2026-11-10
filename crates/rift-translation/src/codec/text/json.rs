//! JSON as text components use it: an ordered value model, a reader as
//! lenient as Gson's lenient mode (old servers and plugins send what
//! vanilla's reader accepts, not just standard JSON) and a strict, compact
//! writer that escapes like vanilla's Gson.
use crate::wire::{MAX_NBT_DEPTH, invalid};
use std::{fmt::Write as _, io};

/// A JSON value with object members in input order. Numbers keep their
/// source text so a value read and written again does not change.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn str(value: impl Into<String>) -> Self {
        Json::String(value.into())
    }

    pub fn int(value: i64) -> Self {
        Json::Number(value.to_string())
    }

    /// A double as Java's `Double.toString` prints integral values (`2.0`).
    pub fn double(value: f64) -> Self {
        if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e7 {
            Json::Number(format!("{value:.1}"))
        } else {
            Json::Number(format!("{value}"))
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(members) => Some(members),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(values) => Some(values),
            _ => None,
        }
    }

    /// A string, number or boolean as text, the way Gson's `getAsString`
    /// reads any primitive.
    pub fn primitive_string(&self) -> Option<String> {
        match self {
            Json::String(value) => Some(value.clone()),
            Json::Number(text) => Some(java_number_text(text)),
            Json::Bool(value) => Some(value.to_string()),
            _ => None,
        }
    }

    /// Gson's `getAsBoolean`: a boolean, or any primitive whose text is
    /// `true` ignoring case.
    pub fn primitive_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            Json::String(_) | Json::Number(_) => {
                Some(self.primitive_string()?.eq_ignore_ascii_case("true"))
            }
            _ => None,
        }
    }

    /// A number's value; strings holding a number count too, as Gson
    /// parses them lazily.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(text) | Json::String(text) => parse_number(text),
            _ => None,
        }
    }

    /// An integral number (Gson's `getAsInt` truncates fractions).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Number(text) | Json::String(text) => text
                .parse::<i64>()
                .ok()
                .or_else(|| parse_number(text).map(|value| value as i64)),
            _ => None,
        }
    }

    /// Parse leniently; content after the first value is ignored, as
    /// vanilla's readers stop after one value.
    pub fn parse(text: &str) -> io::Result<Json> {
        let mut parser = Parser {
            input: text.as_bytes(),
            text,
            position: 0,
        };
        parser.value(0)
    }

    /// Compact JSON as vanilla's Gson writes it (without HTML escaping).
    pub fn write(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    fn write_into(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Json::Number(text) => out.push_str(text),
            Json::String(value) => write_string(value, out),
            Json::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    value.write_into(out);
                }
                out.push(']');
            }
            Json::Object(members) => {
                out.push('{');
                for (index, (name, value)) in members.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_string(name, out);
                    out.push(':');
                    value.write_into(out);
                }
                out.push('}');
            }
        }
    }
}

/// How Java prints a number Gson parsed lazily: `getAsString` returns the
/// source text unchanged.
fn java_number_text(text: &str) -> String {
    text.to_owned()
}

fn parse_number(text: &str) -> Option<f64> {
    let value: f64 = text.trim().parse().ok()?;
    Some(value)
}

fn write_string(value: &str, out: &mut String) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{c}' => out.push_str("\\f"),
            '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    input: &'a [u8],
    text: &'a str,
    position: usize,
}

/// Characters that end an unquoted literal in Gson's lenient mode.
fn ends_literal(byte: u8) -> bool {
    matches!(
        byte,
        b'/' | b'\\'
            | b';'
            | b'#'
            | b'='
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b':'
            | b','
            | b' '
            | b'\t'
            | b'\x0c'
            | b'\r'
            | b'\n'
            | b'"'
            | b'\''
    )
}

impl Parser<'_> {
    fn error(&self, message: &str) -> io::Error {
        invalid(&format!("invalid JSON at {}: {message}", self.position))
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }

    /// Skip whitespace and the comments lenient Gson allows.
    fn skip(&mut self) -> io::Result<()> {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => self.position += 1,
                b'#' => self.skip_line(),
                b'/' if self.input.get(self.position + 1) == Some(&b'/') => self.skip_line(),
                b'/' if self.input.get(self.position + 1) == Some(&b'*') => {
                    let rest = &self.text[self.position + 2..];
                    let end = rest
                        .find("*/")
                        .ok_or_else(|| self.error("unterminated comment"))?;
                    self.position += 2 + end + 2;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn skip_line(&mut self) {
        while let Some(byte) = self.peek() {
            self.position += 1;
            if byte == b'\n' || byte == b'\r' {
                break;
            }
        }
    }

    fn value(&mut self, depth: usize) -> io::Result<Json> {
        if depth > MAX_NBT_DEPTH {
            return Err(self.error("nesting limit exceeded"));
        }
        self.skip()?;
        match self.peek().ok_or_else(|| self.error("unexpected end"))? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            quote @ (b'"' | b'\'') => Ok(Json::String(self.string(quote)?)),
            _ => self.literal(),
        }
    }

    fn object(&mut self, depth: usize) -> io::Result<Json> {
        self.position += 1;
        let mut members: Vec<(String, Json)> = Vec::new();
        loop {
            self.skip()?;
            match self.peek() {
                Some(b'}') => {
                    self.position += 1;
                    return Ok(Json::Object(members));
                }
                None => return Err(self.error("unterminated object")),
                _ => {}
            }
            let name = match self.peek() {
                Some(quote @ (b'"' | b'\'')) => self.string(quote)?,
                _ => {
                    let literal = self.literal_text();
                    if literal.is_empty() {
                        return Err(self.error("expected a name"));
                    }
                    literal.to_owned()
                }
            };
            self.skip()?;
            match self.peek() {
                Some(b':') => self.position += 1,
                Some(b'=') => {
                    self.position += 1;
                    if self.peek() == Some(b'>') {
                        self.position += 1;
                    }
                }
                _ => return Err(self.error("expected ':'")),
            }
            let value = self.value(depth + 1)?;
            // Gson keeps the last of duplicate names.
            match members.iter_mut().find(|(known, _)| *known == name) {
                Some((_, slot)) => *slot = value,
                None => members.push((name, value)),
            }
            self.skip()?;
            match self.peek() {
                Some(b',' | b';') => self.position += 1,
                Some(b'}') => {}
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> io::Result<Json> {
        self.position += 1;
        let mut values = Vec::new();
        loop {
            self.skip()?;
            match self.peek() {
                Some(b']') => {
                    self.position += 1;
                    return Ok(Json::Array(values));
                }
                // An omitted value between separators reads as null.
                Some(b',' | b';') => {
                    self.position += 1;
                    values.push(Json::Null);
                    continue;
                }
                None => return Err(self.error("unterminated array")),
                _ => {}
            }
            values.push(self.value(depth + 1)?);
            self.skip()?;
            match self.peek() {
                Some(b',' | b';') => {
                    self.position += 1;
                    self.skip()?;
                    if self.peek() == Some(b']') {
                        values.push(Json::Null);
                    }
                }
                Some(b']') => {}
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn string(&mut self, quote: u8) -> io::Result<String> {
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
                        b'u' => {
                            let unit = self.hex4()?;
                            if (0xd800..0xdc00).contains(&unit)
                                && self.input.get(self.position) == Some(&b'\\')
                                && self.input.get(self.position + 1) == Some(&b'u')
                            {
                                let save = self.position;
                                self.position += 2;
                                let low = self.hex4()?;
                                if (0xdc00..0xe000).contains(&low) {
                                    let code = 0x10000
                                        + ((u32::from(unit) - 0xd800) << 10)
                                        + (u32::from(low) - 0xdc00);
                                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                    continue;
                                }
                                self.position = save;
                            }
                            out.push(char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}'));
                        }
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'n' | b'\n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b'f' => out.push('\u{c}'),
                        b'\'' | b'"' | b'\\' | b'/' => out.push(escape as char),
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

    fn hex4(&mut self) -> io::Result<u16> {
        let digits = self
            .text
            .get(self.position..self.position + 4)
            .ok_or_else(|| self.error("short \\u escape"))?;
        let unit = u16::from_str_radix(digits, 16).map_err(|_| self.error("bad \\u escape"))?;
        self.position += 4;
        Ok(unit)
    }

    fn literal_text(&mut self) -> &str {
        let start = self.position;
        while let Some(byte) = self.peek() {
            if ends_literal(byte) {
                break;
            }
            self.position += 1;
        }
        &self.text[start..self.position]
    }

    /// A keyword, a number, or (lenient) an unquoted string.
    fn literal(&mut self) -> io::Result<Json> {
        let text = self.literal_text();
        if text.is_empty() {
            return Err(self.error("unexpected character"));
        }
        Ok(if text.eq_ignore_ascii_case("true") {
            Json::Bool(true)
        } else if text.eq_ignore_ascii_case("false") {
            Json::Bool(false)
        } else if text.eq_ignore_ascii_case("null") {
            Json::Null
        } else if is_json_number(text) {
            Json::Number(text.to_owned())
        } else {
            Json::String(text.to_owned())
        })
    }
}

/// Whether `text` is a number in JSON's own grammar.
fn is_json_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    if bytes.first() == Some(&b'-') {
        index += 1;
    }
    let digits = |index: &mut usize| {
        let start = *index;
        while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
            *index += 1;
        }
        *index > start
    };
    if !digits(&mut index) {
        return false;
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        if !digits(&mut index) {
            return false;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        if !digits(&mut index) {
            return false;
        }
    }
    index == bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_json_round_trips_in_order() {
        let text = r#"{"text":"a\"b\\c\n\u0001","extra":[1,-2.5e3,true,null,{"z":1,"a":2}]}"#;
        let value = Json::parse(text).unwrap();
        assert_eq!(value.write(), text);
        assert_eq!(value.get("extra").unwrap().as_array().unwrap().len(), 5);
    }

    #[test]
    fn lenient_syntax_is_accepted() {
        let value =
            Json::parse("{text:'it''s' ; bold = TRUE, 'x'=>[1;;2,] # comment\n}").unwrap_err();
        assert!(value.to_string().contains("expected"));
        let value =
            Json::parse("/* c */ {text:'it\\'s' ; bold = TRUE, 'x'=>[1;;2,] # comment\n} junk")
                .unwrap();
        assert_eq!(value.get("text"), Some(&Json::str("it's")));
        assert_eq!(value.get("bold"), Some(&Json::Bool(true)));
        assert_eq!(
            value.get("x"),
            Some(&Json::Array(vec![
                Json::Number("1".into()),
                Json::Null,
                Json::Number("2".into()),
                Json::Null
            ]))
        );
        assert_eq!(Json::parse("hello").unwrap(), Json::str("hello"));
        assert_eq!(Json::parse("  42 ").unwrap(), Json::Number("42".into()));
        assert_eq!(Json::parse("1e").unwrap(), Json::str("1e"));
    }

    #[test]
    fn primitives_read_like_gson() {
        assert_eq!(Json::Number("5".into()).primitive_string().unwrap(), "5");
        assert_eq!(Json::str("TRUE").primitive_bool(), Some(true));
        assert_eq!(Json::Number("1".into()).primitive_bool(), Some(false));
        assert_eq!(Json::Object(vec![]).primitive_string(), None);
    }

    #[test]
    fn surrogates_and_errors() {
        assert_eq!(
            Json::parse(r#""\ud83d\ude00""#).unwrap(),
            Json::str("\u{1F600}")
        );
        assert!(Json::parse(r#""abc"#).is_err());
        assert!(Json::parse(r#""\q""#).is_err());
        assert!(Json::parse("{").is_err());
        assert!(Json::parse("").is_err());
        let deep = "[".repeat(1000);
        assert!(Json::parse(&deep).is_err());
        assert_eq!(Json::str("\u{2028}").write(), "\"\\u2028\"");
    }
}
