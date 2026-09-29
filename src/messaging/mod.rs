//! Shared, bounded asynchronous messaging for Rift and its plugins.
//!
//! Core publish never awaits a consumer or disk. Payload clones share their
//! allocation. Persistence is an explicit operation in [`stream`].

pub mod quic;
pub mod stream;
pub use stream::{ConsumerConfig, Delivery, DurableConsumer, Record, Stream, StreamConfig};

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    InvalidSubject(String),
    InvalidQueue,
    InvalidConfig(String),
    PayloadTooLarge { size: usize, limit: usize },
    SubscriptionLimit,
    SlowConsumer,
    Closed,
    Timeout,
    NoResponders,
    Io(std::io::Error),
    Storage(String),
    StreamNotMatched,
    ConsumerNotFound,
    InvalidAck,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSubject(s) => write!(f, "invalid subject: {s}"),
            Self::InvalidQueue => f.write_str("invalid queue group"),
            Self::InvalidConfig(s) => write!(f, "invalid messaging configuration: {s}"),
            Self::PayloadTooLarge { size, limit } => {
                write!(f, "payload size {size} exceeds limit {limit}")
            }
            Self::SubscriptionLimit => f.write_str("subscription limit reached"),
            Self::SlowConsumer => {
                f.write_str("subscriber exceeded its bounded queue and was disconnected")
            }
            Self::Closed => f.write_str("subscription closed"),
            Self::Timeout => f.write_str("request timed out"),
            Self::NoResponders => f.write_str("no responders available"),
            Self::Io(e) => write!(f, "messaging I/O: {e}"),
            Self::Storage(s) => write!(f, "stream storage: {s}"),
            Self::StreamNotMatched => f.write_str("subject does not match this stream"),
            Self::ConsumerNotFound => f.write_str("consumer does not exist"),
            Self::InvalidAck => f.write_str("sequence is not pending acknowledgement"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerConfig {
    pub subscription_capacity: usize,
    pub max_payload_bytes: usize,
    pub max_subject_bytes: usize,
    pub max_subscriptions: usize,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            subscription_capacity: 1024,
            max_payload_bytes: 1024 * 1024,
            max_subject_bytes: 1024,
            max_subscriptions: 65_536,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub subject: Arc<str>,
    pub reply: Option<Arc<str>>,
    pub payload: Bytes,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublishReport {
    pub delivered: usize,
    pub slow_consumers: usize,
}

#[derive(Clone)]
pub struct Broker {
    inner: Arc<Inner>,
}

struct Inner {
    config: BrokerConfig,
    registry: RwLock<Registry>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct Registry {
    exact: HashMap<Arc<str>, Vec<Arc<Entry>>>,
    wildcard: Vec<Arc<Entry>>,
    queues: HashMap<Arc<str>, Arc<QueueGroup>>,
    count: usize,
}

struct Entry {
    id: u64,
    pattern: Arc<str>,
    queue: Option<Arc<QueueGroup>>,
    sender: mpsc::Sender<Message>,
    slow: Arc<AtomicBool>,
    last_delivery: AtomicU64,
}

struct QueueGroup {
    name: Arc<str>,
    cursor: AtomicU64,
}

pub struct Subscription {
    id: u64,
    pattern: Arc<str>,
    queue_name: Option<Arc<str>>,
    slow: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Message>,
    broker: Weak<Inner>,
    closed: bool,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new(BrokerConfig::default()).expect("valid default broker configuration")
    }
}

impl Broker {
    pub fn new(config: BrokerConfig) -> Result<Self> {
        if config.subscription_capacity == 0
            || config.subscription_capacity > tokio::sync::Semaphore::MAX_PERMITS
            || config.max_payload_bytes == 0
            || config.max_subject_bytes == 0
            || config.max_subscriptions == 0
        {
            return Err(Error::InvalidConfig("limits must be positive".into()));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                registry: RwLock::new(Registry::default()),
                next_id: AtomicU64::new(1),
            }),
        })
    }

    pub fn config(&self) -> &BrokerConfig {
        &self.inner.config
    }

    pub fn subscribe(&self, subject: &str, queue: Option<&str>) -> Result<Subscription> {
        validate_subject(subject, true, self.inner.config.max_subject_bytes)?;
        if queue.is_some_and(|q| {
            q.is_empty()
                || q.len() > 128
                || !q
                    .bytes()
                    .all(|c| c.is_ascii_graphic() && c != b'*' && c != b'>')
        }) {
            return Err(Error::InvalidQueue);
        }
        let mut registry = self
            .inner
            .registry
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if registry.count >= self.inner.config.max_subscriptions {
            return Err(Error::SubscriptionLimit);
        }
        let (sender, receiver) = mpsc::channel(self.inner.config.subscription_capacity);
        let queue = queue.map(|name| {
            registry
                .queues
                .entry(Arc::from(name))
                .or_insert_with(|| {
                    Arc::new(QueueGroup {
                        name: Arc::from(name),
                        cursor: AtomicU64::new(0),
                    })
                })
                .clone()
        });
        let entry = Arc::new(Entry {
            id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
            pattern: Arc::from(subject),
            queue,
            sender,
            slow: Arc::new(AtomicBool::new(false)),
            last_delivery: AtomicU64::new(0),
        });
        if subject.contains(['*', '>']) {
            registry.wildcard.push(entry.clone());
        } else {
            registry
                .exact
                .entry(entry.pattern.clone())
                .or_default()
                .push(entry.clone());
        }
        registry.count += 1;
        Ok(Subscription {
            id: entry.id,
            pattern: entry.pattern.clone(),
            queue_name: entry.queue.as_ref().map(|queue| queue.name.clone()),
            slow: entry.slow.clone(),
            receiver,
            broker: Arc::downgrade(&self.inner),
            closed: false,
        })
    }

    /// Delivers immediately to available subscribers. Full queues are disconnected
    /// and counted in the report; publisher latency never depends on consumption.
    pub fn publish(&self, subject: &str, payload: Bytes) -> Result<PublishReport> {
        self.publish_with_reply(subject, None, payload)
    }

    pub fn publish_with_reply(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<PublishReport> {
        validate_subject(subject, false, self.inner.config.max_subject_bytes)?;
        if let Some(reply) = reply {
            validate_subject(reply, false, self.inner.config.max_subject_bytes)?;
        }
        if payload.len() > self.inner.config.max_payload_bytes {
            return Err(Error::PayloadTooLarge {
                size: payload.len(),
                limit: self.inner.config.max_payload_bytes,
            });
        }
        let registry = self
            .inner
            .registry
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let exact = registry
            .exact
            .get(subject)
            .map(Vec::as_slice)
            .unwrap_or_default();
        // No allocation when an exact subject has no listeners and there are no
        // wildcard listeners. Ordinary fanout requires no temporary candidate list.
        if exact.is_empty() && registry.wildcard.is_empty() {
            return Ok(PublishReport::default());
        }
        let message = Message {
            subject: exact
                .first()
                .map_or_else(|| Arc::from(subject), |entry| entry.pattern.clone()),
            reply: reply.map(Arc::from),
            payload,
        };
        let mut report = PublishReport::default();
        let mut queues: HashMap<&str, Vec<&Entry>> = HashMap::new();
        for entry in exact.iter().chain(
            registry
                .wildcard
                .iter()
                .filter(|e| subject_matches(&e.pattern, subject)),
        ) {
            if entry.slow.load(Ordering::Acquire) || entry.sender.is_closed() {
                continue;
            }
            if let Some(queue) = &entry.queue {
                queues.entry(&queue.name).or_default().push(entry);
            } else {
                deliver(entry, &message, &mut report);
            }
        }
        if !queues.is_empty() {
            for candidates in queues.values() {
                let stamp = candidates[0]
                    .queue
                    .as_ref()
                    .unwrap()
                    .cursor
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                // Least recently served is fair even when different subjects
                // produce overlapping candidate sets for the same queue name.
                let preferred = candidates
                    .iter()
                    .min_by_key(|entry| entry.last_delivery.load(Ordering::Relaxed))
                    .unwrap();
                preferred.last_delivery.store(stamp, Ordering::Relaxed);
                if deliver(preferred, &message, &mut report) {
                    continue;
                }
                for entry in candidates.iter().filter(|entry| entry.id != preferred.id) {
                    if entry.slow.load(Ordering::Acquire) {
                        continue;
                    }
                    entry.last_delivery.store(stamp, Ordering::Relaxed);
                    if deliver(entry, &message, &mut report) {
                        break;
                    }
                }
            }
        }
        Ok(report)
    }

    pub async fn request(
        &self,
        subject: &str,
        payload: Bytes,
        timeout: Duration,
    ) -> Result<Message> {
        let mut random = [0u8; 16];
        aws_lc_rs::rand::fill(&mut random)
            .map_err(|_| Error::Storage("request inbox entropy unavailable".into()))?;
        let inbox = format!("_INBOX.{:032x}", u128::from_le_bytes(random));
        let mut subscription = self.subscribe(&inbox, None)?;
        let report = self.publish_with_reply(subject, Some(&inbox), payload)?;
        if report.delivered == 0 {
            return Err(Error::NoResponders);
        }
        tokio::time::timeout(timeout, subscription.recv())
            .await
            .map_err(|_| Error::Timeout)?
    }

    pub fn subscription_count(&self) -> usize {
        self.inner
            .registry
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .count
    }
}

