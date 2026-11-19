//! Peer links over QUIC: ALPN `rift-cluster/1`, TLS 1.3, mutual
//! authentication against the cluster CA.
//!
//! Every peer certificate is used both ways (a peer dials and accepts), so it
//! must be signed by the CA in `ca`, name the host of `advertise` (DNS name
//! or IP address), and allow both server and client authentication (no
//! extended key usage, or both `serverAuth` and `clientAuth`). Each message
//! is one bidirectional stream; the binding for token proofs is TLS keying
//! material exported from the connection (RFC 5705).
use super::{
    link::{ALPN, MAX_MESSAGE},
    settings::{Settings, host_of},
    transport::{BoxFuture, Connection, Incoming, ReplySink, Transport},
};
use bytes::Bytes;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::{io, path::Path, sync::Arc, time::Duration};

const BINDING_LABEL: &[u8] = b"rift-cluster/1 binding";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct QuicTransport {
    endpoint: quinn::Endpoint,
}

impl QuicTransport {
    /// Bind `listen` with the certificate, key and CA of `settings`.
    pub fn bind(settings: &Settings) -> io::Result<Self> {
        let (server, client) = configs(settings)?;
        let mut endpoint = quinn::Endpoint::server(server, settings.listen).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cluster: cannot listen on {}: {error}", settings.listen),
            )
        })?;
        endpoint.set_default_client_config(client);
        Ok(Self { endpoint })
    }
}

/// Load the certificate, key and CA of `settings` without binding: what
/// `rift check --strict` verifies.
pub fn check(settings: &Settings) -> io::Result<()> {
    configs(settings).map(|_| ())
}

/// Server and client TLS for peer links: both present this peer's
/// certificate and require one signed by the cluster CA.
fn configs(settings: &Settings) -> io::Result<(quinn::ServerConfig, quinn::ClientConfig)> {
    let certs = certificates(&settings.certificate)?;
    let key = PrivateKeyDer::from_pem_file(&settings.private_key)
        .map_err(|error| pem("private_key", &settings.private_key, error))?;
    let mut roots = rustls::RootCertStore::empty();
    for ca in certificates(&settings.ca)? {
        roots.add(ca).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cluster: ca ({}): {error}", settings.ca.display()),
            )
        })?;
    }
    let roots = Arc::new(roots);
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        roots.clone(),
        provider.clone(),
    )
    .build()
    .map_err(io::Error::other)?;
    let tls_error = |error: rustls::Error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "cluster: {} and {}: {error}",
                settings.certificate.display(),
                settings.private_key.display()
            ),
        )
    };
    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs.clone(), key.clone_key())
        .map_err(tls_error)?;
    server.alpn_protocols = vec![ALPN.to_vec()];
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(tls_error)?;
    client.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT stays off: a replayed Raft message is never acceptable.
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server).map_err(io::Error::other)?,
    ));
    server.transport_config(Arc::new(transport_config()));
    let mut client = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client).map_err(io::Error::other)?,
    ));
    client.transport_config(Arc::new(transport_config()));
    Ok((server, client))
}

fn transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_uni_streams(0u32.into());
    config.max_concurrent_bidi_streams(1024u32.into());
    config.keep_alive_interval(Some(Duration::from_secs(2)));
    config.max_idle_timeout(Some(
        Duration::from_secs(10)
            .try_into()
            .expect("constant timeout"),
    ));
    config
}

fn certificates(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|error| pem("certificate", path, error))?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cluster: {}: no certificates found", path.display()),
        ));
    }
    Ok(certs)
}

fn pem(kind: &str, path: &Path, error: rustls::pki_types::pem::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("cluster: {kind} ({}): {error}", path.display()),
    )
}

impl Transport for QuicTransport {
    fn connect(&self, address: &str) -> BoxFuture<'_, io::Result<Arc<dyn Connection>>> {
        let address = address.to_owned();
        Box::pin(async move {
            let target = tokio::net::lookup_host(address.as_str())
                .await?
                .next()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("{address}: no address"))
                })?;
            let connecting = self
                .endpoint
                .connect(target, host_of(&address))
                .map_err(io::Error::other)?;
            let connection = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("{address}: TLS handshake timed out"),
                    )
                })?
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("{address}: {error}"),
                    )
                })?;
            Ok(Arc::new(QuicConnection(connection)) as Arc<dyn Connection>)
        })
    }

    fn accept(&self) -> BoxFuture<'_, Option<Arc<dyn Connection>>> {
        Box::pin(async move {
            loop {
                let incoming = self.endpoint.accept().await?;
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
                    Ok(Ok(connection)) => {
                        return Some(Arc::new(QuicConnection(connection)) as Arc<dyn Connection>);
                    }
                    Ok(Err(error)) => log_debug!("cluster: inbound peer handshake failed: {error}"),
                    Err(_) => log_debug!("cluster: inbound peer handshake timed out"),
                }
            }
        })
    }

    fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutting down");
    }
}

struct QuicConnection(quinn::Connection);

impl Connection for QuicConnection {
    fn exchange(
        &self,
        message: Bytes,
        reply: bool,
        limit: usize,
    ) -> BoxFuture<'_, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            let (mut send, mut recv) = self.0.open_bi().await.map_err(io::Error::other)?;
            send.write_all(&message).await.map_err(io::Error::other)?;
            send.finish().map_err(io::Error::other)?;
            if !reply {
                return Ok(None);
            }
            let answer = recv.read_to_end(limit).await.map_err(io::Error::other)?;
            Ok(Some(Bytes::from(answer)))
        })
    }

    fn next(&self) -> BoxFuture<'_, Option<Incoming>> {
        Box::pin(async move {
            let (send, mut recv) = self.0.accept_bi().await.ok()?;
            Some(Incoming {
                read: Box::pin(async move {
                    let message = recv
                        .read_to_end(MAX_MESSAGE)
                        .await
                        .map_err(io::Error::other)?;
                    let sink = ReplySink::new(move |reply: Bytes| async move {
                        let mut send = send;
                        let _ = send.write_all(&reply).await;
                        let _ = send.finish();
                    });
                    Ok((Bytes::from(message), sink))
                }),
            })
        })
    }

    fn binding(&self) -> io::Result<[u8; 32]> {
        let mut binding = [0u8; 32];
        self.0
            .export_keying_material(&mut binding, BINDING_LABEL, b"")
            .map_err(|_| io::Error::other("TLS keying material is unavailable"))?;
        Ok(binding)
    }

    fn remote(&self) -> String {
        self.0.remote_address().to_string()
    }

    fn close(&self, reason: &str) {
        self.0.close(0u32.into(), reason.as_bytes());
    }

    fn closed(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.0.closed().await;
        })
    }
}
