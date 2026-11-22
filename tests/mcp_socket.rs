//! The MCP contract over the Unix-socket transport: the same suite `tests/mcp_stdio.rs`
//! runs over stdio (`tests/common/mcp_suite.rs`: handshake, advertised surface, reads,
//! the read-only refusals, resources, gating, annotations, the `alwaysLoad` pin), driven
//! by rmcp's own Unix-socket client against the real `kaibo serve --http` binary.
//!
//! Below the shared suite are the checks only a socket has: several clients at once, a
//! client that leaves and one that arrives, a large resource, a server that outlives its
//! clients, and a tool call on a connection that stays open.
//!
//! Same blank-environment rule as `tests/mcp_stdio.rs`. The socket lives under a short
//! `/tmp` path (`--socket`), because a socket path must fit in 107 bytes.
#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    UnixSocketHttpClient,
};
use rmcp::ServiceExt as _;
use tempfile::TempDir;

#[path = "common/mcp_suite.rs"]
mod suite;
use suite::{bounded, ChildEnv, Connected};

/// The server process and the directory holding its socket. Dropping kills the child.
struct SocketGuard {
    child: Child,
    _dir: TempDir,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const URI: &str = "http://localhost/mcp";

/// An rmcp client over the socket at `path`.
async fn client_for(path: &std::path::Path) -> RunningService<RoleClient, ()> {
    let http = UnixSocketHttpClient::new(path.to_str().unwrap(), URI);
    let transport = StreamableHttpClientTransport::with_client(
        http,
        StreamableHttpClientTransportConfig::with_uri(URI),
    );
    bounded("the MCP handshake", ().serve(transport))
        .await
        .expect("kaibo should complete `initialize` over the socket")
}

/// Start `kaibo serve --http` on a short socket path and connect one client.
async fn connect(env: &ChildEnv, args: &[&str]) -> Connected {
    let dir = tempfile::Builder::new()
        .prefix("kb")
        .tempdir_in("/tmp")
        .expect("a short directory for the socket");
    let socket = dir.path().join("s");
    let child = Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .env_clear()
        .env("HOME", &env.home)
        .env("XDG_CONFIG_HOME", &env.config_home)
        .env("XDG_STATE_HOME", &env.state_home)
        .env("XDG_DATA_HOME", &env.data_home)
        .env("RUST_LOG", "error")
        .args(["serve", "--http", "--socket"])
        .arg(&socket)
        .arg("--root")
        .arg(&env.root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn the kaibo binary");
    let guard = SocketGuard { child, _dir: dir };
    // The socket appears at bind; a connect succeeding means the server accepts.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the server never accepted on {}",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let client = client_for(&socket).await;
    // The path rides along so a test can open more clients on the same socket.
    Connected {
        client,
        guard: Box::new((guard, socket)),
    }
}

/// The socket path of a started server, recovered from its guard.
fn socket_of(server: &suite::Server) -> std::path::PathBuf {
    server
        .guard_as::<(SocketGuard, std::path::PathBuf)>()
        .1
        .clone()
}

/// Two clients on one server at once each get their own session and both work.
#[tokio::test]
async fn two_clients_share_one_server() {
    let server = suite::Server::start().await;
    let second = client_for(&socket_of(&server)).await;

    let a = server.run_kaish("cat -n Cargo.toml").await;
    assert!(a.contains(suite::PROJECT_MARKER), "{a}");

    let params = CallToolRequestParams::new("run_kaish").with_arguments(
        serde_json::json!({
            "script": "cat -n Cargo.toml",
            "path": server.root().to_string_lossy(),
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let b = bounded("second client tools/call", second.call_tool(params))
        .await
        .expect("the second client's call succeeds");
    let text: String = b
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    assert!(text.contains(suite::PROJECT_MARKER), "{text}");

    let _ = second.cancel().await;
    server.shutdown().await;
}

/// The server outlives its clients: one leaves, a new one connects and works. This is
/// the point of a long-lived shared server, and a stdio server cannot do it.
#[tokio::test]
async fn the_server_outlives_a_client_and_serves_the_next() {
    let server = suite::Server::start().await;
    let socket = socket_of(&server);

    let first = client_for(&socket).await;
    let mut names: Vec<_> = bounded("first list", first.list_all_tools())
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    assert!(names.contains(&"run_kaish".to_string()));
    bounded("first client closes", first.cancel()).await.ok();

    let second = client_for(&socket).await;
    let again = bounded("second list", second.list_all_tools())
        .await
        .unwrap();
    assert_eq!(
        again.len(),
        names.len(),
        "the new client sees the same surface"
    );
    let _ = second.cancel().await;
    server.shutdown().await;
}

/// Many requests in flight on one connection set all come back, none crossed.
#[tokio::test]
async fn concurrent_calls_on_one_client_return_their_own_results() {
    let server = suite::Server::start().await;
    let scripts: Vec<String> = (0..8).map(|i| format!("echo marker-{i}")).collect();
    let results = futures::future::join_all(scripts.iter().map(|s| server.run_kaish(s))).await;
    for (i, out) in results.iter().enumerate() {
        assert!(out.contains(&format!("marker-{i}")), "call {i} got: {out}");
    }
    server.shutdown().await;
}

/// The longest embedded resource crosses the socket whole.
#[tokio::test]
async fn a_large_resource_crosses_the_socket_whole() {
    let server = suite::Server::start().await;
    let guide = server.read_resource("kaibo://config/guide").await;
    assert_eq!(guide, include_str!("../docs/config.md"));
    server.shutdown().await;
}

/// The guide for this transport is served over it.
#[tokio::test]
async fn the_socket_guide_is_a_resource_served_over_the_socket() {
    let server = suite::Server::start().await;
    let guide = server.read_resource("kaibo://config/http").await;
    assert_eq!(guide, include_str!("../docs/http.md"));
    assert!(guide.contains("Unix socket"), "{guide}");
    server.shutdown().await;
}

/// A call that fails inside kaibo comes back as an MCP error over the socket, and the
/// connection stays usable afterwards.
#[tokio::test]
async fn a_refused_call_leaves_the_connection_usable() {
    let server = suite::Server::start().await;
    let params = CallToolRequestParams::new("no_such_tool");
    let err = bounded("unknown tool", server.client.call_tool(params))
        .await
        .expect_err("an unknown tool is refused");
    assert!(err.to_string().contains("not found"), "{err}");
    let out = server.run_kaish("cat -n Cargo.toml").await;
    assert!(out.contains(suite::PROJECT_MARKER), "{out}");
    server.shutdown().await;
}
