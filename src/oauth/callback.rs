//! The sign-in callback: a one-shot HTTP listener on loopback.
//!
//! This is the only socket kaibo binds (`tests/no_bind.rs` pins it). It exists because
//! the providers' OAuth flows return the code to a browser redirect, and the only
//! redirect a local tool can receive is one aimed at itself. It is shaped to be as
//! small as that need:
//!
//! - **Loopback only.** [`bind_loopback`] takes no address from anyone: it binds
//!   `127.0.0.1` (and `::1` beside it, for providers whose redirect says `localhost`),
//!   never a wildcard or a routable interface.
//! - **Only `kaibo login` starts it**, after printing the URL to open. The MCP server
//!   never does, and no tool can reach it.
//! - **It ends at the first request that carries this attempt's `state`**, success or
//!   provider-reported failure, and at [`CALLBACK_TIMEOUT`] otherwise. A request on
//!   any other path, or with any other `state`, gets a short refusal and the listener
//!   keeps waiting, so a stray local request cannot end or hijack the sign-in.
//! - It reads at most [`MAX_REQUEST_HEAD`] bytes of each request and serves nothing
//!   but a one-line HTML page.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{Attempt, CallbackCode, CallbackError, Profile};

/// How long `kaibo login` waits for the browser to come back.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// The most of one request kaibo reads: the request line and headers of a redirect.
pub const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// How many connections the callback reads at once.
const MAX_CONCURRENT_READS: usize = 64;

/// How long one connection may take to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The bound callback socket(s), ready to receive one redirect.
pub struct CallbackListener {
    listeners: Vec<TcpListener>,
    port: u16,
}

impl CallbackListener {
    /// The port the redirect URI must name.
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Bind the callback on loopback: `preferred` when it is free, else a port the OS
/// picks. Also binds `::1` on the same port when it can, so a browser that resolves
/// `localhost` to IPv6 still arrives; IPv4 is required, IPv6 best-effort.
pub async fn bind_loopback(preferred: u16) -> Result<CallbackListener> {
    let v4 = match bind(SocketAddr::from((Ipv4Addr::LOCALHOST, preferred))).await {
        Ok(l) => l,
        Err(_) => bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .context("binding the sign-in callback on 127.0.0.1")?,
    };
    let port = v4.local_addr()?.port();
    let mut listeners = vec![v4];
    if let Ok(v6) = bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await {
        listeners.push(v6);
    }
    Ok(CallbackListener { listeners, port })
}

/// The one bind site. Private, and only ever handed a loopback address by
/// [`bind_loopback`].
async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    debug_assert!(addr.ip().is_loopback(), "the callback binds loopback only");
    TcpListener::bind(addr).await // loopback-bind: the one-shot sign-in callback
}

impl CallbackListener {
    /// Wait for the redirect that carries `attempt`'s state, answer the browser, and
    /// return the checked code. Ends with an error at `timeout`, or when the provider
    /// reports a failure for this attempt.
    pub async fn receive(
        self,
        profile: &Profile,
        attempt: &Attempt,
        known_client_id: Option<&str>,
        timeout: Duration,
    ) -> Result<CallbackCode> {
        // Each connection's request is read in its own task, so a local process that
        // opens connections and sends nothing cannot hold the browser's redirect behind
        // it; requests are handled in the order they finish arriving.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(TcpStream, String)>(16);
        // At most this many connections are read at once; past it, a new one is closed
        // unread, which a browser retries.
        let reading = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_READS));
        let wait = async {
            loop {
                let (mut stream, target) = tokio::select! {
                    accepted = accept_any(&self.listeners) => {
                        let (mut stream, _) = accepted?;
                        let Ok(permit) = reading.clone().try_acquire_owned() else {
                            continue;
                        };
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Some(target) = read_request_target(&mut stream).await {
                                let _ = tx.send((stream, target)).await;
                            }
                        });
                        continue;
                    }
                    Some(read) = rx.recv() => read,
                };
                let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
                // A provider whose state rides in the path (`Profile::state_in_path`):
                // that segment is the state, and any `state` in the query is ignored.
                let (path, query) = match path
                    .strip_prefix(profile.callback_path)
                    .and_then(|rest| rest.strip_prefix('/'))
                {
                    Some(segment) if profile.state_in_path() => {
                        let kept: String = url::form_urlencoded::Serializer::new(String::new())
                            .extend_pairs(
                                url::form_urlencoded::parse(query.as_bytes())
                                    .filter(|(k, _)| k != "state"),
                            )
                            .append_pair("state", segment)
                            .finish();
                        (profile.callback_path, kept)
                    }
                    _ => (path, query.to_string()),
                };
                if path != profile.callback_path {
                    respond(
                        &mut stream,
                        "404 Not Found",
                        "Not the kaibo sign-in callback.",
                    )
                    .await;
                    continue;
                }
                match profile.check_callback(attempt, &query, known_client_id) {
                    Ok(code) => {
                        respond(
                            &mut stream,
                            "200 OK",
                            "kaibo received the sign-in. Close this tab; the terminal shows whether it completed.",
                        )
                        .await;
                        return Ok(code);
                    }
                    Err(e) if e.downcast_ref() == Some(&CallbackError::StateMismatch) => {
                        // Not this attempt's redirect: refuse it and keep waiting.
                        respond(
                            &mut stream,
                            "400 Bad Request",
                            "This sign-in link is stale or not from kaibo.",
                        )
                        .await;
                        continue;
                    }
                    Err(e) => {
                        respond(
                            &mut stream,
                            "400 Bad Request",
                            "The sign-in did not complete. See the terminal.",
                        )
                        .await;
                        return Err(e);
                    }
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| {
            anyhow!(
                "no sign-in arrived within {} seconds — run `kaibo login` again and open the \
                 printed URL in a browser on this machine (over SSH, forward port {} first: \
                 `ssh -L {}:127.0.0.1:{} host`)",
                timeout.as_secs(),
                self.port,
                self.port,
                self.port
            )
        })?
    }
}