fn deliver(entry: &Entry, message: &Message, report: &mut PublishReport) -> bool {
    match entry.sender.try_send(message.clone()) {
        Ok(()) => {
            report.delivered += 1;
            true
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            if !entry.slow.swap(true, Ordering::AcqRel) {
                report.slow_consumers += 1;
            }
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

impl Subscription {
    pub async fn recv(&mut self) -> Result<Message> {
        self.check_status()?;
        let message = self.receiver.recv().await.ok_or(Error::Closed)?;
        self.check_status()?;
        Ok(message)
    }

    pub fn try_recv(&mut self) -> Result<Option<Message>> {
        self.check_status()?;
        match self.receiver.try_recv() {
            Ok(message) => {
                self.check_status()?;
                Ok(Some(message))
            }
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(Error::Closed),
        }
    }

    fn check_status(&mut self) -> Result<()> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.slow.load(Ordering::Acquire) {
            self.close();
            return Err(Error::SlowConsumer);
        }
        Ok(())
    }

    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.receiver.close();
        if let Some(broker) = self.broker.upgrade() {
            let mut registry = broker.registry.write().unwrap_or_else(|p| p.into_inner());
            if self.pattern.contains(['*', '>']) {
                registry.wildcard.retain(|e| e.id != self.id);
            } else if let Some(entries) = registry.exact.get_mut(&self.pattern) {
                entries.retain(|e| e.id != self.id);
                if entries.is_empty() {
                    registry.exact.remove(&self.pattern);
                }
            }
            registry.count -= 1;
            if let Some(name) = &self.queue_name
                && registry
                    .queues
                    .get(name)
                    .is_some_and(|queue| Arc::strong_count(queue) == 1)
            {
                registry.queues.remove(name);
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) fn validate_subject(subject: &str, wildcards: bool, max_bytes: usize) -> Result<()> {
    if subject.is_empty()
        || subject.len() > max_bytes
        || subject.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::InvalidSubject(subject.into()));
    }
    let mut tokens = subject.split('.').peekable();
    while let Some(token) = tokens.next() {
        if token.is_empty()
            || (token.contains(['*', '>']) && (!wildcards || (token != "*" && token != ">")))
            || (token == ">" && tokens.peek().is_some())
        {
            return Err(Error::InvalidSubject(subject.into()));
        }
    }
    Ok(())
}

pub(crate) fn subject_matches(pattern: &str, subject: &str) -> bool {
    let mut subject = subject.split('.');
    for token in pattern.split('.') {
        let Some(actual) = subject.next() else {
            return false;
        };
        if token == ">" {
            return true;
        }
        if token != "*" && token != actual {
            return false;
        }
    }
    subject.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[tokio::test]
    async fn exact_wildcard_and_shared_payload() {
        let broker = Broker::default();
        let mut exact = broker.subscribe("orders.created", None).unwrap();
        let mut star = broker.subscribe("orders.*", None).unwrap();
        let mut tail = broker.subscribe("orders.>", None).unwrap();
        let payload = Bytes::from(vec![42; 256]);
        assert_eq!(
            broker
                .publish("orders.created", payload.clone())
                .unwrap()
                .delivered,
            3
        );
        for message in [
            exact.recv().await.unwrap(),
            star.recv().await.unwrap(),
            tail.recv().await.unwrap(),
        ] {
            assert_eq!(message.payload.as_ptr(), payload.as_ptr());
        }
        assert_eq!(broker.publish("orders", Bytes::new()).unwrap().delivered, 0);
        assert_eq!(
            broker
                .publish("orders.created.local", Bytes::new())
                .unwrap()
                .delivered,
            1
        );
    }

    #[tokio::test]
    async fn queues_balance_and_fanout_receives_every_message() {
        let broker = Broker::default();
        let mut a = broker.subscribe("jobs.*", Some("workers")).unwrap();
        let mut b = broker.subscribe("jobs.run", Some("workers")).unwrap();
        let mut fanout = broker.subscribe("jobs.run", None).unwrap();
        for _ in 0..20 {
            assert_eq!(
                broker.publish("jobs.run", Bytes::new()).unwrap().delivered,
                2
            );
        }
        for _ in 0..10 {
            a.recv().await.unwrap();
            b.recv().await.unwrap();
        }
        assert!(a.try_recv().unwrap().is_none());
        assert!(b.try_recv().unwrap().is_none());
        for _ in 0..20 {
            fanout.recv().await.unwrap();
        }
    }

    #[test]
    fn interleaved_queue_groups_do_not_starve_workers() {
        let broker = Broker::default();
        let mut a = broker.subscribe("a", Some("group_a")).unwrap();
        let mut b = broker.subscribe("a", Some("group_a")).unwrap();
        let mut c = broker.subscribe("b", Some("group_a")).unwrap();
        let mut d = broker.subscribe("b", Some("group_a")).unwrap();
        for _ in 0..20 {
            broker.publish("a", Bytes::new()).unwrap();
            broker.publish("b", Bytes::new()).unwrap();
        }
        for sub in [&mut a, &mut b, &mut c, &mut d] {
            for _ in 0..10 {
                assert!(sub.try_recv().unwrap().is_some());
            }
            assert!(sub.try_recv().unwrap().is_none());
        }
        drop((a, b, c, d));
        assert!(broker.inner.registry.read().unwrap().queues.is_empty());
    }

    #[tokio::test]
    async fn slow_consumer_is_explicit_and_queue_can_fail_over() {
        let broker = Broker::new(BrokerConfig {
            subscription_capacity: 1,
            ..Default::default()
        })
        .unwrap();
        let mut slow = broker.subscribe("x", None).unwrap();
        broker.publish("x", Bytes::new()).unwrap();
        assert_eq!(broker.publish("x", Bytes::new()).unwrap().slow_consumers, 1);
        assert!(matches!(slow.recv().await, Err(Error::SlowConsumer)));
        assert!(matches!(slow.recv().await, Err(Error::Closed)));
        assert_eq!(broker.subscription_count(), 0);
        let mut a = broker.subscribe("q", Some("group")).unwrap();
        let mut b = broker.subscribe("q", Some("group")).unwrap();
        broker.publish("q", Bytes::new()).unwrap();
        broker.publish("q", Bytes::new()).unwrap();
        b.recv().await.unwrap();
        let report = broker.publish("q", Bytes::new()).unwrap();
        assert_eq!(
            report,
            PublishReport {
                delivered: 1,
                slow_consumers: 1
            }
        );
        assert!(matches!(a.recv().await, Err(Error::SlowConsumer)));
        b.recv().await.unwrap();
    }

    #[tokio::test]
    async fn request_reply_timeout_and_inbox_cleanup() {
        let broker = Broker::default();
        let mut server = broker.subscribe("echo", None).unwrap();
        let responder = broker.clone();
        let task = tokio::spawn(async move {
            let request = server.recv().await.unwrap();
            responder
                .publish(request.reply.as_deref().unwrap(), request.payload)
                .unwrap();
        });
        assert_eq!(
            broker
                .request("echo", Bytes::from_static(b"hello"), Duration::from_secs(1))
                .await
                .unwrap()
                .payload,
            "hello"
        );
        task.await.unwrap();
        assert_eq!(broker.subscription_count(), 0);
        assert!(matches!(
            broker
                .request("absent", Bytes::new(), Duration::from_secs(1))
                .await,
            Err(Error::NoResponders)
        ));
        let _idle = broker.subscribe("idle", None).unwrap();
        assert!(matches!(
            broker
                .request("idle", Bytes::new(), Duration::from_millis(1))
                .await,
            Err(Error::Timeout)
        ));
        assert_eq!(broker.subscription_count(), 1);
    }

    #[tokio::test]
    async fn dropping_last_broker_wakes_waiting_subscriber() {
        let broker = Broker::default();
        let mut subscription = broker.subscribe("shutdown", None).unwrap();
        let task = tokio::spawn(async move { subscription.recv().await });
        tokio::task::yield_now().await;
        drop(broker);
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(Error::Closed)));
    }

    #[test]
    fn validation_limits_and_drop_cleanup() {
        assert!(
            Broker::new(BrokerConfig {
                subscription_capacity: usize::MAX,
                ..Default::default()
            })
            .is_err()
        );
        let broker = Broker::new(BrokerConfig {
            max_subscriptions: 1,
            max_payload_bytes: 2,
            ..Default::default()
        })
        .unwrap();
        for invalid in ["", ".x", "x.", "x..y", "x.>.y", "x.a*", "x y"] {
            assert!(broker.subscribe(invalid, None).is_err());
        }
        for invalid in ["x.*", "x.>"] {
            assert!(broker.publish(invalid, Bytes::new()).is_err());
        }
        assert!(broker.publish("x", Bytes::from_static(b"123")).is_err());
        let sub = broker.subscribe("x", None).unwrap();
        assert!(matches!(
            broker.subscribe("y", None),
            Err(Error::SubscriptionLimit)
        ));
        drop(sub);
        assert_eq!(broker.subscription_count(), 0);
        assert!(broker.subscribe("y", None).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_publishers_deliver_each_message_once() {
        let broker = Broker::new(BrokerConfig {
            subscription_capacity: 4096,
            ..Default::default()
        })
        .unwrap();
        let mut sub = broker.subscribe("parallel", None).unwrap();
        let mut tasks = Vec::new();
        for publisher in 0..4 {
            let broker = broker.clone();
            tasks.push(tokio::spawn(async move {
                for sequence in 0..500 {
                    broker
                        .publish("parallel", Bytes::from(format!("{publisher}:{sequence}")))
                        .unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let mut seen = HashSet::new();
        for _ in 0..2000 {
            assert!(seen.insert(sub.recv().await.unwrap().payload));
        }
        assert_eq!(seen.len(), 2000);
    }
}
