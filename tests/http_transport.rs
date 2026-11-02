//! The HTTP transport preview, end to end: the real binary, a real loopback socket, and
//! raw HTTP/1.1 requests, so each check is observed the way a client meets it.
//!
//! Blank environment (`env_clear()`), temp `HOME` and XDG roots, as `tests/mcp_stdio.rs`
//! does. `--listen 127.0.0.1:0` lets the OS pick a port; the startup log names it.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

/// A running `kaibo serve --http`, killed on drop.
struct Server {
    child: Child,
    port: u16,
    env: Env,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The temp roots one test runs under.
struct Env {
    _root: TempDir,
    home: PathBuf,
    state: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        let project = home.join("src");
        let state = root.path().join("state");
        for d in [&home, &project, &state] {
            std::fs::create_dir_all(d).unwrap();
        }
        Self {
            home,
            state,
            project,
            _root: root,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaibo"));
        cmd.env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("RUST_LOG", "kaibo=info")
            .env("NO_COLOR", "1")
            .current_dir(&self.home);
        cmd
    }

    fn token_path(&self) -> PathBuf {
        self.state.join("kaibo").join("http-token")
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.token_path())
            .unwrap()
            .trim()
            .to_string()
    }
}

impl Server {
    fn start() -> Self {
        let env = Env::new();
        let mut child = env
            .command()
            .args(["serve", "--http", "--listen", "127.0.0.1:0", "--root"])
            .arg(&env.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kaibo serve --http");
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(rest) = line.split("url=http://127.0.0.1:").nth(1) {
                    let port: String = rest.chars().take_while(char::is_ascii_digit).collect();
                    let _ = tx.send(port.parse::<u16>().unwrap());
                }
            }
        });
        let port = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the server logs its URL at startup");
        Self { child, port, env }
    }

    /// Send one request and return the status line and headers.
    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n",
            self.port,
            body.len()
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(body);
        stream.write_all(req.as_bytes()).unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    fn initialize(&self, auth: Option<&str>) -> String {
        let bearer = auth.map(|t| format!("Bearer {t}"));
        let mut headers = vec![
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ];
        if let Some(b) = &bearer {
            headers.push(("Authorization", b));
        }
        self.request("POST", "/mcp", &headers, INITIALIZE)
    }
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

fn status(head: &str) -> &str {
    head.split_whitespace().nth(1).unwrap_or("")
}

#[test]
fn the_token_admits_an_initialize_and_nothing_else_gets_in() {
    let server = Server::start();
    let token = server.env.token();
    assert_eq!(
        token.len(),
        43,
        "the server created the token file at startup"
    );

    let ok = server.initialize(Some(&token));
    assert_eq!(status(&ok), "200", "{ok}");

    let none = server.initialize(None);
    assert_eq!(status(&none), "401", "{none}");
    assert!(
        none.to_ascii_lowercase()
            .contains("www-authenticate: bearer"),
        "a 401 names the bearer scheme: {none}"
    );
    assert_eq!(status(&server.initialize(Some("wrong"))), "401");

    let browser = server.request(
        "POST",
        "/mcp",
        &[
            ("Origin", "https://evil.example"),
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
        ],
        INITIALIZE,
    );
    assert_eq!(status(&browser), "403", "{browser}");

    let oauth = server.request("GET", "/.well-known/oauth-protected-resource", &[], "");
    assert_eq!(status(&oauth), "404", "{oauth}");
}

#[test]
fn rotation_takes_effect_on_the_next_request_without_a_restart() {
    let server = Server::start();
    let old = server.env.token();
    let out = server
        .env
        .command()
        .args(["http-token", "--rotate", "--root"])
        .arg(&server.env.project)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let new = server.env.token();
    assert_ne!(old, new);
    assert_eq!(status(&server.initialize(Some(&old))), "401");
    assert_eq!(status(&server.initialize(Some(&new))), "200");
}

#[test]
fn http_token_prints_the_path_the_token_and_the_header() {
    let env = Env::new();
    let run = |args: &[&str]| {
        let out = env.command().args(args).output().unwrap();
        (
            out.status.code(),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (code, _, stderr) = run(&["http-token", "--header"]);
    assert_eq!(code, Some(3), "no token yet: a setup error");
    assert!(stderr.contains("--rotate"), "names the fix: {stderr}");

    let project = env.project.to_str().unwrap();
    let (code, out, _) = run(&["http-token", "--rotate", "--root", project]);
    assert_eq!(code, Some(0));
    assert_eq!(Path::new(out.trim()), env.token_path());

    let token = env.token();
    assert_eq!(
        run(&["http-token"]).1.trim(),
        env.token_path().to_str().unwrap()
    );
    assert_eq!(run(&["http-token", "--print"]).1.trim(), token);
    let header: serde_json::Value =
        serde_json::from_str(&run(&["http-token", "--header"]).1).unwrap();
    assert_eq!(header["Authorization"], format!("Bearer {token}"));
}

#[test]
fn a_non_loopback_listen_address_is_refused_at_startup() {
    let env = Env::new();
    let out = env
        .command()
        .args(["serve", "--http", "--listen", "0.0.0.0:0", "--root"])
        .arg(&env.project)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not a loopback address"), "{stderr}");
}

#[test]
fn an_http_server_with_nothing_to_read_refuses_to_start() {
    let env = Env::new();
    // Launched from $HOME with no root: the cwd is not adopted, so no tree is allowed.
    let out = env
        .command()
        .args(["serve", "--http", "--listen", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no allowed tree"), "{stderr}");
    assert!(
        !env.token_path().exists(),
        "a refused start writes no token file"
    );
}
