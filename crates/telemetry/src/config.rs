use std::{env, path::PathBuf};

/// How log lines are rendered on stdout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// One line per event, with `trace_id`/`span_id` for correlation.
    Line,
    /// Multi-line: event, source location, and the span stack with fields.
    Pretty,
    /// One JSON object per event, `trace_id`/`span_id` as top-level keys.
    Json,
}

impl std::str::FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "line" | "plain" | "compact" => Ok(Self::Line),
            "pretty" | "context" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            other => Err(format!("unknown LOG_FORMAT {other:?} (line|pretty|json)")),
        }
    }
}

/// Everything is read from the environment so the same binary can run as a
/// chatty dev build or a quiet production one.
///
/// | variable                       | default                  |
/// |--------------------------------|--------------------------|
/// | `SERVICE_NAME`                 | per-binary               |
/// | `DEPLOY_ENV`                   | `dev`                    |
/// | `LOG_FORMAT`                   | `line`                   |
/// | `LOG_FILTER` (or `RUST_LOG`)   | `info`                   |
/// | `TRACE_FILTER`                 | same as `LOG_FILTER`     |
/// | `OTEL_EXPORTER_OTLP_ENDPOINT`  | unset = don't export     |
/// | `LOG_COLOR`                    | `auto` (colour on a TTY) |
/// | `CONTROL_SOCKET`               | `/tmp/<service>.ctl`     |
#[derive(Clone, Debug)]
pub struct Config {
    pub service_name: String,
    pub environment: String,
    pub log_format: LogFormat,
    pub log_filter: String,
    pub trace_filter: String,
    pub otlp_enabled: bool,
    /// `auto`: colour when stdout is a terminal. `always` is for viewers
    /// that render ANSI but aren't a TTY themselves, like `podman logs`.
    pub log_color: bool,
    pub control_socket: Option<PathBuf>,
}

impl Config {
    pub fn from_env(default_service_name: &str) -> Self {
        let var = |name: &str| env::var(name).ok().filter(|v| !v.trim().is_empty());

        let service_name = var("SERVICE_NAME").unwrap_or_else(|| default_service_name.to_owned());
        let log_format = var("LOG_FORMAT")
            .map(|v| v.parse().unwrap_or_else(|e| panic!("{e}")))
            .unwrap_or(LogFormat::Line);
        let log_filter = var("LOG_FILTER")
            .or_else(|| var("RUST_LOG"))
            .unwrap_or_else(|| "info".to_owned());
        let trace_filter = var("TRACE_FILTER").unwrap_or_else(|| log_filter.clone());
        let control_socket = match var("CONTROL_SOCKET").as_deref() {
            Some("off") => None,
            Some(path) => Some(PathBuf::from(path)),
            None => Some(env::temp_dir().join(format!("{service_name}.ctl"))),
        };

        let log_color = match var("LOG_COLOR").as_deref() {
            Some("always") => true,
            Some("never") => false,
            Some("auto") | None => {
                var("NO_COLOR").is_none() && std::io::IsTerminal::is_terminal(&std::io::stdout())
            }
            Some(other) => panic!("unknown LOG_COLOR {other:?} (auto|always|never)"),
        };

        Self {
            log_color,
            environment: var("DEPLOY_ENV").unwrap_or_else(|| "dev".to_owned()),
            otlp_enabled: var("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
                || var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_some(),
            service_name,
            log_format,
            log_filter,
            trace_filter,
            control_socket,
        }
    }
}
