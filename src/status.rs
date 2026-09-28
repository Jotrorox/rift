use rift::config::StatusCache;
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, Weak},
    time::Instant,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::Mutex as AsyncMutex,
};

struct Entry {
    expires: Instant,
    response: Arc<Vec<u8>>,
}

type Key = (String, Vec<u8>);
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct Entries {
    values: HashMap<Key, Entry>,
    bytes: usize,
}

#[derive(Default)]
pub struct Cache {
    entries: Mutex<Entries>,
    flights: Mutex<HashMap<Key, Weak<AsyncMutex<()>>>>,
}

impl Cache {
    pub fn get(&self, listener: &str, handshake: &[u8]) -> Option<Arc<Vec<u8>>> {
        let mut entries = self.entries.lock().unwrap();
        let key = (listener.to_owned(), handshake.to_vec());
        if entries
            .values
            .get(&key)
            .is_some_and(|entry| entry.expires <= Instant::now())
        {
            let removed = entries.values.remove(&key).unwrap();
            entries.bytes -= key.0.len() + key.1.len() + removed.response.len();
        }
        entries.values.get(&key).map(|entry| entry.response.clone())
    }

    // Coalesce concurrent misses without a global async lock. Weak references
    // ensure failed/cancelled fills release their lock; the index is bounded.
    pub fn fill_lock(
        &self,
        listener: &str,
        handshake: &[u8],
        maximum: usize,
    ) -> Option<Arc<AsyncMutex<()>>> {
        let key = (listener.to_owned(), handshake.to_vec());
        let mut flights = self.flights.lock().unwrap();
        if let Some(lock) = flights.get(&key).and_then(Weak::upgrade) {
            return Some(lock);
        }
        if flights.len() >= maximum {
            flights.retain(|_, lock| lock.strong_count() != 0);
            if flights.len() >= maximum {
                return None;
            }
        }
        let lock = Arc::new(AsyncMutex::new(()));
        flights.insert(key, Arc::downgrade(&lock));
        Some(lock)
    }

    pub fn insert(
        &self,
        listener: &str,
        handshake: &[u8],
        response: Vec<u8>,
        settings: StatusCache,
    ) {
        let mut entries = self.entries.lock().unwrap();
        let now = Instant::now();
        let size = listener.len() + handshake.len() + response.len();
        if entries.values.len() >= settings.max_entries || entries.bytes + size > MAX_CACHE_BYTES {
            let mut removed = 0;
            entries.values.retain(|key, entry| {
                if entry.expires > now {
                    return true;
                }
                removed += key.0.len() + key.1.len() + entry.response.len();
                false
            });
            entries.bytes -= removed;
        }
        // At capacity, relay without caching. No random-hostname eviction churn.
        if entries.values.len() < settings.max_entries && entries.bytes + size <= MAX_CACHE_BYTES {
            let key = (listener.to_owned(), handshake.to_vec());
            if let Some(old) = entries.values.remove(&key) {
                entries.bytes -= key.0.len() + key.1.len() + old.response.len();
            }
            entries.bytes += size;
            entries.values.insert(
                key,
                Entry {
                    expires: now + settings.ttl,
                    response: Arc::new(response),
                },
            );
        }
    }
}

pub async fn frame(reader: &mut (impl AsyncRead + Unpin), max: usize) -> io::Result<Vec<u8>> {
    let mut prefix = Vec::with_capacity(5);
    loop {
        let byte = reader.read_u8().await?;
        prefix.push(byte);
        if prefix.len() == 5 && byte > 15 {
            return Err(invalid("invalid status frame length"));
        }
        if byte & 128 == 0 {
            break;
        }
    }
    let size = crate::handshake::varint(&mut prefix.as_slice())? as usize;
    if size == 0 || size > max {
        return Err(invalid("status frame exceeds size limit"));
    }
    let offset = prefix.len();
    prefix.resize(offset + size, 0);
    reader.read_exact(&mut prefix[offset..]).await?;
    Ok(prefix)
}

fn body(packet: &[u8]) -> io::Result<&[u8]> {
    let mut bytes = packet;
    crate::handshake::varint(&mut bytes)?;
    Ok(bytes)
}

pub fn validate_response(packet: &[u8]) -> io::Result<()> {
    let mut bytes = body(packet)?;
    if crate::handshake::varint(&mut bytes)? != 0 {
        return Err(invalid("expected status response"));
    }
    let length = crate::handshake::varint(&mut bytes)? as usize;
    if length != bytes.len() {
        return Err(invalid("invalid status JSON length"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| invalid("invalid status JSON"))?;
    if !value.is_object() {
        return Err(invalid("status JSON must be an object"));
    }
    Ok(())
}

pub async fn request(client: &mut crate::metrics::Metered<'_>) -> io::Result<()> {
    let packet = frame(client, 5).await?;
    let mut bytes = body(&packet)?;
    if crate::handshake::varint(&mut bytes)? != 0 || !bytes.is_empty() {
        return Err(invalid("expected empty status request"));
    }
    Ok(())
}

pub async fn respond(client: &mut crate::metrics::Metered<'_>, response: &[u8]) -> io::Result<()> {
    client.write_all(response).await?;
    // The ping belongs to this client, never the cached backend connection.
    let ping = match frame(client, 13).await {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        result => result?,
    };
    let mut bytes = body(&ping)?;
    if crate::handshake::varint(&mut bytes)? != 1 || bytes.len() != 8 {
        return Err(invalid("expected status ping"));
    }
    client.write_all(&ping).await?;
    client.shutdown().await
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_json_and_packet_shapes_are_not_cached() {
        for bytes in [
            vec![3, 0, 1, b'{'],
            vec![4, 0, 2, b'[', b']'],
            vec![4, 1, 2, b'{', b'}'],
            vec![4, 0, 3, b'{', b'}'],
        ] {
            assert!(validate_response(&bytes).is_err());
        }
        assert!(validate_response(&[4, 0, 2, b'{', b'}']).is_ok());
    }
    #[test]
    fn cache_is_bounded_and_separates_listener_and_handshake() {
        let cache = Cache::default();
        let settings = StatusCache {
            ttl: std::time::Duration::from_secs(1),
            max_entries: 1,
            max_response_bytes: 1024,
        };
        cache.insert("a", b"one", vec![1], settings);
        cache.insert("a", b"two", vec![2], settings);
        assert_eq!(&**cache.get("a", b"one").unwrap(), &[1]);
        assert!(cache.get("b", b"one").is_none());
        assert!(cache.get("a", b"two").is_none());
        cache
            .entries
            .lock()
            .unwrap()
            .values
            .values_mut()
            .next()
            .unwrap()
            .expires = Instant::now();
        assert!(cache.get("a", b"one").is_none());
        cache.insert("a", b"two", vec![2], settings);
        assert!(cache.get("a", b"two").is_some());
    }
}
