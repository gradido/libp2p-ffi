//! Peer classes and the rate limits that apply per class.
//!
//! A class belongs to a group: every node of a group is in the group's class. The caller decides
//! the classes; the module only knows two of them, `LP2P_CLASS_UNKNOWN` (every group nobody
//! classified) and `LP2P_CLASS_BLOCKED` (refused outright).
//!
//! A limit is a token bucket for one class, one scope and one protocol or all of them. A request
//! is admitted only if every limit that matches it has a token left. Scopes:
//!
//! ```text
//! peer        one bucket per node
//! ip prefix   one bucket per IPv4 /24 or IPv6 /56 of the connection -- not applied to relayed
//!             connections, whose visible address is the relay's
//! global      one bucket for the whole class
//! ```
//!
//! Peer ids cost nothing to create, which is why a per-peer limit alone is not a limit, and why
//! the prefix and the global scope exist.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use libp2p::PeerId;

use crate::abi::*;

/// Buckets that are full again are forgotten; this bounds how many there are between sweeps.
const MAX_BUCKETS: usize = 1 << 16;
/// LP2P_EV_LIMITED events per second at most; the rest are only counted. A flood of refused
/// requests must not become a flood of events.
const EVENTS_PER_SECOND: u32 = 10;

/// What identifies a limit: a second limit for the same class, scope and protocol replaces it.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct RuleId {
    class: u8,
    scope: u8,
    protocol: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rule {
    id: RuleId,
    rate: lp2p_rate,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Key {
    Peer(PeerId),
    Prefix([u8; 8]),
    Global,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// What a request is checked by.
pub struct Request<'a> {
    pub group: &'a lp2p_key,
    pub peer: PeerId,
    /// The remote IP, or None for a relayed connection.
    pub ip: Option<IpAddr>,
    pub protocol: u16,
}

#[derive(Default)]
pub struct Limits {
    classes: HashMap<lp2p_key, u8>,
    rules: Vec<Rule>,
    buckets: HashMap<(RuleId, Key), Bucket>,
    events_window: Option<Instant>,
    events_in_window: u32,
}

fn prefix(ip: IpAddr) -> [u8; 8] {
    let mut out = [0u8; 8];
    match ip {
        IpAddr::V4(v4) => {
            out[0] = 4;
            out[1..4].copy_from_slice(&v4.octets()[..3]);
        }
        IpAddr::V6(v6) => {
            out[0] = 6;
            out[1..8].copy_from_slice(&v6.octets()[..7]);
        }
    }
    out
}

fn refill(bucket: &mut Bucket, rate: &lp2p_rate, now: Instant) {
    let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
    let per_second = rate.amount as f64 * 1000.0 / rate.interval_ms as f64;
    bucket.tokens = (bucket.tokens + elapsed * per_second).min(rate.burst as f64);
    bucket.updated = now;
}

impl Limits {
    pub fn set_class(&mut self, group: lp2p_key, class: u8) {
        if class == LP2P_CLASS_UNKNOWN {
            self.classes.remove(&group);
        } else {
            self.classes.insert(group, class);
        }
    }

    pub fn class_of(&self, group: &lp2p_key) -> u8 {
        self.classes.get(group).copied().unwrap_or(LP2P_CLASS_UNKNOWN)
    }

    /// Sets, replaces or -- with any zero field in @p rate -- removes the limit for this class,
    /// scope and protocol.
    pub fn set_limit(&mut self, class: u8, scope: u8, protocol: u16, rate: lp2p_rate) {
        let id = RuleId {
            class,
            scope,
            protocol,
        };
        self.rules.retain(|r| r.id != id);
        if rate.amount != 0 && rate.interval_ms != 0 && rate.burst != 0 {
            // A changed rate keeps the buckets: what a peer has used stays used.
            self.rules.push(Rule { id, rate });
        } else {
            self.buckets.retain(|(rule, _), _| *rule != id);
        }
    }

    /// Admits the request, or answers the reason it is refused: LP2P_LIMITED_BLOCKED, or the
    /// scope of the first limit that had no token left.
    pub fn check(&mut self, request: &Request, now: Instant) -> Result<(), u16> {
        let class = self.class_of(request.group);
        if class == LP2P_CLASS_BLOCKED {
            return Err(LP2P_LIMITED_BLOCKED);
        }
        let matching: Vec<(Rule, Key)> = self
            .rules
            .iter()
            .filter(|r| {
                r.id.class == class
                    && (r.id.protocol == LP2P_PROTOCOL_ANY || r.id.protocol == request.protocol)
            })
            .filter_map(|r| {
                let key = match r.id.scope {
                    LP2P_SCOPE_PEER => Key::Peer(request.peer),
                    LP2P_SCOPE_IP_PREFIX => Key::Prefix(prefix(request.ip?)),
                    _ => Key::Global,
                };
                Some((*r, key))
            })
            .collect();
        // Checked before anything is taken, so a request refused by one limit does not use up
        // the tokens of the others.
        for (rule, key) in &matching {
            let bucket = self.buckets.entry((rule.id, *key)).or_insert(Bucket {
                tokens: rule.rate.burst as f64,
                updated: now,
            });
            refill(bucket, &rule.rate, now);
            if bucket.tokens < 1.0 {
                return Err(rule.id.scope as u16);
            }
        }
        for (rule, key) in &matching {
            if let Some(bucket) = self.buckets.get_mut(&(rule.id, *key)) {
                bucket.tokens -= 1.0;
            }
        }
        if self.buckets.len() > MAX_BUCKETS {
            self.sweep(now);
        }
        Ok(())
    }

    /// Forgets buckets that have refilled completely: they are indistinguishable from new ones.
    pub fn sweep(&mut self, now: Instant) {
        let rules = &self.rules;
        self.buckets.retain(|(id, _), bucket| {
            let Some(rule) = rules.iter().find(|r| r.id == *id) else {
                return false;
            };
            refill(bucket, &rule.rate, now);
            bucket.tokens < rule.rate.burst as f64
        });
    }

    /// Whether another LP2P_EV_LIMITED may be emitted now.
    pub fn may_report(&mut self, now: Instant) -> bool {
        match self.events_window {
            Some(start) if now.duration_since(start) < Duration::from_secs(1) => {
                self.events_in_window += 1;
                self.events_in_window <= EVENTS_PER_SECOND
            }
            _ => {
                self.events_window = Some(now);
                self.events_in_window = 1;
                true
            }
        }
    }
}

/// A fixed token bucket per peer, for announcements: gossipsub delivers what any peer publishes,
/// and without this one node could make every other node report a stream of them.
pub struct SourceRate {
    per_second: f64,
    burst: f64,
    buckets: HashMap<PeerId, Bucket>,
}

impl SourceRate {
    pub fn new(interval: Duration, burst: u32) -> Self {
        SourceRate {
            per_second: 1.0 / interval.as_secs_f64(),
            burst: burst as f64,
            buckets: HashMap::new(),
        }
    }

    pub fn allow(&mut self, peer: PeerId, now: Instant) -> bool {
        if self.buckets.len() > MAX_BUCKETS {
            let (per_second, burst) = (self.per_second, self.burst);
            self.buckets.retain(|_, b| {
                b.tokens + now.saturating_duration_since(b.updated).as_secs_f64() * per_second < burst
            });
        }
        let bucket = self.buckets.entry(peer).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.per_second).min(self.burst);
        bucket.updated = now;
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

    const GROUP: lp2p_key = [1; 32];

    fn request(peer: PeerId, ip: Option<&str>, protocol: u16) -> Request<'static> {
        Request {
            group: &GROUP,
            peer,
            ip: ip.map(|s| s.parse().unwrap()),
            protocol,
        }
    }

    fn rate(amount: u32, interval_ms: u32, burst: u32) -> lp2p_rate {
        lp2p_rate {
            amount,
            interval_ms,
            burst,
        }
    }

    #[test]
    fn a_bucket_allows_its_burst_and_refills_at_its_rate() {
        let mut limits = Limits::default();
        limits.set_limit(
            LP2P_CLASS_UNKNOWN,
            LP2P_SCOPE_PEER,
            LP2P_PROTOCOL_ANY,
            rate(1, 1000, 2),
        );
        let peer = PeerId::random();
        let start = Instant::now();
        assert_eq!(limits.check(&request(peer, None, 0), start), Ok(()));
        assert_eq!(limits.check(&request(peer, None, 1), start), Ok(()));
        assert_eq!(
            limits.check(&request(peer, None, 0), start),
            Err(LP2P_SCOPE_PEER as u16)
        );
        // Another peer has a bucket of its own.
        assert_eq!(limits.check(&request(PeerId::random(), None, 0), start), Ok(()));
        // One token per second.
        let later = start + Duration::from_millis(1100);
        assert_eq!(limits.check(&request(peer, None, 0), later), Ok(()));
        assert!(limits.check(&request(peer, None, 0), later).is_err());
    }

    #[test]
    fn a_prefix_is_shared_and_a_relayed_connection_has_none() {
        let mut limits = Limits::default();
        limits.set_limit(
            LP2P_CLASS_UNKNOWN,
            LP2P_SCOPE_IP_PREFIX,
            LP2P_PROTOCOL_ANY,
            rate(1, 60_000, 1),
        );
        let now = Instant::now();
        assert_eq!(
            limits.check(&request(PeerId::random(), Some("10.0.0.1"), 0), now),
            Ok(())
        );
        assert_eq!(
            limits.check(&request(PeerId::random(), Some("10.0.0.200"), 0), now),
            Err(LP2P_SCOPE_IP_PREFIX as u16)
        );
        assert_eq!(
            limits.check(&request(PeerId::random(), Some("10.0.1.1"), 0), now),
            Ok(())
        );
        assert_eq!(limits.check(&request(PeerId::random(), None, 0), now), Ok(()));
    }

    #[test]
    fn classes_protocols_and_removal() {
        let mut limits = Limits::default();
        limits.set_limit(7, LP2P_SCOPE_GLOBAL, 3, rate(1, 60_000, 1));
        let peer = PeerId::random();
        let now = Instant::now();
        // The group is unknown, so class 7's limit does not apply.
        assert_eq!(limits.check(&request(peer, None, 3), now), Ok(()));
        assert_eq!(limits.check(&request(peer, None, 3), now), Ok(()));
        limits.set_class(GROUP, 7);
        assert_eq!(limits.check(&request(peer, None, 3), now), Ok(()));
        assert!(limits.check(&request(peer, None, 3), now).is_err());
        // Another protocol is not limited.
        assert_eq!(limits.check(&request(peer, None, 4), now), Ok(()));
        // A zero rate removes the limit.
        limits.set_limit(7, LP2P_SCOPE_GLOBAL, 3, rate(0, 0, 0));
        assert_eq!(limits.check(&request(peer, None, 3), now), Ok(()));
        limits.set_class(GROUP, LP2P_CLASS_BLOCKED);
        assert_eq!(
            limits.check(&request(peer, None, 4), now),
            Err(LP2P_LIMITED_BLOCKED)
        );
        limits.set_class(GROUP, LP2P_CLASS_UNKNOWN);
        assert_eq!(limits.class_of(&GROUP), LP2P_CLASS_UNKNOWN);
    }

    #[test]
    fn a_refused_request_takes_no_token_from_the_other_limits() {
        let mut limits = Limits::default();
        limits.set_limit(
            LP2P_CLASS_UNKNOWN,
            LP2P_SCOPE_PEER,
            LP2P_PROTOCOL_ANY,
            rate(1, 60_000, 5),
        );
        limits.set_limit(
            LP2P_CLASS_UNKNOWN,
            LP2P_SCOPE_GLOBAL,
            LP2P_PROTOCOL_ANY,
            rate(1, 60_000, 1),
        );
        let peer = PeerId::random();
        let now = Instant::now();
        assert_eq!(limits.check(&request(peer, None, 0), now), Ok(()));
        for _ in 0..10 {
            assert_eq!(
                limits.check(&request(peer, None, 0), now),
                Err(LP2P_SCOPE_GLOBAL as u16)
            );
        }
        // The global limit goes away; the peer still has four of its five tokens.
        limits.set_limit(
            LP2P_CLASS_UNKNOWN,
            LP2P_SCOPE_GLOBAL,
            LP2P_PROTOCOL_ANY,
            rate(0, 0, 0),
        );
        for _ in 0..4 {
            assert_eq!(limits.check(&request(peer, None, 0), now), Ok(()));
        }
        assert!(limits.check(&request(peer, None, 0), now).is_err());
    }

    #[test]
    fn announcements_are_limited_per_source() {
        let mut rate = SourceRate::new(Duration::from_secs(10), 3);
        let (a, b) = (PeerId::random(), PeerId::random());
        let now = Instant::now();
        assert!((0..3).all(|_| rate.allow(a, now)));
        assert!(!rate.allow(a, now));
        assert!(rate.allow(b, now));
        assert!(rate.allow(a, now + Duration::from_secs(10)));
    }

    #[test]
    fn full_buckets_are_forgotten_and_events_are_bounded() {
        let mut limits = Limits::default();
        limits.set_limit(
            LP2P_CLASS_UNKNOWN,
            LP2P_SCOPE_PEER,
            LP2P_PROTOCOL_ANY,
            rate(1, 1000, 1),
        );
        let now = Instant::now();
        limits.check(&request(PeerId::random(), None, 0), now).unwrap();
        assert_eq!(limits.buckets.len(), 1);
        limits.sweep(now + Duration::from_secs(2));
        assert!(limits.buckets.is_empty());

        let reported = (0..50).filter(|_| limits.may_report(now)).count();
        assert_eq!(reported, EVENTS_PER_SECOND as usize);
        assert!(limits.may_report(now + Duration::from_secs(1)));
    }
}
