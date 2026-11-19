//! Bus federation over the peer links' `bus` stream.
//!
//! [`Federation::send`] only queues: each peer whose link is up has a
//! bounded FIFO queue and one worker that sends what accumulated as one
//! batch and waits for the peer's answer before it sends the next, so the
//! peer handles frames in the order they were queued. A batch is answered
//! after the receiving broker handled every frame in it.
//!
//! Nothing is retried: the bus delivers at most once. A queue that is full
//! drops the frame, and a batch that fails drops itself and what waits
//! behind it; either way the peer may have missed interest updates, so the
//! worker then sends it this broker's whole interest again (as if the peer
//! had asked with [`FederationFrame::Resync`]), after a growing pause while
//! batches keep failing.
use super::FEDERATION;
use crate::cluster::{Links, StreamKind};
use crate::messaging::{Broker, Federation, FederationFrame, PeerId};
use bytes::{BufMut, Bytes, BytesMut};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{sync::Notify, task::JoinHandle};

/// Frames queued for one peer at most.
const MAX_QUEUED_FRAMES: usize = 16_384;
/// Bytes queued for one peer at most: room for a few maximal messages.
const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;
/// The largest batch, unless one frame alone is larger.
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_FIRST: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Per-peer counters for `/metrics`.
#[derive(Default)]
pub(super) struct Counters {
    /// Messages delivered to each peer.
    forwarded: Mutex<BTreeMap<String, u64>>,
    /// Frames dropped for each peer: a full queue or a failed batch.
    dropped: Mutex<BTreeMap<String, u64>>,
}

impl Counters {
    fn add(map: &Mutex<BTreeMap<String, u64>>, peer: &str, count: u64) {
        if count > 0 {
            *lock(map).entry(peer.to_owned()).or_default() += count;
        }
    }

    #[cfg(test)]
    pub(super) fn forwarded(&self, peer: &str) -> u64 {
        lock(&self.forwarded).get(peer).copied().unwrap_or(0)
    }

    pub(super) fn render(&self) -> String {
        let mut text = String::from("# TYPE rift_cluster_bus_forwarded_total counter\n");
        for (peer, count) in lock(&self.forwarded).iter() {
            text.push_str(&format!(
                "rift_cluster_bus_forwarded_total{{peer=\"{peer}\"}} {count}\n"
            ));
        }
        text.push_str("# TYPE rift_cluster_bus_dropped_total counter\n");
        for (peer, count) in lock(&self.dropped).iter() {
            text.push_str(&format!(
                "rift_cluster_bus_dropped_total{{peer=\"{peer}\"}} {count}\n"
            ));
        }
        text
    }
}

#[derive(Default)]
struct Queue {
    /// Encoded frames, each marked whether it carries a message.
    frames: VecDeque<(Bytes, bool)>,
    bytes: usize,
    /// Frames were dropped since the peer last got the whole interest.
    overflowed: bool,
    closed: bool,
}

#[derive(Default)]
struct Outbox {
    queue: Mutex<Queue>,
    wake: Notify,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Outbox {
    fn close(&self) {
        lock(&self.queue).closed = true;
        self.wake.notify_one();
        if let Some(worker) = lock(&self.worker).take() {
            worker.abort();
        }
    }
}

/// The cluster's [`Federation`]: one outbox per peer whose link is up and
/// that runs a bus.
pub(super) struct PeerFederation {
    local: PeerId,
    links: Links,
    outboxes: Mutex<HashMap<PeerId, Arc<Outbox>>>,
    counters: Arc<Counters>,
}

impl PeerFederation {
    pub(super) fn new(local: &str, links: Links, counters: Arc<Counters>) -> Arc<Self> {
        Arc::new(Self {
            local: PeerId::from(local),
            links,
            outboxes: Mutex::new(HashMap::new()),
            counters,
        })
    }

    pub(super) fn has(&self, peer: &str) -> bool {
        lock(&self.outboxes).contains_key(peer)
    }

    pub(super) fn peer_names(&self) -> Vec<String> {
        lock(&self.outboxes)
            .keys()
            .map(|peer| peer.to_string())
            .collect()
    }

    /// The link to `peer` came up (again): a fresh outbox, and an interest
    /// exchange through the broker.
    pub(super) fn up(&self, peer: &str, broker: &Broker) {
        let peer = PeerId::from(peer);
        let outbox = Arc::new(Outbox::default());
        let previous = lock(&self.outboxes).insert(peer.clone(), outbox.clone());
        if let Some(previous) = previous {
            previous.close();
        }
        let worker = tokio::spawn(run(
            self.links.clone(),
            peer.clone(),
            outbox.clone(),
            broker.clone(),
            self.counters.clone(),
        ));
        *lock(&outbox.worker) = Some(worker);
        broker.peer_up(&peer);
        // The peer's whole interest may have arrived before this side saw
        // the link come up, and `peer_up` just forgot it: ask again.
        self.send(&peer, FederationFrame::Resync);
    }

