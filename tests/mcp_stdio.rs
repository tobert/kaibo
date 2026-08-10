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

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ReadResourceRequestParams, ServerPeerInfo, Tool,
};
use rmcp::service::{RoleClient, RunningService, ServiceError, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use tempfile::TempDir;

/// Ceiling on any single MCP request, and on the handshake. Generous on purpose: this
/// is a hang detector, not a performance assertion, so a slow machine never flakes.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The exact tool surface a keyless kaibo advertises, sorted.
///
/// This is a *stock* install with no credentials at all: the built-in `openai-local`
/// backend needs no key, so the casts it staffs keep `consult` / `consult_submit` /
/// `explore` / `oneshot` (and the job verbs that collect `consult_submit`'s handles)
/// advertised — kaibo does not check that a local server is actually listening, only
/// that a cast can be staffed. `run_kaish`, `list_models`, and `read_cas` need no cast
/// at all. What is *missing* is the other half of the story: `batch_submit`,
/// `deliberate`, and `generate` all need a cast shape no built-in provides, so their
/// routes are dropped rather than advertised-and-broken.
const KEYLESS_TOOLS: &[&str] = &[
    "consult",
    "consult_submit",
    "explore",
    "job_cancel",
    "job_get",
    "job_list",
    "job_wait",
    "list_models",
    "oneshot",
    "read_cas",
    "run_kaish",
];

/// Marker line written into the temp project's `Cargo.toml`, so a successful read is
/// unmistakably *this* file and not the repo's own.
const PROJECT_MARKER: &str = "kai-the-crab-was-here";

/// Bound a request so a wedged server fails one test loudly instead of hanging `cargo
/// test` until someone notices.
async fn bounded<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match tokio::time::timeout(CALL_TIMEOUT, fut).await {
        Ok(v) => v,
        Err(_) => panic!("{what} did not finish within {CALL_TIMEOUT:?} — the server is hung"),
    }
}

/// A live kaibo server plus the temp dirs it was pointed at. Dropping it kills the
/// child (rmcp's transport does that) and removes the dirs.
struct Server {
    /// `HOME` and the XDG roots. Held so the dirs outlive the child.
    _xdg: TempDir,
    /// The project kaibo is rooted at — the only tree it may read.
    project: TempDir,
    client: RunningService<RoleClient, ()>,
}

impl Server {
    /// Spawn kaibo on a fresh temp project and complete the MCP handshake.
    async fn start() -> Self {
        Self::start_with_args(&[]).await
    }

