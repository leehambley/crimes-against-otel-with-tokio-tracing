use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use tracing::Metadata;
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

/// Crates that sit underneath the OTLP exporters. If their spans or logs were
/// exported we'd get a feedback loop (exporting creates telemetry that needs
/// exporting...), so they are always switched off for the OTLP layers. They
/// are *not* forced off for stdout; `LOG_FILTER=trace` shows them if you ask.
const EXPORT_GUARD: &str = "opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off,\
                            opentelemetry_http=off,reqwest=off,hyper=off,hyper_util=off,h2=off,\
                            tower=off,rustls=off";

/// Filter for stdout.
pub(crate) fn log_filter(directives: &str) -> anyhow::Result<EnvFilter> {
    Ok(EnvFilter::builder().parse(directives)?)
}

/// Filter for anything leaving the process over OTLP (spans, log records).
/// Per-target directives are more specific than a bare level, so the guard
/// wins over e.g. `TRACE_FILTER=trace`.
pub(crate) fn export_filter(directives: &str) -> anyhow::Result<EnvFilter> {
    Ok(EnvFilter::builder().parse(format!("{directives},{EXPORT_GUARD}"))?)
}

/// Minimum level for events to become span events, shared between the trace
/// layer's filter and the control socket.
#[derive(Clone)]
pub(crate) struct SpanEventLevel(Arc<AtomicU8>);

impl SpanEventLevel {
    pub(crate) fn new(level: LevelFilter) -> Self {
        Self(Arc::new(AtomicU8::new(encode(level))))
    }

    pub(crate) fn get(&self) -> LevelFilter {
        decode(self.0.load(Ordering::Relaxed))
    }

    pub(crate) fn set(&self, level: LevelFilter) {
        self.0.store(encode(level), Ordering::Relaxed);
        // Filter results are cached per callsite; make them re-evaluate.
        tracing::callsite::rebuild_interest_cache();
    }

    /// Spans always pass (TRACE_FILTER decides); events must meet the level.
    pub(crate) fn allows(&self, meta: &Metadata<'_>) -> bool {
        meta.is_span() || *meta.level() <= self.get()
    }
}

fn encode(level: LevelFilter) -> u8 {
    match level.into_level() {
        None => 0,
        Some(tracing::Level::ERROR) => 1,
        Some(tracing::Level::WARN) => 2,
        Some(tracing::Level::INFO) => 3,
        Some(tracing::Level::DEBUG) => 4,
        Some(tracing::Level::TRACE) => 5,
    }
}

fn decode(v: u8) -> LevelFilter {
    match v {
        0 => LevelFilter::OFF,
        1 => LevelFilter::ERROR,
        2 => LevelFilter::WARN,
        3 => LevelFilter::INFO,
        4 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage() {
        assert!(log_filter("info,foo=[[").is_err());
    }

    #[test]
    fn guard_is_appended() {
        let f = export_filter("trace").unwrap().to_string();
        assert!(f.contains("h2=off"), "{f}");
    }

    #[test]
    fn span_event_level_round_trips() {
        for level in [
            LevelFilter::OFF,
            LevelFilter::ERROR,
            LevelFilter::WARN,
            LevelFilter::INFO,
            LevelFilter::DEBUG,
            LevelFilter::TRACE,
        ] {
            assert_eq!(decode(encode(level)), level);
        }
    }
}
