# crimes-against-otel-with-tokio-tracing

Logs, traces and metrics from one source: `tracing` spans and events. The
application code only uses `tracing` macros; one subscriber, configured from
the environment, decides what becomes a log line, a span in Jaeger, or a
Prometheus series.

```
loadgen ──► gateway ──┬──► store ──► valkey
                      └──► stats ──► valkey
   │           │          │
   └───────────┴──── OTLP/HTTP ───► otel-collector ──┬─► Jaeger     (tail-sampled traces)
                                    (tail sampling)  └─► Prometheus (metrics, unsampled)
```

All hops propagate W3C `traceparent`, so a single request is one trace across
four services.

## Run it

```bash
scripts/stack.sh build            # one image with every binary
scripts/stack.sh up               # PROFILE=dev by default
scripts/stack.sh load -n 1000 -c 32
```

Jaeger at http://localhost:16686, Prometheus at http://localhost:9090, gateway at
http://localhost:8080. `loadgen` ends by printing Jaeger links for failed and
slow requests.

Production-like run: `PROFILE=prod scripts/stack.sh up` (and `PROFILE=prod` on
`load`). Tear down with `scripts/stack.sh down`.

For fast iteration, run `scripts/stack.sh infra` and `cargo run -p store` (etc.)
on the host with `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318`.

## One subscriber, four layers

`crates/telemetry` builds a `Registry` with:

| layer | output | filter |
|---|---|---|
| fmt (`line`/`pretty`/`json`) | stdout | `LOG_FILTER`, reloadable |
| `tracing-opentelemetry` | spans → OTLP; events → span events | `TRACE_FILTER`, reloadable |
| span metrics (ours) | `span.duration` histogram, by span name, kind, route, status | INFO+ spans, always on |
| `MetricsLayer` | `monotonic_counter.*` / `histogram.*` events | metric callsites only |

What this means:

- **Logs are span events.** Every event inside an exported span shows up on
  that span in Jaeger, timestamped, and is also printed as a log line if it
  passes `LOG_FILTER`. The two filters are separate, so prod can log at WARN
  but still trace at INFO.
- **Log lines carry `trace_id`/`span_id`** (`line` and `json`; `pretty` shows
  it on the request span), taken from the active OpenTelemetry context. It
  works even when the log filter hides the spans.
- **Metrics come from spans.** Each INFO+ span that closes records its duration
  with `status=ok|error`, so every handler, outbound call and Valkey command
  gets rate, errors and latency without writing a counter. This layer
  ignores the log/trace filters and runs before sampling, so the numbers are
  complete. Explicit counters are plain events
  (`tracing::event!(target: "metrics", Level::TRACE, monotonic_counter.foo = 1u64)`).
- **Errors mark spans.** An `ERROR` event, or `otel.status_code = "ERROR"`,
  sets the span status. The collector's tail sampler looks for this status.

### Span levels = trace verbosity

| level | spans | seen in |
|---|---|---|
| INFO | HTTP server/client, Valkey commands, chaos latency | prod and dev |
| DEBUG | handlers, `authorize`, `validate` | dev |
| TRACE | `encode`, `decode` | dev |

Same binary, different `TRACE_FILTER`.

## Configuration (environment)

| variable | default | notes |
|---|---|---|
| `LOG_FORMAT` | `line` | `line`, `pretty` (multi-line with span stack), `json` |
| `LOG_COLOR` | `auto` | `auto` = colour on a TTY (respects `NO_COLOR`), `always`, `never`; dev forces it on for `podman logs` |
| `LOG_FILTER` / `RUST_LOG` | `info` | `EnvFilter` syntax |
| `TRACE_FILTER` | = `LOG_FILTER` | exporter internals (`hyper`, `h2`, `opentelemetry*`, …) are always excluded to avoid feedback loops |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset = no export | standard OTel SDK vars apply (`OTEL_BSP_*`, `OTEL_METRIC_EXPORT_INTERVAL`, …) |
| `SERVICE_NAME`, `DEPLOY_ENV` | per binary, `dev` | resource attributes |
| `CONTROL_SOCKET` | `$TMPDIR/<service>.ctl` | `off` to disable |
| `CHAOS_FAILURE_PCT` | `0` | % of Valkey commands / auth checks that fail |
| `CHAOS_SLOW_PCT`, `CHAOS_SLOW_MS` | `0`, `250` | injected latency |
| `SAMPLING_PERCENT` | `10` | collector: baseline share of healthy traces kept |
| `SAMPLING_SLOW_MS` | `1000` | collector: traces slower than this are kept |
| `SAMPLING_DECISION_WAIT` | `5s` | collector: how long to buffer a trace before deciding |

Profiles are in `env/dev.env` and `env/prod.env`.

## Changing levels at runtime

`tracing-subscriber` provides `reload` handles but no way to reach them from
outside the process, so each service listens on a Unix control socket:

```bash
scripts/stack.sh ctl store show
scripts/stack.sh ctl store log warn,store=debug       # LOG_FILTER
scripts/stack.sh ctl store trace info,store=trace     # TRACE_FILTER
scripts/stack.sh ctl store level debug                # both
scripts/stack.sh ctl gateway set chaos.failure_pct 25
```

(`ctl` is a small binary in the image; `nc -U` works too.)

## Sampling

Services export every span that passes `TRACE_FILTER`. The OpenTelemetry
Collector buffers each trace and then decides whether to keep it (see
`deploy/otel-collector.yaml`). A trace is kept if any of these match:

1. any span has status ERROR, in any service → **always kept**
2. the trace is slower than `SAMPLING_SLOW_MS`
3. its trace id falls in the `SAMPLING_PERCENT` bucket

Head sampling in the SDK can't do this. When the root span starts, nobody
knows yet whether `stats` will fail 40ms later. So sampling happens in the
collector, and the SDK has no sampler.

Example `prod` run with 1000 requests: 113 traces kept, made up of all 40 that
contained an error span, 20 slow ones and 53 baseline (about 5%). The 40
include 14 requests that returned 2xx but where the stats call failed.

## Notes / deviations

- **"Jaeger format":** the OpenTelemetry Rust Jaeger exporter was removed
  upstream, and Jaeger v2 takes OTLP natively. Everything uses OTLP, and Jaeger
  stores and shows it as usual. The Jaeger `uber-trace-id` propagator
  (`opentelemetry-jaeger-propagator`) is one release behind `opentelemetry`
  0.33, so propagation is W3C only for now.
- Collector 0.161 and Jaeger 2.21 are pinned in `scripts/stack.sh`.
- `stack.sh` uses plain `podman run` on a podman network, so you don't need
  `podman-compose`.
