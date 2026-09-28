//! The client transport and identity belong to the proxy for the entire session.
//! Minecraft requires RSA PKCS#1 v1.5, SHA-1's signed hexadecimal encoding, and
//! AES-128/CFB8 with the shared secret as both key and IV. These algorithms are
//! protocol requirements, not choices for new application protocols.
//!
//! Wire references: Velocity's EncryptionRequestPacket, EncryptionUtils, and
//! InitialLoginSessionHandler in https://github.com/PaperMC/Velocity.

use crate::protocol::{
    Codec, Packet, ProtocolVersion, Reader, read_varint, write_string, write_varint,
};
use aes::Aes128;
use aws_lc_rs::{
    constant_time::verify_slices_are_equal,
    digest::{Context, SHA1_FOR_LEGACY_USE_ONLY},
    encoding::{AsDer, PublicKeyX509Der},
    rand,
    rsa::{KeySize, Pkcs1PrivateDecryptingKey, PrivateDecryptingKey},
};
use cfb8::cipher::{
    IvState, KeyIvInit, SetIvState,
    zeroize::{Zeroize, Zeroizing},
};
use std::{
    io,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const SESSION_URL: &str = "https://sessionserver.mojang.com/session/minecraft/hasJoined";
const MAX_SESSION_BODY: usize = 64 * 1024;
const MAX_PROPERTIES: usize = 16;
const MAX_PROPERTY_LENGTH: usize = 16 * 1024;
const CRYPTO_CHUNK: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileProperty {
    pub name: String,
    pub value: String,
    pub signature: Option<String>,
}

/// Obtained only from the authenticated Mojang session response in production.
/// The UUID supplied in Login Start is never an authentication credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedProfile {
    pub uuid: [u8; 16],
    pub name: String,
    pub properties: Vec<ProfileProperty>,
}

/// Continuous client encryption, independent of packet/compression boundaries.
/// No ciphertext is buffered after a successful write: `write_all` retains the
/// same semantics as the underlying stream and needs no additional flush.
pub struct CryptoStream<S> {
    inner: S,
    encryption: Option<cfb8::Encryptor<Aes128>>,
    decryption: Option<cfb8::Decryptor<Aes128>>,
}

impl<S> CryptoStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            encryption: None,
            decryption: None,
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.encryption.is_some()
    }

    pub fn enable_encryption(&mut self, mut secret: [u8; 16]) -> io::Result<()> {
        if self.is_encrypted() {
            secret.zeroize();
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Client encryption is already enabled.",
            ));
        }
        self.encryption = Some(cfb8::Encryptor::new(&secret.into(), &secret.into()));
        self.decryption = Some(cfb8::Decryptor::new(&secret.into(), &secret.into()));
        secret.zeroize();
        Ok(())
    }
}

impl<S> Deref for CryptoStream<S> {
    type Target = S;
    fn deref(&self) -> &S {
        &self.inner
    }
}