async fn accept_any(listeners: &[TcpListener]) -> Result<(TcpStream, SocketAddr)> {
    // One or two listeners: IPv4, and IPv6 when it bound.
    match listeners {
        [v4] => Ok(v4.accept().await?),
        [v4, v6, ..] => tokio::select! {
            r = v4.accept() => Ok(r?),
            r = v6.accept() => Ok(r?),
        },
        [] => Err(anyhow!("the sign-in callback has no listener")),
    }
}

/// The request target of a `GET`, or `None` for anything else or a malformed request.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let read = async {
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST_HEAD {
                return Some(());
            }
        }
    };
    tokio::time::timeout(READ_TIMEOUT, read).await.ok()??;
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    (method == "GET").then(|| target.to_string())
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>kaibo</title><p style=\"font-family:sans-serif\">{message}</p>"
    );
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await; // callback-reply: blessed sign-in callback answer (AGENTS.md)
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::CHATGPT;

    async fn get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn binds_loopback_only() {
        let l = bind_loopback(0).await.unwrap();
        for listener in &l.listeners {
            assert!(listener.local_addr().unwrap().ip().is_loopback());
        }
    }

    #[tokio::test]
    async fn ignores_strays_then_returns_the_matching_callback() {
        let l = bind_loopback(0).await.unwrap();
        let port = l.port();
        let attempt = CHATGPT.new_attempt(port);
        let state = attempt.state.clone();
        let waiter = tokio::spawn(async move {
            l.receive(&CHATGPT, &attempt, None, Duration::from_secs(10))
                .await
        });
        assert!(get(port, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        assert!(get(port, "/auth/callback?code=x&state=wrong")
            .await
            .starts_with("HTTP/1.1 400"));
        let ok = get(
            port,
            &format!("/auth/callback?code=the-code&state={state}&client_id=oaiapp_1"),
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let code = waiter.await.unwrap().unwrap();
        assert_eq!(code.code, "the-code");
        assert_eq!(code.client_id.as_deref(), Some("oaiapp_1"));
        // One-shot: the listener is gone once the sign-in returned.
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    }

    #[tokio::test]
    async fn an_idle_connection_does_not_hold_up_the_redirect() {
        let l = bind_loopback(0).await.unwrap();
        let port = l.port();
        let attempt = CHATGPT.new_attempt(port);
        let state = attempt.state.clone();
        let waiter = tokio::spawn(async move {
            l.receive(&CHATGPT, &attempt, None, Duration::from_secs(5))
                .await
        });
        // Connections that send nothing, held open past the redirect.
        let mut idle = Vec::new();
        for _ in 0..4 {
            idle.push(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        }
        let started = std::time::Instant::now();
        let ok = get(port, &format!("/auth/callback?code=c&state={state}")).await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the redirect waited behind idle sockets"
        );
        assert_eq!(waiter.await.unwrap().unwrap().code, "c");
        drop(idle);
    }

    #[tokio::test]
    async fn a_denial_for_this_attempt_ends_the_wait() {
        let l = bind_loopback(0).await.unwrap();
        let port = l.port();
        let attempt = CHATGPT.new_attempt(port);
        let state = attempt.state.clone();
        let waiter = tokio::spawn(async move {
            l.receive(&CHATGPT, &attempt, None, Duration::from_secs(10))
                .await
        });
        get(
            port,
            &format!("/auth/callback?error=access_denied&state={state}"),
        )
        .await;
        let err = waiter.await.unwrap().unwrap_err();
        assert!(
            matches!(err.downcast_ref(), Some(CallbackError::Denied { .. })),
            "{err}"
        );
    }

    #[tokio::test]
    async fn times_out_with_a_next_step() {
        let l = bind_loopback(0).await.unwrap();
        let attempt = CHATGPT.new_attempt(l.port());
        let err = l
            .receive(&CHATGPT, &attempt, None, Duration::from_millis(50))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("kaibo login") && err.contains("ssh -L"),
            "{err}"
        );
    }
}
