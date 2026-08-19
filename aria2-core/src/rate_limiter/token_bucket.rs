use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{debug, warn};

use crate::constants;

/// Nanoseconds per second — used for integer time/rate conversions.
const NS_PER_SEC: u64 = 1_000_000_000;

/// Minimum wait duration before issuing a `tokio::time::sleep`.
/// Waits shorter than this use a spin-loop hint instead to avoid
/// the scheduling overhead of waking a task for sub-microsecond delays.
const MIN_SLEEP: Duration = Duration::from_micros(1);

/// Lock-free token bucket using atomic CAS operations.
///
/// All mutable state is stored in `AtomicU64` — no `Mutex` is acquired on the
/// hot path. Token refill is computed lazily on each `acquire` / `try_acquire`
/// call based on elapsed time since the last refill.
///
/// Integer arithmetic is used throughout (no `f64`) for deterministic behaviour
/// and to avoid floating-point CAS issues. Token counts are tracked in
/// **milli-tokens** (tokens * 1000) to provide sub-token precision while
/// staying in integer domain.
///
/// All public methods take `&self` (not `&mut self`), enabling concurrent
/// access from multiple tasks via a shared reference.
pub struct TokenBucket {
    /// Current token count in milli-tokens (tokens * 1000).
    /// Updated via CAS — never read-modify-write without compare_exchange.
    tokens_milli: AtomicU64,
    /// Maximum capacity in milli-tokens. Immutable after construction.
    capacity_milli: u64,
    /// Refill rate in milli-tokens per second. Mutable via `set_rate` for
    /// dynamic rate adjustment.
    /// `rate_milli_per_sec = rate_bytes_per_sec * 1000`.
    rate_milli_per_sec: AtomicU64,
    /// Last refill timestamp — nanoseconds elapsed since `anchor`.
    /// Updated via CAS to claim a refill slot (only the winning thread adds tokens).
    last_refill_elapsed_ns: AtomicU64,
    /// Whether this bucket is unlimited (rate = infinity). Mutable via
    /// `set_unlimited` for dynamic mode switching.
    unlimited: AtomicBool,
    /// Anchor `Instant` created at construction; used to compute elapsed nanoseconds.
    /// Never mutated — `Instant` is `Send + Sync`.
    anchor: Instant,
    /// Broadcasts dynamic rate changes to tasks currently waiting for tokens.
    rate_changed: watch::Sender<u64>,
}

impl fmt::Debug for TokenBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenBucket")
            .field("tokens_milli", &self.tokens_milli.load(Ordering::Relaxed))
            .field("capacity_milli", &self.capacity_milli)
            .field(
                "rate_milli_per_sec",
                &self.rate_milli_per_sec.load(Ordering::Relaxed),
            )
            .field("unlimited", &self.unlimited.load(Ordering::Relaxed))
            .finish()
    }
}

impl TokenBucket {
    /// Create a new token bucket with the given rate and optional burst.
    ///
    /// `rate_bytes_per_sec` of 0 produces a bucket that never refills — callers
    /// should use [`TokenBucket::unlimited`] instead for "no limit" semantics.
    pub fn new(rate_bytes_per_sec: u64, burst_bytes: Option<u64>) -> Self {
        let burst = burst_bytes.unwrap_or(constants::DEFAULT_BURST_BYTES as u64);
        let anchor = Instant::now();
        let (rate_changed, _) = watch::channel(0u64);
        Self {
            tokens_milli: AtomicU64::new(burst.saturating_mul(1000)),
            capacity_milli: burst.saturating_mul(1000),
            rate_milli_per_sec: AtomicU64::new(rate_bytes_per_sec.saturating_mul(1000)),
            last_refill_elapsed_ns: AtomicU64::new(0),
            unlimited: AtomicBool::new(false),
            anchor,
            rate_changed,
        }
    }

