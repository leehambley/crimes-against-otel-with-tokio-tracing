//! One `tracing` subscriber that fans out to logs, traces and metrics.
//!
//! ```text
//!                       ┌─ fmt layer ─────────── stdout (line | pretty | json)       [LOG_FILTER, reloadable]
//!                       ├─ OTel log bridge ───── OTLP log records → collector → Loki [LOG_FILTER, reloadable]
//! tracing spans/events ─┼─ OpenTelemetry layer ─ OTLP spans → collector → Jaeger    [TRACE_FILTER + SPAN_EVENT_LEVEL]
//!                       ├─ span-metrics layer ── semconv duration histograms         [INFO+ spans, always on]
//!                       └─ MetricsLayer ──────── `monotonic_counter.*` events        [metric callsites only]
//! ```
//!
//! Explicit metric events use `target: "metrics"` so they stay out of logs
//! and span events unless someone asks for `metrics=trace`.
//!
//! Application code only ever uses `tracing` macros. Where things end up is
//! decided here, from the environment, and can be changed at runtime through
//! the control socket.

mod config;
pub mod control;
mod filter;
mod format;
pub mod http;
mod span_metrics;

use std::sync::Arc;

use anyhow::Context as _;
use opentelemetry::{
    InstrumentationScope, Key, KeyValue, global, propagation::TextMapCompositePropagator,
    trace::TracerProvider as _,
};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::SdkMeterProvider,
    propagation::{BaggagePropagator, TraceContextPropagator},
    trace::{Sampler, SdkTracerProvider},
};
use tracing_subscriber::{
    Layer, Registry,
    filter::{FilterExt as _, filter_fn},
    layer::SubscriberExt,
    reload,
};

pub use config::{Config, LogFormat};
pub use control::Control;

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Keeps the exporters alive. Call [`Telemetry::shutdown`] before exiting so
/// buffered spans and metrics are flushed.
pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    logger_provider: SdkLoggerProvider,
    control: Arc<Control>,
    config: Config,
}

impl Telemetry {
    /// Builds the subscriber from environment variables (see [`Config`]).
    pub fn init_from_env(default_service_name: &str) -> anyhow::Result<Self> {
        Self::init(Config::from_env(default_service_name))
    }

