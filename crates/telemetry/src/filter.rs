use tracing_subscriber::EnvFilter;

/// Crates that sit underneath the OTLP exporter. If their spans were exported
/// we'd get a feedback loop (exporting a span creates spans that need
/// exporting...), so they are always switched off for the trace layer. They
/// are *not* forced off for logs; `LOG_FILTER=trace` shows them if you ask.
const TRACE_GUARD: &str = "opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off,\
                           opentelemetry_http=off,reqwest=off,hyper=off,hyper_util=off,h2=off,\
                           tower=off,rustls=off";

pub(crate) fn log_filter(directives: &str) -> anyhow::Result<EnvFilter> {
    Ok(EnvFilter::builder().parse(directives)?)
}

pub(crate) fn trace_filter(directives: &str) -> anyhow::Result<EnvFilter> {
    // Per-target directives are more specific than a bare level, so the guard
    // wins over e.g. `TRACE_FILTER=trace`.
    Ok(EnvFilter::builder().parse(format!("{directives},{TRACE_GUARD}"))?)
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
        let f = trace_filter("trace").unwrap().to_string();
        assert!(f.contains("h2=off"), "{f}");
    }
}
