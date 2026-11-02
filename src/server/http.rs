//! The loopback HTTP transport (preview): kaibo's MCP surface served as streamable HTTP.
//!
//! stdio stays the default and the reference. This front door exists for hosts whose
//! sandboxes make launching a process hard, and for one long-lived kaibo shared by several
//! agents on one machine. Every request passes four checks, in this order, before rmcp sees
//! it:
//!
//! 1. **No `Origin` header.** MCP clients send none; a browser always does. A web page
//!    that found the port cannot get past this, whatever it guesses about the token.
//! 2. **Path `/mcp`.** Everything else is a 404, including the OAuth discovery paths
//!    (`/.well-known/...`) that Codex, OpenCode, and Gemini CLI probe. A 404 there tells
//!    them kaibo speaks bearer tokens, not OAuth.
//! 3. **`Authorization: Bearer <token>`** matching the token file, re-read per request
//!    (`crate::http_token`). A miss is a 401 with `WWW-Authenticate: Bearer`, which is what
//!    makes Claude Code's `headersHelper` and Codex's `http_headers_helper` re-read a
//!    rotated token and retry.
//! 4. **rmcp's own `Host` allowlist** (loopback names plus the bound address), which stops
//!    DNS rebinding.
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
    origin: Option<&str>,
    authorization: Option<&str>,
    token_path: &std::path::Path,
) -> Option<Response<Body>> {
    if origin.is_some() {
        return Some(plain(
            StatusCode::FORBIDDEN,
            "kaibo refused a request with an Origin header: browsers send one and MCP clients \
             do not, so a web page cannot reach this server. Connect from an MCP client.",
        ));
    }
    if method_path != MCP_PATH {
        return Some(plain(
            StatusCode::NOT_FOUND,
            "kaibo serves MCP at /mcp only. It uses a bearer token, not OAuth: send \
             `Authorization: Bearer <token>`, and run `kaibo http-token` on this machine to \
             find the token file.",
        ));
    }
    if !crate::http_token::authorizes(token_path, authorization) {
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
        wait_for_signal().await;
        tracing::info!("shutting down the HTTP server");
        stop.cancel();
    });

    loop {
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
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                let service = service.clone();
                let token_path = Arc::clone(&token_path);
                async move {
                    let header = |name| {
                        req.headers()
                            .get(name)
                            .and_then(|v: &hyper::header::HeaderValue| v.to_str().ok())
                    };
                    if let Some(resp) = refuse(
                        req.uri().path(),
                        header(hyper::header::ORIGIN),
                        header(hyper::header::AUTHORIZATION),
                        &token_path,
                    ) {
                        return Ok::<_, Infallible>(resp);
                    }
                    Ok(service.handle(req).await)
                }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), svc);
            tokio::pin!(conn);
            tokio::select! {
                result = conn.as_mut() => {
                    if let Err(e) = result {
                        tracing::debug!(error = %e, "HTTP connection ended with an error");
                    }
                }
                () = conn_shutdown.cancelled() => conn.as_mut().graceful_shutdown(),
            }
        });
    }
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install a SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
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

        assert_eq!(status(refuse("/mcp", None, Some(&good), &path)), None);
        assert_eq!(
            status(refuse(
                "/mcp",
                Some("https://evil.example"),
                Some(&good),
                &path
            )),
            Some(StatusCode::FORBIDDEN),
            "an Origin header is refused even with the right token"
        );
        assert_eq!(
            status(refuse(
                "/.well-known/oauth-protected-resource",
                None,
                Some(&good),
                &path
            )),
            Some(StatusCode::NOT_FOUND),
            "OAuth discovery finds nothing"
        );
        assert_eq!(
            status(refuse("/mcp", None, None, &path)),
            Some(StatusCode::UNAUTHORIZED)
        );
        let stale = refuse("/mcp", None, Some("Bearer stale"), &path).unwrap();
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
