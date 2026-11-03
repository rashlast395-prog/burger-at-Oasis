//! Hybrid logical clock, injectable time source, and id generation.
//!
//! An [`Hlc`] orders events across devices without trusting any wall clock completely:
//! it never goes backwards on one device (even if the OS clock is set back), and a device
//! that has *seen* an event always stamps its next event after it.
//!
//! The textual form `"{wall:016x}-{counter:08x}-{device}"` compares correctly as a plain
//! string, and the trailing device id breaks ties between devices, so any two events have a
//! strict total order that every replica computes identically.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of "now" in milliseconds since the Unix epoch.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }
}

/// A clock you set by hand, for tests and for simulating skewed devices.
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(ms: u64) -> ManualClock {
        ManualClock(AtomicU64::new(ms))
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Hlc {
    pub wall: u64,
    pub counter: u32,
}

impl Hlc {
    /// Stamp a new local event: strictly greater than anything this clock produced or saw.
    pub fn tick(&mut self, now_ms: u64) -> Hlc {
        let wall_now = Hlc { wall: now_ms, counter: 0 };
        *self = if wall_now > *self { wall_now } else { Hlc { wall: self.wall, counter: self.counter + 1 } };
        *self
    }

    /// Record that an event stamped `remote` exists, so later local events sort after it.
    /// A remote wall time more than `max_drift_ms` ahead of local time is clamped, so one
    /// device with a wildly wrong clock cannot drag every other clock into the future.
    /// Returns `true` if clamping happened. (Event ordering itself always uses the event's
    /// own stamp; this only limits how far the *local* clock moves.)
    pub fn observe(&mut self, remote: Hlc, now_ms: u64, max_drift_ms: u64) -> bool {
        let limit = now_ms.saturating_add(max_drift_ms);
        let (seen, clamped) = if remote.wall > limit { (Hlc { wall: limit, counter: 0 }, true) } else { (remote, false) };
        if seen > *self {
            *self = seen;
        }
        clamped
    }

    pub fn encode(&self, device: &str) -> String {
        format!("{:016x}-{:08x}-{device}", self.wall, self.counter)
    }

    /// Parse the textual form; returns the clock value and the device id.
    pub fn decode(s: &str) -> Option<(Hlc, &str)> {
        let mut parts = s.splitn(3, '-');
        let wall = u64::from_str_radix(parts.next()?, 16).ok()?;
        let counter = u32::from_str_radix(parts.next()?, 16).ok()?;
        let device = parts.next()?;
        if s.len() < 27 + 1 || !valid_device_id(device) {
            return None;
        }
        Some((Hlc { wall, counter }, device))
    }
}

pub fn valid_device_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("the operating system's random source is unavailable");
    b
}

pub fn new_device_id() -> String {
    random_bytes::<16>().iter().map(|b| format!("{b:02x}")).collect()
}

/// A UUIDv7: 48-bit millisecond timestamp, then random bits. Ids sort by creation time.
pub fn new_uuid_v7(now_ms: u64) -> String {
    let r = random_bytes::<10>();
    let mut b = [0u8; 16];
    b[0..6].copy_from_slice(&now_ms.to_be_bytes()[2..8]);
    b[6] = 0x70 | (r[0] & 0x0F);
    b[7] = r[1];
    b[8] = 0x80 | (r[2] & 0x3F);
    b[9..16].copy_from_slice(&r[3..10]);
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn valid_uuid_v7(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() == 36
        && [8, 13, 18, 23].iter().all(|&i| b[i] == b'-')
        && s.bytes().enumerate().all(|(i, c)| [8, 13, 18, 23].contains(&i) || c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        && b[14] == b'7'
        && matches!(b[19], b'8' | b'9' | b'a' | b'b')
}
