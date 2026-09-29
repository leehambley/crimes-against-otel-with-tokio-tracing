//! Client for a service's control socket.
//!
//! ```text
//! ctl [-s SOCKET] show
//! ctl [-s SOCKET] log warn,store=debug
//! ctl [-s SOCKET] set chaos.failure_pct 20
//! ```
//!
//! SOCKET defaults to `$CONTROL_SOCKET`.

use anyhow::Context as _;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let socket = if args.first().is_some_and(|a| a == "-s") {
        args.remove(0);
        Some(args.remove(0))
    } else {
        std::env::var("CONTROL_SOCKET").ok()
    }
    .context("no socket: pass -s PATH or set CONTROL_SOCKET")?;
    let command = if args.is_empty() {
        "help".to_owned()
    } else {
        args.join(" ")
    };

    let mut stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connecting to {socket}"))?;
    stream.write_all(format!("{command}\n").as_bytes()).await?;
    stream.shutdown().await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    print!("{reply}");
    if reply.starts_with("error:") {
        std::process::exit(1);
    }
    Ok(())
}
