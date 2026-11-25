//! The stdio bridge (`kaibo serve --connect`): the same MCP contract the other two
//! transports pass, reached through a forwarding process that holds no handler.
//!
//! Each test starts a real daemon (`kaibo serve --http` on a short `/tmp` socket) and a
//! real bridge (`kaibo serve --connect=required --socket …`) that rmcp's child-process
//! client drives over stdio, so the host's whole path is covered: stdio, the bridge's
//! pump, the socket, the daemon. The shared suite in `tests/common/mcp_suite.rs` runs
//! over it unchanged. The daemon gets the server flags (`--root`, `--no-<tool>`); the
//! bridge gets none, because the daemon's root and gates are what the host sees.
//!
//! Below the suite are the checks only a bridge has: a daemon that is absent, a daemon
//! that dies mid-session, a host that leaves, and two hosts on one daemon.
//!
//! Same blank-environment rule as `tests/mcp_stdio.rs`.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::service::ServiceExt;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use tempfile::TempDir;

#[path = "common/mcp_suite.rs"]
mod suite;
use suite::{bounded, ChildEnv, Connected};

/// The daemon process and the directory holding its socket. Dropping kills the daemon.
struct Daemon {
    child: Child,
    socket: PathBuf,
    _dir: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn base(env: &ChildEnv) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaibo"));
    cmd.env_clear()
        .env("HOME", &env.home)
        .env("XDG_CONFIG_HOME", &env.config_home)
        .env("XDG_STATE_HOME", &env.state_home)
        .env("XDG_DATA_HOME", &env.data_home)
        .env("RUST_LOG", "error");
    cmd
}

fn short_socket_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("kb")
        .tempdir_in("/tmp")
        .expect("a short directory for the socket")
}

/// Start the daemon and wait until it accepts.
async fn start_daemon(env: &ChildEnv, args: &[&str]) -> Daemon {
    let dir = short_socket_dir();
    let socket = dir.path().join("s");
    let child = base(env)
        .args(["serve", "--http", "--socket"])
        .arg(&socket)
        .arg("--root")
        .arg(&env.root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn the daemon");
    let daemon = Daemon {
        child,
        socket: socket.clone(),
        _dir: dir,
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never accepted on {}",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
}

/// An rmcp client whose server is a bridge process forwarding to `socket`.
async fn bridge_client(
    env: &ChildEnv,
    socket: &Path,
) -> rmcp::service::RunningService<rmcp::service::RoleClient, ()> {
    let (home, config_home, state_home, data_home) = (
        env.home.clone(),
        env.config_home.clone(),
        env.state_home.clone(),
        env.data_home.clone(),
    );
    let socket = socket.to_path_buf();
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_kaibo")).configure(|cmd| {
            cmd.env_clear()
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &config_home)
                .env("XDG_STATE_HOME", &state_home)
                .env("XDG_DATA_HOME", &data_home)
                .env("RUST_LOG", "error")
                .args(["serve", "--connect=required", "--socket"])
                .arg(&socket);
        }),
    )
    .expect("spawn the bridge");
    bounded("the MCP handshake through the bridge", ().serve(transport))
        .await
        .expect("the bridge should complete `initialize` against the daemon")
}

/// Called by the shared suite: a daemon started with the server flags, and a host
/// connected to it through a bridge.
async fn connect(env: &ChildEnv, args: &[&str]) -> Connected {
    let daemon = start_daemon(env, args).await;
    let client = bridge_client(env, &daemon.socket).await;
    Connected {
        client,
        guard: Box::new(daemon),
    }
}

fn daemon_of(server: &suite::Server) -> &Daemon {
    server.guard_as::<Daemon>()
}

/// A host's raw stdio, for tests that need the bridge's exit status.
struct RawBridge {
    child: Child,
    lines: BufReader<std::process::ChildStdout>,
}

impl RawBridge {
    fn spawn(env: &ChildEnv, args: &[&str]) -> Self {
        let mut child = base(env)
            .args(["serve"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the bridge");
        let lines = BufReader::new(child.stdout.take().unwrap());
        Self { child, lines }
    }

    fn send(&mut self, json: serde_json::Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{json}").unwrap();
        stdin.flush().unwrap();
    }

    fn read(&mut self) -> Option<serde_json::Value> {
        let mut line = String::new();
        let n = self.lines.read_line(&mut line).ok()?;
        (n > 0).then(|| serde_json::from_str(&line).expect("one JSON message per line"))
    }

    fn initialize(&mut self) -> serde_json::Value {
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "raw", "version": "0"}
            }
        }));
        let reply = self.read().expect("an initialize reply");
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }));
        reply
    }

    /// Wait for the bridge to exit, failing the test instead of hanging it.
    async fn exit_status(&mut self) -> std::process::ExitStatus {
        bounded("the bridge exits", async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            std::io::Read::read_to_string(&mut e, &mut text).ok();
        }
        text
    }
}