    /// [`start`](Self::start) with extra CLI flags — how a test asks for a different
    /// server posture (a `--no-<tool>` gate, say).
    ///
    /// `--root` names the project, which also drops the invocation cwd from the allowed
    /// set — so the kaibo checkout this test runs from is *not* readable by the server
    /// under test, and a read that lands on the temp project can only have come from
    /// the temp project.
    async fn start_with_args(args: &[&str]) -> Self {
        let xdg = TempDir::new().expect("tempdir for the child's HOME/XDG roots");
        let project = TempDir::new().expect("tempdir for the served project");
        std::fs::write(
            project.path().join("Cargo.toml"),
            format!("[package]\nname = \"{PROJECT_MARKER}\"\n"),
        )
        .expect("seed the temp project");

        let config_home = xdg.path().join("config");
        let state_home = xdg.path().join("state");
        let data_home = xdg.path().join("data");
        let home = xdg.path().join("home");
        for dir in [&config_home, &state_home, &data_home, &home] {
            std::fs::create_dir_all(dir).expect("create an XDG root");
        }

        let root = project.path().to_path_buf();
        let transport = TokioChildProcess::new(
            tokio::process::Command::new(env!("CARGO_BIN_EXE_kaibo")).configure(|cmd| {
                // Clear first: the developer's provider keys and `KAIBO_*` overrides would
                // otherwise decide which tools get advertised. kaibo runs no external
                // command, so it needs nothing else from the environment.
                cmd.env_clear()
                    .env("HOME", &home)
                    .env("XDG_CONFIG_HOME", &config_home)
                    .env("XDG_STATE_HOME", &state_home)
                    .env("XDG_DATA_HOME", &data_home)
                    // Child logs go to the test runner's stderr; keep them to real errors.
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

        Self {
            _xdg: xdg,
            project,
            client,
        }
    }

    /// The project root the server was launched against.
    fn root(&self) -> &Path {
        self.project.path()
    }

    /// What the server said about itself at `initialize`.
    fn handshake(&self) -> ServerPeerInfo {
        let info = self
            .client
            .peer_info()
            .expect("peer info is set once `initialize` completes");
        (*info).clone()
    }

    /// Every advertised tool as the wire carries it — schema, `_meta`, and all — sorted
    /// by name.
    async fn tools(&self) -> Vec<Tool> {
        let mut tools = bounded("tools/list", self.client.list_all_tools())
            .await
            .expect("tools/list should succeed");
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Every advertised tool name, sorted.
    async fn tool_names(&self) -> Vec<String> {
        self.tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    /// Ask the server to run a kaish script against the served root, returning whatever
    /// `tools/call` answered — including a protocol-level refusal.
    async fn try_run_kaish(&self, script: &str) -> Result<CallToolResult, ServiceError> {
        let args = serde_json::json!({
            "script": script,
            "path": self.root().to_string_lossy(),
        });
        let params = CallToolRequestParams::new("run_kaish").with_arguments(
            args.as_object()
                .expect("the argument literal is an object")
                .clone(),
        );
        bounded("tools/call run_kaish", self.client.call_tool(params)).await
    }

    /// Run a kaish script against the served root through the `run_kaish` tool, and
    /// flatten the result to text.
    async fn run_kaish(&self, script: &str) -> String {
        let result = self
            .try_run_kaish(script)
            .await
            .expect("tools/call run_kaish should return a result, not a protocol error");
        result
            .content
            .into_iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Read one resource and flatten its text contents.
    async fn read_resource(&self, uri: &str) -> String {
        let params = ReadResourceRequestParams::new(uri);
        let result = bounded("resources/read", self.client.read_resource(params))
            .await
            .unwrap_or_else(|e| panic!("resources/read {uri} should succeed: {e}"));
        result
            .contents
            .into_iter()
            .filter_map(|c| match c {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => Some(text),
                // Blob (and any future variant) is not what kaibo's config resources
                // serve; keeping the text arm alone is the assertion.
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Close the session and let the child exit. Dropping would also kill it; doing it
    /// explicitly keeps a test from racing the reaper on the way out.
    async fn shutdown(self) {
        let _ = bounded("session shutdown", self.client.cancel()).await;
    }
}

/// `initialize` completes, kaibo names itself, and the instructions the calling agent
/// reads carry the front-door vocabulary.
///
/// The instructions are the retrieval index in deferral hosts (see AGENTS.md,
/// "Client-facing text"), so their opening words are load-bearing product, not prose.
#[tokio::test]
async fn handshake_names_kaibo_and_ships_read_only_instructions() {
    let server = Server::start().await;
    let info = server.handshake();

    let implementation = info
        .server_info
        .expect("kaibo names its implementation at initialize");
    assert_eq!(
        implementation.name, "kaibo",
        "the server must identify as kaibo, not as the rmcp crate"
    );
    assert_eq!(
        implementation.version,
        env!("CARGO_PKG_VERSION"),
        "the advertised version must be the crate's"
    );
    assert!(
        info.capabilities.tools.is_some(),
        "kaibo advertises tools; capabilities said otherwise: {:?}",
        info.capabilities
    );
    assert!(
        info.capabilities.resources.is_some(),
        "kaibo advertises resources; capabilities said otherwise: {:?}",
        info.capabilities
    );

    let instructions = info
        .instructions
        .expect("kaibo sends instructions at initialize");
    for word in ["read-only", "consult", "cast"] {
        assert!(
            instructions.contains(word),
            "the instructions must carry the front-door word `{word}`; got: {instructions}"
        );
    }

    server.shutdown().await;
}

/// A keyless server advertises exactly the tools it can actually staff.
///
/// Pinning the whole set (not "contains run_kaish") is what makes a gating regression
/// visible: a tool that starts being advertised without a cast that can staff it, or
/// one that silently drops off a stock install, fails here.
#[tokio::test]
async fn a_keyless_server_advertises_exactly_the_castless_surface() {
    let server = Server::start().await;

    assert_eq!(
        server.tool_names().await,
        KEYLESS_TOOLS,
        "a keyless kaibo's advertised surface changed"
    );

    server.shutdown().await;
}

/// A real read through the shipped MCP surface: `cat -n` on a file in the served root
/// comes back with its contents.
#[tokio::test]
async fn run_kaish_reads_a_file_under_the_served_root() {
    let server = Server::start().await;

    let out = server.run_kaish("cat -n Cargo.toml").await;

    assert!(
        out.contains("exit: 0"),
        "the read should succeed; got: {out}"
    );
    assert!(
        out.contains(PROJECT_MARKER),
        "the read should return the temp project's file; got: {out}"
    );

    server.shutdown().await;
}

/// The read-only invariant, probed through MCP for the first time: every mutation is
/// refused, and the project on disk is untouched afterwards.
///
/// Two assertions per case on purpose. The refusal text proves kaish said no at the VFS
/// layer rather than failing for some unrelated reason; the filesystem check proves
/// nothing landed even if the text ever changes.
#[tokio::test]
async fn run_kaish_refuses_every_write_and_the_project_is_untouched() {
    let server = Server::start().await;

    for (script, victim) in [
        ("echo x > written.txt", "written.txt"),
        ("mkdir made", "made"),
        ("touch touched.txt", "touched.txt"),
        ("rm Cargo.toml", "no-victim"),
    ] {
        let out = server.run_kaish(script).await;
        assert!(
            out.contains("read-only"),
            "`{script}` must be refused as read-only; got: {out}"
        );
        assert!(
            !out.contains("exit: 0"),
            "`{script}` must not report success; got: {out}"
        );
        assert!(
            !server.root().join(victim).exists(),
            "`{script}` must leave nothing behind, but {victim} exists"
        );
    }

    assert!(
        server.root().join("Cargo.toml").exists(),
        "the seeded file must survive `rm`"
    );

    server.shutdown().await;
}

/// The config resources round-trip: the resolved state and the shipped template both
/// come back as text a model can read.
#[tokio::test]
async fn the_config_resources_round_trip() {
    let server = Server::start().await;

    let resolved = server.read_resource("kaibo://config").await;
    assert!(
        resolved.contains("advertised_tools"),
        "kaibo://config reports the resolved surface; got: {resolved}"
    );
    assert!(
        resolved.contains(&server.root().to_string_lossy().to_string()),
        "kaibo://config names the served root; got: {resolved}"
    );

    let example = server.read_resource("kaibo://config/example").await;
    assert!(
        example.contains("[backends."),
        "kaibo://config/example is the config.toml template; got: {example}"
    );

    server.shutdown().await;
}

/// A `--no-<tool>` flag is a capability gate on the wire, not a hint: the tool is
/// neither advertised nor callable.
///
/// The second half is the one that matters and the one only an MCP-speaking test can
/// reach. `KaiboHandler::run_kaish` is a plain method — an in-process test calls it
/// whatever the gate says — so "did `tools/call` refuse it?" is a question about the
/// router the handler was actually served with. This is the assertion that caught
/// `#[tool_handler]`'s default router rebuilding an ungated one.
#[tokio::test]
async fn a_gated_tool_is_neither_advertised_nor_callable() {
    let server = Server::start_with_args(&["--no-run-kaish"]).await;

    let tools = server.tool_names().await;
    assert!(
        !tools.contains(&"run_kaish".to_string()),
        "--no-run-kaish must drop the tool from tools/list; got {tools:?}"
    );

    let refusal = server
        .try_run_kaish("cat Cargo.toml")
        .await
        .expect_err("a gated tool must refuse the call, not run it");
    // The wording is rmcp's router, not kaibo's — a gated route simply is not there.
    // Pin the substance (a protocol-level refusal that says the tool is absent), not
    // rmcp's exact phrasing.
    let refusal = refusal.to_string();
    assert!(
        refusal.contains("not found"),
        "a gated tool must be refused as absent; got: {refusal}"
    );

    server.shutdown().await;
}

/// The `alwaysLoad` pin reaches the wire on `consult`, and on nothing else.
///
/// Routing and meta injection are separate steps on the same router
/// (`server.rs`, `new_with_env`): the routes survive a change that drops the `_meta`
/// stamp, so every other assertion here would still pass while the front door quietly
/// stopped being resident under a schema-deferring host. This is the assertion that
/// notices. It also pins the pin's *narrowness* — the whole point is that an unused
/// tool bills nothing until the caller reaches for it.
#[tokio::test]
async fn consult_carries_the_always_load_pin_and_no_other_tool_does() {
    let server = Server::start().await;
    let tools = server.tools().await;

    let consult = tools
        .iter()
        .find(|t| t.name == "consult")
        .expect("`consult` is advertised on a keyless server");
    let meta = consult
        .meta
        .as_ref()
        .expect("`consult` carries `_meta` as served over MCP");
    assert_eq!(
        meta.get("anthropic/alwaysLoad"),
        Some(&serde_json::Value::Bool(true)),
        "`consult` must stay resident under a schema-deferring host; its _meta was {meta:?}"
    );

    let also_pinned: Vec<&str> = tools
        .iter()
        .filter(|t| t.name != "consult")
        .filter(|t| {
            t.meta
                .as_ref()
                .is_some_and(|m| m.contains_key("anthropic/alwaysLoad"))
        })
        .map(|t| t.name.as_ref())
        .collect();
    assert!(
        also_pinned.is_empty(),
        "only `consult` is pinned resident, so an unused tool bills nothing until it is \
         reached for; also pinned: {also_pinned:?}"
    );

    server.shutdown().await;
}

/// A raw JSON-RPC exchange over the binary's stdio, bypassing rmcp's client so the
/// test controls the exact `protocolVersion` on the wire. rmcp's client always asks
/// for its own latest, so the version-skew cases below are unreachable through it.
///
/// Same blank-environment rule as [`Server`]: `env_clear()`, then temp `HOME`/XDG
/// roots, so nothing from the developer's shell shapes the server under test.
async fn raw_initialize_then_list_tools(
    client_version: &str,
) -> (serde_json::Value, serde_json::Value, serde_json::Value) {
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

    async fn call(
        stdin: &mut tokio::process::ChildStdin,
        lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        what: &str,
        msg: serde_json::Value,
        id: u64,
    ) -> serde_json::Value {
        stdin
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .expect("write a request line");
        loop {
            let line = bounded(what, lines.next_line())
                .await
                .expect("read a response line")
                .unwrap_or_else(|| panic!("{what}: the server closed stdout"));
            let value: serde_json::Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("{what}: bad JSON: {e}"));
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return value;
            }
        }
    }

    let init = call(
        &mut stdin,
        &mut lines,
        "raw initialize",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": client_version,
                "capabilities": {},
                "clientInfo": { "name": "raw-probe", "version": "0" }
            }
        }),
        1,
    )
    .await;

    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .expect("write the initialized notification");

    let tools = call(
        &mut stdin,
        &mut lines,
        "raw tools/list",
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        2,
    )
    .await;

    let resources = call(
        &mut stdin,
        &mut lines,
        "raw resources/list",
        serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list", "params": {} }),
        3,
    )
    .await;

    child.kill().await.ok();
    (
        init["result"].clone(),
        tools["result"].clone(),
        resources["result"].clone(),
    )
}

/// A session at the newest negotiable protocol version receives every field that
/// version's schema REQUIRES on a list result.
///
/// The failure this pins (live, 2026-08-10): rmcp negotiates `2026-07-28` whenever a
/// client asks for it, but left `ttlMs` and `cacheScope` unset, which SEP-2549 makes
/// REQUIRED on every list/read result at that version. A strictly-validating client
/// (Claude Code) rejected the whole `tools/list` over the two missing fields: zero
/// tools, kaibo unusable until restart. kaibo fills them itself
/// (`sep_2549_cache_fields`, `server/mod.rs`) with the no-caching floor: `ttlMs: 0`,
/// `cacheScope: "private"`.
///
/// A newer rmcp filling the fields is not the signal to delete that helper: 3.1.1
/// fills only the two results its handler macros generate and answers
/// `cacheScope: public`, which is the wrong answer for a per-user surface. This test
/// asserts kaibo's values, so it fails if the SDK's ever start winning.
#[tokio::test]
async fn a_newest_protocol_session_gets_the_fields_its_schema_requires() {
    let (init, tools, resources) = raw_initialize_then_list_tools("2026-07-28").await;

    assert_eq!(
        init["protocolVersion"], "2026-07-28",
        "rmcp echoes a known client version; the fix serves that contract rather \
         than fighting the echo; got: {init}"
    );

    for (what, result) in [("tools/list", &tools), ("resources/list", &resources)] {
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
    let served = tools["tools"].as_array().expect("tools/list carries a tools array");
    assert!(
        !served.is_empty(),
        "the session serves the real tool surface alongside the cache fields"
    );
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
        for field in ["ttlMs", "cacheScope", "resultType"] {
            assert!(
                result.get(field).is_none(),
                "{what}: no newer-contract `{field}` leaks into an older session; \
                 got: {result}"
            );
        }
    }
}
