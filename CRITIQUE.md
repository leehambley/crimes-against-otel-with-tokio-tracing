# Critique: this project vs. OpenTelemetry practice

This project treats `tracing` as the single source of logs, traces and
metrics. OpenTelemetry treats them as three signals with three APIs, joined
by shared context and resource attributes, and sends everything to a
Collector over OTLP. This document lists where the two disagree, and whether
the disagreement is worth keeping.

Verdicts:

- **Keep:** the tradeoff is worth it.
- **Fix:** we deviated for no good reason.
- **Depends:** it depends on the deployment.

## Summary

The **Status** column shows where things stand now. Items 1–15 are the
original critique, with notes where later work changed the picture.
Item 16 was found while fixing them (see
[What changed in the second pass](#what-changed-in-the-second-pass)).
Items 17–23 are tradeoffs that the fixes themselves introduced (see
[Tradeoffs introduced by the fixes](#tradeoffs-introduced-by-the-fixes)).

| # | Area | Deviation | Verdict | Status |
|---|------|-----------|---------|--------|
| 1 | Logs | Logs are span events + stdout, not OTLP log records | Fix (add OTLP logs, keep stdout) | ✅ Done: appender → collector → Loki |
| 2 | Logs | Span events are the only place logs live in the backend | Fix | ✅ Done: `SPAN_EVENT_LEVEL` (prod: WARN+) |
| 3 | Metrics | One generic `span.duration` instead of semconv instruments | Fix (rename) or move to collector | ✅ Done: semconv instruments, attributes and buckets |
| 4 | Metrics | Metrics from events with magic field prefixes | Keep, with care | Kept |
| 5 | Metrics | No exemplars linking metrics to traces | Fix eventually | ⏳ Open |
| 6 | Traces | Span *verbosity levels* change the shape of the trace | Keep. This is the main idea | Kept, documented |
| 7 | Traces | `ERROR` log ⇒ span status Error ⇒ trace always kept | Keep, but make it explicit | ✅ Convention documented |
| 8 | Traces | Semantic-convention gaps (error.type, client 4xx, ports, …) | Fix | ✅ Done |
| 9 | Sampling | No SDK sampler; all sampling is tail sampling in the collector | Keep, but know what it costs | Kept; late spans fixed, verified 273/273; single collector tier still (⏳) |
| 10 | Sampling | `ParentBased` default can drop spans the tail sampler never sees | Fix | ✅ Done: explicit `AlwaysOn` |
| 11 | Config | Home-grown env vars instead of `OTEL_*` | Fix (support both) | ✅ Done |
| 12 | Config | Control socket instead of OpAMP / declarative config | Keep for now | Kept |
| 13 | Propagation | W3C `traceparent` only; no baggage, no Jaeger header | Fix (baggage), Depends (Jaeger) | ✅ Baggage done; Jaeger still blocked by crate versions |
| 14 | Export | OTLP instead of "Jaeger format" | Keep. OTLP is the standard now | Kept |
| 15 | Bridge | `tracing` → OTel via a community bridge, two context systems | Keep. It's the Rust norm | Kept; see item 16 |
| 16 | Bridge | Unsigned integer fields exported as *strings* | (found in pass two) | ✅ Worked around |
| 17 | Sampling | Client-side 4xx error spans don't force-keep a trace | Keep, but it changes the requirement | ⚠️ Deliberate; documented |
| 18 | Metrics | Demoting a span below INFO silently deletes its metric | Fix | ⏳ Open |
| 19 | Metrics | Two layers each decide "was this an error?" | Fix (consistency test) | ⏳ Open |
| 20 | Sampling | Sampling can't be influenced from `tracing` or at runtime | Depends | ⏳ Open |
| 21 | Logs | Prod has no INFO logs anywhere | Keep (it's a choice) | Documented |
| 22 | Logs | WARN/ERROR stored twice in prod (Loki and span events) | Keep | Documented |
| 23 | Misc | Smaller gaps: OTTL error mode, no Grafana→Jaeger datasource, `service.version`, `span.duration` naming | Mixed | Mixed |

---

## Logs

### 1. Logs are not an OTLP signal here

OpenTelemetry has a stable Logs data model and a Logs Bridge API. In Rust, the
supported path is `opentelemetry-appender-tracing`, which turns `tracing`
events into OTLP `LogRecord`s. The OTel Rust project itself recommends
`tracing` as the logging API. We don't export logs over OTLP at all. They go
to stdout (for whatever log shipper runs next to the container) and onto the
span as span events.

Our JSON lines do follow the OTel guidance for trace context in non-OTLP log
formats. They carry `trace_id` and `span_id` as hex strings, so correlation
works. They lack `trace_flags`, and the other field names (`level`, `message`)
are ours, not the log data model's `severity_text` or `body`.

**Verdict: Fix.** Add the appender as a fifth layer with its own filter, and
keep stdout. It's cheap, and it gives log queries in the backend that don't
depend on a trace being sampled.

### 2. Span events as the log store

The idea behind this project is that a log line is a timestamped event on its
span. That is nice to read in Jaeger, but OpenTelemetry is heading the other
way. Its current direction is to model events as log records that carry trace
context, rather than to grow the span-event API. Problems we inherit today:

- **Logs are sampled with traces.** In `prod`, about 89% of requests' span
  events are dropped by the tail sampler. Stdout keeps them, but the backend
  only sees logs for kept traces.
- **The SDK caps events per span at 128 by default.** In dev at TRACE level, a
  chatty handler can silently lose events past that.
- **Span size grows with log volume.** Every event is buffered in memory until
  the span ends and is exported in one piece.

**Verdict: Fix, partly.** Span events are great in dev. For prod, logs should
be their own signal (item 1). Then span events can be limited, for example to
WARN and above, through `TRACE_FILTER`, which we already have.

> **Update:** `TRACE_FILTER` couldn't do this, because it filters spans and
> events together, and INFO events would have taken INFO spans with them. We
> added `SPAN_EVENT_LEVEL`, a separate runtime setting that only gates
> events. Prod uses `warn`.

---

## Metrics

### 3. `span.duration` vs. semantic-convention instruments

Semconv defines specific instruments:

- `http.server.request.duration` and `http.client.request.duration`, in
  seconds, with recommended bucket boundaries and a required attribute set
  (`http.request.method`, `http.route`, `http.response.status_code`,
  `error.type`, `url.scheme`, `server.address`/`server.port`)
- `db.client.operation.duration`

Dashboards, alerts and vendor UIs expect those names. We emit a single
`span.duration` histogram with `span.name` as a label, our own `status`
label, and our own buckets. It's handy that one generic mechanism covers
every span, including internal ones like `chaos.latency`. But nothing
off-the-shelf will recognise it.

The common alternative is the Collector's `spanmetrics` connector. It computes
the same RED metrics from spans before tail sampling, with names that Jaeger's
Monitor tab and Grafana know about. It moves the work out of process, and it
only sees spans that were exported, so it follows `TRACE_FILTER` rather than
being independent of it.

**Verdict: Fix the naming, keep the in-process approach.** Map
`http.server.request` spans to `http.server.request.duration`, HTTP client
spans to `http.client.request.duration`, and `valkey.command` to
`db.client.operation.duration`, using semconv attributes and buckets. Keep a
generic histogram only for spans with no convention. Doing it in process keeps
the property I like most in this design: metrics are complete no matter how
logs and traces are filtered or sampled.

### 4. Event-based metrics (`monotonic_counter.*`)

`tracing-opentelemetry`'s `MetricsLayer` turns specially named event fields
into instruments. It isn't the OTel Metrics API. It's untyped, the instrument
kind is in a string prefix, it has no units or descriptions, and it needed a
workaround: `target: "metrics"`, and `event!` instead of `trace!` to dodge a
macro-parsing ambiguity. Using the OTel `Meter` directly is what the spec
expects.

**Verdict: Keep, with care.** It fits the idea that everything is a `tracing`
call, and the layer only takes interest in callsites that have metric fields.
For more than a couple of counters, use a `Meter` directly.

### 5. No exemplars

OTel histograms can carry exemplars: a trace id attached to a sample, so you
can jump from a p99 spike to a trace that caused it. Our span-metrics layer
records in `on_close`, when the span's context is no longer active, so no
exemplar is attached.

**Verdict: Fix eventually.** Record inside the span's context. And because of
tail sampling, only keep exemplars from traces that will survive, such as
error traces, or the link will point at a trace that was dropped.

---

## Traces

### 6. Verbosity levels on spans

OpenTelemetry has no concept of span verbosity. You get fewer spans in prod
by sampling whole traces or by turning instrumentations off. In this project,
DEBUG and TRACE spans simply don't exist in prod. Children re-attach to the
nearest enabled parent, because the OTel context is activated on span enter.
So the *same request* produces a 14-span trace in dev and a 7-span trace in
prod.

Costs:

- Dashboards or queries built on span names in dev may find nothing in prod.
- A "gap" can hide where time was spent. Prod shows `get_item` time as part
  of the parent server span.
- Tools that compare traces across environments, or that derive a service map
  from internal spans, see different graphs.

Benefits:

- The volume is controlled where it's produced, with no network, CPU or
  collector-memory cost for spans nobody will read.
- It can be changed per module, at runtime, without a redeploy (item 12).
- Developers don't have to choose between detail and cost when writing code.
  They choose a level, the way they already do for logs.

**Verdict: Keep.** This is what `tracing` gives you that the rest of the
industry mostly doesn't, and I think it's worth the shape changes. Put
anything you'd build a dashboard or SLO on at INFO, and treat DEBUG/TRACE
spans as disposable detail.

### 7. `ERROR` event ⇒ span Error ⇒ trace kept

`tracing-opentelemetry` sets the span status to Error when an ERROR-level
event is logged in it. Semconv sets status from protocol outcome instead: 5xx
for server spans, and 4xx or 5xx for client spans. It warns against marking
spans failed for handled or incidental errors. Here, one developer writing
`error!` for something recoverable changes the span's status, the RED error
rate, and whether the trace is kept.

We mostly avoided this by convention: failures are logged once at the request
boundary, handled errors go to `warn!`, and Valkey spans are marked failed
through `otel.status_code` without logging.

**Verdict: Keep, but make it explicit.** It's a good fit for "any trace with an
error is always kept". Document it as a rule, or turn off
`with_error_events_to_status` and set status only through `otel.status_code`.

### 8. Semantic-convention gaps

Specific deviations in [http.rs](crates/telemetry/src/http.rs) and
[valkey.rs](crates/common/src/valkey.rs):

- **`error.type` is missing.** It's required on failed HTTP/DB spans and
  metrics. We use `otel.status_description` free text instead.
- **Client spans don't treat 4xx as Error.** Semconv says a client span with a
  4xx response is an error. Ours only marks 5xx.
- **Client span names** should be `{method}` or `{method} {url.template}`. We
  use `GET store`. `peer.service` was never stable.
- **Server spans lack `url.scheme`, `server.port`,
  `network.protocol.version` and `user_agent.original`.**
- **DB spans:** `server.address` includes the port (it should be split into
  `server.port`), `db.key` isn't a semconv attribute (`db.query.text` or
  `db.query.summary` are), and `db.namespace` (the Valkey DB index) is
  missing.
- **The tracer's instrumentation scope is the service name.** It should name
  the instrumentation library, for example the crate.
- **Resource:** no `service.version` or `service.instance.id`, and no
  container/host detectors.

**Verdict: Fix all of these.** They cost little, and backends key features off
them.

---

## Sampling

### 9. Tail-only sampling

We run no SDK sampler, export every span that passes `TRACE_FILTER`, and let
the Collector's `tail_sampling` processor decide. This is the only way to
"always keep traces with an error anywhere", and it matches common practice
for that requirement. What it costs:

- **Network and CPU for 100% of spans**, most of which get dropped. The
  verbosity filter (item 6) is what keeps this affordable.
- **Collector memory.** Every trace is held for `decision_wait` (now 10s in
  prod).
  `num_traces: 50000` caps it, and under overload traces get dropped
  unpredictably.
- **Scaling needs two tiers.** All spans of a trace must reach the same
  collector instance. With more than one instance you need a
  `loadbalancing` exporter tier routing by trace id in front of the sampling
  tier. We run a single collector, so we never hit this.
- **Late spans.** A trace longer than `decision_wait`, or a span flushed late
  by a batch processor, gets a separate decision and can end up split.

**Verdict: Keep.** It's the right tool for the requirement. Before production,
add the two-tier setup and consider a light SDK head sample for very
high-traffic, low-value routes.

> **Update:** the late-spans risk was real. Prod exported spans every 5s, the
> same as `decision_wait`, and an audit counted 19 late spans. Now prod
> exports every 2s, the collector waits 10s, and it remembers its decisions
> so late spans follow their trace's verdict. Checked against Loki's
> unsampled ERROR logs: 273 of 273 error traces kept, all complete, no late
> spans. The two-tier setup is still to do.

### 10. `ParentBased` can undermine tail sampling

The SDK's default sampler is `ParentBased(AlwaysOn)`, and it honours
`OTEL_TRACES_SAMPLER`. If a caller outside this system sends `traceparent`
with the sampled flag off, our services **won't record their spans at all**.
The tail sampler never sees them, so an error in that request can't be kept.
Our own `loadgen` always starts sampled traces, so the demo never shows this.

**Verdict: Fix.** When sampling is done in the collector, set `AlwaysOn`
explicitly, so the SDK records regardless of the incoming flag. Also decide
what `sampled` should mean on outgoing `traceparent` headers to systems
outside the tail-sampling setup.

---

## Configuration and control

### 11. `SERVICE_NAME`, `LOG_FILTER`, … vs. `OTEL_*`

OTel defines a standard set of environment variables:

- `OTEL_SERVICE_NAME`
- `OTEL_RESOURCE_ATTRIBUTES`
- `OTEL_SDK_DISABLED`
- `OTEL_TRACES_SAMPLER`
- `OTEL_PROPAGATORS`
- `OTEL_EXPORTER_OTLP_*`

We honour the exporter and batch variables because the SDK reads them. But
we invented `SERVICE_NAME` and `DEPLOY_ENV`. `with_service_name()` overrides
`OTEL_SERVICE_NAME`, even though the resource builder would otherwise read it.
And we treat an unset endpoint as "don't export", where the spec's default is
`localhost:4318`.

`LOG_FILTER` and `TRACE_FILTER` have no OTel counterpart, because OTel has no
verbosity (item 6). Those are fine.

**Verdict: Fix.** Read `OTEL_SERVICE_NAME` first, fall back to
`SERVICE_NAME`, drop `DEPLOY_ENV` in favour of `OTEL_RESOURCE_ATTRIBUTES`,
and respect `OTEL_SDK_DISABLED`. The "unset endpoint = off" default is a
deliberate developer convenience. Keep it, but document it.

### 12. Control socket

OTel's answers to remote configuration are:

- **OpAMP**, a protocol for a management server to push config to agents and
  SDKs.
- **Declarative configuration**, a standard config file format for SDKs.

As far as I know, neither has meaningful support in the Rust SDK yet
(*not verified for this document*), and neither has a concept of per-module
verbosity to change. The Unix socket is simple, local, and
needs `exec` access to the container, which is a reasonable security
boundary.

**Verdict: Keep for now.** If OpAMP support arrives in Rust, the filters would
be a natural custom capability to expose over it.

---

## Propagation and export

### 13. Propagators

The spec's default propagator set is `tracecontext,baggage`. We install only
tracecontext, so W3C `baggage` is silently dropped at every hop. The Jaeger
`uber-trace-id` propagator was left out because
`opentelemetry-jaeger-propagator` is one release behind `opentelemetry` 0.33.

**Verdict: Fix baggage**, a one-line `TextMapCompositePropagator`. **Jaeger
headers: Depends.** Only needed if there are legacy Jaeger-client services in
the path, and there usually aren't any more.

### 14. "Jaeger format" export

The Jaeger Thrift exporters are gone from opentelemetry-rust, and Jaeger v2 is
built on the Collector and takes OTLP natively. Using OTLP, and letting the
Collector talk to Jaeger, *is* the current best practice.

**Verdict: Keep.** This was the industry moving on in a good way.

### 15. The `tracing` bridge

`tracing-opentelemetry` is maintained under tokio-rs, not by OpenTelemetry.
It maps one context system (`tracing` spans in the registry) onto another
(OTel `Context`). Consequences we hit or worked around:

- `set_parent` must happen before the span is started.
- Spans below `TRACE_FILTER` fall back to the ambient context
  (`context_of()`).
- Log correlation had to read the active OTel context, because the fmt
  layer's per-layer filter hid the spans from it. That was the prod bug we
  fixed.
- Libraries instrumented with the OTel API directly only nest correctly
  because context activation is on.

It's commonly said to cost more CPU per span than the native OTel API. *We
haven't measured that here*, so treat it as a claim to benchmark before
relying on it.

**Verdict: Keep.** In Rust, `tracing` is still how libraries are
instrumented. `tokio`, `hyper`, `tower`, `reqwest` and `sqlx` all emit
`tracing`, not OTel. A native-OTel-only design would lose all of that.

---

## What changed in the second pass

- **Sampling correctness (item 10):** the SDK sampler is now `AlwaysOn`. A
  caller sending `sampled=0` can no longer stop spans reaching the tail
  sampler.
- **Propagation (item 13):** the propagator is the spec's default
  `tracecontext,baggage`.
- **Semconv spans (item 8):**
  - `error.type` on every failed HTTP and DB span.
  - Client spans are errors on 4xx/5xx and named `{method} {url.template}`.
  - Server spans add `url.scheme`, `server.address`/`server.port`,
    `network.protocol.version` and `user_agent.original`.
  - DB spans split `server.port` out, add `db.namespace`, and move the key to
    our own `valkey.key` attribute.
  - The instrumentation scope is the crate (`telemetry`), not the service.
- **Sampler vs. client 4xx:** making client 4xx an error would have
  force-kept every trace containing a 404. The collector's error policy is
  now an OTTL condition that ignores client spans with a 4xx status. Checked
  with 1000 prod requests: 2 of 33 404 traces kept, which is the 5% baseline.
- **Semconv metrics (item 3):** `http.server.request.duration`,
  `http.client.request.duration` and `db.client.operation.duration`, each
  with the semconv attribute set and recommended buckets. `span.duration`
  remains for spans with no convention. The status label is replaced by
  `error.type`. Unit tests cover the routing.
- **Logs (items 1–2):**
  - `opentelemetry-appender-tracing` exports log records over OTLP, under the
    same reloadable `LOG_FILTER` as stdout, plus the exporter feedback guard.
  - The collector sends them to Loki, unsampled.
  - `SPAN_EVENT_LEVEL` limits which events become span events. It can be
    changed at runtime, and prod uses `warn`.
  - JSON stdout gained `trace_flags`.
- **Configuration (item 11):**
  - `OTEL_SERVICE_NAME` takes precedence (`SERVICE_NAME` is an alias).
  - `OTEL_RESOURCE_ATTRIBUTES` replaces `DEPLOY_ENV`.
  - `OTEL_SDK_DISABLED` is honoured.
  - `service.version` and a per-process `service.instance.id` are added.
  - The one remaining deviation, "no endpoint = no export", is documented.
- **Grafana** is provisioned with Loki and Prometheus. Log lines link to their
  trace in the Jaeger UI. A Jaeger datasource wasn't possible, because Jaeger
  2.21 serves only its v3 API and Grafana 13's Jaeger datasource doesn't use
  it.
- **Collector self-metrics** are scraped by Prometheus, including
  tail-sampling decisions per policy.
- **Checked "always keep errors" against the logs:**
  - Loki's unsampled ERROR records serve as ground truth. Result: 273 of 273
    error traces kept, all complete.
  - The first audit found late spans. Prod exported spans every 5s (the SDK
    default), which was the same as `decision_wait`. Now prod exports every
    2s and the collector waits 10s. The collector also remembers its
    decisions, so late spans follow their trace's verdict.
  - The OTTL status-code comparison uses `Int()`, so a type mismatch can't
    silently turn "keep" into "drop".

### 16. Unsigned integers become strings (found in pass two)

`tracing-opentelemetry` 0.34's span and event visitors implement
`record_i64` but not `record_u64`. `tracing`'s default then routes `u64`
through `record_debug`, so `status.as_u16()` was exported as the *string*
`"404"`. That violates semconv (`http.response.status_code` is an int). It
also made the collector's OTTL `< 500` comparison unreliable: in one run it
force-kept every 404 trace, and in another it didn't. We now record every
integer span or event field as `i64`, and the reason is documented in
[http.rs](crates/telemetry/src/http.rs). This is a small example of the cost
of the bridge (item 15). A fix upstream (`record_u64` → `Value::I64` when it
fits) would remove the workaround.

### Still open

- **Exemplars (item 5):** needs the span's context active at record time, and
  a policy for linking only to traces that will survive tail sampling.
- **Two-tier collector (item 9):** a `loadbalancing` exporter routing by trace
  id is needed before running more than one collector.
- **Jaeger propagator (item 13):** waiting on
  `opentelemetry-jaeger-propagator` to catch up with `opentelemetry` 0.33.

## Tradeoffs introduced by the fixes

### 17. Client 4xx errors don't force-keep a trace

The original requirement was that **any trace containing a span with an
error is kept at 100%**. Semconv says an HTTP client span that gets a 4xx is
an error, and item 8 adopted that. Taken literally, every lookup that 404s
would then force-keep its trace. So the collector's error policy excludes
client spans whose only problem is a 4xx
([otel-collector.yaml](deploy/otel-collector.yaml)).

The consequence: **a trace whose only error span is a client-side 4xx is not
guaranteed to be kept.** It falls back to the baseline percentage and the
slow-trace rule. Server-side 5xx, transport failures and failed Valkey calls
are unaffected, and the audit shows 100% of those kept.

**Verdict: Keep, but know it's a change to the requirement.** A 4xx is usually
the caller's fault and is visible on the server span as a 4xx anyway. If a
service needs 4xx traces, make the exclusion configurable, for example a
`SAMPLING_KEEP_CLIENT_4XX` variable that drops the `not (…)` clause, rather
than removing it everywhere.

### 18. Demoting a span deletes its metric

The span-metrics layer only measures spans at INFO or above, so metrics don't
depend on `TRACE_FILTER`. That also means moving `valkey.command` to
`debug_span!` would silently remove `db.client.operation.duration`, with no
error and no warning.

**Verdict: Fix.** Choose spans for metrics by *what they are*, not only their
level. Span metadata includes field names, so the metrics filter can also
accept any span that declares `otel.kind` or `db.system.name`, whatever its
level. Add a test that lists the spans that must produce metrics.

### 19. Two layers decide "was this an error?"

The OpenTelemetry layer sets the span status from ERROR events and
`otel.status_code`. The span-metrics layer computes `error.type` from the same
two signals, with its own code. They agree today. Nothing makes sure they
keep agreeing, and a disagreement would mean dashboards and trace sampling
see different error rates.

**Verdict: Fix.** Add a test that sends one span through every layer and
checks that the exported span status, the metric's `error.type` and the OTLP
log record agree.

### 20. Sampling is out of `tracing`'s reach

Every other control lives in `tracing` terms: levels, filters, fields, and the
control socket. The keep/drop decision lives only in the collector's config,
and changing it means restarting the collector. That is the right place to
*make* the decision, because only the collector sees the whole trace. But
there's no way to *influence* it from code or at runtime.

**Verdict: Depends.** A cheap bridge: the collector honours a span field such
as `sampling.keep = true` (one more OTTL condition). Code can then mark
important operations with a field, and a control-socket setting could turn it
on per route. Worth doing if the project grows; not needed for the demo.

### 21. Prod has no INFO logs anywhere

`LOG_FILTER` now governs OTLP logs as well as stdout, which is the right
default: one knob, one meaning. With `LOG_FILTER=warn` in prod, INFO events
such as "request completed" aren't printed, aren't in Loki, and (because
`SPAN_EVENT_LEVEL=warn`) aren't on spans. They exist only as metrics.

**Verdict: Keep. It's a choice, not a bug.** If you want INFO in Loki but not
on stdout, add a separate `LOG_EXPORT_FILTER` (a third reload handle, about
ten lines). Until then, raising the level at runtime via the control socket is
the way to see INFO in prod.

### 22. WARN and ERROR are stored twice in prod

A `warn!` in prod becomes an OTLP log record (Loki) and a span event (Jaeger,
if its trace is kept). That duplicates storage for exactly the events you most
want to see.

**Verdict: Keep.** The volume is small, and each copy is useful where it
lives: in Jaeger you see the failure in the timeline, and in Loki you can
search it even when the trace was dropped.

### 23. Smaller gaps

- **The OTTL rule uses `error_mode: ignore`.** Any evaluation error means
  "don't keep". `Int()` removed the known type issue, but a future attribute
  change could reintroduce silent drops. **Fix:** alert on
  `otelcol_processor_tail_sampling_sampling_policy_evaluation_error` (it's
  already in Prometheus).
- **Grafana has no Jaeger datasource.** Jaeger 2.21 serves only its v3 API,
  which Grafana 13's Jaeger datasource doesn't use. Logs link out to the
  Jaeger UI, but Grafana can't show traces itself. **Depends** on Grafana
  catching up, or on adding Tempo.
- **`service.version` comes from the telemetry crate,** not each binary. It's
  correct here only because the workspace shares one version. **Fix:** pass
  the version from each `main`.
- **`span.duration` isn't a standard name** and has no namespace. **Fix,
  cheaply:** rename to something like `tracing.span.duration`.

## Overall assessment

You're right that the industry moved, but it moved in a particular direction.
It standardised:

- **the wire:** OTLP
- **the vocabulary:** semantic conventions
- **the control point:** the Collector, where sampling, span-derived metrics,
  routing and redaction happen

It didn't standardise on a better *instrumentation experience* than `tracing`.
OpenTelemetry's Rust project uses `tracing` for its own logging and recommends
it for yours.

What still holds up from the original idea:

- **One instrumentation API, many signals.** Engineers write `info_span!` and
  `warn!` and don't think about exporters. This is still the best part.
- **Verbosity as a cost control.** Choosing detail with the same level
  vocabulary as logging, per module and at runtime, is something OTel users
  mostly do by hand with sampling rules and instrumentation on/off switches.
  It's a real advantage.
- **Metrics that don't depend on sampling.** Correct, and it matches best
  practice.

Where the design gave ground, and it was worth it:

- **Output formats are OTel's now:** semconv span and metric names, OTLP logs,
  `OTEL_*` env vars, baggage. Standard dashboards and backends understand the
  data without any mapping.
- **Span events are no longer the log store.** Logs are their own signal, and
  span events are a level-gated extra on top.
- **Sampling is hardened:** explicit `AlwaysOn`, decision timing tuned,
  decisions remembered, and "always keep errors" checked against unsampled
  logs.

What it cost:

- **More glue:** the telemetry crate has five layers, an export guard and an
  integer-type workaround.
- **More containers:** six infrastructure containers where there were four.
- **New sharp edges:** items 17–23. The most important is item 17, where
  "always keep errors" now has a stated exception for client-side 4xx.

In short: keep `tracing` as the way the code is written, keep the output
OTel-conformant, and let the Collector make the sampling decision. The core
idea holds. One instrumentation API, where levels control cost and detail and
can be changed at runtime, still has no real equivalent elsewhere.

### Next pass, in order

1. **Metrics chosen by span kind as well as level (item 18).** Small change,
   and it removes a silent failure.
2. **Test that all three signals agree on errors (item 19).** Protects the
   "everything comes from one source" claim.
3. **Alert on OTTL evaluation errors, and make the client-4xx exclusion
   configurable (items 17 and 23).**
4. **`sampling.keep` span field (item 20).** Lets code influence sampling in
   `tracing` terms.
5. **Small fixes:** per-binary `service.version`, rename `span.duration`, and
   benchmark the bridge (item 15).
6. **Before real traffic:** two-tier collector and exemplars (items 5 and 9).
