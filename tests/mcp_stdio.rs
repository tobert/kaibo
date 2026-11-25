//! kaibo's MCP surface, driven the way a client drives it: spawn the real binary,
//! speak MCP over its stdio, and assert on what comes back.
//!
//! Every other test in this tree reaches `KaiboHandler` in-process, so the wire between
//! a client and kaibo — `initialize`, `tools/list`, `tools/call`, `resources/read` —
//! had no in-tree coverage. This file closes that gap with rmcp's *client* half
//! (a dev-dependency; kaibo already ships rmcp's server half) over
//! `TokioChildProcess`, which spawns `env!("CARGO_BIN_EXE_kaibo")` — the binary cargo
//! built for this test run, never a hardcoded target path.
//!
//! Two properties make this safe to run anywhere, including CI and a laptop with a
//! full keyring:
//!
//! - **Hermetic.** The child's environment is *cleared*, then rebuilt from temp dirs:
//!   `HOME` plus every XDG root kaibo reads (`XDG_CONFIG_HOME` → `config.toml`,
//!   `XDG_STATE_HOME` → the persistence db, `XDG_DATA_HOME` → the media CAS). Clearing
//!   is what makes the run deterministic: no `ANTHROPIC_API_KEY`, no `KAIBO_*`, no
//!   `RUST_LOG` from the developer's shell can change the advertised surface. A missing
//!   `config.toml` is a non-error by design, so the empty config dir means "built-ins".
//! - **Offline.** Nothing here needs a provider key or a socket. The model-driven tools
//!   are listed but never called; the tool call under test is `run_kaish`, which has no
//!   model in its loop.
//!
//! Shape: one server process per test, each with its own temp dirs, so tests share no
//! state and run in any order or in parallel. Every request is wrapped in
//! [`CALL_TIMEOUT`] — a hung server fails its own test with a readable message instead
//! of wedging the suite.
//!
//! Teeth: flip an assertion (e.g. drop `run_kaish` from [`KEYLESS_TOOLS`], or expect
//! `mkdir` to succeed) and the matching test fails.
//!
//! The harness and the transport-agnostic tests live in `tests/common/mcp_suite.rs`,
//! shared with `tests/mcp_socket.rs`; this file keeps only the stdio connection and
//! the raw-wire protocol-version tests, which speak newline-delimited JSON-RPC.

use rmcp::service::ServiceExt;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use tempfile::TempDir;

#[path = "common/mcp_suite.rs"]
mod suite;
use suite::{bounded, ChildEnv, Connected};

/// Spawn the binary on stdio and complete the handshake.
async fn connect(env: &ChildEnv, args: &[&str]) -> Connected {
    let root = env.root.clone();
    let (home, config_home, state_home, data_home) = (
        env.home.clone(),
        env.config_home.clone(),
        env.state_home.clone(),
        env.data_home.clone(),
    );
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_kaibo")).configure(|cmd| {
            // Clear first: the developer's provider keys and `KAIBO_*` overrides would
            // otherwise decide which tools get advertised.
            cmd.env_clear()
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &config_home)
                .env("XDG_STATE_HOME", &state_home)
                .env("XDG_DATA_HOME", &data_home)
                .env("RUST_LOG", "error")
                .arg("--root")
                .arg(&root)
                .args(args);
        }),
    )
    .expect("spawn the kaibo binary");
    let client = bounded("the MCP handshake", ().serve(transport))
        .await
        .expect("kaibo should complete `initialize` over stdio");
    Connected {
        client,
        guard: Box::new(()),
    }
}

