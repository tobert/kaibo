//! The Unix-socket transport preview, end to end: the real binary, a real socket file, and
//! rmcp's own Unix-socket client, so the door is observed the way a client meets it.
//!
//! Blank environment (`env_clear()`), temp `HOME` and XDG roots, as `tests/mcp_stdio.rs`
//! does. `XDG_RUNTIME_DIR` points at a short temp path, because a socket path must fit in
//! 107 bytes and the default temp dir can be longer.
#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    UnixSocketHttpClient,
};
use rmcp::ServiceExt as _;
use tempfile::TempDir;

/// The temp roots one test runs under.
struct Env {
    _tmp: TempDir,
    home: PathBuf,
    run: PathBuf,
    state: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("kb")
            .tempdir_in("/tmp")
            .unwrap();
        let home = tmp.path().join("h");
        let project = home.join("src");
        let run = tmp.path().join("r");
        let state = tmp.path().join("s");
        for d in [&home, &project, &run, &state] {
            std::fs::create_dir_all(d).unwrap();
        }
        Self {
            home,
            run,
            state,
            project,
            _tmp: tmp,
        }
    }

    /// Where the default socket lands under this environment.
    fn socket(&self) -> PathBuf {
        self.run.join("kaibo").join("mcp.sock")
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaibo"));
        cmd.env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_RUNTIME_DIR", &self.run)
            .env("RUST_LOG", "kaibo=info")
            .env("NO_COLOR", "1")
            .current_dir(&self.home);
        cmd
    }

    /// `kaibo serve --http --root <project>`, with stderr piped.
    fn serve_command(&self) -> Command {
        let mut cmd = self.command();
        cmd.args(["serve", "--http", "--root"])
            .arg(&self.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }
}

