# crimes-against-otel-with-tokio-tracing

Logs, traces and metrics from one source: `tracing` spans and events. The
application code only uses `tracing` macros; one subscriber, configured from
the environment, decides what becomes a log line, a span in Jaeger, a log
record in Loki, or a Prometheus series. The output follows OpenTelemetry
conventions (OTLP, semantic-convention names, `OTEL_*` variables), so
standard tooling understands it.

```
loadgen ──► gateway ──┬──► store ──► valkey
                      └──► stats ──► valkey
   │           │          │
   └───────────┴──── OTLP/HTTP ───► otel-collector ──┬─► Jaeger     (traces, tail-sampled)
                                    (tail sampling)  ├─► Loki       (logs, unsampled)
                                                     └─► Prometheus (metrics, unsampled)
                                                              ▲
                                               Grafana ───────┘ (Loki + Prometheus, links to Jaeger)
```

All hops propagate W3C `traceparent` and `baggage`, so a single request is one
trace across four services.

## Run it

```bash
scripts/stack.sh build            # one image with every binary
scripts/stack.sh up               # PROFILE=dev by default
scripts/stack.sh load -n 1000 -c 32
```

| UI | URL |
|---|---|
| Jaeger (traces) | http://localhost:16686 |
| Grafana (logs, metrics) | http://localhost:3000 |
| Prometheus | http://localhost:9090 |
| gateway | http://localhost:8080 |

`loadgen` ends by printing Jaeger links for failed and slow requests. In
Grafana → Explore → Loki, each log line with a `trace_id` has a "View trace in
Jaeger" link.

Production-like run: `PROFILE=prod scripts/stack.sh up` (and `PROFILE=prod` on
`load`). Tear down with `scripts/stack.sh down`.

For fast iteration, run `scripts/stack.sh infra` and `cargo run -p store` (etc.)
on the host with `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318`.

## One subscriber, five layers

`crates/telemetry` builds a `Registry` with:

| layer | output | filter |
|---|---|---|
| fmt (`line`/`pretty`/`json`) | stdout | `LOG_FILTER`, reloadable |
| `opentelemetry-appender-tracing` | OTLP log records → Loki | `LOG_FILTER`, reloadable |
| `tracing-opentelemetry` | spans → OTLP; events → span events | `TRACE_FILTER` + `SPAN_EVENT_LEVEL`, reloadable |
| span metrics (ours) | semconv duration histograms | INFO+ spans, always on |
| `MetricsLayer` | `monotonic_counter.*` / `histogram.*` events | metric callsites only |

What this means:

- **Logs are OTLP log records** carrying the trace context, so they're
  searchable in Loki whether or not their trace was sampled. They're also
  printed to stdout.
- **Logs are span events too,** down to `SPAN_EVENT_LEVEL`. In dev every event
  rides on its span in Jaeger. In prod only WARN and ERROR do, which keeps
  spans small.
- **stdout lines carry `trace_id`/`span_id`/`trace_flags`** (`line` and
  `json`; `pretty` shows the trace id on the request span), taken from the
  active OpenTelemetry context. It works even when the log filter hides the
  spans.
- **Metrics come from spans,** under semantic-convention names:

  | span | instrument |
  |---|---|
  | HTTP server | `http.server.request.duration` |
  | HTTP client | `http.client.request.duration` |
  | Valkey command | `db.client.operation.duration` |
  | any other INFO+ span | `span.duration` |

  Each carries the semconv attributes, including `error.type` on failure.
  This layer ignores the log/trace filters and runs before sampling, so the
  numbers are complete. Explicit counters are plain events
  (`tracing::event!(target: "metrics", Level::TRACE, monotonic_counter.foo = 1u64)`).
- **Errors mark spans.** An `ERROR` event, or `otel.status_code = "ERROR"`,
  sets the span status, and the collector's tail sampler looks for it. The
  convention in this repo is that `error!` means "this operation failed";
  anything handled or recoverable is `warn!`.
- **Span status follows semconv.** Server spans are errors on 5xx. Client
  spans are errors on 4xx and 5xx, but the sampler ignores client-side 4xx,
  so 404s don't force-keep traces.

### Span levels = trace verbosity

| level | spans | seen in |
|---|---|---|
| INFO | HTTP server/client, Valkey commands, chaos latency | prod and dev |
| DEBUG | handlers, `authorize`, `validate` | dev |
| TRACE | `encode`, `decode` | dev |

Same binary, different `TRACE_FILTER`. Put anything you'd build a dashboard
or SLO on at INFO.

## Configuration (environment)

