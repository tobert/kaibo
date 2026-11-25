//! The stdio bridge: `kaibo serve --connect` forwards one MCP host to a daemon.
//!
//! The host talks stdio to this process, as it does to any kaibo. This process holds no
//! handler, no provider key, and no root. It opens the daemon's Unix socket
//! (`kaibo serve --http`) and copies MCP messages both ways, so the daemon's tools,
//! config, and job state serve a host that cannot open a socket itself.
//!
//! The two ends are rmcp transports whose message types line up: the stdio end receives
//! what a client sends and sends what a server sends, and the socket end does the
//! opposite. The pump therefore moves each message unchanged and needs no knowledge of
//! MCP beyond that. It binds nothing and writes nothing, so the read-only invariants
//! and the `no_bind` and `no_write_path` guards are untouched.
//!
//! The pump ends when the host closes stdin (a normal exit) or when the daemon ends the
//! stream (an error: the host restarts this process, and a restart with
//! `--connect=auto` serves in-process if the daemon is gone).

use std::path::Path;
use std::time::Duration;

use rmcp::service::{RoleClient, RoleServer};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{
    IntoTransport, StreamableHttpClientTransport, Transport, UnixSocketHttpClient,
};

/// The URI the daemon serves MCP on. The host part is unused on a Unix socket.
const URI: &str = "http://localhost/mcp";

/// How long to wait for a daemon to accept a connection before calling it absent.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// How long closing both ends may take. A wedged daemon must not hold the exit.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the pump stopped.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("the daemon closed the connection")]
    DaemonClosed,
    #[error("a message to the daemon failed: {0}")]
    Daemon(String),
    #[error("a message to the host failed: {0}")]
    Host(String),
    #[error("the socket path is not valid UTF-8, which MCP over a socket needs: {0}")]
    Path(String),
}

/// A send in flight, type-erased so one queue holds either direction's futures.
type SendFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

/// How many sends may wait per direction before the loop stops reading from the other
/// end. Small on purpose: this bounds memory, and the host or daemon sees backpressure.
const QUEUE: usize = 64;

/// Whether a daemon of this user accepts connections on `path`. A socket that belongs to
/// another user is refused: the door is the file's owner, and a forwarder must not hand
/// a host's MCP traffic (project reads included) to someone else's process.
pub async fn probe(path: &Path) -> std::io::Result<()> {
    let connect = tokio::net::UnixStream::connect(path);
    let stream = match tokio::time::timeout(PROBE_TIMEOUT, connect).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no answer within 1 second",
            ))
        }
    };
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let mine = unsafe { libc::geteuid() };
    let peer = stream.peer_cred()?.uid();
    if peer != mine {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("the socket's server runs as uid {peer}, not as you (uid {mine})"),
        ));
    }
    Ok(())
}

/// Copy MCP messages between `host` (the stdio side) and `daemon` (the socket side) until
/// one side ends. `Ok(())` means the host closed its input.
///
/// Receiving from each end happens in one loop; sending to each end happens in its own
/// task, fed through an ordered queue. A transport's `send` returns a future that does not
/// borrow the transport, so the loop only creates it, and a long write toward one end
/// never stops the loop from reading the other (a host's cancellation, say, arrives while
/// a large answer is still being written). Each queue runs its sends in order, which the
/// protocol needs: `initialize` before `notifications/initialized`.
///
/// `receive` is raced in a `select!`, so a dropped `receive` future must lose no message.
/// Both rmcp transports used here read from a channel, which has that property.
pub async fn pump<H, D>(mut host: H, mut daemon: D) -> Result<(), BridgeError>
where
    H: Transport<RoleServer>,
    D: Transport<RoleClient>,
{
    let (to_daemon, daemon_queue) = tokio::sync::mpsc::channel::<SendFuture>(QUEUE);
    let (to_host, host_queue) = tokio::sync::mpsc::channel::<SendFuture>(QUEUE);
    let drain = |mut queue: tokio::sync::mpsc::Receiver<SendFuture>| async move {
        while let Some(send) = queue.recv().await {
            send.await?;
        }
        Ok::<(), String>(())
    };
    let mut daemon_writer = tokio::spawn(drain(daemon_queue));
    let mut host_writer = tokio::spawn(drain(host_queue));

    let result = loop {
        tokio::select! {
            from_host = host.receive() => match from_host {
                Some(message) => {
                    let send = daemon.send(message);
                    let send: SendFuture = Box::pin(async move { send.await.map_err(|e| e.to_string()) });
                    if to_daemon.send(send).await.is_err() {
                        break Err(BridgeError::Daemon("the writer stopped".into()));
                    }
                }
                None => break Ok(()),
            },
            from_daemon = daemon.receive() => match from_daemon {
                Some(message) => {
                    let send = host.send(message);
                    let send: SendFuture = Box::pin(async move { send.await.map_err(|e| e.to_string()) });
                    if to_host.send(send).await.is_err() {
                        break Err(BridgeError::Host("the writer stopped".into()));
                    }
                }
                None => break Err(BridgeError::DaemonClosed),
            },
            done = &mut daemon_writer => break Err(match done {
                Ok(Err(e)) => BridgeError::Daemon(e),
                _ => BridgeError::Daemon("the writer stopped".into()),
            }),
            done = &mut host_writer => break Err(match done {
                Ok(Err(e)) => BridgeError::Host(e),
                _ => BridgeError::Host("the writer stopped".into()),
            }),
        }
    };
    // Let queued messages go out (a final answer the host is still waiting for), then
    // close both ends. Closing is bounded: a wedged daemon must not hold the exit.
    drop(to_daemon);
    drop(to_host);
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        let _ = daemon_writer.await;
        let _ = host_writer.await;
        let _ = daemon.close().await;
        let _ = host.close().await;
    })
    .await;
    result
}

/// Forward this process's stdio to the daemon on `path`.
pub async fn run(path: &Path) -> Result<(), BridgeError> {
    let path_str = path
        .to_str()
        .ok_or_else(|| BridgeError::Path(path.display().to_string()))?;
    let daemon = StreamableHttpClientTransport::with_client(
        UnixSocketHttpClient::new(path_str, URI),
        StreamableHttpClientTransportConfig::with_uri(URI),
    );
    let host = IntoTransport::<RoleServer, _, _>::into_transport(rmcp::transport::io::stdio());
    pump(host, daemon).await
}
