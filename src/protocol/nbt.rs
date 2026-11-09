//! Bounded skipping of named NBT used by pre-configuration dimension registries.
//! Values are copied verbatim; Rift does not interpret or rewrite registries.
//! Backend-supplied JSON text components are encoded for 1.20.3+ clients, and
//! dialog click payloads (untrusted client NBT) are decoded into JSON.
use super::{invalid, packets::take, write_modified_utf8};
use serde_json::Value;
use std::io;

fn string(bytes: &mut &[u8]) -> io::Result<()> {
    let length = u16::from_be_bytes(take(bytes, 2)?.try_into().unwrap());
    take(bytes, usize::from(length))?;
    Ok(())
}

fn payload(bytes: &mut &[u8], tag: u8, depth: usize) -> io::Result<()> {
    if depth > 64 {
        return Err(invalid("NBT nesting limit exceeded"));
    }
    match tag {
        1..=6 => {
            take(bytes, [1, 2, 4, 8, 4, 8][usize::from(tag - 1)])?;
        }
        7 | 11 | 12 => {
            let count = i32::from_be_bytes(take(bytes, 4)?.try_into().unwrap());
            let width = match tag {
                7 => 1,
                11 => 4,
                _ => 8,
            };
            let length = usize::try_from(count)
                .ok()
                .and_then(|n| n.checked_mul(width))
                .ok_or_else(|| invalid("invalid NBT array length"))?;
            take(bytes, length)?;
        }
        8 => string(bytes)?,
        9 => {
            let kind = take(bytes, 1)?[0];
            let count = i32::from_be_bytes(take(bytes, 4)?.try_into().unwrap());
            if count < 0 || count as usize > bytes.len() || (count > 0 && kind == 0) {
                return Err(invalid("invalid NBT list"));
            }
            for _ in 0..count {
                payload(bytes, kind, depth + 1)?;
            }
        }
        10 => loop {
            let kind = take(bytes, 1)?[0];
            if kind == 0 {
                break;
            }
            string(bytes)?;
            payload(bytes, kind, depth + 1)?;
        },
        _ => return Err(invalid("invalid NBT tag")),
    }
    Ok(())
}

pub(super) fn named<'a>(bytes: &mut &'a [u8]) -> io::Result<&'a [u8]> {
    let start = *bytes;
    let kind = take(bytes, 1)?[0];
    if kind != 10 {
        return Err(invalid("expected NBT compound"));
    }
    string(bytes)?;
    payload(bytes, kind, 0)?;
    Ok(&start[..start.len() - bytes.len()])
}

/// Skip one anonymous network NBT tag, as used for 1.20.3+ text components.
pub(super) fn anonymous(bytes: &mut &[u8]) -> io::Result<()> {
    let kind = take(bytes, 1)?[0];
    payload(bytes, kind, 0)
}

/// Encode a JSON text component as anonymous network NBT, following the
/// vanilla JsonOps-to-NbtOps conversion: integers become TAG_Int (or TAG_Long),
/// other numbers TAG_Double, booleans TAG_Byte, and object nulls are omitted.
pub(super) fn component(value: &Value, bytes: &mut Vec<u8>) -> io::Result<()> {
    bytes.push(json_tag(value)?);
    json_payload(value, bytes, 0)
}

fn json_tag(value: &Value) -> io::Result<u8> {
    Ok(match value {
        Value::Null => return Err(invalid("JSON null has no NBT form")),
        Value::Bool(_) => 1,
        Value::Number(number) => match number.as_i64() {
            Some(number) if i32::try_from(number).is_ok() => 3,
            Some(_) => 4,
            None => 6,
        },
        Value::String(_) => 8,
        Value::Array(_) => 9,
        Value::Object(_) => 10,
    })
}