    /// Create an unlimited token bucket — `acquire` / `try_acquire` always
    /// succeed instantly without consuming any real tokens.
    pub fn unlimited() -> Self {
        let anchor = Instant::now();
        let (rate_changed, _) = watch::channel(0u64);
        // Use a large but safe value to avoid overflow on arithmetic.
        let huge = u64::MAX / 4;
        Self {
            tokens_milli: AtomicU64::new(huge),
            capacity_milli: huge,
            rate_milli_per_sec: AtomicU64::new(huge),
            last_refill_elapsed_ns: AtomicU64::new(0),
            unlimited: AtomicBool::new(true),
            anchor,
            rate_changed,
        }
    }

    /// Returns `true` if this bucket has no rate limit.
    pub fn is_unlimited(&self) -> bool {
        self.unlimited.load(Ordering::Relaxed)
    }

    /// Returns the configured rate in bytes per second (as `f64` for API compat).
    /// Returns `f64::MAX` for unlimited buckets.
    pub fn rate(&self) -> f64 {
        if self.unlimited.load(Ordering::Relaxed) {
            f64::MAX
        } else {
            self.rate_milli_per_sec.load(Ordering::Relaxed) as f64 / 1000.0
        }
    }

    /// Returns the current available tokens (as `f64` for API compat).
    /// Triggers a lazy refill before reading.
    /// Returns `f64::MAX` for unlimited buckets.
    pub fn available_tokens(&self) -> f64 {
        if self.unlimited.load(Ordering::Relaxed) {
            return f64::MAX;
        }
        self.refill();
        self.tokens_milli.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Nanoseconds elapsed since the anchor `Instant`.
    #[inline]
    fn now_ns(&self) -> u64 {
        // saturating_duration_since avoids panic on clock anomalies.
        // now - anchor = elapsed time since construction.
        Instant::now()
            .saturating_duration_since(self.anchor)
            .as_nanos() as u64
    }

    /// Lazily refill tokens based on elapsed time since the last refill.
    ///
    /// Uses a **CAS-claim** pattern: only the thread that successfully advances
    /// `last_refill_elapsed_ns` adds tokens. This prevents double-counting when
    /// multiple threads call `refill` concurrently.
    ///
    /// Formula: `added_milli = elapsed_ns * rate_milli_per_sec / NS_PER_SEC`
    /// (the 1000× from milli-tokens cancels with the 1000× in rate_milli_per_sec).
    fn refill(&self) {
        if self.unlimited.load(Ordering::Relaxed) {
            return;
        }
        let now = self.now_ns();
        let last = self.last_refill_elapsed_ns.load(Ordering::Relaxed);
        if now <= last {
            // No time elapsed since last refill (or clock went backwards).
            return;
        }
        let elapsed_ns = now - last;
        // u128 to avoid overflow: elapsed_ns (u64) * rate_milli_per_sec (u64).
        let added_milli = ((elapsed_ns as u128)
            * (self.rate_milli_per_sec.load(Ordering::Relaxed) as u128)
            / NS_PER_SEC as u128) as u64;
        if added_milli == 0 {
            // Less than 1 milli-token elapsed — do NOT advance last_refill to
            // preserve fractional accumulation for the next call.
            return;
        }
        // Claim the refill: only the winner of this CAS proceeds to add tokens.
        // Losers abort — another thread already refilled for a overlapping period.
        match self.last_refill_elapsed_ns.compare_exchange(
            last,
            now,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                // Won the claim — add tokens, capping at capacity.
                loop {
                    let current = self.tokens_milli.load(Ordering::Relaxed);
                    let new = current.saturating_add(added_milli).min(self.capacity_milli);
                    match self.tokens_milli.compare_exchange_weak(
                        current,
                        new,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(_) => continue, // Another thread modified tokens — retry.
                    }
                }
            }
            Err(_) => {
                // Lost the claim — another thread already refilled. Nothing to do.
            }
        }
    }

