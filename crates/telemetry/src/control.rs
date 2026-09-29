//! Runtime control over a Unix socket.
//!
//! `tracing-subscriber` gives us [`reload`] handles but no transport, so this
//! module adds a tiny line protocol on top. One command per connection line:
//!
//! ```text
//! show                           current filters and knobs
//! log   <directives>             replace LOG_FILTER,   e.g. `log warn,store=debug`
//! trace <directives>             replace TRACE_FILTER, e.g. `trace info,store::db=trace`
//! level <directives>             replace both
//! set   <knob> <value>           app-registered knobs, e.g. `set chaos.failure_pct 25`
//! help
//! ```
//!
//! Use the `ctl` binary, or `echo 'log debug' | nc -U /tmp/store.ctl`.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};
use tracing_subscriber::{EnvFilter, Registry, reload};

use crate::filter;

type FilterHandle = reload::Handle<EnvFilter, Registry>;

pub type KnobGetter = Box<dyn Fn() -> String + Send + Sync>;
pub type KnobSetter = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// A named runtime-adjustable value.
pub struct Knob {
    pub get: KnobGetter,
    pub set: KnobSetter,
}

pub struct Control {
    log: FilterHandle,
    trace: FilterHandle,
    current: Mutex<(String, String)>,
    knobs: Mutex<BTreeMap<String, Arc<Knob>>>,
}

impl Control {
    pub(crate) fn new(
        log: FilterHandle,
        trace: FilterHandle,
        log_s: String,
        trace_s: String,
    ) -> Self {
        Self {
            log,
            trace,
            current: Mutex::new((log_s, trace_s)),
            knobs: Mutex::default(),
        }
    }

    pub fn register(&self, name: impl Into<String>, knob: Knob) {
        self.knobs
            .lock()
            .unwrap()
            .insert(name.into(), Arc::new(knob));
    }

    pub fn set_log_filter(&self, directives: &str) -> anyhow::Result<()> {
        let new = filter::log_filter(directives)?;
        self.log.reload(new)?;
        let old = std::mem::replace(&mut self.current.lock().unwrap().0, directives.to_owned());
        tracing::info!(%old, new = %directives, "log filter changed");
        Ok(())
    }

    pub fn set_trace_filter(&self, directives: &str) -> anyhow::Result<()> {
        let new = filter::trace_filter(directives)?;
        self.trace.reload(new)?;
        let old = std::mem::replace(&mut self.current.lock().unwrap().1, directives.to_owned());
        tracing::info!(%old, new = %directives, "trace filter changed");
        Ok(())
    }

    /// Executes one command line and returns the reply.
    pub fn execute(&self, line: &str) -> String {
        let line = line.trim();
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let result = match cmd {
            "" | "help" => Ok(HELP.to_owned()),
            "show" => Ok(self.show()),
            "log" => self.set_log_filter(rest).map(|_| self.show()),
            "trace" => self.set_trace_filter(rest).map(|_| self.show()),
            "level" => self
                .set_log_filter(rest)
                .and_then(|_| self.set_trace_filter(rest))
                .map(|_| self.show()),
            "set" => self.set_knob(rest).map(|_| self.show()),
            other => Err(anyhow::anyhow!("unknown command {other:?}\n{HELP}")),
        };
        match result {
            Ok(reply) => reply,
            Err(err) => format!("error: {err}\n"),
        }
    }

    fn set_knob(&self, rest: &str) -> anyhow::Result<()> {
        let (name, value) = rest
            .split_once(char::is_whitespace)
            .ok_or_else(|| anyhow::anyhow!("usage: set <knob> <value>"))?;
        let knob = self
            .knobs
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown knob {name:?}"))?;
        (knob.set)(value.trim()).map_err(|e| anyhow::anyhow!(e))?;
        tracing::info!(knob = name, value = value.trim(), "knob changed");
        Ok(())
    }

    fn show(&self) -> String {
        let (log, trace) = self.current.lock().unwrap().clone();
        let mut out = format!("log   = {log}\ntrace = {trace}\n");
        for (name, knob) in self.knobs.lock().unwrap().iter() {
            out.push_str(&format!("{name} = {}\n", (knob.get)()));
        }
        out
    }

    pub(crate) async fn serve(self: Arc<Self>, path: &Path) -> std::io::Result<()> {
        // A stale socket from a previous run would make bind fail.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        tracing::info!(path = %path.display(), "control socket listening");
        loop {
            let (stream, _) = listener.accept().await?;
            let control = self.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let reply = control.execute(&line);
                    if write.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    }
}

const HELP: &str = "\
commands:
  show                    current filters and knobs
  log   <directives>      set LOG_FILTER   (EnvFilter syntax, e.g. warn,store=debug)
  trace <directives>      set TRACE_FILTER
  level <directives>      set both
  set   <knob> <value>    set an application knob
";