/// A raw JSON-RPC exchange over the binary's stdio, bypassing rmcp's client so the
/// test controls the exact protocol version on the wire. rmcp's client always asks
/// for its own latest, so the version-skew cases below are unreachable through it.
///
/// `script` is sent line by line, in order. A message with an `id` waits for the
/// response carrying that id, which is returned in script order; a notification
/// (no `id`) is sent and not waited on. Other lines the server writes (log
/// notifications, say) are skipped.
///
/// Same blank-environment rule as [`Server`]: `env_clear()`, then temp `HOME`/XDG
/// roots, so nothing from the developer's shell shapes the server under test.
async fn raw_exchange(script: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let xdg = TempDir::new().expect("tempdir for the child's HOME/XDG roots");
    let project = TempDir::new().expect("tempdir for the served project");
    std::fs::write(project.path().join("Cargo.toml"), "[package]\n")
        .expect("seed the temp project");
    let home = xdg.path().join("home");
    let config_home = xdg.path().join("config");
    let state_home = xdg.path().join("state");
    let data_home = xdg.path().join("data");
    for dir in [&home, &config_home, &state_home, &data_home] {
        std::fs::create_dir_all(dir).expect("create an XDG root");
    }

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("RUST_LOG", "error")
        .arg("--root")
        .arg(project.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the kaibo binary");

    let mut stdin = child.stdin.take().expect("child stdin is piped");
    let stdout = child.stdout.take().expect("child stdout is piped");
    let mut lines = BufReader::new(stdout).lines();

    let mut responses = Vec::new();
    for msg in script {
        stdin
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .expect("write a request line");
        let Some(id) = msg.get("id").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let what = format!("raw {}", msg["method"]);
        loop {
            let line = bounded(&what, lines.next_line())
                .await
                .expect("read a response line")
                .unwrap_or_else(|| panic!("{what}: the server closed stdout"));
            let value: serde_json::Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("{what}: bad JSON: {e}"));
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                responses.push(value);
                break;
            }
        }
    }

    child.kill().await.ok();
    responses
}

/// The legacy entry: `initialize` at `client_version`, `notifications/initialized`,
/// then `tools/list` and `resources/list`. Returns the three results.
async fn raw_initialize_then_list_tools(
    client_version: &str,
) -> (serde_json::Value, serde_json::Value, serde_json::Value) {
    let responses = raw_exchange(vec![
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": client_version,
                "capabilities": {},
                "clientInfo": { "name": "raw-probe", "version": "0" }
            }
        }),
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list", "params": {} }),
    ])
    .await;
    let [init, tools, resources] = <[_; 3]>::try_from(responses).expect("three responses");
    (
        init["result"].clone(),
        tools["result"].clone(),
        resources["result"].clone(),
    )
}

