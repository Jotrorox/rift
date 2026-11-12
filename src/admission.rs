use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr},
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};

use rift::config::RateLimit;

struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    fn new(burst: usize, now: Instant) -> Self {
        Self {
            tokens: burst as f64,
            updated: now,
        }
    }

    fn refill(&mut self, rate: usize, burst: usize, now: Instant) {
        self.tokens = (self.tokens + now.duration_since(self.updated).as_secs_f64() * rate as f64)
            .min(burst as f64);
        self.updated = now;
    }
}

#[derive(Default)]
pub struct Admission {
    generation: u64,
    settings: Option<RateLimit>,
    global: Option<Bucket>,
    ips: HashMap<IpAddr, Bucket>,
    last_prune: Option<Instant>,
    /// Open connections per IPv4 address or IPv6 /64.
    open: HashMap<IpAddr, usize>,
}

/// One open connection counted against its address; released on drop.
pub struct AddressSlot {
    admission: Arc<Mutex<Admission>>,
    key: IpAddr,
}

impl AddressSlot {
    /// Count a connection from `ip`, unless its address already holds
    /// `maximum`. Without this one host could take every connection slot.
    pub fn claim(admission: &Arc<Mutex<Admission>>, ip: IpAddr, maximum: usize) -> Option<Self> {
        let key = address_key(ip);
        let mut state = admission.lock().unwrap_or_else(PoisonError::into_inner);
        let open = state.open.entry(key).or_insert(0);
        if *open >= maximum {
            if *open == 0 {
                state.open.remove(&key);
            }
            return None;
        }
        *open += 1;
        Some(Self {
            admission: admission.clone(),
            key,
        })
    }
}

impl Drop for AddressSlot {
    fn drop(&mut self) {
        let mut state = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(open) = state.open.get_mut(&self.key) {
            *open -= 1;
            if *open == 0 {
                state.open.remove(&self.key);
            }
        }
    }
}

/// An IPv4 address (mapped ones included) or the /64 of an IPv6 address: a
/// single host usually controls a whole /64.
fn address_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(ip) => {
            let mut segments = ip.segments();
            segments[4..].fill(0);
            IpAddr::V6(Ipv6Addr::from(segments))
        }
        ip => ip,
    }
}

impl Admission {
    /// Pending handshakes from an older snapshot must not reset newer buckets.
    pub fn allow_generation(
        &mut self,
        ip: IpAddr,
        settings: Option<RateLimit>,
        generation: u64,
        now: Instant,
    ) -> bool {
        let settings = if generation < self.generation {
            self.settings
        } else {
            self.generation = generation;
            settings
        };
        self.allow(ip, settings, now)
    }

