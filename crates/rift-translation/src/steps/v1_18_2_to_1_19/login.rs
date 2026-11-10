//! Login-phase packets as 1.18.2 to 1.19.3 lay them out.
//!
//! 1.19 lets the client send its chat profile key with `hello` and sign the
//! server's nonce with it instead of returning the encrypted nonce; 1.19.1
//! adds the player's UUID to `hello`; 1.19.3 drops the key again. Each
//! version's key is signed differently and Rift's own online login may have
//! replaced the client's, so steps treat keys as absent: they never forward
//! one, and a nonce signed with one cannot be translated.
use crate::wire::{Reader, Writer, invalid};
use std::io;

/// Player names are at most 16 characters.
const MAX_NAME: usize = 16;

/// The parts of a serverbound `hello` that survive translation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub name: String,
    /// 1.19.1 and later.
    pub uuid: Option<u128>,
}

fn skip_profile_key(r: &mut Reader<'_>) -> io::Result<()> {
    if r.bool()? {
        r.i64()?; // Expiry.
        r.byte_array()?; // Public key.
        r.byte_array()?; // Mojang's signature.
    }
    Ok(())
}

fn optional_uuid(r: &mut Reader<'_>) -> io::Result<Option<u128>> {
    Ok(if r.bool()? { Some(r.uuid()?) } else { None })
}

impl Hello {
    pub fn read(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let name = r.string(MAX_NAME)?.to_owned();
        if (759..761).contains(&protocol) {
            skip_profile_key(&mut r)?;
        }
        let uuid = if protocol >= 760 {
            optional_uuid(&mut r)?
        } else {
            None
        };
        r.finish()?;
        Ok(Self { name, uuid })
    }

    pub fn write(&self, protocol: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(&self.name);
        if (759..761).contains(&protocol) {
            w.bool(false);
        }
        if protocol >= 760 {
            w.bool(self.uuid.is_some());
            if let Some(uuid) = self.uuid {
                w.uuid(uuid);
            }
        }
        w.buf
    }
}

/// A serverbound encryption response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub secret: Vec<u8>,
    /// The server's nonce encrypted with its key; `None` when a 1.19 to
    /// 1.19.2 client signed it with its profile key instead.
    pub token: Option<Vec<u8>>,
}

impl Key {
    pub fn read(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let secret = r.byte_array()?.to_vec();
        let token = if (759..761).contains(&protocol) && !r.bool()? {
            r.i64()?; // Salt.
            r.byte_array()?; // Signature.
            None
        } else {
            Some(r.byte_array()?.to_vec())
        };
        r.finish()?;
        Ok(Self { secret, token })
    }

    pub fn write(&self, protocol: i32) -> io::Result<Vec<u8>> {
        let mut w = Writer::new();
        w.byte_array(&self.secret);
        let token = self.token.as_deref();
        match ((759..761).contains(&protocol), token) {
            (true, Some(token)) => {
                w.bool(true).byte_array(token);
            }
            (false, Some(token)) => {
                w.byte_array(token);
            }
            (_, None) => {
                return Err(invalid(
                    "the client signed the encryption nonce with a chat profile key, \
                     which cannot be translated; log in without a profile key",
                ));
            }
        }
        Ok(w.buf)
    }
}

/// Velocity's modern forwarding answers the version a backend asks for;
/// versions 2 and 3 carry a 1.19 or 1.19.1 chat profile key, which cannot
/// survive translation across those versions. Ask for version 1 instead.
/// `data` is a clientbound `custom_query`; `None` when it needs no change.
pub fn clamp_velocity_forwarding(data: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut r = Reader::new(data);
    let id = r.varint()?;
    if r.str()? != "velocity:player_info" {
        return Ok(None);
    }
    match r.rest() {
        [version] if *version > 1 => {
            let mut w = Writer::new();
            w.varint(id).string("velocity:player_info").u8(1);
            Ok(Some(w.buf))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(write: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut writer = Writer::new();
        write(&mut writer);
        writer.buf
    }

    #[test]
    fn hello_drops_keys_and_keeps_the_uuid_where_it_exists() {
        let keyed = bytes(|w| {
            w.string("Alex")
                .bool(true)
                .i64(5)
                .byte_array(&[1, 2])
                .byte_array(&[3]);
        });
        let hello = Hello::read(759, &keyed).unwrap();
        assert_eq!(
            hello,
            Hello {
                name: "Alex".into(),
                uuid: None
            }
        );
        assert_eq!(
            hello.write(758),
            bytes(|w| {
                w.string("Alex");
            })
        );
        assert_eq!(
            hello.write(759),
            bytes(|w| {
                w.string("Alex").bool(false);
            })
        );
        let hello = Hello {
            name: "Alex".into(),
            uuid: Some(7),
        };
        let with_uuid = hello.write(760);
        assert_eq!(Hello::read(760, &with_uuid).unwrap(), hello);
        assert_eq!(
            hello.write(761),
            bytes(|w| {
                w.string("Alex").bool(true).uuid(7);
            })
        );
        assert!(Hello::read(758, &with_uuid).is_err());
    }

    #[test]
    fn signed_nonces_cannot_be_translated() {
        let signed = bytes(|w| {
            w.byte_array(&[9]).bool(false).i64(1).byte_array(&[2]);
        });
        let key = Key::read(759, &signed).unwrap();
        assert_eq!(key.token, None);
        assert!(key.write(758).is_err());
        let plain = Key::read(
            758,
            &bytes(|w| {
                w.byte_array(&[9]).byte_array(&[4]);
            }),
        )
        .unwrap();
        let keyed = plain.write(760).unwrap();
        assert_eq!(
            keyed,
            bytes(|w| {
                w.byte_array(&[9]).bool(true).byte_array(&[4]);
            })
        );
        assert_eq!(
            Key::read(760, &keyed).unwrap().write(761).unwrap(),
            bytes(|w| {
                w.byte_array(&[9]).byte_array(&[4]);
            })
        );
    }

    #[test]
    fn velocity_forwarding_asks_for_the_plain_profile() {
        let request = |data: &[u8]| {
            bytes(|w| {
                w.varint(3).string("velocity:player_info").bytes(data);
            })
        };
        assert_eq!(
            clamp_velocity_forwarding(&request(&[4])).unwrap(),
            Some(request(&[1]))
        );
        assert_eq!(clamp_velocity_forwarding(&request(&[1])).unwrap(), None);
        assert_eq!(clamp_velocity_forwarding(&request(&[])).unwrap(), None);
        let other = bytes(|w| {
            w.varint(3).string("other").u8(4);
        });
        assert_eq!(clamp_velocity_forwarding(&other).unwrap(), None);
    }
}
