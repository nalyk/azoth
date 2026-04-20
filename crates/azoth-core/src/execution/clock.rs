//! `Clock` — injected time source, the "time is taint, not preface"
//! foundation.
//!
//! The runtime has exactly one canonical clock seam. Every wall-clock read
//! and every monotonic tick that ends up in a persisted event or an
//! evidence timestamp must flow through `Clock`. Tests get `FrozenClock`,
//! replay gets `VirtualClock`, production gets `SystemClock`.
//!
//! This generalizes the TUI's established dual-clock pattern (monotonic
//! `Instant` for animation + wall-clock `SystemTime` for display/resume)
//! to the core runtime. The two clocks serve different failure modes:
//!
//! - `Instant` does not jump backward on DST/NTP, so animation and
//!   elapsed-since math stay sane.
//! - `SystemTime` survives process restarts, so historical timestamps
//!   read coherently after resume.
//!
//! Chronon Plane invariant: anything that affects control flow on the
//! basis of elapsed time — wall-clock budgets, heartbeat stalls, freshness
//! decay — goes through `Clock`. Same seed + same `VirtualClock` trace =
//! byte-identical replay.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Canonical clock interface. All runtime time reads flow through a
/// trait object so tests and replay can substitute deterministic sources.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Wall-clock now. Use for display, RFC3339 event timestamps, and
    /// anything that must survive process restart (resume, forensic
    /// replay).
    fn now(&self) -> SystemTime;

    /// Monotonic now. Use for elapsed-since-T math, animation cadence,
    /// and deadline races — anything that must not jump backward on
    /// DST/NTP.
    fn now_instant(&self) -> Instant;

    /// Convenience: RFC3339 UTC string of the current wall-clock reading.
    /// Chosen over letting callers format themselves so the output format
    /// stays stable across the codebase and survives event-log replay.
    fn now_iso(&self) -> String {
        let st = self.now();
        let secs = st.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let nanos = st
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let odt = time::OffsetDateTime::from_unix_timestamp(secs as i64)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
            + Duration::from_nanos(nanos as u64);
        odt.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
    }
}

/// Production clock: both reads hit the OS.
#[derive(Debug, Default, Clone)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn now_instant(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock frozen at construction. Both reads return the seeded
/// instant forever. Use in unit tests that assert on timestamps.
#[derive(Debug, Clone)]
pub struct FrozenClock {
    wall: SystemTime,
    mono: Instant,
}

impl FrozenClock {
    pub fn new(wall: SystemTime) -> Self {
        Self {
            wall,
            mono: Instant::now(),
        }
    }

    /// Freeze at a specific Unix epoch second. Handy for deterministic
    /// ISO-8601 assertions.
    pub fn from_unix_secs(secs: u64) -> Self {
        Self::new(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

impl Default for FrozenClock {
    fn default() -> Self {
        // 2026-01-01T00:00:00Z — a stable, non-epoch default so tests
        // that forget to seed still get a reasonable ISO timestamp.
        Self::from_unix_secs(1_767_225_600)
    }
}

impl Clock for FrozenClock {
    fn now(&self) -> SystemTime {
        self.wall
    }
    fn now_instant(&self) -> Instant {
        self.mono
    }
}

/// Replay clock whose current readings are externally driven. Used by
/// the forensic projection and by tests that advance time in controlled
/// steps.
#[derive(Debug, Clone)]
pub struct VirtualClock {
    state: Arc<Mutex<VirtualState>>,
}

#[derive(Debug)]
struct VirtualState {
    wall: SystemTime,
    mono: Instant,
}

impl VirtualClock {
    pub fn new(wall: SystemTime) -> Self {
        Self {
            state: Arc::new(Mutex::new(VirtualState {
                wall,
                mono: Instant::now(),
            })),
        }
    }

    pub fn from_unix_secs(secs: u64) -> Self {
        Self::new(UNIX_EPOCH + Duration::from_secs(secs))
    }

    /// Advance both wall and monotonic readings by `d`. Monotonic moves
    /// by real elapsed instant delta — we add `d` to a captured `Instant`
    /// using checked_add, falling back to the old value on overflow
    /// rather than panicking.
    pub fn advance(&self, d: Duration) {
        let mut s = self.state.lock().unwrap();
        s.wall += d;
        if let Some(next) = s.mono.checked_add(d) {
            s.mono = next;
        }
    }

    pub fn set(&self, wall: SystemTime) {
        let mut s = self.state.lock().unwrap();
        s.wall = wall;
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> SystemTime {
        self.state.lock().unwrap().wall
    }
    fn now_instant(&self) -> Instant {
        self.state.lock().unwrap().mono
    }
}

/// Convenience constructor used throughout the crate for default wiring.
pub fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_advances_on_real_time() {
        let c = SystemClock;
        let a = c.now_instant();
        std::thread::sleep(Duration::from_millis(5));
        let b = c.now_instant();
        assert!(b.duration_since(a) >= Duration::from_millis(4));
    }

    #[test]
    fn frozen_clock_returns_identical_readings() {
        let c = FrozenClock::from_unix_secs(1_700_000_000);
        let a = c.now();
        std::thread::sleep(Duration::from_millis(2));
        let b = c.now();
        assert_eq!(a, b);
    }

    #[test]
    fn frozen_clock_iso_is_stable() {
        let c = FrozenClock::from_unix_secs(1_700_000_000);
        assert_eq!(c.now_iso(), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn virtual_clock_advance_moves_both_readings() {
        let c = VirtualClock::from_unix_secs(1_700_000_000);
        let w0 = c.now();
        let m0 = c.now_instant();
        c.advance(Duration::from_secs(60));
        assert_eq!(c.now().duration_since(w0).unwrap(), Duration::from_secs(60));
        assert!(c.now_instant().duration_since(m0) >= Duration::from_secs(60));
    }

    #[test]
    fn virtual_clock_set_jumps_wall() {
        let c = VirtualClock::from_unix_secs(1_700_000_000);
        c.set(UNIX_EPOCH + Duration::from_secs(1_800_000_000));
        assert_eq!(c.now_iso(), "2027-01-15T08:00:00Z");
    }

    #[test]
    fn clock_trait_is_object_safe() {
        let _: Arc<dyn Clock> = Arc::new(SystemClock);
        let _: Arc<dyn Clock> = Arc::new(FrozenClock::default());
        let _: Arc<dyn Clock> = Arc::new(VirtualClock::from_unix_secs(0));
    }
}
