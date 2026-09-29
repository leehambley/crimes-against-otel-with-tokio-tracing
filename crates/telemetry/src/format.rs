//! Log formatters that stamp every line with the OpenTelemetry trace/span id
//! of the span the event happened in, so a log line can be pasted into Jaeger.
//!
//! The `pretty` format is tracing-subscriber's own; it prints the whole span
//! stack with fields, and the request span carries `trace_id` as a field.

use std::fmt;

use opentelemetry::trace::{SpanId, TraceContextExt, TraceFlags, TraceId};
use serde_json::{Map, Value};
use tracing::{Event, Level, Subscriber, field::Field};
use tracing_subscriber::{
    field::Visit,
    fmt::{
        FmtContext, FormatEvent, FormatFields,
        format::Writer,
        time::{FormatTime, SystemTime},
    },
    registry::LookupSpan,
};

/// `2026-09-29T10:00:00.123Z  INFO store::routes: stored item key="a" trace_id=… span_id=…`
pub(crate) struct Line;

/// `{"timestamp":…,"level":"INFO","target":…,"message":…,"trace_id":…,"span_id":…,…}`
pub(crate) struct Json;

impl<S, N> FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let ansi = writer.has_ansi_escapes();

        if ansi {
            write!(writer, "\x1b[2m")?;
        }
        SystemTime.format_time(&mut writer)?;
        if ansi {
            write!(
                writer,
                "\x1b[0m {}{:>5}\x1b[0m ",
                level_colour(meta.level()),
                meta.level()
            )?;
        } else {
            write!(writer, " {:>5} ", meta.level())?;
        }
        write!(writer, "{}: ", meta.target())?;
        ctx.format_fields(writer.by_ref(), event)?;

        if let Some((trace_id, span_id, _)) = otel_ids() {
            if ansi {
                write!(
                    writer,
                    " \x1b[2mtrace_id={trace_id} span_id={span_id}\x1b[0m"
                )?;
            } else {
                write!(writer, " trace_id={trace_id} span_id={span_id}")?;
            }
        }
        writeln!(writer)
    }
}

impl<S, N> FormatEvent<S, N> for Json
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let mut timestamp = String::new();
        SystemTime.format_time(&mut Writer::new(&mut timestamp))?;

        let mut obj = Map::new();
        obj.insert("timestamp".into(), timestamp.into());
        obj.insert("level".into(), meta.level().as_str().into());
        obj.insert("target".into(), meta.target().into());
        event.record(&mut JsonVisitor(&mut obj));

        if let Some(scope) = ctx.event_scope() {
            let spans: Vec<Value> = scope.from_root().map(|s| s.name().into()).collect();
            if let Some(Value::String(current)) = spans.last() {
                obj.insert("span".into(), current.clone().into());
            }
            obj.insert("spans".into(), spans.into());
        }
        // Field names per the OTel spec for trace context in non-OTLP logs.
        if let Some((trace_id, span_id, flags)) = otel_ids() {
            obj.insert("trace_id".into(), trace_id.to_string().into());
            obj.insert("span_id".into(), span_id.to_string().into());
            obj.insert(
                "trace_flags".into(),
                format!("{:02x}", flags.to_u8()).into(),
            );
        }

        let line = serde_json::to_string(&obj).map_err(|_| fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

/// The ids of the innermost entered span that the OpenTelemetry layer knows
/// about. Entering a span activates its OpenTelemetry context, so this is
/// right even when the span is hidden from the *log* layer (e.g. INFO spans
/// with `LOG_FILTER=warn`), and a DEBUG log inside a span that TRACE_FILTER
/// drops still correlates with the nearest exported parent.
fn otel_ids() -> Option<(TraceId, SpanId, TraceFlags)> {
    let cx = opentelemetry::Context::current();
    let sc = cx.span().span_context().clone();
    sc.is_valid()
        .then(|| (sc.trace_id(), sc.span_id(), sc.trace_flags()))
}

fn level_colour(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "\x1b[31m",
        Level::WARN => "\x1b[33m",
        Level::INFO => "\x1b[32m",
        Level::DEBUG => "\x1b[34m",
        Level::TRACE => "\x1b[35m",
    }
}

struct JsonVisitor<'a>(&'a mut Map<String, Value>);

impl Visit for JsonVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.0.insert(field.name().into(), value.to_string().into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().into(), format!("{value:?}").into());
    }
}