impl<S> DerefMut for CryptoStream<S> {
    fn deref_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CryptoStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Some(cipher) = &mut this.decryption {
            // Only received bytes advance the cipher, including when the caller
            // cancels its next read. Reader owns partially received frames.
            cipher.decrypt(&mut buf.filled_mut()[before..]);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CryptoStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let Some(cipher) = &mut this.encryption else {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        };
        // Encrypt at most one bounded chunk. Restore the IV on Pending/error;
        // cancellation must never advance the cipher for bytes not accepted by
        // the socket. For a short write, CFB8's IV is simply the last 16 bytes of
        // the previously committed IV plus the accepted ciphertext prefix.
        let initial_iv = cipher.iv_state();
        let mut ciphertext = buf[..buf.len().min(CRYPTO_CHUNK)].to_vec();
        cipher.encrypt(&mut ciphertext);
        let result = Pin::new(&mut this.inner).poll_write(cx, &ciphertext);
        let accepted = match result {
            Poll::Ready(Ok(n)) => n,
            _ => 0,
        };
        if accepted != ciphertext.len() {
            let mut committed_iv = initial_iv;
            if accepted >= committed_iv.len() {
                committed_iv.copy_from_slice(&ciphertext[accepted - 16..accepted]);
            } else {
                committed_iv.copy_within(accepted.., 0);
                committed_iv[16 - accepted..].copy_from_slice(&ciphertext[..accepted]);
            }
            cipher.set_iv(&committed_iv);
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// One keypair and bounded HTTPS client per proxy process, shared across logins.
pub struct Authenticator {
    private_key: Pkcs1PrivateDecryptingKey,
    public_key: Vec<u8>,
    http: reqwest::Client,
    #[cfg(test)]
    session_url: Option<String>,
}

impl Authenticator {
    pub fn new(timeout: Duration) -> io::Result<Self> {
        let private_key = PrivateDecryptingKey::generate(KeySize::Rsa2048)
            .map_err(|_| io::Error::other("Could not generate proxy encryption key."))?;
        let public_key = AsDer::<PublicKeyX509Der>::as_der(&private_key.public_key())
            .map_err(|_| io::Error::other("Could not encode proxy encryption key."))?
            .as_ref()
            .to_vec();
        let http = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout)
            .timeout(timeout)
            .user_agent(concat!("Rift/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| io::Error::other("Could not initialize authentication HTTPS client."))?;
        Ok(Self {
            private_key: Pkcs1PrivateDecryptingKey::new(private_key)
                .map_err(|_| io::Error::other("Could not initialize proxy encryption key."))?,
            public_key,
            http,
            #[cfg(test)]
            session_url: None,
        })
    }

    /// No endpoint override exists in production configuration or environment.
    #[cfg(test)]
    pub(crate) fn for_test_session_server(url: String, timeout: Duration) -> Self {
        let mut auth = Self::new(timeout).unwrap();
        auth.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .unwrap();
        auth.session_url = Some(url);
        auth
    }

    /// The caller bounds the complete login with its connection deadline. HTTP
    /// requests also have their own timeout. Encryption starts before hasJoined
    /// so outages and rejected sessions receive an encrypted login disconnect.
    pub async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut CryptoStream<S>,
        version: ProtocolVersion,
        name: &str,
    ) -> io::Result<AuthenticatedProfile> {
        if !valid_name(name) || stream.is_encrypted() {
            return Err(failed());
        }
        let mut token = [0; 16];
        rand::fill(&mut token)
            .map_err(|_| io::Error::other("Could not generate login challenge."))?;
        let mut request = Vec::new();
        write_string("", &mut request);
        write_bytes(&self.public_key, &mut request);
        write_bytes(&token, &mut request);
        if version.number() >= 766 {
            request.push(1); // shouldAuthenticate: never offer unauthenticated encryption
        }
        Codec::default()
            .write(stream, &Packet::new(1, request))
            .await?;
        // A separate exact-length Reader cannot consume the first encrypted
        // packet while reading the final plaintext Encryption Response.
        let frame = Reader::default()
            .read_frame(stream, 1024)
            .await?
            .ok_or_else(failed)?;
        let response = Packet::from_body(&frame).map_err(|_| failed())?;
        if response.id != 1 {
            return Err(failed());
        }
        let mut bytes = response.data.as_slice();
        let secret_ciphertext = read_rsa_bytes(&mut bytes, self.private_key.key_size_bytes())?;
        let token_ciphertext = read_rsa_bytes(&mut bytes, self.private_key.key_size_bytes())?;
        if !bytes.is_empty() {
            return Err(failed());
        }
        let mut secret_buffer = Zeroizing::new(vec![0; self.private_key.min_output_size()]);
        let mut token_buffer = Zeroizing::new(vec![0; self.private_key.min_output_size()]);
        // Perform both decryptions before validating either result; all malformed
        // RSA/token failures have the same externally visible error.
        let secret = self
            .private_key
            .decrypt(secret_ciphertext, &mut secret_buffer);
        let actual_token = self
            .private_key
            .decrypt(token_ciphertext, &mut token_buffer);
        let (Ok(secret), Ok(actual_token)) = (secret, actual_token) else {
            return Err(failed());
        };
        if verify_slices_are_equal(&token, actual_token).is_err() {
            return Err(failed());
        }
        let mut secret: [u8; 16] = secret.try_into().map_err(|_| failed())?;
        let hash = server_hash(&secret, &self.public_key);
        stream.enable_encryption(secret)?;
        secret.zeroize();
        drop(secret_buffer);
        drop(token_buffer);
        self.has_joined(name, &hash).await
    }

    async fn has_joined(&self, name: &str, hash: &str) -> io::Result<AuthenticatedProfile> {
        #[cfg(not(test))]
        let url = SESSION_URL;
        #[cfg(test)]
        let url = self.session_url.as_deref().unwrap_or(SESSION_URL);
        let mut response = self
            .http
            .get(url)
            .query(&[("username", name), ("serverId", hash)])
            .send()
            .await
            .map_err(|_| unavailable())?;
        match response.status().as_u16() {
            200 => {}
            204 | 403 => return Err(failed()),
            _ => return Err(unavailable()),
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_SESSION_BODY as u64)
        {
            return Err(unavailable());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if chunk.len() > MAX_SESSION_BODY - body.len() {
                return Err(unavailable());
            }
            body.extend_from_slice(&chunk);
        }
        parse_profile(&body, name)
    }
}

fn valid_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn parse_profile(body: &[u8], requested_name: &str) -> io::Result<AuthenticatedProfile> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| unavailable())?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(unavailable)?;
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(unavailable)?;
    if !valid_name(name) || !name.eq_ignore_ascii_case(requested_name) {
        return Err(failed());
    }
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unavailable());
    }
    let mut uuid = [0; 16];
    for (i, byte) in uuid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).map_err(|_| unavailable())?;
    }
    if uuid == [0; 16] {
        return Err(unavailable());
    }
    let properties = value
        .get("properties")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(unavailable)?;
    if properties.len() > MAX_PROPERTIES {
        return Err(unavailable());
    }
    let properties = properties
        .iter()
        .map(|property| {
            let string = |field| -> io::Result<String> {
                let value = property
                    .get(field)
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(unavailable)?;
                if value.len() > MAX_PROPERTY_LENGTH {
                    return Err(unavailable());
                }
                Ok(value.to_owned())
            };
            let name = string("name")?;
            if name.is_empty() || name.len() > 64 {
                return Err(unavailable());
            }
            Ok(ProfileProperty {
                name,
                value: string("value")?,
                signature: if property.get("signature").is_some() {
                    Some(string("signature")?)
                } else {
                    None
                },
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(AuthenticatedProfile {
        uuid,
        name: name.to_owned(),
        properties,
    })
}

fn write_bytes(bytes: &[u8], output: &mut Vec<u8>) {
    write_varint(bytes.len() as i32, output);
    output.extend_from_slice(bytes);
}

fn read_rsa_bytes<'a>(bytes: &mut &'a [u8], required: usize) -> io::Result<&'a [u8]> {
    let length = read_varint(bytes).map_err(|_| failed())?;
    if length < 0 || length as usize != required {
        return Err(failed());
    }
    let value = bytes.get(..required).ok_or_else(failed)?;
    *bytes = &bytes[required..];
    Ok(value)
}

fn server_hash(secret: &[u8], public_key: &[u8]) -> String {
    let mut hash = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    hash.update(secret);
    hash.update(public_key);
    signed_hex(hash.finish().as_ref())
}

fn signed_hex(bytes: &[u8]) -> String {
    let negative = bytes[0] & 128 != 0;
    let mut magnitude = bytes.to_vec();
    if negative {
        let mut carry = true;
        for byte in magnitude.iter_mut().rev() {
            let (value, overflow) = (!*byte).overflowing_add(u8::from(carry));
            *byte = value;
            carry = overflow;
        }
    }
    let hex: String = magnitude.iter().map(|b| format!("{b:02x}")).collect();
    let hex = hex.trim_start_matches('0');
    if hex.is_empty() {
        "0".to_owned()
    } else if negative {
        format!("-{hex}")
    } else {
        hex.to_owned()
    }
}

fn failed() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Failed to verify your Minecraft session. Please restart your launcher and try again.",
    )
}

fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "Minecraft authentication servers are unavailable. Please try again later.",
    )
}

#[cfg(test)]
mod tests;
