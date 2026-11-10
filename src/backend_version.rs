//! Which Minecraft version a backend runs, so players on another version can
//! be translated without declaring every backend's version by hand.
//!
//! A backend's status response names its protocol. Rift asks with the
//! player's own protocol in the handshake, as the player's client would: a
//! backend that already accepts that protocol (natively or through its own
//! ViaVersion) answers with it and is left alone. Answers are cached briefly
//! per backend address and client protocol; a backend that does not answer
//! is assumed to run the player's version, as before.
use crate::{
    config::Config,
    protocol::{Handshake, NextState, Packet, ProtocolVersion, Reader, read_string},
    routing::Backend,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

/// How long an answer is trusted. Short, so a server that is upgraded or
/// replaced on the same address is noticed without a restart.
const FRESH: Duration = Duration::from_secs(30);
/// A backend that did not answer is asked again sooner.
const RETRY: Duration = Duration::from_secs(5);
/// A status exchange on a healthy network takes milliseconds.
const TIMEOUT: Duration = Duration::from_secs(2);
/// A status document with a favicon stays well below this.
const MAX_STATUS: usize = 256 * 1024;
/// Backends times client protocols stays far below this; a bound all the same.
const MAX_ENTRIES: usize = 4096;

type Key = (String, i32);

struct Entry {
    protocol: Option<i32>,
    checked: Instant,
}

fn cache() -> &'static Mutex<HashMap<Key, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<Key, Entry>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn cached(key: &Key) -> Option<Option<i32>> {
    let cache = cache().lock().unwrap();
    let entry = cache.get(key)?;
    let limit = if entry.protocol.is_some() {
        FRESH
    } else {
        RETRY
    };
    (entry.checked.elapsed() < limit).then_some(entry.protocol)
}

fn remember(key: Key, protocol: Option<i32>) {
    let mut cache = cache().lock().unwrap();
    if cache.len() >= MAX_ENTRIES {
        cache.retain(|_, entry| entry.checked.elapsed() < FRESH);
        if cache.len() >= MAX_ENTRIES {
            cache.clear();
        }
    }
    cache.insert(
        key,
        Entry {
            protocol,
            checked: Instant::now(),
        },
    );
}

/// Ask `backend` for its status and return the protocol it names.
async fn ask(backend: &Backend, listeners: &[SocketAddr], client: i32) -> Option<i32> {
    let exchange = async {
        let mut stream = backend.connect(listeners).await.ok()?;
        stream.set_nodelay(true).ok()?;
        let (host, port) = backend.address().rsplit_once(':')?;
        let handshake = Handshake {
            protocol: client,
            address: host.trim_matches(['[', ']']).to_owned(),
            port: port.parse().ok()?,
            next_state: NextState::Status,
        };
        let codec = crate::protocol::Codec::default();
        let mut request = codec.encode(&handshake.packet()).ok()?;
        request.extend(codec.encode(&Packet::empty(0)).ok()?);
        stream.write_all(&request).await.ok()?;
        let frame = Reader::default()
            .read_frame(&mut stream, MAX_STATUS)
            .await
            .ok()??;
        let response = Packet::from_body(&frame).ok()?;
        if response.id != 0 {
            return None;
        }
        let document = read_string(&mut response.data.as_slice(), MAX_STATUS).ok()?;
        let document: serde_json::Value = serde_json::from_str(document).ok()?;
        i32::try_from(document.get("version")?.get("protocol")?.as_i64()?).ok()
    };
    tokio::time::timeout(TIMEOUT, exchange).await.ok().flatten()
}

/// The protocol to speak to backend `name` for a player on `client`: the
/// version its configuration declares, else the one it reports, else `None`
/// for the player's own. A reported version Rift cannot translate to is
/// ignored, leaving the backend to accept or refuse the player itself.
pub async fn protocol(
    config: &Config,
    name: &str,
    client: ProtocolVersion,
    listeners: &[SocketAddr],
) -> Option<ProtocolVersion> {
    let backend = config.backends.get(name)?;
    if !config.translation.enabled {
        return None;
    }
    if let Some(declared) = backend.version() {
        return Some(declared);
    }
    if !config.translation.detect {
        return None;
    }
    let key = (backend.address().to_owned(), client.number());
    let reported = match cached(&key) {
        Some(reported) => reported,
        None => {
            let reported = ask(backend, listeners, client.number()).await;
            remember(key, reported);
            reported
        }
    };
    let reported = ProtocolVersion::new(reported?).ok()?;
    (reported != client && crate::protocol::translatable(client, reported)).then_some(reported)
}

/// Whether backend `name` can take a player on `client` at all, going by its
/// declared version; a backend without one is asked when the player connects.
pub fn serves(config: &Config, name: &str, client: ProtocolVersion) -> bool {
    config
        .backends
        .get(name)
        .and_then(Backend::version)
        .is_none_or(|backend| {
            backend == client
                || (config.translation.enabled && crate::protocol::translatable(client, backend))
        })
}