| variable | default | notes |
|---|---|---|
| `OTEL_SERVICE_NAME` | `SERVICE_NAME`, then per binary | standard; `SERVICE_NAME` is an alias |
| `OTEL_RESOURCE_ATTRIBUTES` | | e.g. `deployment.environment.name=prod`; `service.version` and `service.instance.id` are filled in |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset = no export | deliberate deviation from the spec default of `localhost:4318`; other `OTEL_EXPORTER_OTLP_*`, `OTEL_BSP_*`, `OTEL_METRIC_EXPORT_INTERVAL` apply as usual |
| `OTEL_SDK_DISABLED` | `false` | `true` turns off all export |
| `LOG_FORMAT` | `line` | `line`, `pretty` (multi-line with span stack), `json` |
| `LOG_COLOR` | `auto` | `auto` = colour on a TTY (respects `NO_COLOR`), `always`, `never`; dev forces it on for `podman logs` |
| `LOG_FILTER` / `RUST_LOG` | `info` | `EnvFilter` syntax; stdout and OTLP logs |
| `TRACE_FILTER` | = `LOG_FILTER` | which spans (and events) are exported |
| `SPAN_EVENT_LEVEL` | `trace` | events below this aren't attached to spans |
| `CONTROL_SOCKET` | `$TMPDIR/<service>.ctl` | `off` to disable |
| `CHAOS_FAILURE_PCT` | `0` | % of Valkey commands / auth checks that fail |
| `CHAOS_SLOW_PCT`, `CHAOS_SLOW_MS` | `0`, `250` | injected latency |
| `SAMPLING_PERCENT` | `10` | collector: baseline share of healthy traces kept |
| `SAMPLING_SLOW_MS` | `1000` | collector: traces slower than this are kept |
| `SAMPLING_DECISION_WAIT` | `10s` (dev `5s`) | collector: how long to buffer a trace before deciding; must exceed the longest trace plus `OTEL_BSP_SCHEDULE_DELAY` |

Exporter internals (`hyper`, `h2`, `opentelemetry*`, …) are always excluded
from the OTLP layers to avoid feedback loops. Profiles are in `env/dev.env`
and `env/prod.env`.

## Changing levels at runtime

`tracing-subscriber` provides `reload` handles but no way to reach them from
outside the process, so each service listens on a Unix control socket:

```bash
scripts/stack.sh ctl store show
scripts/stack.sh ctl store log warn,store=debug       # LOG_FILTER (stdout + OTLP logs)
scripts/stack.sh ctl store trace info,store=trace     # TRACE_FILTER
scripts/stack.sh ctl store level debug                # both
scripts/stack.sh ctl store set span_event_level debug
scripts/stack.sh ctl gateway set chaos.failure_pct 25
```

(`ctl` is a small binary in the image; `nc -U` works too.)

## Sampling

The SDK records every span that passes `TRACE_FILTER` (`AlwaysOn`, whatever
the caller's `sampled` flag says). The OpenTelemetry Collector buffers each
trace and then decides whether to keep it (see `deploy/otel-collector.yaml`).
A trace is kept if any of these match:

1. any span has status ERROR, in any service, other than a client-side 4xx →
   **always kept**
2. the trace is slower than `SAMPLING_SLOW_MS`
3. its trace id falls in the `SAMPLING_PERCENT` bucket

Head sampling in the SDK can't do this. When the root span starts, nobody
knows yet whether `stats` will fail 40ms later.

**Verified.** Loki receives every log unsampled, so its ERROR records give an
independent list of which traces failed. Across 2000 prod requests with 5%
chaos per service, all 273 traces with an ERROR log were in Jaeger, each
complete. The collector's `keep-all-errors` count was also 273.

What can still escape: spans the services never send (below `TRACE_FILTER`,
or dropped when the SDK export queue overflows), and traces lost if the
collector restarts or runs out of memory during `decision_wait`. The
collector remembers its decisions, so a span that arrives late follows its
trace's verdict instead of being judged on its own.

Example `prod` run with 1000 requests: 101 traces kept. That's every failed
request and every request with a failed downstream call (including ones that
returned 2xx because stats was optional), the slow ones, and about 5% of the
rest. 2 of the 33 404s were kept, which is the baseline, as intended. The
collector's per-policy counters (`otelcol_processor_tail_sampling_*`) are in
Prometheus.

## Notes / deviations

See [CRITIQUE.md](CRITIQUE.md) for a full comparison with OpenTelemetry
practice. In brief:

- **"Jaeger format":** the OpenTelemetry Rust Jaeger exporter was removed
  upstream, and Jaeger v2 takes OTLP natively. The Jaeger `uber-trace-id`
  propagator is one release behind `opentelemetry` 0.33, so propagation is W3C
  (`traceparent` + `baggage`).
- **No Jaeger datasource in Grafana:** Jaeger 2.21 serves only its v3 API,
  which Grafana 13's Jaeger datasource doesn't use. Logs link out to the
  Jaeger UI instead.
- **Integer span fields are recorded as `i64`:** `tracing-opentelemetry` exports
  `u16`/`u64` fields as strings.
- **Pinned versions:** Collector 0.161, Jaeger 2.21, Loki 3.7.8, Grafana
  13.2.3 and Prometheus 3.15, all in `scripts/stack.sh`.
- **No `podman-compose` needed:** `stack.sh` uses plain `podman run` on a
  podman network.
