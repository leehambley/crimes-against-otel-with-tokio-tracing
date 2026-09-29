//! Metrics derived from spans: every INFO-or-above span that closes records
//! its duration in a `span.duration` histogram, labelled with the span name,
//! kind, a handful of low-cardinality attributes, and `status` (`ok`/`error`).
//!
//! That gives RED metrics (rate = histogram count, errors = `status="error"`,
//! duration = the histogram) for every HTTP handler, outbound call and
//! Valkey command without a single hand-written counter. The layer has its
//! own filter, so metrics stay complete regardless of LOG_FILTER,
//! TRACE_FILTER or trace sampling.

use std::time::Instant;

use opentelemetry::{
    KeyValue,
    metrics::{Histogram, MeterProvider},
};
use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span,
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// Span fields copied onto the metric. Keep this list low-cardinality.
const LABEL_FIELDS: &[&str] = &[
    "otel.kind",
    "http.request.method",
    "http.route",
    "http.response.status_code",
    "db.operation.name",
    "peer.service",
];

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
    duration: Histogram<f64>,
}

impl SpanMetricsLayer {
    pub(crate) fn new(provider: &impl MeterProvider) -> Self {
        let meter = provider.meter("span-metrics");
        Self {
            duration: meter
                .f64_histogram("span.duration")
                .with_unit("s")
                .with_description("Wall-clock duration of INFO+ spans, derived from tracing spans")
                .with_boundaries(vec![
                    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
                    10.0,
                ])
                .build(),
        }
    }
}

struct Timing {
    start: Instant,
    error: bool,
    labels: Vec<KeyValue>,
}

impl Timing {
    fn visitor(&mut self) -> LabelVisitor<'_> {
        LabelVisitor(self)
    }
}

struct LabelVisitor<'a>(&'a mut Timing);

impl LabelVisitor<'_> {
    fn put(&mut self, field: &Field, value: String) {
        let name = field.name();
        if name == "otel.status_code" {
            self.0.error |= value.eq_ignore_ascii_case("error");
        } else if LABEL_FIELDS.contains(&name) {
            self.0.labels.retain(|kv| kv.key.as_str() != name);
            self.0.labels.push(KeyValue::new(name, value));
        }
    }
}

impl Visit for LabelVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.to_owned());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `%value` fields arrive here; Debug of a Display wrapper is the Display output.
        self.put(field, format!("{value:?}"));
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
            labels: Vec::with_capacity(4),
        };
        attrs.record(&mut timing.visitor());
        span.extensions_mut().insert(timing);
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        if let Some(timing) = span.extensions_mut().get_mut::<Timing>() {
            values.record(&mut timing.visitor());
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
        let Some(mut timing) = span.extensions_mut().remove::<Timing>() else {
            return;
        };
        timing.labels.push(KeyValue::new("span.name", span.name()));
        timing.labels.push(KeyValue::new(
            "status",
            if timing.error { "error" } else { "ok" },
        ));
        self.duration
            .record(timing.start.elapsed().as_secs_f64(), &timing.labels);
    }
}
