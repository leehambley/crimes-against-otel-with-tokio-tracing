//! Load generator / test client. Each request is the root of a trace that
//! flows loadgen → gateway → store/stats → valkey. At the end it prints a
//! latency summary and Jaeger links for failed requests.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;
use serde_json::json;
use telemetry::Telemetry;
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::{Instrument as _, field::Empty};

#[derive(Debug, Parser)]
struct Args {
    /// Gateway base URL.
    #[arg(long, env = "TARGET_URL", default_value = "http://127.0.0.1:8080")]
    target: String,
    /// Total requests to send.
    #[arg(short = 'n', long, default_value_t = 200)]
    requests: usize,
    /// Requests in flight at once.
    #[arg(short = 'c', long, default_value_t = 8)]
    concurrency: usize,
    /// Size of the key space.
    #[arg(long, default_value_t = 25)]
    keys: usize,
    /// Fraction of requests that are GETs (the rest are PUTs).
    #[arg(long, default_value_t = 0.7)]
    read_ratio: f64,
    /// Where to point the trace links.
    #[arg(long, env = "JAEGER_UI", default_value = "http://localhost:16686")]
    jaeger_ui: String,
}

struct Outcome {
    op: &'static str,
    status: Option<u16>,
    latency: Duration,
    trace_id: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let telemetry = Telemetry::init_from_env("loadgen")?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let permits = Arc::new(Semaphore::new(args.concurrency.max(1)));
    let target: Arc<str> = args.target.trim_end_matches('/').into();
    let mut tasks = JoinSet::new();

    let started = Instant::now();
    for i in 0..args.requests {
        let permit = permits.clone().acquire_owned().await?;
        let (client, target) = (client.clone(), target.clone());
        let key = format!("key-{}", rand::random_range(0..args.keys.max(1)));
        let read = rand::random_bool(args.read_ratio.clamp(0.0, 1.0));
        tasks.spawn(async move {
            let outcome = one_request(&client, &target, i, &key, read).await;
            drop(permit);
            outcome
        });
    }
    let outcomes: Vec<Outcome> = tasks.join_all().await;
    let elapsed = started.elapsed();

    report(&args, &outcomes, elapsed);
    telemetry.shutdown();
    Ok(())
}

async fn one_request(
    client: &reqwest::Client,
    target: &str,
    seq: usize,
    key: &str,
    read: bool,
) -> Outcome {
    let op = if read { "GET" } else { "PUT" };
    let span = tracing::info_span!(
        "loadgen.request",
        otel.name = %format!("loadgen {op} /items/{{key}}"),
        otel.status_code = Empty,
        seq,
        key,
        trace_id = Empty,
    );
    let trace_id = telemetry::http::trace_id(&span);
    span.record("trace_id", &trace_id);

    let url = format!("{target}/items/{key}");
    let request = if read {
        client.get(url)
    } else {
        client
            .put(url)
            .json(&json!({ "value": { "seq": seq, "note": "hello" } }))
    };

    let started = Instant::now();
    let status = async {
        let result = match request.build() {
            Ok(req) => telemetry::http::send(client, req, "gateway").await,
            Err(e) => Err(e),
        };
        match result {
            Ok(resp) => Some(resp.status().as_u16()),
            Err(err) => {
                tracing::warn!(error = %err, "request failed");
                None
            }
        }
    }
    .instrument(span.clone())
    .await;

    if status.is_none_or(|s| s >= 500) {
        span.record("otel.status_code", "ERROR");
    }
    Outcome {
        op,
        status,
        latency: started.elapsed(),
        trace_id,
    }
}

fn report(args: &Args, outcomes: &[Outcome], elapsed: Duration) {
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    for o in outcomes {
        let label = o.status.map_or("transport error".into(), |s| s.to_string());
        *by_status.entry(format!("{} {label}", o.op)).or_default() += 1;
    }
    let mut latencies: Vec<Duration> = outcomes.iter().map(|o| o.latency).collect();
    latencies.sort();
    let pct = |p: f64| -> Duration {
        if latencies.is_empty() {
            return Duration::ZERO;
        }
        latencies[((latencies.len() - 1) as f64 * p).round() as usize]
    };

    println!(
        "\n{} requests in {:.2?} ({:.0} req/s), concurrency {}",
        outcomes.len(),
        elapsed,
        outcomes.len() as f64 / elapsed.as_secs_f64(),
        args.concurrency
    );
    println!(
        "latency p50 {:.2?}  p95 {:.2?}  p99 {:.2?}  max {:.2?}",
        pct(0.5),
        pct(0.95),
        pct(0.99),
        pct(1.0)
    );
    for (label, count) in &by_status {
        println!("  {label:<22} {count}");
    }

    let failed: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.status.is_none_or(|s| s >= 500))
        .collect();
    if !failed.is_empty() {
        println!(
            "\n{} failed; these traces are always kept by the tail sampler:",
            failed.len()
        );
        for o in failed.iter().take(10) {
            println!("  {}/trace/{}", args.jaeger_ui, o.trace_id);
        }
    }
    let slowest = outcomes.iter().max_by_key(|o| o.latency);
    if let Some(o) = slowest {
        println!(
            "\nslowest ({:.2?}): {}/trace/{}",
            o.latency, args.jaeger_ui, o.trace_id
        );
    }
}
