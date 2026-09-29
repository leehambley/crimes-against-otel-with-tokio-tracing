//! Metrics derived from spans. Every INFO-or-above span that closes records
//! its duration in the semantic-convention instrument for what it is:
//!
//! | span                                   | instrument                      |
//! |----------------------------------------|---------------------------------|
//! | `otel.kind=server` + HTTP attributes   | `http.server.request.duration`  |
//! | `otel.kind=client` + HTTP attributes   | `http.client.request.duration`  |
//! | `db.system.name` set                   | `db.client.operation.duration`  |
//! | anything else                          | `span.duration` (ours)          |
//!
//! Each gets the attributes semconv lists for it, including `error.type` on
//! failure. That gives RED metrics for every handler, outbound call and
//! Valkey command without a hand-written counter, under names dashboards
//! already know. The layer has its own filter, so the numbers are complete
//! regardless of LOG_FILTER, TRACE_FILTER or trace sampling.

use std::time::Instant;

use opentelemetry::{
    KeyValue, Value,
    metrics::{Histogram, MeterProvider},
};
use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span,
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// Semconv-recommended HTTP duration buckets (seconds).
const HTTP_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];
/// Semconv-recommended DB duration buckets (seconds).
const DB_BUCKETS: &[f64] = &[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0];

const HTTP_SERVER_ATTRS: &[&str] = &[
    "http.request.method",
    "http.route",
    "http.response.status_code",
    "url.scheme",
    "network.protocol.version",
    "error.type",
];
const HTTP_CLIENT_ATTRS: &[&str] = &[
    "http.request.method",
    "url.template",
    "url.scheme",
    "server.address",
    "server.port",
    "http.response.status_code",
    "network.protocol.version",
    "error.type",
];
const DB_ATTRS: &[&str] = &[
    "db.system.name",
    "db.operation.name",
    "db.namespace",
    "server.address",
    "server.port",
    "error.type",
];
const GENERIC_ATTRS: &[&str] = &["error.type"];

/// Per-layer filter: spans at INFO+ (what we measure) and ERROR events (so
/// we can mark the enclosing span as failed).
pub(crate) fn interesting(meta: &Metadata<'_>) -> bool {
    if meta.is_span() {
        *meta.level() <= Level::INFO
    } else {
        *meta.level() == Level::ERROR
    }
}

pub(crate) struct SpanMetricsLayer {
    http_server: Histogram<f64>,
    http_client: Histogram<f64>,
    db_client: Histogram<f64>,
    generic: Histogram<f64>,
}

impl SpanMetricsLayer {
    pub(crate) fn new(provider: &impl MeterProvider) -> Self {
        let meter = provider.meter(env!("CARGO_PKG_NAME"));
        let histogram = |name: &'static str, description: &'static str, buckets: &[f64]| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_description(description)
                .with_boundaries(buckets.to_vec())
                .build()
        };
        Self {
            http_server: histogram(
                "http.server.request.duration",
                "Duration of HTTP server requests.",
                HTTP_BUCKETS,
            ),
            http_client: histogram(
                "http.client.request.duration",
                "Duration of HTTP client requests.",
                HTTP_BUCKETS,
            ),
            db_client: histogram(
                "db.client.operation.duration",
                "Duration of database client operations.",
                DB_BUCKETS,
            ),
            generic: histogram(
                "span.duration",
                "Duration of INFO+ tracing spans with no semantic-convention instrument.",
                HTTP_BUCKETS,
            ),
        }
    }
}