fn json_payload(value: &Value, bytes: &mut Vec<u8>, depth: usize) -> io::Result<()> {
    // serde_json stops parsing at the same depth.
    if depth > 128 {
        return Err(invalid("JSON component nesting limit exceeded"));
    }
    match value {
        Value::Null => return Err(invalid("JSON null has no NBT form")),
        Value::Bool(value) => bytes.push(u8::from(*value)),
        Value::Number(number) => match (json_tag(value)?, number.as_i64()) {
            (3, Some(number)) => bytes.extend((number as i32).to_be_bytes()),
            (4, Some(number)) => bytes.extend(number.to_be_bytes()),
            _ => bytes.extend(number.as_f64().unwrap_or(f64::NAN).to_be_bytes()),
        },
        Value::String(text) => write_modified_utf8(text, bytes)?,
        Value::Array(items) => {
            let tags = items.iter().map(json_tag).collect::<io::Result<Vec<_>>>()?;
            let homogeneous = tags.windows(2).all(|pair| pair[0] == pair[1]);
            // Mixed lists hold compounds; others are wrapped as {"": value}.
            bytes.push(match tags.first() {
                None => 0,
                Some(&tag) if homogeneous => tag,
                Some(_) => 10,
            });
            let count = i32::try_from(items.len()).map_err(|_| invalid("JSON list too long"))?;
            bytes.extend(count.to_be_bytes());
            for (item, tag) in items.iter().zip(tags) {
                if homogeneous || tag == 10 {
                    json_payload(item, bytes, depth + 1)?;
                } else {
                    bytes.push(tag);
                    write_modified_utf8("", bytes)?;
                    json_payload(item, bytes, depth + 1)?;
                    bytes.push(0);
                }
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields.iter().filter(|(_, value)| !value.is_null()) {
                bytes.push(json_tag(value)?);
                write_modified_utf8(key, bytes)?;
                json_payload(value, bytes, depth + 1)?;
            }
            bytes.push(0);
        }
    }
    Ok(())
}

/// Decode Java's modified UTF-8 (UTF-16 units, NUL as two bytes).
fn read_modified_utf8(bytes: &mut &[u8]) -> io::Result<String> {
    let length = u16::from_be_bytes(take(bytes, 2)?.try_into().unwrap());
    let mut data = take(bytes, usize::from(length))?;
    let mut units = Vec::with_capacity(data.len());
    let malformed = || invalid("malformed modified UTF-8 in NBT");
    while let Some((&first, rest)) = data.split_first() {
        data = rest;
        let unit = match first {
            0x01..=0x7f => u16::from(first),
            0xc0..=0xdf => {
                let (&second, rest) = data.split_first().ok_or_else(malformed)?;
                data = rest;
                if second & 0xc0 != 0x80 {
                    return Err(malformed());
                }
                (u16::from(first & 0x1f) << 6) | u16::from(second & 0x3f)
            }
            0xe0..=0xef => {
                let [second, third, ..] = *data else {
                    return Err(malformed());
                };
                data = &data[2..];
                if second & 0xc0 != 0x80 || third & 0xc0 != 0x80 {
                    return Err(malformed());
                }
                (u16::from(first & 0x0f) << 12)
                    | (u16::from(second & 0x3f) << 6)
                    | u16::from(third & 0x3f)
            }
            _ => return Err(malformed()),
        };
        units.push(unit);
    }
    String::from_utf16(&units).map_err(|_| malformed())
}

/// The JSON form of one anonymous network NBT tag, the inverse of
/// [`component`] as far as JSON allows: numbers become JSON numbers (floats
/// keep their shortest decimal form), arrays and lists JSON arrays and
/// compounds objects. Booleans arrive as the bytes 1 and 0.
pub(super) fn to_json(bytes: &mut &[u8]) -> io::Result<Value> {
    let kind = take(bytes, 1)?[0];
    tag_json(bytes, kind, 0)
}

fn tag_json(bytes: &mut &[u8], tag: u8, depth: usize) -> io::Result<Value> {
    // Vanilla accepts untrusted tags at most 16 levels deep.
    if depth > 32 {
        return Err(invalid("NBT nesting limit exceeded"));
    }
    let fixed = |bytes: &mut &[u8], width: usize| -> io::Result<[u8; 8]> {
        let mut buffer = [0; 8];
        buffer[8 - width..].copy_from_slice(take(bytes, width)?);
        Ok(buffer)
    };
    Ok(match tag {
        1 => Value::from(take(bytes, 1)?[0] as i8),
        2 => Value::from(i16::from_be_bytes(take(bytes, 2)?.try_into().unwrap())),
        3 => Value::from(i32::from_be_bytes(take(bytes, 4)?.try_into().unwrap())),
        4 => Value::from(i64::from_be_bytes(fixed(bytes, 8)?)),
        5 => {
            let value = f32::from_be_bytes(take(bytes, 4)?.try_into().unwrap());
            // The shortest decimal that reads back as this f32: 0.3, not
            // 0.30000001192092896.
            number(value.to_string().parse().unwrap_or(f64::NAN))
        }
        6 => number(f64::from_be_bytes(fixed(bytes, 8)?)),
        7 | 11 | 12 => {
            let count = i32::from_be_bytes(take(bytes, 4)?.try_into().unwrap());
            let width = match tag {
                7 => 1,
                11 => 4,
                _ => 8,
            };
            let count = usize::try_from(count)
                .ok()
                .filter(|count| count.checked_mul(width).is_some_and(|n| n <= bytes.len()))
                .ok_or_else(|| invalid("invalid NBT array length"))?;
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                let raw = take(bytes, width)?;
                values.push(match width {
                    1 => Value::from(raw[0] as i8),
                    4 => Value::from(i32::from_be_bytes(raw.try_into().unwrap())),
                    _ => Value::from(i64::from_be_bytes(raw.try_into().unwrap())),
                });
            }
            Value::Array(values)
        }
        8 => Value::String(read_modified_utf8(bytes)?),
        9 => {
            let kind = take(bytes, 1)?[0];
            let count = i32::from_be_bytes(take(bytes, 4)?.try_into().unwrap());
            if count < 0 || count as usize > bytes.len() || (count > 0 && kind == 0) {
                return Err(invalid("invalid NBT list"));
            }
            let mut values = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let value = tag_json(bytes, kind, depth + 1)?;
                // A mixed list wraps entries that are not compounds as {"": value}.
                values.push(match value {
                    Value::Object(mut fields) if kind == 10 && fields.len() == 1 => {
                        match fields.remove("") {
                            Some(inner) => inner,
                            None => Value::Object(fields),
                        }
                    }
                    value => value,
                });
            }
            Value::Array(values)
        }
        10 => {
            let mut fields = serde_json::Map::new();
            loop {
                let kind = take(bytes, 1)?[0];
                if kind == 0 {
                    break;
                }
                let key = read_modified_utf8(bytes)?;
                fields.insert(key, tag_json(bytes, kind, depth + 1)?);
            }
            Value::Object(fields)
        }
        _ => return Err(invalid("invalid NBT tag")),
    })
}

/// A finite number as JSON; NaN and infinities have no JSON form.
fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}