    pub fn allow(&mut self, ip: IpAddr, settings: Option<RateLimit>, now: Instant) -> bool {
        // Retain balances across unchanged reloads. A changed policy starts a new
        // set of buckets; established connections never use this lock.
        if self.settings != settings {
            self.settings = settings;
            self.ips.clear();
            self.last_prune = None;
            self.global = settings.map(|s| Bucket::new(s.global_burst, now));
        }
        let Some(s) = settings else {
            return true;
        };
        let global = self.global.as_mut().expect("configured bucket");
        global.refill(s.global_per_second, s.global_burst, now);
        if global.tokens < 1.0 {
            return false;
        }
        // Charge every attempt, including per-IP rejections and full tables.
        global.tokens -= 1.0;
        let ip = match ip {
            IpAddr::V6(ip) => ip
                .to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(ip)),
            ip => ip,
        };
        if !self.ips.contains_key(&ip) && self.ips.len() >= s.max_ips {
            if self
                .last_prune
                .is_some_and(|last| now.duration_since(last).as_secs() < 1)
            {
                return false;
            }
            self.last_prune = Some(now);
            // Evict only fully replenished buckets; rotating source addresses
            // cannot reset a depleted bucket and bypass its limit.
            self.ips.retain(|_, bucket| {
                bucket.refill(s.per_ip_per_second, s.per_ip_burst, now);
                bucket.tokens < s.per_ip_burst as f64
            });
            if self.ips.len() >= s.max_ips {
                return false;
            }
        }
        let bucket = self
            .ips
            .entry(ip)
            .or_insert_with(|| Bucket::new(s.per_ip_burst, now));
        bucket.refill(s.per_ip_per_second, s.per_ip_burst, now);
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn per_ip_global_and_bounded_table_refill_without_eviction_bypass() {
        let settings = Some(RateLimit {
            per_ip_per_second: 2,
            per_ip_burst: 2,
            global_per_second: 4,
            global_burst: 4,
            max_ips: 1,
        });
        let mut limiter = Admission::default();
        let now = Instant::now();
        let a = "127.0.0.1".parse().unwrap();
        let b = "127.0.0.2".parse().unwrap();
        assert!(limiter.allow(a, settings, now));
        assert!(limiter.allow(a, settings, now));
        assert!(!limiter.allow(a, settings, now));
        assert!(!limiter.allow(b, settings, now));
        assert_eq!(limiter.ips.len(), 1);
        assert!(limiter.allow(a, settings, now + Duration::from_millis(500)));
        assert!(!limiter.allow(a, settings, now + Duration::from_millis(500)));
        assert!(limiter.allow(b, settings, now + Duration::from_secs(2)));
        assert_eq!(limiter.ips.len(), 1);
    }

    #[test]
    fn open_connections_are_capped_per_address_and_released() {
        let admission = Arc::new(Mutex::new(Admission::default()));
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let first = AddressSlot::claim(&admission, a, 2).unwrap();
        let _second =
            AddressSlot::claim(&admission, "::ffff:192.0.2.1".parse().unwrap(), 2).unwrap();
        assert!(AddressSlot::claim(&admission, a, 2).is_none());
        // Another address is unaffected; one /64 is a single address.
        let _other = AddressSlot::claim(&admission, "192.0.2.2".parse().unwrap(), 2).unwrap();
        let _v6 = AddressSlot::claim(&admission, "2001:db8::1".parse().unwrap(), 1).unwrap();
        assert!(AddressSlot::claim(&admission, "2001:db8::2".parse().unwrap(), 1).is_none());
        assert!(AddressSlot::claim(&admission, "2001:db8:0:1::2".parse().unwrap(), 1).is_some());
        drop(first);
        assert!(AddressSlot::claim(&admission, a, 2).is_some());
        assert_eq!(admission.lock().unwrap().open.len(), 3);
    }

    #[test]
    fn mapped_ipv4_shares_bucket_and_global_limit_spans_ips() {
        let settings = Some(RateLimit {
            per_ip_per_second: 1,
            per_ip_burst: 1,
            global_per_second: 1,
            global_burst: 3,
            max_ips: 8,
        });
        let mut limiter = Admission::default();
        let now = Instant::now();
        assert!(limiter.allow("127.0.0.1".parse().unwrap(), settings, now));
        assert!(!limiter.allow("::ffff:127.0.0.1".parse().unwrap(), settings, now));
        assert!(limiter.allow("127.0.0.2".parse().unwrap(), settings, now));
        assert!(!limiter.allow("127.0.0.3".parse().unwrap(), settings, now));
    }

    #[test]
    fn old_handshakes_cannot_reset_new_login_buckets() {
        let now = Instant::now();
        let ip = "127.0.0.1".parse().unwrap();
        let settings = Some(RateLimit {
            per_ip_per_second: 1,
            per_ip_burst: 1,
            global_per_second: 1,
            global_burst: 1,
            max_ips: 8,
        });
        let mut limiter = Admission::default();
        assert!(limiter.allow_generation(ip, None, 1, now));
        assert!(limiter.allow_generation(ip, settings, 2, now));
        assert!(!limiter.allow_generation(ip, None, 1, now));
        assert!(!limiter.allow_generation(ip, settings, 3, now));
        assert!(limiter.allow_generation(ip, settings, 3, now + Duration::from_secs(1)));
    }
}