impl Drop for RawBridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn env_for_test() -> (TempDir, TempDir, ChildEnv) {
    let xdg = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    std::fs::write(
        project.path().join("Cargo.toml"),
        format!("[package]\nname = \"{}\"\n", suite::PROJECT_MARKER),
    )
    .unwrap();
    let dirs: Vec<PathBuf> = ["config", "state", "data", "home"]
        .iter()
        .map(|d| xdg.path().join(d))
        .collect();
    for d in &dirs {
        std::fs::create_dir_all(d).unwrap();
    }
    let env = ChildEnv {
        config_home: dirs[0].clone(),
        state_home: dirs[1].clone(),
        data_home: dirs[2].clone(),
        home: dirs[3].clone(),
        root: project.path().to_path_buf(),
    };
    (xdg, project, env)
}

/// `required` with no daemon refuses to start, exits 3, and says how to start one.
#[tokio::test]
async fn required_without_a_daemon_refuses_and_names_the_fix() {
    let (_x, _p, env) = env_for_test();
    let dir = short_socket_dir();
    let mut bridge = RawBridge::spawn(
        &env,
        &[
            "--connect=required",
            "--socket",
            dir.path().join("none").to_str().unwrap(),
        ],
    );
    let status = bridge.exit_status().await;
    assert_eq!(status.code(), Some(3), "stderr: {}", bridge.stderr());
    let stderr = bridge.stderr();
    assert!(stderr.contains("kaibo serve --http"), "{stderr}");
    assert!(stderr.contains("--connect=auto"), "{stderr}");
}

/// `auto` with no daemon serves in-process, so the host still works.
#[tokio::test]
async fn auto_without_a_daemon_serves_in_process() {
    let (_x, _p, env) = env_for_test();
    let dir = short_socket_dir();
    let socket = dir.path().join("none");
    let mut bridge = RawBridge::spawn(
        &env,
        &[
            "--connect",
            "--socket",
            socket.to_str().unwrap(),
            "--root",
            env.root.to_str().unwrap(),
        ],
    );
    let reply = bridge.initialize();
    assert_eq!(reply["result"]["serverInfo"]["name"], "kaibo", "{reply}");
}

/// The host closing stdin ends the bridge cleanly with status 0, and the daemon stays up.
#[tokio::test]
async fn a_host_that_leaves_ends_the_bridge_with_status_zero() {
    let (_x, _p, env) = env_for_test();
    let daemon = start_daemon(&env, &[]).await;
    let mut bridge = RawBridge::spawn(
        &env,
        &[
            "--connect=required",
            "--socket",
            daemon.socket.to_str().unwrap(),
        ],
    );
    let reply = bridge.initialize();
    assert_eq!(reply["result"]["serverInfo"]["name"], "kaibo", "{reply}");
    drop(bridge.child.stdin.take());
    let status = bridge.exit_status().await;
    assert_eq!(status.code(), Some(0), "stderr: {}", bridge.stderr());
    assert!(
        UnixStream::connect(&daemon.socket).is_ok(),
        "the daemon outlives its host"
    );
}

