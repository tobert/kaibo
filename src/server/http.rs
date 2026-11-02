//! The loopback HTTP transport (preview): kaibo's MCP surface served as streamable HTTP.
//!
//! stdio stays the default and the reference. This front door exists for hosts whose
//! sandboxes make launching a process hard, and for one long-lived kaibo shared by several
//! agents on one machine. Every request passes four checks, in this order, before rmcp sees
//! it:
//!
//! 1. **No `Origin` header.** MCP clients send none, and a browser sends one on every
//!    cross-origin request, so a page on another site cannot drive kaibo. (A same-origin
//!    request after DNS rebinding carries no `Origin`; the token and check 4 stop that.)
//! 2. **Path `/mcp`.** Everything else is a 404, including the OAuth discovery paths
//!    (`/.well-known/...`) that Codex, OpenCode, and Gemini CLI probe. A 404 there tells
//!    them kaibo speaks bearer tokens, not OAuth.
//! 3. **`Authorization: Bearer <token>`** matching the token file, re-read per request
//!    (`crate::http_token`). A miss is a 401 with `WWW-Authenticate: Bearer`, which is what
//!    makes Claude Code's `headersHelper` and Codex's `http_headers_helper` re-read a
//!    rotated token and retry.
//! 4. **rmcp's own `Host` allowlist** (loopback names plus the bound address): a rebound
//!    name such as `evil.example` is refused even with a token. `tests/http_transport.rs`
//!    pins it.
//!
//! At most [`MAX_CONNECTIONS`] connections are served at once, a client gets 30 seconds
//! to send its headers, and on SIGINT or SIGTERM the server stops accepting, lets open
//! connections finish for up to [`SHUTDOWN_GRACE`], then exits. Each refusal is logged
//! with its reason, never with a header value.
//!
//! The listener binds loopback only (`config::parse_http_listen`). Every token holder is
//! the operator during the preview: sessions, jobs, and `kaibo://config` are shared across
//! connections, as they are across stdio restarts. MCP log notifications are not forwarded
//! over HTTP yet; logs go to stderr (the journal, under a service manager), and `job_wait`
//! still drains its own buffer.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, combinators::BoxBody};
use hyper::{Request, Response, StatusCode, body::Incoming};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
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

/// The checks that run before rmcp, as a pure function of the request head and the token
/// file, so tests can drive them without a socket. `None` means the request may proceed.
pub fn refuse(
    method_path: &str,
    has_origin: bool,
    authorization: Option<&str>,
    token_path: &std::path::Path,
) -> Option<Response<Body>> {
    if has_origin {
        tracing::warn!(
            reason = "origin",
            "refused an HTTP request that carried an Origin header"
        );
        return Some(plain(
            StatusCode::FORBIDDEN,
            "kaibo refused a request with an Origin header: browsers send one and MCP clients \
             do not, so a web page cannot reach this server. Connect from an MCP client.",
        ));
    }
    if method_path != MCP_PATH {
        tracing::info!(reason = "path", path = %method_path, "refused an HTTP request for a path other than /mcp");
        return Some(plain(
            StatusCode::NOT_FOUND,
            "kaibo serves MCP at /mcp only. It uses a bearer token, not OAuth: send \
             `Authorization: Bearer <token>`, and run `kaibo http-token` on this machine to \
             find the token file.",
        ));
    }
    if !crate::http_token::authorizes(token_path, authorization) {
        tracing::warn!(
            reason = "token",
            "refused an HTTP request without the current bearer token"
        );
        let mut resp = plain(
            StatusCode::UNAUTHORIZED,
            "kaibo needs `Authorization: Bearer <token>` with the current token. Run \
             `kaibo http-token --header` on this machine to print the header; a client that \
             runs that command on each connect follows rotation without a restart.",
        );
        resp.headers_mut().insert(
            hyper::header::WWW_AUTHENTICATE,
            hyper::header::HeaderValue::from_static("Bearer realm=\"kaibo\""),
        );
        return Some(resp);
    }
    None
}

/// Bind `listen` and serve until SIGINT or SIGTERM. Logs the bound address, never the token.
pub async fn serve(handler: KaiboHandler, listen: SocketAddr, token_path: PathBuf) -> Result<()> {
    // Install the signal handlers before the socket exists and before the startup log line,
    // so a service manager's SIGTERM right after launch is a clean shutdown, not a kill.
    let signals = Signals::install()?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding the HTTP listener on {listen}"))?;
    let bound = listener.local_addr().context("reading the bound address")?;
    tracing::info!(
        url = %format!("http://{bound}{MCP_PATH}"),
        token_file = %token_path.display(),
        "kaibo MCP server listening on HTTP (preview)"
    );

    let shutdown = CancellationToken::new();
    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    hosts.push(bound.ip().to_string());
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(shutdown.child_token())
        .with_allowed_hosts(hosts)
        .enforce_origin_validation();
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let token_path = Arc::new(token_path);

    let stop = shutdown.clone();
    tokio::spawn(async move {
        signals.wait().await;
        tracing::info!("shutting down the HTTP server");
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
        let accepted: std::io::Result<(tokio::net::TcpStream, SocketAddr)> = tokio::select! {
            accepted = listener.accept() => accepted,
            () = shutdown.cancelled() => break,
        };
        let (stream, _) = match accepted {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "could not accept an HTTP connection");
                continue;
            }
        };
        let service = service.clone();
        let token_path = Arc::clone(&token_path);
        let conn_shutdown = shutdown.clone();
        conns.spawn(async move {
            let _permit = permit;
            let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                let service = service.clone();
                let token_path = Arc::clone(&token_path);
                async move {
                    // Presence, not parse: an Origin that is not valid text still counts.
                    let has_origin = req.headers().contains_key(hyper::header::ORIGIN);
                    let authorization = req
                        .headers()
                        .get(hyper::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok());
                    if let Some(resp) =
                        refuse(req.uri().path(), has_origin, authorization, &token_path)
                    {
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
                tracing::debug!(error = %e, "HTTP connection ended with an error");
            }
        });
    }
    let open = conns.len();
    if open > 0 {
        tracing::info!(open, "waiting for open HTTP connections to finish");
    }
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            open = conns.len(),
            "closing HTTP connections still open after the shutdown grace period"
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
            use tokio::signal::unix::{SignalKind, signal};
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

    fn status(resp: Option<Response<Body>>) -> Option<StatusCode> {
        resp.map(|r| r.status())
    }

    #[test]
    fn the_checks_run_in_order_and_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http-token");
        let token = crate::http_token::ensure(&path).unwrap();
        let good = format!("Bearer {token}");

        assert_eq!(status(refuse("/mcp", false, Some(&good), &path)), None);
        assert_eq!(
            status(refuse("/mcp", true, Some(&good), &path)),
            Some(StatusCode::FORBIDDEN),
            "an Origin header is refused even with the right token"
        );
        assert_eq!(
            status(refuse(
                "/.well-known/oauth-protected-resource",
                false,
                Some(&good),
                &path
            )),
            Some(StatusCode::NOT_FOUND),
            "OAuth discovery finds nothing"
        );
        assert_eq!(
            status(refuse("/mcp", false, None, &path)),
            Some(StatusCode::UNAUTHORIZED)
        );
        let stale = refuse("/mcp", false, Some("Bearer stale"), &path).unwrap();
        assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            stale
                .headers()
                .get(hyper::header::WWW_AUTHENTICATE)
                .unwrap(),
            "Bearer realm=\"kaibo\"",
            "a 401 names the scheme, so header helpers re-read and retry"
        );
    }
}