/// A backend's own status response as a player on `client` should see it in
/// the server list: where Rift translates between the two versions, the
/// backend's protocol number is replaced by the player's, so the client does
/// not show a server it can join as incompatible. Anything else, including a
/// response that does not parse, is returned unchanged.
pub fn status_for_client(config: &Config, client: i32, response: Packet) -> Packet {
    let rewritten = || {
        if !config.translation.enabled {
            return None;
        }
        let client = ProtocolVersion::new(client).ok()?;
        let document = read_string(&mut response.data.as_slice(), MAX_STATUS).ok()?;
        let mut document: serde_json::Value = serde_json::from_str(document).ok()?;
        let reported = document.get("version")?.get("protocol")?.as_i64()?;
        let backend = ProtocolVersion::new(i32::try_from(reported).ok()?).ok()?;
        if backend == client || !crate::protocol::translatable(client, backend) {
            return None;
        }
        document["version"]["protocol"] = client.number().into();
        let mut data = Vec::new();
        crate::protocol::write_string(&document.to_string(), &mut data);
        Some(Packet::new(response.id, data))
    };
    rewritten().unwrap_or(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::AsyncReadExt,
        net::{TcpListener, TcpStream},
    };

    /// A server that answers one status request, naming `protocol`.
    async fn status_server(protocol: i32) -> (SocketAddr, tokio::task::JoinHandle<i32>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _): (TcpStream, _) = listener.accept().await.unwrap();
            let mut reader = Reader::default();
            let frame = reader.read_frame(&mut stream, 1024).await.unwrap().unwrap();
            let handshake = Handshake::decode(&Packet::from_body(&frame).unwrap()).unwrap();
            assert_eq!(handshake.next_state, NextState::Status);
            reader.read_frame(&mut stream, 16).await.unwrap().unwrap();
            let response = crate::protocol::status_response(protocol, 0, 20, "test");
            let frame = crate::protocol::Codec::default().encode(&response).unwrap();
            stream.write_all(&frame).await.unwrap();
            let _ = stream.read(&mut [0; 16]).await;
            handshake.protocol
        });
        (address, task)
    }

    fn config(address: SocketAddr) -> Config {
        let mut config = Config::default();
        config.backends.insert(
            "lobby".into(),
            Backend::parse(&address.to_string()).unwrap(),
        );
        config
    }

    fn version(protocol: i32) -> ProtocolVersion {
        ProtocolVersion::new(protocol).unwrap()
    }

    #[tokio::test]
    async fn a_backend_on_another_version_is_detected_asked_as_the_client_and_cached() {
        let (address, server) = status_server(771).await;
        let config = config(address);
        let detected = protocol(&config, "lobby", version(772), &[]).await;
        assert_eq!(detected, Some(version(771)));
        assert_eq!(server.await.unwrap(), 772);
        // The server is gone; the cached answer still holds.
        assert_eq!(
            protocol(&config, "lobby", version(772), &[]).await,
            Some(version(771))
        );
    }

    #[tokio::test]
    async fn same_version_untranslatable_and_silent_backends_use_the_client_protocol() {
        let (address, _server) = status_server(772).await;
        assert_eq!(
            protocol(&config(address), "lobby", version(772), &[]).await,
            None
        );
        // 1.8 is outside the translated range: leave the decision to the backend.
        let (address, _server) = status_server(47).await;
        assert_eq!(
            protocol(&config(address), "lobby", version(772), &[]).await,
            None
        );
        let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = unused.local_addr().unwrap();
        drop(unused);
        assert_eq!(
            protocol(&config(address), "lobby", version(772), &[]).await,
            None
        );
    }

    #[test]
    fn a_translatable_backend_status_shows_the_players_protocol() {
        let status = |protocol| crate::protocol::status_response(protocol, 3, 20, "hello");
        let protocol_of = |packet: &Packet| {
            let text = read_string(&mut packet.data.as_slice(), MAX_STATUS).unwrap();
            let document: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(document["players"]["online"], 3);
            document["version"]["protocol"].as_i64().unwrap()
        };
        let mut config = Config::default();
        assert_eq!(
            protocol_of(&status_for_client(&config, 772, status(771))),
            772
        );
        assert_eq!(
            protocol_of(&status_for_client(&config, 772, status(772))),
            772
        );
        // Not translatable, an unknown client, or translation off: unchanged.
        assert_eq!(
            protocol_of(&status_for_client(&config, 772, status(47))),
            47
        );
        assert_eq!(
            protocol_of(&status_for_client(&config, 5, status(771))),
            771
        );
        config.translation.enabled = false;
        assert_eq!(
            protocol_of(&status_for_client(&config, 772, status(771))),
            771
        );
        let garbage = Packet::new(0, vec![1, 2, 3]);
        assert_eq!(status_for_client(&config, 772, garbage.clone()), garbage);
    }

    #[tokio::test]
    async fn declared_versions_and_the_translation_switch_take_precedence() {
        let (address, _server) = status_server(771).await;
        let mut config = config(address);
        config.translation.detect = false;
        assert_eq!(protocol(&config, "lobby", version(772), &[]).await, None);
        let declared = config.backends["lobby"].clone().with_version(version(771));
        config.backends.insert("lobby".into(), declared);
        assert_eq!(
            protocol(&config, "lobby", version(772), &[]).await,
            Some(version(771))
        );
        assert!(serves(&config, "lobby", version(772)));
        assert!(!serves(&config, "lobby", version(47)));
        config.translation.enabled = false;
        assert_eq!(protocol(&config, "lobby", version(772), &[]).await, None);
        assert!(!serves(&config, "lobby", version(772)));
        assert!(serves(&config, "lobby", version(771)));
    }
}