/// A daemon that dies mid-session ends the bridge with a failure status, so the host
/// restarts it (and a restart under `auto` serves in-process).
#[tokio::test]
async fn a_daemon_that_dies_ends_the_bridge_with_a_failure() {
    let (_x, _p, env) = env_for_test();
    let mut daemon = start_daemon(&env, &[]).await;
    let mut bridge = RawBridge::spawn(
        &env,
        &[
            "--connect=required",
            "--socket",
            daemon.socket.to_str().unwrap(),
        ],
    );
    bridge.initialize();
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    // The host asks for something; the dead daemon cannot answer.
    bridge.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let status = bounded("the bridge exits after its daemon dies", async {
        loop {
            if let Some(s) = bridge.child.try_wait().unwrap() {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert_eq!(status.code(), Some(1), "stderr: {}", bridge.stderr());
    assert!(
        bridge.stderr().contains("ended the session"),
        "names what happened"
    );
}

/// Two hosts, two bridges, one daemon: both work, and the daemon's gating shows through.
#[tokio::test]
async fn two_hosts_share_one_daemon() {
    let (_x, _p, env) = env_for_test();
    let daemon = start_daemon(&env, &["--no-oneshot"]).await;
    let a = bridge_client(&env, &daemon.socket).await;
    let b = bridge_client(&env, &daemon.socket).await;
    for client in [&a, &b] {
        let names: Vec<String> = bounded("list tools", client.list_all_tools())
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(names.contains(&"run_kaish".to_string()), "{names:?}");
        assert!(
            !names.contains(&"oneshot".to_string()),
            "the daemon's gate reaches every host: {names:?}"
        );
    }
    let _ = a.cancel().await;
    let _ = b.cancel().await;
}

/// A large resource crosses the bridge whole, in both pumps.
#[tokio::test]
async fn a_large_resource_crosses_the_bridge_whole() {
    let server = suite::Server::start().await;
    let guide = server.read_resource("kaibo://config/guide").await;
    assert_eq!(guide, include_str!("../docs/config.md"));
    let _ = daemon_of(&server);
    server.shutdown().await;
}

/// `auto` with a live daemon forwards: the daemon's gate shows through, which an
/// in-process fallback (it would advertise `oneshot`) could not fake.
#[tokio::test]
async fn auto_with_a_live_daemon_forwards() {
    let (_x, _p, env) = env_for_test();
    let daemon = start_daemon(&env, &["--no-oneshot"]).await;
    let mut bridge = RawBridge::spawn(
        &env,
        &["--connect", "--socket", daemon.socket.to_str().unwrap()],
    );
    bridge.initialize();
    bridge.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let reply = bridge.read().expect("a tools/list reply");
    let names: Vec<String> = reply["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"run_kaish".to_string()), "{names:?}");
    assert!(
        !names.contains(&"oneshot".to_string()),
        "forwarded to the daemon, which gates oneshot off: {names:?}"
    );
}

/// A server that serves the socket ignores `connect`, even `required`: it must not look
/// for another daemon, and it comes up.
#[tokio::test]
async fn a_serving_daemon_ignores_connect() {
    let (_x, _p, env) = env_for_test();
    let daemon = start_daemon(&env, &["--connect=required"]).await;
    assert!(UnixStream::connect(&daemon.socket).is_ok());
}

/// `--no-connect` closes what the environment opened, end to end: with
/// `KAIBO_CONNECT=required` and no daemon the process would exit 3, and it serves instead.
#[tokio::test]
async fn no_connect_beats_the_environment() {
    let (_x, _p, env) = env_for_test();
    let dir = short_socket_dir();
    let socket = dir.path().join("none");
    let mut cmd = base(&env);
    cmd.env("KAIBO_CONNECT", "required")
        .args(["serve", "--no-connect", "--socket"])
        .arg(&socket)
        .arg("--root")
        .arg(&env.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().unwrap();
    let mut bridge = {
        let mut child = child;
        let lines = BufReader::new(child.stdout.take().unwrap());
        RawBridge { child, lines }
    };
    let reply = bridge.initialize();
    assert_eq!(reply["result"]["serverInfo"]["name"], "kaibo", "{reply}");
}

/// Many requests in flight at once through the pump all come back, none crossed.
#[tokio::test]
async fn concurrent_calls_through_the_bridge_return_their_own_results() {
    let server = suite::Server::start().await;
    let scripts: Vec<String> = (0..16).map(|i| format!("echo marker-{i}")).collect();
    let results = futures::future::join_all(scripts.iter().map(|s| server.run_kaish(s))).await;
    for (i, out) in results.iter().enumerate() {
        assert!(out.contains(&format!("marker-{i}")), "call {i} got: {out}");
    }
    server.shutdown().await;
}

/// A large message from the host crosses to the daemon whole (the other tests move large
/// messages the other way), and the connection stays usable after it.
#[tokio::test]
async fn a_large_request_crosses_the_bridge_whole() {
    let server = suite::Server::start().await;
    let padding = "x".repeat(300_000);
    let out = server
        .run_kaish(&format!("echo big-request-ok # {padding}"))
        .await;
    assert!(
        out.contains("big-request-ok"),
        "{}",
        &out[..out.len().min(200)]
    );
    let again = server.run_kaish("echo still-usable").await;
    assert!(again.contains("still-usable"), "{again}");
    server.shutdown().await;
}
