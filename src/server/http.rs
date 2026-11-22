//! The Unix-socket HTTP transport (preview): kaibo's MCP surface served as streamable HTTP
//! over a socket file, with no network address.
//!
//! stdio stays the default and the reference. This front door exists for hosts whose
//! sandboxes make launching a process hard, and for one long-lived kaibo shared by several
//! agents on one machine. The door is the socket file (`crate::http_socket`): mode `0600`
//! in a per-user directory, so only the user who started kaibo can open it. There is no
//! token to leak, rotate, or forget, and a browser cannot open a Unix socket.
//!
//! Every accepted connection passes two checks before rmcp sees a byte, and each request
//! passes one more:
//!
//! 1. **Peer uid.** The kernel reports the connecting process's uid; one that is not
//!    kaibo's own uid is dropped. This backs up the file mode for the instant between
//!    `bind` and `chmod`, and for a socket path in a directory someone else controls.
//! 2. **Path `/mcp`.** Everything else is a 404, including the OAuth discovery paths
//!    (`/.well-known/...`) that Codex and OpenCode probe. A 404 there tells them kaibo
//!    needs no sign-in.
//!
//! rmcp's `Host` and `Origin` allowlists are off: both defend a TCP listener against a
//! browser, and a Unix socket has no browser to defend against.
//!
//! At most [`MAX_CONNECTIONS`] connections are served at once, a client gets 30 seconds
//! to send its headers, and on SIGINT or SIGTERM the server stops accepting, lets open
//! connections finish for up to [`SHUTDOWN_GRACE`], removes the socket file, and exits.
//! Each refusal is logged with its reason.
//!
//! Every client that can open the socket is the operator: sessions, jobs, and
//! `kaibo://config` are shared across connections, as they are across stdio restarts. MCP
//! log notifications are not forwarded over this transport yet; logs go to stderr (the
//! journal, under a service manager), and `job_wait` still drains its own buffer.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt as _, Full};
use hyper::{body::Incoming, Request, Response, StatusCode};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;

use super::KaiboHandler;

/// The one path kaibo serves.
pub const MCP_PATH: &str = "/mcp";

/// How many connections are served at once. One host holds a few; a local process that
/// opens thousands waits in the accept queue instead of starving the runtime.
pub const MAX_CONNECTIONS: usize = 64;

/// How long shutdown waits for open connections to finish.
pub const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

type Body = BoxBody<Bytes, Infallible>;