    pub fn init(config: Config) -> anyhow::Result<Self> {
        // The spec's default propagator set: W3C trace context + baggage.
        global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
            Box::new(TraceContextPropagator::new()),
            Box::new(BaggagePropagator::new()),
        ]));

        let resource = resource(&config);
        let mut tracer_provider = SdkTracerProvider::builder()
            // Sampling is decided by the collector once it has seen the whole
            // trace (tail sampling, see deploy/otel-collector.yaml). The SDK
            // default, ParentBased(AlwaysOn), would drop our spans whenever a
            // caller sends `sampled=0`, and the tail sampler could then never
            // keep that trace, even if it failed here. So: always record.
            .with_sampler(Sampler::AlwaysOn)
            .with_resource(resource.clone());
        let mut meter_provider = SdkMeterProvider::builder().with_resource(resource.clone());
        let mut logger_provider = SdkLoggerProvider::builder().with_resource(resource);
        if config.otlp_enabled {
            // Endpoint, headers, timeouts come from the standard OTEL_EXPORTER_OTLP_* variables.
            tracer_provider = tracer_provider.with_batch_exporter(
                opentelemetry_otlp::SpanExporter::builder()
                    .with_http()
                    .build()
                    .context("building OTLP span exporter")?,
            );
            meter_provider = meter_provider.with_periodic_exporter(
                opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .build()
                    .context("building OTLP metric exporter")?,
            );
            logger_provider = logger_provider.with_batch_exporter(
                opentelemetry_otlp::LogExporter::builder()
                    .with_http()
                    .build()
                    .context("building OTLP log exporter")?,
            );
        }
        let tracer_provider = tracer_provider.build();
        let meter_provider = meter_provider.build();
        let logger_provider = logger_provider.build();
        global::set_meter_provider(meter_provider.clone());

        let (log_filter, log_handle) = reload::Layer::new(filter::log_filter(&config.log_filter)?);
        let (log_export_filter, log_export_handle) =
            reload::Layer::new(filter::export_filter(&config.log_filter)?);
        let (trace_filter, trace_handle) =
            reload::Layer::new(filter::export_filter(&config.trace_filter)?);
        let span_event_level = filter::SpanEventLevel::new(config.span_event_level);

        // LOG_COLOR: auto = colour only on a TTY.
        let fmt = tracing_subscriber::fmt::layer().with_ansi(config.log_color);
        let fmt_layer: BoxedLayer = match config.log_format {
            LogFormat::Line => fmt.event_format(format::Line).boxed(),
            LogFormat::Pretty => fmt.pretty().boxed(),
            LogFormat::Json => fmt.with_ansi(false).event_format(format::Json).boxed(),
        };

        let scope = InstrumentationScope::builder(env!("CARGO_PKG_NAME"))
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        let otel_layer = tracing_opentelemetry::layer()
            .with_tracer(tracer_provider.tracer_with_scope(scope))
            // Error-level events mark the span as failed; that is what the
            // collector's tail sampler keys on to keep 100% of failing traces.
            // Convention in this repo: `error!` means "this operation failed";
            // handled/recoverable problems are `warn!`.
            .with_error_events_to_status(true)
            .with_error_events_to_exceptions(true)
            .with_error_records_to_exceptions(true)
            .with_location(true)
            .with_threads(false);
        let events_gate = span_event_level.clone();

        let layers: Vec<BoxedLayer> = vec![
            fmt_layer.with_filter(log_filter).boxed(),
            OpenTelemetryTracingBridge::new(&logger_provider)
                .with_filter(log_export_filter)
                .boxed(),
            otel_layer
                .with_filter(trace_filter.and(filter_fn(move |meta| events_gate.allows(meta))))
                .boxed(),
            span_metrics::SpanMetricsLayer::new(&meter_provider)
                .with_filter(filter_fn(span_metrics::interesting))
                .boxed(),
            tracing_opentelemetry::MetricsLayer::new(meter_provider.clone()).boxed(),
        ];
        tracing::subscriber::set_global_default(Registry::default().with(layers))
            .context("a global tracing subscriber was already installed")?;

        let control = Arc::new(Control::new(
            log_handle,
            log_export_handle,
            trace_handle,
            config.log_filter.clone(),
            config.trace_filter.clone(),
        ));
        let (get, set) = (span_event_level.clone(), span_event_level);
        control.register(
            "span_event_level",
            control::Knob {
                get: Box::new(move || get.get().to_string()),
                set: Box::new(move |v| {
                    set.set(v.parse().map_err(|e| format!("{e}"))?);
                    Ok(())
                }),
            },
        );

        tracing::info!(
            service = %config.service_name,
            log_format = ?config.log_format,
            log_filter = %config.log_filter,
            trace_filter = %config.trace_filter,
            span_event_level = %config.span_event_level,
            otlp = config.otlp_enabled,
            "telemetry initialised"
        );

        Ok(Self {
            tracer_provider,
            meter_provider,
            logger_provider,
            control,
            config,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Runtime knobs (log/trace filters and anything registered by the app).
    pub fn control(&self) -> Arc<Control> {
        self.control.clone()
    }

    /// Serves the control socket at `CONTROL_SOCKET`, if configured. Must be
    /// called from inside a tokio runtime.
    pub fn spawn_control_socket(&self) {
        if let Some(path) = self.config.control_socket.clone() {
            let control = self.control.clone();
            tokio::spawn(async move {
                if let Err(err) = control.serve(&path).await {
                    tracing::error!(error = %err, path = %path.display(), "control socket failed");
                }
            });
        }
    }

    /// Flushes and stops the exporters.
    pub fn shutdown(self) {
        if let Err(err) = self.tracer_provider.shutdown() {
            eprintln!("tracer provider shutdown: {err}");
        }
        if let Err(err) = self.meter_provider.shutdown() {
            eprintln!("meter provider shutdown: {err}");
        }
        if let Err(err) = self.logger_provider.shutdown() {
            eprintln!("logger provider shutdown: {err}");
        }
    }
}

/// `service.name` from config; everything in `OTEL_RESOURCE_ATTRIBUTES`
/// (e.g. `deployment.environment.name`) via the SDK's env detector; plus
/// `service.version` (unless given) and a per-process `service.instance.id`.
fn resource(config: &Config) -> Resource {
    let detected = Resource::builder().build();
    let mut builder = Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attribute(KeyValue::new(
            "service.instance.id",
            uuid::Uuid::new_v4().to_string(),
        ));
    if detected
        .get(&Key::from_static_str("service.version"))
        .is_none()
    {
        builder =
            builder.with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")));
    }
    builder.build()
}
