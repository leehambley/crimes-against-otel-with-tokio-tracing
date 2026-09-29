//! Public entry point. Proxies to `store` and `stats`, propagating the trace.
//!
//! `PUT /items/{key}`: store, then record a write event (stats failure is
//! tolerated: the request succeeds but the trace contains an error span).
//! `GET /items/{key}`: store read and stats event in parallel.
//! `GET /top`: stats passthrough.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, RawQuery, State},
    http::StatusCode,
    routing::{get, put},
};
use common::{AppError, Chaos, env_or};
use serde_json::{Value, json};
use telemetry::Telemetry;

#[derive(Clone)]
struct Upstreams {
    client: reqwest::Client,
    store: Arc<str>,
    stats: Arc<str>,
    chaos: Arc<Chaos>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let telemetry = Telemetry::init_from_env("gateway")?;
    telemetry.spawn_control_socket();
    let chaos = Chaos::from_env();
    chaos.register(&telemetry.control());

    let upstreams = Upstreams {
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()?,
        store: env_or("STORE_URL", "http://127.0.0.1:8081").into(),
        stats: env_or("STATS_URL", "http://127.0.0.1:8082").into(),
        chaos,
    };
    let app = Router::new()
        .route("/items/{key}", put(put_item).get(get_item))
        .route("/top", get(top))
        .route_layer(axum::middleware::from_fn(telemetry::http::server_span))
        .with_state(upstreams);

    let result = common::serve(app, "0.0.0.0:8080").await;
    telemetry.shutdown();
    result
}

#[tracing::instrument(level = "debug", skip(up, body))]
async fn put_item(
    State(up): State<Upstreams>,
    Path(key): Path<String>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    authorize(&up).await?;
    let stored = up
        .call(
            "store",
            "/items/{key}",
            up.client
                .put(format!("{}/items/{key}", up.store))
                .json(&body),
        )
        .await?;
    if let Err(err) = up.record_event(&key, "write").await {
        tracing::warn!(error = %err, "stats unavailable, write not counted");
    }
    Ok((StatusCode::CREATED, Json(stored)))
}

#[tracing::instrument(level = "debug", skip(up))]
async fn get_item(
    State(up): State<Upstreams>,
    Path(key): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize(&up).await?;
    // Two concurrent child spans under the same parent.
    let (item, event) = tokio::join!(
        up.call(
            "store",
            "/items/{key}",
            up.client.get(format!("{}/items/{key}", up.store))
        ),
        up.record_event(&key, "read"),
    );
    if let Err(err) = event {
        tracing::warn!(error = %err, "stats unavailable, read not counted");
    }
    Ok(Json(item?))
}

#[tracing::instrument(level = "debug", skip(up))]
async fn top(
    State(up): State<Upstreams>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, AppError> {
    let url = match query {
        Some(q) => format!("{}/top?{q}", up.stats),
        None => format!("{}/top", up.stats),
    };
    Ok(Json(up.call("stats", "/top", up.client.get(url)).await?))
}

/// Stand-in for an auth check: a place for gateway-local chaos to strike.
#[tracing::instrument(level = "debug", skip_all)]
async fn authorize(up: &Upstreams) -> Result<(), AppError> {
    up.chaos.inject("authorize").await?;
    Ok(())
}

impl Upstreams {
    async fn record_event(&self, key: &str, kind: &str) -> Result<Value, AppError> {
        self.call(
            "stats",
            "/events",
            self.client
                .post(format!("{}/events", self.stats))
                .json(&json!({ "key": key, "kind": kind })),
        )
        .await
    }

    /// Sends a request with trace propagation and maps the outcome.
    /// `url_template` names the client span and labels its metrics.
    async fn call(
        &self,
        peer: &'static str,
        url_template: &'static str,
        request: reqwest::RequestBuilder,
    ) -> Result<Value, AppError> {
        let upstream = |detail: String| AppError::Upstream { peer, detail };
        let request = request.build().map_err(|e| upstream(e.to_string()))?;
        let response = telemetry::http::send(&self.client, request, url_template)
            .await
            .map_err(|e| upstream(e.to_string()))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        match status {
            s if s.is_success() => Ok(body),
            StatusCode::NOT_FOUND => Err(AppError::NotFound),
            s if s.is_client_error() => Err(AppError::BadRequest(body.to_string())),
            s => Err(upstream(format!("{s}: {body}"))),
        }
    }
}