    /// The link to `peer` went down: drop its queue.
    pub(super) fn down(&self, peer: &str, broker: &Broker) {
        let removed = lock(&self.outboxes).remove(peer);
        if let Some(outbox) = removed {
            outbox.close();
        }
        broker.peer_down(&PeerId::from(peer));
    }

    pub(super) fn close_all(&self) {
        let outboxes: Vec<Arc<Outbox>> = lock(&self.outboxes).drain().map(|(_, o)| o).collect();
        for outbox in outboxes {
            outbox.close();
        }
    }
}

impl Federation for PeerFederation {
    fn local_peer(&self) -> PeerId {
        self.local.clone()
    }

    fn peers(&self) -> Vec<PeerId> {
        let mut peers: Vec<PeerId> = lock(&self.outboxes).keys().cloned().collect();
        peers.sort();
        peers
    }

    fn send(&self, peer: &PeerId, frame: FederationFrame) {
        let Some(outbox) = lock(&self.outboxes).get(peer).cloned() else {
            return;
        };
        let message = matches!(frame, FederationFrame::Message(_));
        let encoded = match frame.encode() {
            Ok(encoded) => encoded,
            Err(error) => {
                log_debug!("cluster bus: a frame for {peer} cannot be encoded: {error}");
                Counters::add(&self.counters.dropped, peer, 1);
                return;
            }
        };
        {
            let mut queue = lock(&outbox.queue);
            if queue.closed {
                return;
            }
            if queue.frames.len() >= MAX_QUEUED_FRAMES
                || queue.bytes + encoded.len() > MAX_QUEUED_BYTES
            {
                queue.overflowed = true;
                drop(queue);
                Counters::add(&self.counters.dropped, peer, 1);
                return;
            }
            queue.bytes += encoded.len();
            queue.frames.push_back((encoded, message));
        }
        outbox.wake.notify_one();
    }
}

enum Next {
    Closed,
    Idle,
    Resync,
    Batch {
        body: Bytes,
        frames: u64,
        messages: u64,
    },
}

/// Take what is queued for `outbox` as one batch body.
fn next(outbox: &Outbox) -> Next {
    let mut queue = lock(&outbox.queue);
    if queue.closed {
        return Next::Closed;
    }
    if queue.frames.is_empty() {
        if std::mem::take(&mut queue.overflowed) {
            return Next::Resync;
        }
        return Next::Idle;
    }
    let mut body = BytesMut::new();
    body.put_u8(FEDERATION);
    let (mut frames, mut messages) = (0, 0);
    while let Some(size) = queue.frames.front().map(|(frame, _)| frame.len()) {
        if frames > 0 && body.len() + 4 + size > MAX_BATCH_BYTES {
            break;
        }
        let Some((frame, message)) = queue.frames.pop_front() else {
            break;
        };
        queue.bytes -= frame.len();
        body.put_u32(u32::try_from(frame.len()).unwrap_or(u32::MAX));
        body.put_slice(&frame);
        frames += 1;
        messages += u64::from(message);
    }
    Next::Batch {
        body: body.freeze(),
        frames,
        messages,
    }
}

/// The worker of one peer's outbox.
async fn run(
    links: Links,
    peer: PeerId,
    outbox: Arc<Outbox>,
    broker: Broker,
    counters: Arc<Counters>,
) {
    let mut backoff = BACKOFF_FIRST;
    loop {
        match next(&outbox) {
            Next::Closed => return,
            Next::Idle => outbox.wake.notified().await,
            // As if the peer had asked: the broker queues its whole interest.
            Next::Resync => broker.handle_federation_frame(&peer, FederationFrame::Resync),
            Next::Batch {
                body,
                frames,
                messages,
            } => match links
                .request(&peer, StreamKind::Bus, body, REQUEST_TIMEOUT)
                .await
            {
                Ok(_) => {
                    backoff = BACKOFF_FIRST;
                    Counters::add(&counters.forwarded, &peer, messages);
                }
                Err(error) => {
                    log_debug!("cluster bus: frames to {peer} were lost: {error}");
                    let waiting = {
                        let mut queue = lock(&outbox.queue);
                        let waiting = queue.frames.len() as u64;
                        queue.frames.clear();
                        queue.bytes = 0;
                        queue.overflowed = true;
                        waiting
                    };
                    Counters::add(&counters.dropped, &peer, frames + waiting);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            },
        }
    }
}

/// Handle a batch `from` another peer, frame by frame in order.
pub(super) fn receive(broker: &Broker, from: &str, mut body: Bytes) -> Result<(), String> {
    let from = PeerId::from(from);
    while !body.is_empty() {
        if body.len() < 4 {
            return Err("truncated federation batch".into());
        }
        let length = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        let _ = body.split_to(4);
        if body.len() < length {
            return Err("truncated federation batch".into());
        }
        let frame = body.split_to(length);
        let frame = FederationFrame::decode(frame)
            .map_err(|error| format!("malformed federation frame: {error}"))?;
        broker.handle_federation_frame(&from, frame);
    }
    Ok(())
}