/// A session at `2026-07-28` receives every field that version's schema REQUIRES on
/// a list result.
///
/// That version has no `initialize`: a client enters with `server/discover` and
/// states its protocol version and capabilities in each request's `_meta`, so this
/// test enters the same way.
///
/// The failure this pins (live, 2026-08-10): rmcp negotiated `2026-07-28` but left
/// `ttlMs` and `cacheScope` unset, which SEP-2549 makes REQUIRED on every list/read
/// result at that version. A strictly-validating client (Claude Code) rejected the
/// whole `tools/list` over the two missing fields: zero tools, kaibo unusable until
/// restart. kaibo fills them itself (`sep_2549_cache_fields`, `server/mod.rs`) with
/// the no-caching floor: `ttlMs: 0`, `cacheScope: "private"`. The helper reads the
/// version from the request's `_meta` first, which is what makes it reach a session
/// with no handshake.
///
/// A newer rmcp filling the fields is not the signal to delete that helper: rmcp's
/// handler macros answer `cacheScope: public`, which is the wrong answer for a
/// per-user surface. This test asserts kaibo's values, so it fails if the SDK's ever
/// start winning.
#[tokio::test]
async fn a_newest_protocol_session_gets_the_fields_its_schema_requires() {
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "raw-probe", "version": "0" }
    });
    // Every list/read result kaibo answers, plus discovery. `prompts/get` is not here:
    // its result type carries no SEP-2549 fields.
    let requests: [(&str, serde_json::Value); 6] = [
        ("server/discover", serde_json::json!({})),
        ("tools/list", serde_json::json!({})),
        ("resources/list", serde_json::json!({})),
        ("resources/templates/list", serde_json::json!({})),
        (
            "resources/read",
            serde_json::json!({ "uri": "kaibo://config" }),
        ),
        ("prompts/list", serde_json::json!({})),
    ];
    let script = requests
        .iter()
        .zip(1u64..)
        .map(|((method, params), id)| {
            let mut params = params.clone();
            params["_meta"] = meta.clone();
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        })
        .collect();
    let responses = raw_exchange(script).await;
    let results: Vec<(&str, &serde_json::Value)> = requests
        .iter()
        .zip(&responses)
        .map(|((method, _), response)| {
            assert!(
                response["result"].is_object(),
                "{method} answers a result, not an error; got: {response}"
            );
            (*method, &response["result"])
        })
        .collect();
    let (discover, tools) = (results[0].1, results[1].1);

    assert!(
        discover["supportedVersions"]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v == "2026-07-28")),
        "server/discover offers the version this session speaks; got: {discover}"
    );
    assert_eq!(
        discover["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "kaibo",
        "discovery identifies kaibo, not rmcp; got: {discover}"
    );
    assert!(
        discover["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("READ-ONLY")),
        "discovery carries the instructions `initialize` carried before; got: {discover}"
    );

    for &(what, result) in &results {
        assert_eq!(
            result["ttlMs"], 0,
            "{what} must carry the REQUIRED `ttlMs`, as the no-caching floor; got: {result}"
        );
        assert_eq!(
            result["cacheScope"], "private",
            "{what} must carry the REQUIRED `cacheScope`; the surface is this \
             user's own configuration; got: {result}"
        );
        assert_eq!(
            result["resultType"], "complete",
            "{what} carries the SEP-2322 discriminator at this version; got: {result}"
        );
    }
    let served = tools["tools"]
        .as_array()
        .expect("tools/list carries a tools array");
    assert!(
        !served.is_empty(),
        "the session serves the real tool surface alongside the cache fields"
    );
}

/// An `initialize` naming `2026-07-28` is answered with `2025-11-25`, the newest
/// version that still has an `initialize` handshake, and the session keeps that
/// version's legacy wire shape.
///
/// rmcp decides this (since 3.2), not kaibo: `2026-07-28` replaced the handshake
/// with per-request `_meta`, so a client can reach it only through `server/discover`.
/// This pins the downgrade so an rmcp release that changes it shows up here, beside
/// the SEP-2549 test above that depends on which door a newest-version client uses.
#[tokio::test]
async fn an_initialize_naming_the_newest_version_gets_the_newest_legacy_one() {
    let (init, tools, resources) = raw_initialize_then_list_tools("2026-07-28").await;

    assert_eq!(
        init["protocolVersion"], "2025-11-25",
        "`initialize` negotiates the newest version that has it; got: {init}"
    );
    for (what, result) in [("tools/list", &tools), ("resources/list", &resources)] {
        assert!(
            result.is_object(),
            "{what} answers a result, so the absences below are real; got: {result}"
        );
        for field in ["ttlMs", "cacheScope", "resultType"] {
            assert!(
                result.get(field).is_none(),
                "{what}: a `2025-11-25` session gets no `2026-07-28` `{field}`; got: {result}"
            );
        }
    }
}

/// A client on an older protocol version keeps the legacy wire shape — none of the
/// newer contract's fields leak into a session whose schema does not know them.
#[tokio::test]
async fn an_older_protocol_session_keeps_the_legacy_wire_shape() {
    let (init, tools, resources) = raw_initialize_then_list_tools("2025-06-18").await;

    assert_eq!(
        init["protocolVersion"], "2025-06-18",
        "an in-range client version is echoed, not lifted or lowered; got: {init}"
    );
    for (what, result) in [("tools/list", &tools), ("resources/list", &resources)] {
        assert!(
            result.is_object(),
            "{what} answers a result, so the absences below are real; got: {result}"
        );
        for field in ["ttlMs", "cacheScope", "resultType"] {
            assert!(
                result.get(field).is_none(),
                "{what}: no newer-contract `{field}` leaks into an older session; \
                 got: {result}"
            );
        }
    }
}
