//! Client side of Rift's QUIC messaging protocol (`docs/messaging-protocol.md`):
//! TLS 1.3 with ALPN `rift-messaging/1`, token authentication, publish,
//! subscribe, request/reply and retained-stream consumers. The proxy's server
//! (`rift::messaging::quic::Server`) shares this crate's [`frame`] codec and
//! TLS configuration.
//!
//! ```rust,no_run
//! use bytes::Bytes;
//! use rift_messaging::{Client, tls_client_config};
//!
//! # async fn example() -> std::io::Result<()> {
//! let tls = tls_client_config("certs/ca.pem")?;
//! let client = Client::connect(
//!     "127.0.0.1:7443".parse().unwrap(), "localhost", tls,
//!     &std::env::var("RIFT_PLUGIN_TOKEN").expect("token"),
//! ).await?;
//! let mut scores = client.subscribe("game.score.*", None).await?;
//! client.publish("game.score.player42", Bytes::from_static(b"7")).await?;
//! assert_eq!(&*scores.recv().await?.subject, "game.score.player42");
//! # Ok(()) }
//! ```

mod client;
pub mod frame;

use std::{io, path::Path, sync::Arc, time::Duration};

use bytes::Bytes;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer,
    pem::{self, PemObject},
};

pub use client::{Client, Publisher, RemoteSubscription};
use frame::{invalid, other};

pub type Result<T> = io::Result<T>;
pub const ALPN: &[u8] = b"rift-messaging/1";
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = MAX_PAYLOAD_BYTES + 64 * 1024;

/// One message on a subject, with an optional reply subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub subject: Arc<str>,
    pub reply: Option<Arc<str>>,
    pub payload: Bytes,
}

/// Where and how a durable consumer reads a retained stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumerConfig {
    pub filter_subject: String,
    /// Positive whole milliseconds, persisted across restarts.
    pub ack_wait: Duration,
    pub max_ack_pending: usize,
    pub start_sequence: u64,
}

impl Default for ConsumerConfig {
    fn default() -> Self {
        Self {
            filter_subject: ">".into(),
            ack_wait: Duration::from_secs(30),
            max_ack_pending: 128,
            start_sequence: 1,
        }
    }
}

/// A message stored in a retained stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub sequence: u64,
    pub timestamp_millis: u64,
    pub message: Message,
}

/// A record delivered to a durable consumer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub record: Record,
    pub delivery_count: u32,
}

/// QUIC transport settings shared by both sides: at most `streams`
/// concurrent bidirectional streams from the peer, ten-second keepalives, a
/// 60-second idle timeout and windows sized for maximum frames.
pub fn transport_config(streams: u32) -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_uni_streams(0u32.into());
    config.max_concurrent_bidi_streams(streams.into());
    config.keep_alive_interval(Some(Duration::from_secs(10)));
    config.max_idle_timeout(Some(
        Duration::from_secs(60)
            .try_into()
            .expect("constant timeout"),
    ));
    config.stream_receive_window((2 * MAX_FRAME_BYTES as u32).into());
    config.receive_window((8 * MAX_FRAME_BYTES as u32).into());
    config.send_window(8 * MAX_FRAME_BYTES as u64);
    config.datagram_receive_buffer_size(None);
    config
}

/// Build TLS configuration from a PEM certificate chain and unencrypted PEM key.
pub fn tls_server_config(
    certificate: impl AsRef<Path>,
    private_key: impl AsRef<Path>,
) -> Result<quinn::ServerConfig> {
    let certs = read_certificates(certificate.as_ref())?;
    let private_key = private_key.as_ref();
    let key = PrivateKeyDer::from_pem_file(private_key)
        .map_err(|error| pem_error("private key", private_key, error))?;
    server_config_from_der(certs, key)
}

/// Build TLS configuration from DER material, also useful with a certificate manager.
pub fn server_config_from_der(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(other)?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(other)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT is disabled: replayed publications/control commands are unacceptable.
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto).map_err(other)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config(128)));
    Ok(config)
}

/// Trust the given PEM CA/server certificates; DNS/IP name verification stays enabled.
pub fn tls_client_config(ca_certificates: impl AsRef<Path>) -> Result<quinn::ClientConfig> {
    let certs = read_certificates(ca_certificates.as_ref())?;
    client_config_from_der(certs)
}

/// The certificates in a PEM file; fails if there are none.
pub fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|error| pem_error("certificate", path, error))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| pem_error("certificate", path, error))?;
    if certs.is_empty() {
        return Err(invalid(format!(
            "certificate PEM ({}): no certificates found",
            path.display()
        )));
    }
    Ok(certs)
}

fn pem_error(kind: &str, path: &Path, error: pem::Error) -> io::Error {
    let error_kind = match &error {
        pem::Error::Io(error) => error.kind(),
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(
        error_kind,
        format!("{kind} PEM ({}): {error}", path.display()),
    )
}

/// Trust exactly these DER certificates.
pub fn client_config_from_der(certs: Vec<CertificateDer<'static>>) -> Result<quinn::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots.add(cert).map_err(other)?;
    }
    let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(other)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).map_err(other)?,
    ));
    config.transport_config(Arc::new(transport_config(0)));
    Ok(config)
}
