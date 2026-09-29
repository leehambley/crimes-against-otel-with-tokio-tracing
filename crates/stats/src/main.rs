//! Access statistics backed by Valkey.
//!
//! `POST /events` `{"key": "...", "kind": "read"|"write"}` and `GET /top?n=5`.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use common::{AppError, Chaos, Valkey, env_or};
use serde::Deserialize;
use serde_json::{Value, json};
use telemetry::Telemetry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let telemetry = Telemetry::init_from_env("stats")?;
    telemetry.spawn_control_socket();
    let chaos = Chaos::from_env();
    chaos.register(&telemetry.control());

    let valkey = Valkey::connect(&env_or("VALKEY_URL", "redis://127.0.0.1:6379"), chaos).await?;
    let app = Router::new()
        .route("/events", post(record_event))
        .route("/top", get(top))
        .route_layer(axum::middleware::from_fn(telemetry::http::server_span))
        .with_state(valkey);

    let result = common::serve(app, "0.0.0.0:8082").await;
    telemetry.shutdown();
    result
}

#[derive(Debug, Deserialize)]
struct AccessEvent {
    key: String,
    kind: String,
}

#[tracing::instrument(level = "debug", skip(db), fields(key = %event.key, kind = %event.kind))]
async fn record_event(
    State(db): State<Valkey>,
    Json(event): Json<AccessEvent>,
) -> Result<StatusCode, AppError> {
    if !matches!(event.kind.as_str(), "read" | "write") {
        return Err(AppError::BadRequest(format!(
            "unknown kind {:?}",
            event.kind
        )));
    }
    let mut pipe = redis::pipe();
    pipe.atomic()
        .incr(format!("stats:{}:total", event.kind), 1)
        .ignore()
        .zincr("stats:hot", &event.key, 1)
        .ignore();
    db.pipeline::<()>("MULTI", &event.key, &pipe).await?;
    tracing::debug!("event recorded");
    Ok(StatusCode::ACCEPTED)
}

#[derive(Debug, Deserialize)]
struct TopQuery {
    n: Option<isize>,
}

#[tracing::instrument(level = "debug", skip(db))]
async fn top(State(db): State<Valkey>, Query(q): Query<TopQuery>) -> Result<Json<Value>, AppError> {
    let n = q.n.unwrap_or(5).clamp(1, 100);
    let (reads, writes, hot): (Option<u64>, Option<u64>, Vec<(String, f64)>) = db
        .pipeline(
            "MULTI",
            "stats:hot",
            redis::pipe()
                .get("stats:read:total")
                .get("stats:write:total")
                .zrevrange_withscores("stats:hot", 0, n - 1),
        )
        .await?;
    Ok(Json(json!({
        "reads": reads.unwrap_or(0),
        "writes": writes.unwrap_or(0),
        "hot": hot.into_iter().map(|(k, s)| json!({"key": k, "hits": s})).collect::<Vec<_>>(),
    })))
}
