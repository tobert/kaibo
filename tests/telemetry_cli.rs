//! The CLI path exports telemetry too, and actually flushes it — end to end, over a
//! real socket, driving the real binary the way a terminal user does.
//!
//! Before this file's fix, `kaibo::telemetry::init` was called only from `serve()`
//! (`src/main.rs`), inside the MCP-server dispatch arm — every CLI subcommand
//! (`consult`, `kaish`, ...) ran with no exporter at all, so a terminal `kaibo kaish`
//! shipped nothing even with `[telemetry] enabled = true` in `config.toml`. The
//! offline unit tests in `src/telemetry.rs` prove the exporter *builds*; they can't
//! prove the CLI actually *reaches* it, and they can't catch the specific footgun
//! this fix has to dodge: every CLI arm leaves through `std::process::exit`, which
//! runs no destructors, so a guard that is merely constructed and dropped (rather
//! than explicitly `.shutdown()`-ed before the exit call) would silently lose every
//! span the batch processor hasn't exported yet. A short-lived `kaish -c "echo hi"`
//! finishes and would exit before the processor's own export interval ever fires —
//! so the only way to prove the flush works is to watch a real request land on a
//! real socket, from a real spawned process, the way `tests/openai_images_transport.rs`
//! and `tests/llm_timeout.rs` do for other transports.
//!
//! Hermetic like `tests/mcp_stdio.rs`: `env_clear()`, then rebuild only `HOME` and
//! `XDG_CONFIG_HOME` from temp dirs, so nothing in the developer's shell (a stray
//! `KAIBO_*`, an inherited `RUST_LOG`) can change what's under test.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

/// One captured HTTP request: the request line and the body length the client
/// declared (protobuf, not JSON — we don't need to decode it, only prove it arrived).
struct Captured {
    request_line: String,
    content_type: Option<String>,
    body_len: usize,
}

/// Accept exactly one HTTP exchange on an ephemeral port, hand back a 200 with an
/// empty (still-valid) protobuf body, and surrender the captured request over the
/// returned receiver. Runs on its own OS thread so it can accept while the test's
/// main thread blocks on the child process via `Command::output()`.
fn one_shot_collector() -> (u16, mpsc::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut sock, _) = match listener.accept() {
            Ok(pair) => pair,
            // No connection ever arrived (the flush regressed): let the receiver's
            // `recv_timeout` in the test report that, rather than panicking a
            // detached thread where the message wouldn't surface.
            Err(_) => return,
        };
        let mut buf = Vec::new();
        let head_end = loop {
            let mut chunk = [0u8; 4096];
            let n = match sock.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if buf.len() > 1 << 20 {
                return; // a request head this large is not one of ours
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let content_length: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);
        while buf.len() < head_end + content_length {
            let mut chunk = [0u8; 4096];
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let content_type = head.lines().find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-type:")
                .map(|v| v.trim().to_string())
        });
        let response = b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let _ = sock.write_all(response);
        let _ = sock.shutdown(std::net::Shutdown::Both);
        let _ = tx.send(Captured {
            request_line: head.lines().next().unwrap_or("").to_string(),
            content_type,
            body_len: content_length,
        });
    });
    (port, rx)
}

/// Spawn the real binary as `kaibo kaish -c "echo …"` against a hermetic `HOME` /
/// `XDG_CONFIG_HOME`, with `[telemetry] enabled = true` pointed at `port`. Returns
/// once the process exits — the point in time at which every buffered span must
/// already be on the wire, if the flush-before-exit fix is in place.
fn run_kaish_with_telemetry(port: u16, logs: bool) -> std::process::Output {
    let xdg_home = tempfile::tempdir().expect("tempdir for HOME/XDG_CONFIG_HOME");
    let home = xdg_home.path().join("home");
    let config_dir = xdg_home.path().join("config").join("kaibo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
            [telemetry]
            enabled = true
            endpoint = "http://127.0.0.1:{port}/v1/traces"
            logs = {logs}
            service_name = "kaibo-cli-otel-test"
            "#,
        ),
    )
    .expect("write config.toml");

    let project = tempfile::tempdir().expect("tempdir for the --root project");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"p\"\n",
    )
    .unwrap();

    Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", xdg_home.path().join("config"))
        .args([
            "--root",
            project.path().to_str().unwrap(),
            "kaish",
            "-c",
            "echo cli-otel-flush-probe",
        ])
        .output()
        .expect("spawn the kaibo binary")
}

/// The teeth: a CLI `kaish` run with telemetry configured must deliver a trace
/// export to the collector before the process exits, not merely construct an
/// exporter it never flushes. Proven by breaking the fix (dropping the guard
/// instead of calling `.shutdown()`) and watching this test time out with no
/// connection ever accepted — see the pull request for that run.
#[test]
fn cli_kaish_flushes_a_trace_export_before_the_process_exits() {
    let (port, rx) = one_shot_collector();

    let out = run_kaish_with_telemetry(port, false);
    assert!(
        out.status.success(),
        "the script itself must have run cleanly: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cli-otel-flush-probe"),
        "the script's own output must still reach stdout unchanged"
    );

    let captured = rx.recv_timeout(Duration::from_secs(10)).expect(
        "no OTLP export reached the collector within 10s after the CLI process \
             exited — the flush-before-exit wiring regressed (a dropped-not-shutdown \
             guard loses every buffered span to std::process::exit)",
    );
    assert_eq!(
        captured.request_line, "POST /v1/traces HTTP/1.1",
        "the exporter must POST to the configured traces path"
    );
    assert_eq!(
        captured.content_type.as_deref(),
        Some("application/x-protobuf"),
        "OTLP/HTTP protobuf, the transport kaibo is pinned to"
    );
    assert!(
        captured.body_len > 0,
        "the exported request must carry actual span bytes, not an empty POST"
    );
}

/// A default CLI run (no `[telemetry]` in config.toml) must ship nothing — telemetry
/// stays off by default even though `init_cli_telemetry` now runs unconditionally
/// ahead of every subcommand. Proven by pointing a would-be collector at a real port
/// and confirming nothing ever connects.
#[test]
fn cli_kaish_without_telemetry_configured_dials_nothing() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener
        .set_nonblocking(true)
        .expect("nonblocking so the accept below can poll instead of hang");
    let port = listener.local_addr().expect("local_addr").port();

    let xdg_home = tempfile::tempdir().expect("tempdir for HOME/XDG_CONFIG_HOME");
    let home = xdg_home.path().join("home");
    let config_dir = xdg_home.path().join("config").join("kaibo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    // No config.toml at all: `Config::load` treats that as "use the built-in
    // defaults", and the built-in default has `[telemetry] enabled = false`.
    let project = tempfile::tempdir().expect("tempdir for the --root project");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"p\"\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", xdg_home.path().join("config"))
        .args([
            "--root",
            project.path().to_str().unwrap(),
            "kaish",
            "-c",
            "echo no-telemetry-probe",
        ])
        .output()
        .expect("spawn the kaibo binary");
    assert!(
        out.status.success(),
        "the script itself must still run cleanly"
    );

    // Give a regression a fair chance to dial in before we declare it clean — a
    // false pass here (checking immediately, before any export would have had time
    // to land) would be worse than a slow test. Nothing else in this hermetic test
    // process holds `port`, so any connection here can only be from the child.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        listener.accept().is_err(),
        "telemetry OFF (the default) must dial no socket at all (port {port})"
    );
}
