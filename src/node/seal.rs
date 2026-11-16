//! The forwarding secret on its way to an agent, readable only with that
//! node's token.
//!
//! The controller puts the Velocity secret or BungeeGuard token into
//! [`super::protocol::Prepare`] so operators do not have to copy it to every
//! node by hand (and get players rejected when one copy is stale). The bus is
//! TLS already; sealing additionally keeps the plain secret out of command
//! payloads, so only a holder of the node's token can read it, the same party
//! that receives the node's bus logins in start commands.
//!
//! ```text
//! key    = HKDF-SHA256(salt = "rift-node-forwarding/1", ikm = node token, info = node name)
//! sealed = "fs1." ‖ base64(nonce ‖ AES-256-GCM(key, nonce, aad = node name, secret))
//! ```
use std::io;

use aws_lc_rs::{
    aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey},
    hkdf::{HKDF_SHA256, Salt},
};

use super::protocol::{decode_base64, encode_base64};

const PREFIX: &str = "fs1.";
const SALT: &[u8] = b"rift-node-forwarding/1";
/// Secrets are 1..=1024 bytes; anything longer is not a sealed secret.
const MAX_SEALED: usize = 2048;

fn key(token: &str, node: &str) -> io::Result<LessSafeKey> {
    let info = [node.as_bytes()];
    let prk = Salt::new(HKDF_SHA256, SALT).extract(token.as_bytes());
    let okm = prk
        .expand(&info, &AES_256_GCM)
        .map_err(|_| io::Error::other("key derivation failed"))?;
    Ok(LessSafeKey::new(UnboundKey::from(okm)))
}

/// `secret` for node `node`, which authenticates with `token`.
pub fn seal(token: &str, node: &str, secret: &[u8]) -> io::Result<String> {
    let mut nonce = [0u8; NONCE_LEN];
    aws_lc_rs::rand::fill(&mut nonce).map_err(|_| io::Error::other("random source failed"))?;
    let mut sealed = secret.to_vec();
    key(token, node)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(node.as_bytes()),
            &mut sealed,
        )
        .map_err(|_| io::Error::other("sealing the forwarding secret failed"))?;
    let mut bytes = nonce.to_vec();
    bytes.append(&mut sealed);
    Ok(format!("{PREFIX}{}", encode_base64(&bytes)))
}

/// The secret in `sealed`, or an error when it was sealed for another node
/// or token (the agent's and the controller's token for this node differ).
pub fn open(token: &str, node: &str, sealed: &str) -> io::Result<Vec<u8>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the controller's forwarding secret could not be opened with this node's token",
        )
    };
    let mut bytes = sealed
        .strip_prefix(PREFIX)
        .filter(|text| text.len() <= MAX_SEALED)
        .and_then(decode_base64)
        .filter(|bytes| bytes.len() > NONCE_LEN)
        .ok_or_else(invalid)?;
    let mut sealed = bytes.split_off(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(&bytes).map_err(|_| invalid())?;
    let secret = key(token, node)?
        .open_in_place(nonce, Aad::from(node.as_bytes()), &mut sealed)
        .map_err(|_| invalid())?;
    Ok(secret.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "node-token-node-token-node-token-1";

    #[test]
    fn only_the_same_node_and_token_open_a_sealed_secret() {
        let sealed = seal(TOKEN, "node1", b"velocity secret").unwrap();
        assert!(sealed.starts_with(PREFIX));
        assert!(!sealed.contains("velocity"));
        assert_eq!(open(TOKEN, "node1", &sealed).unwrap(), b"velocity secret");
        // Fresh nonces: sealing twice differs.
        assert_ne!(seal(TOKEN, "node1", b"velocity secret").unwrap(), sealed);
        for (token, node) in [
            ("node-token-node-token-node-token-2", "node1"),
            (TOKEN, "node2"),
        ] {
            let error = open(token, node, &sealed).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        let mut tampered = sealed.clone().into_bytes();
        let last = tampered.len() - 3;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        for bad in ["", "fs1.", "fs1.AAAA", "velocity secret", tampered.as_str()] {
            assert!(open(TOKEN, "node1", bad).is_err(), "{bad}");
        }
    }
}