/// Span fields we care about, captured as they're recorded.
#[derive(Default)]
struct Fields(Vec<(&'static str, Value)>);

impl Fields {
    fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
    }

    fn put(&mut self, name: &'static str, value: Value) {
        self.0.retain(|(k, _)| *k != name);
        self.0.push((name, value));
    }
}

struct Timing {
    start: Instant,
    error: bool,
    fields: Fields,
}

impl Visit for Timing {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "otel.status_code" {
            self.error |= value.eq_ignore_ascii_case("error");
        } else {
            self.fields.put(field.name(), value.to_owned().into());
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.put(field.name(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.put(field.name(), (value as i64).into());
    }
    fn record_bool(&mut self, _: &Field, _: bool) {}
    fn record_f64(&mut self, _: &Field, _: f64) {}
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `%value` fields arrive here; Debug of a Display wrapper is the Display output.
        if field.name() == "otel.status_code" {
            self.error |= format!("{value:?}").eq_ignore_ascii_case("error");
        } else {
            self.fields.put(field.name(), format!("{value:?}").into());
        }
    }
}

impl Timing {
    fn attributes(&self, keys: &[&'static str]) -> Vec<KeyValue> {
        let mut attrs: Vec<KeyValue> = keys
            .iter()
            .filter_map(|k| self.fields.get(k).map(|v| KeyValue::new(*k, v.clone())))
            .collect();
        // Semconv: `error.type` is required when the operation failed; `_OTHER`
        // when nothing more specific was recorded.
        if self.error && self.fields.get("error.type").is_none() {
            attrs.push(KeyValue::new("error.type", "_OTHER"));
        }
        attrs
    }

    fn kind(&self) -> Option<String> {
        self.fields.get("otel.kind").map(|v| v.as_str().to_string())
    }
}

impl<S> Layer<S> for SpanMetricsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut timing = Timing {
            start: Instant::now(),
            error: false,
            fields: Fields::default(),
        };
        attrs.record(&mut timing);
        span.extensions_mut().insert(timing);
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        if let Some(timing) = span.extensions_mut().get_mut::<Timing>() {
            values.record(timing);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        if let Some(timing) = span.extensions_mut().get_mut::<Timing>() {
            timing.error = true;
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(timing) = span.extensions_mut().remove::<Timing>() else {
            return;
        };
        let seconds = timing.start.elapsed().as_secs_f64();
        let is_http = timing.fields.get("http.request.method").is_some();

        match timing.kind().as_deref() {
            Some("server") if is_http => self
                .http_server
                .record(seconds, &timing.attributes(HTTP_SERVER_ATTRS)),
            Some("client") if is_http => self
                .http_client
                .record(seconds, &timing.attributes(HTTP_CLIENT_ATTRS)),
            _ if timing.fields.get("db.system.name").is_some() => {
                self.db_client.record(seconds, &timing.attributes(DB_ATTRS))
            }
            _ => {
                let mut attrs = timing.attributes(GENERIC_ATTRS);
                attrs.push(KeyValue::new("span.name", span.name()));
                self.generic.record(seconds, &attrs);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::metrics::{
        InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        data::{AggregatedMetrics, MetricData},
    };
    use tracing_subscriber::{filter::filter_fn, layer::SubscriberExt as _};

    /// Runs `f` under a subscriber with only the span-metrics layer and
    /// returns `(instrument, attributes)` for every histogram data point.
    fn collect(f: impl FnOnce()) -> Vec<(String, Vec<(String, String)>)> {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(SpanMetricsLayer::new(&provider).with_filter(filter_fn(interesting)));
        tracing::subscriber::with_default(subscriber, f);
        provider.force_flush().unwrap();

        let mut out = Vec::new();
        for rm in exporter.get_finished_metrics().unwrap() {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    let AggregatedMetrics::F64(MetricData::Histogram(h)) = m.data() else {
                        continue;
                    };
                    for dp in h.data_points() {
                        let mut attrs: Vec<(String, String)> = dp
                            .attributes()
                            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                            .collect();
                        attrs.sort();
                        out.push((m.name().to_owned(), attrs));
                    }
                }
            }
        }
        out
    }

    fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn server_span_becomes_http_server_duration() {
        let points = collect(|| {
            let span = tracing::info_span!(
                "http.server.request",
                otel.kind = "server",
                otel.status_code = tracing::field::Empty,
                http.request.method = "GET",
                http.route = "/items/{key}",
                url.path = "/items/abc",
                http.response.status_code = tracing::field::Empty,
                error.type = tracing::field::Empty,
            );
            span.record("http.response.status_code", 503_i64);
            span.record("otel.status_code", "ERROR");
            span.record("error.type", "503");
        });
        let (name, attrs) = &points[0];
        assert_eq!(name, "http.server.request.duration");
        assert_eq!(attr(attrs, "http.route"), Some("/items/{key}"));
        assert_eq!(attr(attrs, "http.response.status_code"), Some("503"));
        assert_eq!(attr(attrs, "error.type"), Some("503"));
        // High-cardinality fields must not leak into metric attributes.
        assert_eq!(attr(attrs, "url.path"), None);
    }

    #[test]
    fn db_span_and_error_event_give_other_error_type() {
        let points = collect(|| {
            let span = tracing::info_span!(
                "valkey.command",
                otel.kind = "client",
                db.system.name = "redis",
                db.operation.name = "GET",
            );
            span.in_scope(|| tracing::error!("boom"));
        });
        let (name, attrs) = &points[0];
        assert_eq!(name, "db.client.operation.duration");
        assert_eq!(attr(attrs, "db.operation.name"), Some("GET"));
        assert_eq!(attr(attrs, "error.type"), Some("_OTHER"));
    }

    #[test]
    fn other_spans_get_generic_duration_and_debug_spans_are_ignored() {
        let points = collect(|| {
            drop(tracing::info_span!("chaos.latency"));
            drop(tracing::debug_span!("validate"));
        });
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].0, "span.duration");
        assert_eq!(attr(&points[0].1, "span.name"), Some("chaos.latency"));
    }
}
