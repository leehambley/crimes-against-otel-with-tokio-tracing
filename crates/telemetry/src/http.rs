//! Integer fields are recorded as `i64`: tracing-opentelemetry has no
//! `record_u64`, so `u16`/`u64` values would be exported as *strings*, which
//! breaks semconv (`http.response.status_code` is an int) and numeric
//! comparisons in the collector's sampling rules.
//!
//! W3C `traceparent` + `baggage` propagation for inbound (axum) and outbound (reqwest)
//! HTTP, and the server/client spans around them.

use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use opentelemetry::{Context, context::FutureExt as _, global, trace::TraceContextExt as _};
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use tracing::{Instrument as _, Span, field::Empty};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// The OpenTelemetry context carried by these headers (empty if none).
pub fn extract(headers: &http::HeaderMap) -> Context {
    global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(headers)))
}

/// Writes `traceparent`/`tracestate` for `cx` into `headers`.
pub fn inject(cx: &Context, headers: &mut http::HeaderMap) {
    global::get_text_map_propagator(|p| p.inject_context(cx, &mut HeaderInjector(headers)));
}

/// The OpenTelemetry context of `span`, falling back to the ambient context
/// when the span is filtered out of the trace layer. That keeps propagation
/// intact even when `TRACE_FILTER` is stricter than the span's level.
pub fn context_of(span: &Span) -> Context {
    let cx = span.context();
    if cx.span().span_context().is_valid() {
        cx
    } else {
        Context::current()
    }
}

/// Hex trace id of `span` (all zeros if there is no trace context).
pub fn trace_id(span: &Span) -> String {
    context_of(span)
        .span()
        .span_context()
        .trace_id()
        .to_string()
}

/// axum middleware (install with `Router::route_layer` so `MatchedPath` is
/// known): continues the caller's trace, opens the server span named
/// `{method} {http.route}`, and records the outcome with semconv attributes.
pub async fn server_span(request: Request, next: Next) -> Response {
    let parent = extract(request.headers());
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| request.uri().path().to_owned());
    let host = request
        .headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let (address, port) = match host.rsplit_once(':') {
        Some((a, p)) => (a, p.parse::<u16>().ok()),
        None => (host, None),
    };

    let span = tracing::info_span!(
        "http.server.request",
        otel.name = %format!("{method} {route}"),
        otel.kind = "server",
        otel.status_code = Empty,
        http.request.method = %method,
        http.route = %route,
        url.path = %request.uri().path(),
        url.scheme = "http",
        server.address = address,
        server.port = port.map(i64::from),
        network.protocol.version = protocol_version(request.version()),
        user_agent.original = request
            .headers()
            .get(http::header::USER_AGENT)
            .and_then(|h| h.to_str().ok()),
        http.response.status_code = Empty,
        error.type = Empty,
        trace_id = Empty,
    );
    // Must happen before anything starts the span (entering it, or context()).
    let _ = span.set_parent(parent.clone());
    span.record("trace_id", trace_id(&span));

    let started = Instant::now();
    let response = next
        .run(request)
        .instrument(span.clone())
        // Fallback for when the span itself is filtered out: outbound calls
        // still see (and propagate) the caller's context.
        .with_context(parent)
        .await;

    let status = response.status();
    span.record("http.response.status_code", i64::from(status.as_u16()));
    // Semconv: for server spans only 5xx is an error; 4xx is the caller's fault.
    if status.is_server_error() {
        span.record("otel.status_code", "ERROR");
        span.record("error.type", status.as_str());
    }
    let elapsed_ms = (started.elapsed().as_secs_f64() * 1e6).round() / 1e3;
    span.in_scope(|| {
        tracing::info!(
            status = i64::from(status.as_u16()),
            elapsed_ms,
            "request completed"
        )
    });
    response
}

/// Sends `request` inside a client span named `{method} {url_template}`,
/// propagating the span's context in `traceparent`/`baggage`. `url_template`
/// is the low-cardinality path, e.g. `/items/{key}`.
pub async fn send(
    client: &reqwest::Client,
    mut request: reqwest::Request,
    url_template: &'static str,
) -> reqwest::Result<reqwest::Response> {
    let method = request.method().clone();
    let url = request.url();
    let peer = url.host_str().unwrap_or_default().to_owned();
    let span = tracing::info_span!(
        "http.client.request",
        otel.name = %format!("{method} {url_template}"),
        otel.kind = "client",
        otel.status_code = Empty,
        http.request.method = %method,
        url.full = %url,
        url.scheme = url.scheme(),
        url.template = url_template,
        server.address = %peer,
        server.port = url.port_or_known_default().map(i64::from),
        network.protocol.version = Empty,
        http.response.status_code = Empty,
        error.type = Empty,
    );
    inject(&context_of(&span), request.headers_mut());

    async move {
        let result = client.execute(request).await;
        let span = Span::current();
        match &result {
            Ok(response) => {
                let status = response.status();
                span.record("http.response.status_code", i64::from(status.as_u16()));
                span.record(
                    "network.protocol.version",
                    protocol_version(response.version()),
                );
                // Semconv: for client spans both 4xx and 5xx are errors. The
                // collector's sampler ignores client-side 4xx (see
                // deploy/otel-collector.yaml) so 404s don't force-keep traces.
                if status.is_client_error() || status.is_server_error() {
                    span.record("otel.status_code", "ERROR");
                    span.record("error.type", status.as_str());
                }
                if status.is_server_error() {
                    tracing::warn!(
                        status = i64::from(status.as_u16()),
                        "{peer} returned a server error"
                    );
                } else {
                    tracing::debug!(status = i64::from(status.as_u16()), "{peer} responded");
                }
            }
            Err(err) => {
                span.record("error.type", reqwest_error_type(err));
                tracing::error!(error = %err, "request to {peer} failed");
            }
        }
        result
    }
    .instrument(span)
    .await
}

fn protocol_version(version: http::Version) -> &'static str {
    match version {
        http::Version::HTTP_09 => "0.9",
        http::Version::HTTP_10 => "1.0",
        http::Version::HTTP_2 => "2",
        http::Version::HTTP_3 => "3",
        _ => "1.1",
    }
}

/// Low-cardinality `error.type` for transport failures.
fn reqwest_error_type(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_body() || err.is_decode() {
        "body"
    } else if err.is_request() {
        "request"
    } else {
        "_OTHER"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};
    use opentelemetry_sdk::propagation::TraceContextPropagator;

    #[test]
    fn traceparent_round_trips() {
        global::set_text_map_propagator(TraceContextPropagator::new());
        let sc = SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
            SpanId::from_hex("00f067aa0ba902b7").unwrap(),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let mut headers = http::HeaderMap::new();
        inject(
            &Context::new().with_remote_span_context(sc.clone()),
            &mut headers,
        );
        assert_eq!(
            headers["traceparent"],
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );

        let back = extract(&headers);
        assert_eq!(back.span().span_context().trace_id(), sc.trace_id());
        assert_eq!(back.span().span_context().span_id(), sc.span_id());
    }
}
