//! Item storage backed by Valkey.
//!
//! `PUT /items/{key}` `{"value": ...}` and `GET /items/{key}`.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::put,
};
use common::{AppError, Chaos, Valkey, env_or};
use serde_json::{Value, json};
use telemetry::Telemetry;

const TTL_SECONDS: u64 = 3600;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let telemetry = Telemetry::init_from_env("store")?;
    telemetry.spawn_control_socket();
    let chaos = Chaos::from_env();
    chaos.register(&telemetry.control());

    let valkey = Valkey::connect(&env_or("VALKEY_URL", "redis://127.0.0.1:6379"), chaos).await?;
    let app = Router::new()
        .route("/items/{key}", put(put_item).get(get_item))
        .route_layer(axum::middleware::from_fn(telemetry::http::server_span))
        .with_state(valkey);

    let result = common::serve(app, "0.0.0.0:8081").await;
    telemetry.shutdown();
    result
}

#[tracing::instrument(level = "debug", skip(db, body))]
async fn put_item(
    State(db): State<Valkey>,
    Path(key): Path<String>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let value = validate(&key, body)?;
    let (encoded, checksum) = encode(&value);

    db.cmd::<()>(
        "SET",
        &key,
        redis::cmd("SET")
            .arg(format!("item:{key}"))
            .arg(&encoded)
            .arg("EX")
            .arg(TTL_SECONDS),
    )
    .await?;

    tracing::event!(target: "metrics", tracing::Level::TRACE, monotonic_counter.store_bytes_written = encoded.len() as u64);
    tracing::debug!(bytes = encoded.len() as i64, %checksum, "item stored");
    Ok((
        StatusCode::CREATED,
        Json(json!({ "key": key, "bytes": encoded.len(), "checksum": checksum })),
    ))
}

#[tracing::instrument(level = "debug", skip(db))]
async fn get_item(
    State(db): State<Valkey>,
    Path(key): Path<String>,
) -> Result<Json<Value>, AppError> {
    let raw: Option<String> = db
        .cmd("GET", &key, redis::cmd("GET").arg(format!("item:{key}")))
        .await?;
    let Some(raw) = raw else {
        tracing::debug!("cache miss");
        return Err(AppError::NotFound);
    };
    let value: Value = decode(&raw)?;
    tracing::debug!(bytes = raw.len() as i64, "item loaded");
    Ok(Json(json!({ "key": key, "value": value })))
}

/// DEBUG-level span: visible in dev traces, absent in prod.
#[tracing::instrument(level = "debug", skip(body), err(level = "debug"))]
fn validate(key: &str, body: Value) -> Result<Value, AppError> {
    let valid_key = !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid_key {
        return Err(AppError::BadRequest(format!("invalid key {key:?}")));
    }
    let Value::Object(mut map) = body else {
        return Err(AppError::BadRequest("body must be an object".into()));
    };
    map.remove("value")
        .ok_or_else(|| AppError::BadRequest("missing \"value\"".into()))
}

/// TRACE-level span: only in the most verbose traces.
#[tracing::instrument(level = "trace", skip_all, fields(bytes))]
fn encode(value: &Value) -> (String, String) {
    let encoded = value.to_string();
    let checksum = format!("{:016x}", fnv1a(encoded.as_bytes()));
    tracing::Span::current().record("bytes", encoded.len() as i64);
    tracing::trace!(%checksum, "encoded");
    (encoded, checksum)
}

#[tracing::instrument(level = "trace", skip_all, fields(bytes = raw.len() as i64))]
fn decode(raw: &str) -> Result<Value, AppError> {
    serde_json::from_str(raw).map_err(|e| AppError::BadRequest(format!("corrupt item: {e}")))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
