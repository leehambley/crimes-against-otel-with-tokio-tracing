//! Simulated failures and latency, so a benchmark run produces a known mix
//! of good, slow and failed traces.
//!
//! | variable            | meaning                                   |
//! |---------------------|-------------------------------------------|
//! | `CHAOS_FAILURE_PCT` | % of operations that fail (default 0)     |
//! | `CHAOS_SLOW_PCT`    | % of operations that stall (default 0)    |
//! | `CHAOS_SLOW_MS`     | how long a stall lasts (default 250)      |
//!
//! All three are also knobs on the control socket (`set chaos.failure_pct 20`).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use telemetry::control::{Control, Knob};
use tracing::Instrument as _;

#[derive(Debug, thiserror::Error)]
#[error("injected failure in {operation}: {kind}")]
pub struct ChaosError {
    pub operation: &'static str,
    pub kind: &'static str,
}

const FAILURE_KINDS: &[&str] = &["connection reset by peer", "timed out", "READONLY replica"];

#[derive(Default)]
pub struct Chaos {
    // f64 bit patterns; atomics so the control socket can change them live.
    failure_pct: AtomicU64,
    slow_pct: AtomicU64,
    slow_ms: AtomicU64,
}

impl Chaos {
    pub fn from_env() -> Arc<Self> {
        let pct = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
                .clamp(0.0, 100.0)
        };
        let chaos = Self::default();
        chaos
            .failure_pct
            .store(pct("CHAOS_FAILURE_PCT").to_bits(), Ordering::Relaxed);
        chaos
            .slow_pct
            .store(pct("CHAOS_SLOW_PCT").to_bits(), Ordering::Relaxed);
        let slow_ms = std::env::var("CHAOS_SLOW_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(250);
        chaos.slow_ms.store(slow_ms, Ordering::Relaxed);
        tracing::info!(
            failure_pct = chaos.failure_pct(),
            slow_pct = chaos.slow_pct(),
            slow_ms,
            "chaos configured"
        );
        Arc::new(chaos)
    }

    pub fn failure_pct(&self) -> f64 {
        f64::from_bits(self.failure_pct.load(Ordering::Relaxed))
    }

    pub fn slow_pct(&self) -> f64 {
        f64::from_bits(self.slow_pct.load(Ordering::Relaxed))
    }

    /// Exposes the knobs on the control socket.
    pub fn register(self: &Arc<Self>, control: &Control) {
        let pct_knob = |pick: fn(&Chaos) -> &AtomicU64| {
            let (get, set) = (self.clone(), self.clone());
            Knob {
                get: Box::new(move || {
                    f64::from_bits(pick(&get).load(Ordering::Relaxed)).to_string()
                }),
                set: Box::new(move |v| {
                    let pct: f64 = v.parse().map_err(|e| format!("{e}"))?;
                    if !(0.0..=100.0).contains(&pct) {
                        return Err("expected 0..=100".into());
                    }
                    pick(&set).store(pct.to_bits(), Ordering::Relaxed);
                    Ok(())
                }),
            }
        };
        control.register("chaos.failure_pct", pct_knob(|c| &c.failure_pct));
        control.register("chaos.slow_pct", pct_knob(|c| &c.slow_pct));
        let (get, set) = (self.clone(), self.clone());
        control.register(
            "chaos.slow_ms",
            Knob {
                get: Box::new(move || get.slow_ms.load(Ordering::Relaxed).to_string()),
                set: Box::new(move |v| {
                    set.slow_ms
                        .store(v.parse().map_err(|e| format!("{e}"))?, Ordering::Relaxed);
                    Ok(())
                }),
            },
        );
    }

    /// Call at the point where a real dependency could misbehave.
    pub async fn inject(&self, operation: &'static str) -> Result<(), ChaosError> {
        if rand::random_bool(self.slow_pct() / 100.0) {
            let ms = self.slow_ms.load(Ordering::Relaxed);
            tracing::event!(target: "metrics", tracing::Level::TRACE, monotonic_counter.chaos_injected = 1u64, kind = "latency", operation = operation);
            tokio::time::sleep(Duration::from_millis(ms))
                .instrument(tracing::info_span!(
                    "chaos.latency",
                    operation,
                    delay_ms = ms
                ))
                .await;
            tracing::warn!(operation, delay_ms = ms, "injected latency");
        }
        if rand::random_bool(self.failure_pct() / 100.0) {
            let kind = FAILURE_KINDS[rand::random_range(0..FAILURE_KINDS.len())];
            tracing::event!(target: "metrics", tracing::Level::TRACE, monotonic_counter.chaos_injected = 1u64, kind = "failure", operation = operation);
            return Err(ChaosError { operation, kind });
        }
        Ok(())
    }
}