fn plain(status: StatusCode, text: &str) -> Response<Body> {
    let mut resp = Response::new(Full::new(Bytes::from(format!("{text}\n"))).boxed());
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// Is the connecting process ours? The kernel reports `peer`; `owner` is kaibo's own uid.
pub fn peer_is_owner(peer: u32, owner: u32) -> bool {
    peer == owner
}

/// The per-request check that runs before rmcp, as a pure function so tests can drive it
/// without a socket. `None` means the request may proceed.
pub fn refuse(path: &str) -> Option<Response<Body>> {
    if path != MCP_PATH {
        tracing::info!(reason = "path", path = %path, "refused a request for a path other than /mcp");
        return Some(plain(
            StatusCode::NOT_FOUND,
            "kaibo serves MCP at /mcp only, and needs no sign-in: connect to the socket \
             and POST to /mcp.",
        ));
    }
    None
}

/// Bind the socket at `path` and serve until SIGINT or SIGTERM. The caller has already
/// checked the path against the allowed set.
pub async fn serve(handler: KaiboHandler, path: PathBuf) -> Result<()> {
    // Install the signal handlers before the socket exists and before the startup log line,
    // so a service manager's SIGTERM right after launch is a clean shutdown, not a kill.
    let signals = Signals::install()?;
    let socket = crate::http_socket::bind(&path)?;
    let owner = socket.owner_uid();
    tracing::info!(
        socket = %path.display(),
        url = %format!("http://localhost{MCP_PATH}"),
        "kaibo MCP server listening on a Unix socket (preview)"
    );

    let shutdown = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(shutdown.child_token())
        .disable_allowed_hosts()
        .disable_allowed_origins();
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let stop = shutdown.clone();
    tokio::spawn(async move {
        signals.wait().await;
        tracing::info!("shutting down the socket server");
        stop.cancel();
    });

    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut conns = tokio::task::JoinSet::new();
    loop {
        // Reap finished connections so the set does not grow for the life of the server.
        while conns.try_join_next().is_some() {}
        // Wait for a free slot before accepting, so excess clients queue in the kernel's
        // backlog rather than in kaibo's memory.
        let permit = tokio::select! {
            permit = Arc::clone(&permits).acquire_owned() => {
                permit.expect("the connection semaphore is never closed")
            }
            () = shutdown.cancelled() => break,
        };
        let accepted = tokio::select! {
            accepted = socket.listener.accept() => accepted,
            () = shutdown.cancelled() => break,
        };
        let (stream, _) = match accepted {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "could not accept a connection");
                continue;
            }
        };
        // The kernel reports who is on the other end. Anyone but our own user is dropped
        // before a byte is read.
        match stream.peer_cred() {
            Ok(cred) if peer_is_owner(cred.uid(), owner) => {}
            Ok(cred) => {
                tracing::warn!(
                    reason = "peer-uid",
                    uid = cred.uid(),
                    "refused a connection from another user"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(reason = "peer-uid", error = %e, "refused a connection whose uid could not be read");
                continue;
            }
        }
        let service = service.clone();
        let conn_shutdown = shutdown.clone();
        conns.spawn(async move {
            let _permit = permit;
            let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                let service = service.clone();
                async move {
                    if let Some(resp) = refuse(req.uri().path()) {
                        return Ok::<_, Infallible>(resp);
                    }
                    Ok(service.handle(req).await)
                }
            });
            // The timer is what turns on hyper's 30-second header-read timeout, so a client
            // that opens a socket and sends nothing cannot hold it forever.
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .serve_connection(hyper_util::rt::TokioIo::new(stream), svc);
            tokio::pin!(conn);
            let result = tokio::select! {
                result = conn.as_mut() => result,
                () = conn_shutdown.cancelled() => {
                    // graceful_shutdown only starts the close; the connection finishes its
                    // in-flight response once it is polled to completion.
                    conn.as_mut().graceful_shutdown();
                    conn.as_mut().await
                }
            };
            if let Err(e) = result {
                tracing::debug!(error = %e, "connection ended with an error");
            }
        });
    }
    // Close the door first: while open connections drain, a new client's connect fails
    // instead of queuing on a listener nobody is accepting from.
    drop(socket);
    let open = conns.len();
    if open > 0 {
        tracing::info!(open, "waiting for open connections to finish");
    }
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            open = conns.len(),
            "closing connections still open after the shutdown grace period"
        );
        conns.shutdown().await;
    }
    Ok(())
}

/// SIGINT and SIGTERM, installed up front.
struct Signals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
}

impl Signals {
    fn install() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                term: signal(SignalKind::terminate()).context("installing a SIGTERM handler")?,
                int: signal(SignalKind::interrupt()).context("installing a SIGINT handler")?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    async fn wait(mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => {}
                _ = self.int.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = &mut self;
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only kaibo's own uid gets in. The accept loop runs this on every connection's
    /// reported uid; a real second-user connection needs a second account, so the
    /// decision is tested here and the wiring by reading `serve`.
    #[test]
    fn only_the_owners_uid_is_admitted() {
        assert!(peer_is_owner(1000, 1000));
        assert!(!peer_is_owner(1001, 1000));
        assert!(!peer_is_owner(0, 1000), "root is another user too");
    }

    #[test]
    fn only_the_mcp_path_passes() {
        assert!(refuse("/mcp").is_none());
        for path in [
            "/",
            "/mcp/",
            "//mcp",
            "/%6dcp",
            "/.well-known/oauth-protected-resource",
        ] {
            let resp = refuse(path).unwrap_or_else(|| panic!("{path} must be refused"));
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }
}
