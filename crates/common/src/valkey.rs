//! Valkey access where every command is a `client` span, and chaos is
//! injected *inside* that span so failures land where they'd really occur.

use std::sync::Arc;

use redis::{FromRedisValue, aio::ConnectionManager};
use tracing::{Instrument as _, Span, field::Empty};

use crate::{AppError, Chaos};

#[derive(Clone)]
pub struct Valkey {
    conn: ConnectionManager,
    chaos: Arc<Chaos>,
    host: Arc<str>,
    port: Option<u16>,
    db: i64,
}

impl Valkey {
    pub async fn connect(url: &str, chaos: Arc<Chaos>) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let info = client.get_connection_info();
        let (host, port): (Arc<str>, _) = match info.addr() {
            redis::ConnectionAddr::Tcp(h, p)
            | redis::ConnectionAddr::TcpTls {
                host: h, port: p, ..
            } => (h.as_str().into(), Some(*p)),
            other => (other.to_string().into(), None),
        };
        let db = info.redis_settings().db();
        let conn = ConnectionManager::new(client)
            .instrument(tracing::info_span!(
                "valkey.connect",
                server.address = %host,
                server.port = port.map(i64::from)
            ))
            .await?;
        tracing::info!(address = %host, port, db, "connected to valkey");
        Ok(Self {
            conn,
            chaos,
            host,
            port,
            db,
        })
    }

    /// Runs one command. `op` is the command name used for the span and
    /// metrics (`GET`, `SET`, ...); `key` is recorded as `valkey.key` for
    /// debugging (not a semconv attribute, hence our own namespace).
    pub async fn cmd<T: FromRedisValue>(
        &self,
        op: &'static str,
        key: &str,
        cmd: &redis::Cmd,
    ) -> Result<T, AppError> {
        let mut conn = self.conn.clone();
        self.run(op, key, async move { cmd.query_async(&mut conn).await })
            .await
    }

    /// Runs a MULTI/EXEC pipeline as a single span.
    pub async fn pipeline<T: FromRedisValue>(
        &self,
        op: &'static str,
        key: &str,
        pipe: &redis::Pipeline,
    ) -> Result<T, AppError> {
        let mut conn = self.conn.clone();
        self.run(op, key, async move { pipe.query_async(&mut conn).await })
            .await
    }

    async fn run<T>(
        &self,
        op: &'static str,
        key: &str,
        fut: impl Future<Output = redis::RedisResult<T>>,
    ) -> Result<T, AppError> {
        let span = tracing::info_span!(
            "valkey.command",
            otel.name = op,
            otel.kind = "client",
            otel.status_code = Empty,
            otel.status_description = Empty,
            db.system.name = "redis",
            db.operation.name = op,
            db.namespace = self.db,
            server.address = %self.host,
            server.port = self.port.map(i64::from),
            valkey.key = key,
            error.type = Empty,
        );
        async move {
            let result = match self.chaos.inject(op).await {
                Ok(()) => fut.await.map_err(AppError::from),
                Err(chaos) => Err(chaos.into()),
            };
            match &result {
                Ok(_) => tracing::trace!("command ok"),
                // Mark the span failed without logging: the request boundary
                // logs the error once (see AppError::into_response).
                Err(err) => {
                    let span = Span::current();
                    span.record("otel.status_code", "ERROR");
                    span.record("otel.status_description", tracing::field::display(err));
                    span.record("error.type", err.error_type());
                }
            }
            result
        }
        .instrument(span)
        .await
    }
}
