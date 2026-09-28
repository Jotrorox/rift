use std::{collections::HashMap, net::IpAddr, time::Instant};

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
    settings: Option<RateLimit>,
    global: Option<Bucket>,
    ips: HashMap<IpAddr, Bucket>,
    last_prune: Option<Instant>,
}

impl Admission {
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
}