/// A running `kaibo serve --http`, killed on drop.
struct Server {
    child: Child,
    env: Env,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait until the child's stderr says it listens on a Unix socket.
fn wait_listening(child: &mut Child) {
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("listening on a Unix socket") {
                let _ = tx.send(());
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the server logs that it is listening");
}

/// Send one raw HTTP request over the socket at `path`; return everything the server wrote.
fn raw_request(socket: &std::path::Path, method: &str, path: &str, body: &str) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\
         Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

impl Server {
    fn start() -> Self {
        let env = Env::new();
        let mut child = env
            .serve_command()
            .spawn()
            .expect("spawn kaibo serve --http");
        wait_listening(&mut child);
        Self { child, env }
    }

    fn request(&self, method: &str, path: &str, body: &str) -> String {
        raw_request(&self.env.socket(), method, path, body)
    }
}

fn status(response: &str) -> &str {
    response.lines().next().unwrap_or_default()
}

fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the server did not exit within 10 seconds"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run a command expected to fail at startup; return its exit code and stderr.
fn run_to_failure(mut cmd: Command) -> (Option<i32>, String) {
    let out = cmd.output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The headline: rmcp's own Unix-socket client completes the MCP handshake and lists
/// kaibo's tools, with no token, no header, and no network.
#[tokio::test]
async fn an_rmcp_unix_socket_client_initializes_and_lists_tools() {
    let server = Server::start();
    let socket = server.env.socket();
    let uri = "http://localhost/mcp";
    let client = UnixSocketHttpClient::new(socket.to_str().unwrap(), uri);
    let transport = StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(uri),
    );
    let mcp = tokio::time::timeout(Duration::from_secs(60), ().serve(transport))
        .await
        .expect("the handshake finishes")
        .expect("the handshake succeeds");
    let tools = tokio::time::timeout(Duration::from_secs(60), mcp.list_all_tools())
        .await
        .expect("tools/list finishes")
        .expect("tools/list succeeds");
    assert!(
        tools.iter().any(|t| t.name == "run_kaish"),
        "run_kaish is advertised: {:?}",
        tools.iter().map(|t| t.name.to_string()).collect::<Vec<_>>()
    );
    let _ = mcp.cancel().await;
}

/// The door is the file mode: a socket only its owner can open.
#[test]
fn the_socket_is_a_socket_and_owner_only() {
    let server = Server::start();
    let meta = std::fs::symlink_metadata(server.env.socket()).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn only_the_exact_mcp_path_is_served() {
    let server = Server::start();
    for path in [
        "/",
        "/mcp/",
        "//mcp",
        "/%6dcp",
        "/.well-known/oauth-protected-resource",
    ] {
        let resp = server.request("POST", path, "{}");
        assert!(status(&resp).contains("404"), "{path}: {resp}");
    }
    // /mcp reaches rmcp: whatever it answers to a malformed body, it is not our 404.
    let resp = server.request("POST", "/mcp", "{}");
    assert!(!status(&resp).contains("404"), "{resp}");
}

/// A request that carries no credentials is served: the socket's owner-only mode is the
/// whole door, so there is no bearer-token or `Origin` check to pass.
#[test]
fn no_token_is_needed() {
    let server = Server::start();
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    let resp = server.request("POST", "/mcp", init);
    assert!(status(&resp).contains("200"), "{resp}");
}

#[test]
fn sigterm_shuts_down_cleanly_and_removes_the_socket() {
    let mut server = Server::start();
    let socket = server.env.socket();
    assert!(socket.exists());
    // SAFETY: sending a signal to our own child process.
    assert_eq!(
        unsafe { libc::kill(server.child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let status = wait_exit(&mut server.child);
    assert!(status.success(), "a clean shutdown exits 0: {status}");
    assert!(!socket.exists(), "the socket file is removed at shutdown");
}

/// A crash leaves the file behind; the next start replaces it instead of failing.
#[test]
fn a_stale_socket_from_a_crash_is_replaced() {
    let mut first = Server::start();
    let socket = first.env.socket();
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    assert!(socket.exists(), "SIGKILL leaves the socket file behind");

    let mut second = first.env.serve_command().spawn().unwrap();
    wait_listening(&mut second);
    let resp = raw_request(&socket, "GET", "/nope", "");
    let _ = second.kill();
    let _ = second.wait();
    assert!(
        status(&resp).contains("404"),
        "the replacement answers: {resp}"
    );
}

/// A second server on the same path is refused while the first is live, and the first
/// keeps its socket.
#[test]
fn a_live_socket_is_not_taken_over() {
    let server = Server::start();
    let (code, stderr) = run_to_failure(server.env.serve_command());
    assert_ne!(code, Some(0));
    assert!(stderr.contains("already accepts connections"), "{stderr}");
    let resp = server.request("POST", "/x", "{}");
    assert!(
        status(&resp).contains("404"),
        "the first server still answers: {resp}"
    );
}

/// kaibo never deletes a file that is not a socket.
#[test]
fn a_regular_file_at_the_socket_path_is_refused_and_kept() {
    let env = Env::new();
    let socket = env.socket();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, "precious").unwrap();
    let (code, stderr) = run_to_failure(env.serve_command());
    assert_ne!(code, Some(0));
    assert!(stderr.contains("not a socket"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&socket).unwrap(), "precious");
}

#[test]
fn a_socket_path_inside_the_allowed_tree_is_refused() {
    let env = Env::new();
    let inside = env.project.join("mcp.sock");
    let mut cmd = env.serve_command();
    cmd.arg("--socket").arg(&inside);
    let (code, stderr) = run_to_failure(cmd);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("inside an allowed tree"), "{stderr}");
    assert!(!inside.exists());
}

#[test]
fn a_relative_socket_path_is_refused_naming_the_flag() {
    let env = Env::new();
    let mut cmd = env.serve_command();
    cmd.args(["--socket", "mcp.sock"]);
    let (code, stderr) = run_to_failure(cmd);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("--socket"), "{stderr}");
    assert!(stderr.contains("not an absolute path"), "{stderr}");
}

#[test]
fn a_socket_server_with_nothing_to_read_refuses_to_start() {
    let env = Env::new();
    let mut cmd = env.command();
    cmd.args(["serve", "--http"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let (code, stderr) = run_to_failure(cmd);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("no allowed tree"), "{stderr}");
    assert!(!env.socket().exists(), "nothing was bound");
}

/// The TCP flag and the token command are gone, not quietly ignored.
#[test]
fn the_tcp_flag_and_the_token_command_no_longer_exist() {
    let env = Env::new();
    for args in [
        &["serve", "--listen", "127.0.0.1:0"][..],
        &["http-token"][..],
    ] {
        let mut cmd = env.command();
        cmd.args(args).stdin(Stdio::null());
        let (code, stderr) = run_to_failure(cmd);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
    }
}

/// `--socket PATH` moves the socket, and the path's directory need not exist beforehand.
#[test]
fn the_socket_flag_moves_the_socket() {
    let env = Env::new();
    let custom = env.run.join("elsewhere").join("k.sock");
    let mut cmd = env.serve_command();
    cmd.arg("--socket").arg(&custom);
    let mut child = cmd.spawn().unwrap();
    wait_listening(&mut child);
    let moved = custom.exists() && !env.socket().exists();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        moved,
        "the server bound the path it was given, not the default"
    );
}

/// `--socket` only says where; it does not turn the transport on. Without `--http` the
/// server is stdio (here it reads EOF and exits), and nothing is bound.
#[test]
fn the_socket_flag_alone_does_not_start_the_socket_server() {
    let env = Env::new();
    let custom = env.run.join("k.sock");
    let mut cmd = env.command();
    cmd.args(["serve", "--root"])
        .arg(&env.project)
        .arg("--socket")
        .arg(&custom)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let out = cmd.output().unwrap();
    assert!(!custom.exists(), "no socket without --http");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("listening on a Unix socket"), "{stderr}");
}

/// A socket directory another user could replace is refused before anything is bound.
#[test]
fn a_world_writable_socket_directory_is_refused() {
    let env = Env::new();
    let dir = env.run.join("open");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let sock = dir.join("k.sock");
    let mut cmd = env.serve_command();
    cmd.arg("--socket").arg(&sock);
    let (code, stderr) = run_to_failure(cmd);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("replace the socket"), "{stderr}");
    assert!(!sock.exists());
}