    /// Acquire `bytes` tokens, blocking (async-sleeping) until enough tokens
    /// are available.
    ///
    /// For requests larger than the burst capacity, this method waits for the
    /// deficit and then force-acquires (setting tokens to 0), matching the
    /// original implementation's behaviour of allowing token "debt" clamped to
    /// zero. This prevents infinite loops when `needed > capacity`.
    pub async fn acquire(&self, bytes: u64) {
        if self.unlimited.load(Ordering::Relaxed) {
            return;
        }
        // milli-tokens needed; saturating_mul caps at u64::MAX on overflow.
        let needed_milli = bytes.saturating_mul(1000);

        loop {
            if self.unlimited.load(Ordering::Relaxed) {
                return;
            }

            // Subscribe before inspecting the bucket so a concurrent rate
            // change cannot be missed between the availability check and the
            // wait registration.
            let mut rate_changes = self.rate_changed.subscribe();
            self.refill();
            let current = self.tokens_milli.load(Ordering::Relaxed);
            if current >= needed_milli {
                // Enough tokens — try CAS to deduct.
                match self.tokens_milli.compare_exchange_weak(
                    current,
                    current - needed_milli,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(_) => continue, // Raced — retry.
                }
            }

            // Not enough tokens — compute wait time from the deficit.
            let deficit_milli = needed_milli - current;
            let rate_milli = self.rate_milli_per_sec.load(Ordering::Relaxed);
            if rate_milli == 0 {
                // Rate is 0 — would wait forever. Defensively treat as unlimited
                // rather than hanging the caller.
                warn!("TokenBucket::acquire with rate=0; treating as unlimited");
                return;
            }
            // wait_ns = deficit_milli * NS_PER_SEC / rate_milli_per_sec
            // (u128 to avoid overflow).
            let wait_ns =
                ((deficit_milli as u128) * NS_PER_SEC as u128 / rate_milli as u128) as u64;
            let wait = Duration::from_nanos(wait_ns);

            if wait < MIN_SLEEP {
                // Very short wait — spin instead of paying scheduler overhead.
                std::hint::spin_loop();
                continue;
            }

            debug!(
                bytes = bytes,
                deficit_milli = deficit_milli,
                wait_ns = wait_ns,
                "throttling: sleeping for token refill"
            );
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                changed = rate_changes.changed() => {
                    if changed.is_ok() {
                        continue;
                    }
                }
            }

            // After sleeping, force-acquire: refill, then deduct (clamped to 0).
            // This matches the original behaviour where tokens can go negative
            // (clamped to 0) when the request exceeds burst capacity.
            self.refill();
            loop {
                let cur = self.tokens_milli.load(Ordering::Relaxed);
                let new = cur.saturating_sub(needed_milli);
                match self.tokens_milli.compare_exchange_weak(
                    cur,
                    new,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(_) => continue,
                }
            }
        }
    }

    /// Non-blocking attempt to acquire `bytes` tokens.
    /// Returns `true` if tokens were available and deducted, `false` otherwise.
    pub fn try_acquire(&self, bytes: u64) -> bool {
        if self.unlimited.load(Ordering::Relaxed) {
            return true;
        }
        self.refill();
        let needed_milli = bytes.saturating_mul(1000);
        loop {
            let current = self.tokens_milli.load(Ordering::Relaxed);
            if current < needed_milli {
                return false;
            }
            match self.tokens_milli.compare_exchange_weak(
                current,
                current - needed_milli,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// Update the refill rate dynamically. Takes effect on the next refill cycle.
    /// `rate_bytes_per_sec` of 0 effectively pauses the bucket (no new tokens).
    pub fn set_rate(&self, rate_bytes_per_sec: u64) {
        self.rate_milli_per_sec
            .store(rate_bytes_per_sec.saturating_mul(1000), Ordering::Relaxed);
        self.rate_changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Toggle the unlimited flag. When set to true, acquire/try_acquire always
    /// succeed instantly without consuming tokens.
    pub fn set_unlimited(&self, unlimited: bool) {
        self.unlimited.store(unlimited, Ordering::Relaxed);
        self.rate_changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}
