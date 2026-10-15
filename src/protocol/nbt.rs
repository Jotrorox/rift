//! Bounded skipping of named NBT used by pre-configuration dimension registries.
//! Values are copied verbatim; Rift does not interpret or rewrite registries.
//! Backend-supplied JSON text components are encoded for 1.20.3+ clients.
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
