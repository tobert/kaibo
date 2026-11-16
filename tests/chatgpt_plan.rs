//! The ChatGPT plan route end to end and offline: a signed-in `openai` backend, kaibo's
//! real transport stack (`tls::https_client` → `RetryingHttp` → `GrantAuthHttp` → rig's
//! ChatGPT request contract), and a local server playing both OpenAI's token endpoint
//! and `/v1/responses`.
//!
//! What this proves without a ChatGPT account: the request carries the plan route's
//! shape (streamed, `store: false`, no output cap or sampling), the bearer is the stored
//! grant's, an expiring token is refreshed before use, a 401 is refreshed and sent
//! once more, concurrent callers share one refresh, a dead grant is removed with a
//! `kaibo login` next step, and the plan's usage limit is not retried.
//!
//! All tests share one state db: a process registers the first store it opens as the
//! one its signed-in arms read (`oauth::grant`), exactly as the MCP server does. Each
//! test uses its own backend name.

use std::sync::{Arc, Mutex};

use kaibo::config::{Config, ModelRole};
use kaibo::consult::{oneshot, Arm, PhaseContext};
use kaibo::oauth::{AuthProfile, Grant};
use kaibo::store::CredentialStore;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::OnceCell;

/// One request the fake server saw.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: String,
    body: String,
}

impl Seen {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
    fn bearer(&self) -> Option<String> {
        self.headers
            .lines()
            .find_map(|l| l.strip_prefix("authorization: Bearer "))
            .map(|s| s.trim().to_string())
    }
    fn form(&self) -> std::collections::HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

/// A scripted reply: status line, content type, body.
type Reply = (&'static str, &'static str, String);

/// Serve `replies` in order, one connection each, recording every request.
async fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let (base, seen, _) = serve_queue(replies).await;
    (base, seen)
}

type Queue = Arc<Mutex<std::collections::VecDeque<Reply>>>;

/// [`serve`], also returning the reply queue so a test can add replies later.
async fn serve_queue(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Seen>>>, Queue) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let queue: Queue = Arc::new(Mutex::new(replies.into_iter().collect()));
    let replies = queue.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let log = log.clone();
            let replies = replies.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, length) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().to_string())
                            })
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            break (end, length);
                        }
                    }
                };
                let text = String::from_utf8_lossy(&buf).to_string();
                let head = &text[..head_end];
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let headers = head
                    .lines()
                    .skip(1)
                    .map(|l| match l.split_once(':') {
                        Some((k, v)) => format!("{}:{}", k.to_lowercase(), v),
                        None => l.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let body = text[head_end + 4..head_end + 4 + length].to_string();
                log.lock().unwrap().push(Seen {
                    path: path.clone(),
                    headers,
                    body,
                });
                if path == "/oauth/token" {
                    // Hold a refresh open long enough for concurrent callers to pile up
                    // behind it, the window a double-spend would need.
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                let (status, ctype, body) = replies.lock().unwrap().pop_front().unwrap_or((
                    "500 Internal Server Error",
                    "text/plain",
                    "out of scripted replies".into(),
                ));
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), seen, queue)
}

/// A streamed Responses answer of `text`, the event sequence the plan route sends.
fn sse_answer(text: &str) -> Reply {
    let events = [
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_1", "delta": text}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": text, "annotations": []}]}}),
        json!({"type": "response.completed", "response": {
            "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-6.1-sol",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": text, "annotations": []}]}],
            "usage": {"input_tokens": 40, "output_tokens": 7, "total_tokens": 47}}}),
    ];
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    ("200 OK", "text/event-stream", body)
}

fn token_reply(access: &str, refresh: &str) -> Reply {
    (
        "200 OK",
        "application/json",
        json!({"access_token": access, "refresh_token": refresh, "expires_in": 3600,
               "token_type": "Bearer", "scope": "openid resource.invoke"})
        .to_string(),
    )
}

/// The process's one state db (see the module doc).
async fn store() -> &'static CredentialStore {
    static STORE: OnceCell<(CredentialStore, tempfile::TempDir)> = OnceCell::const_new();
    &STORE
        .get_or_init(|| async {
            let dir = tempfile::TempDir::new().unwrap();
            let store = CredentialStore::open(&dir.path().join("credentials/creds.db"), &[])
                .await
                .expect("open credentials db");
            (store, dir)
        })
        .await
        .0
}

/// A signed-in plan backend named `name`, aimed at `base`, and its synth arm.
fn plan_arm(name: &str, base: &str) -> Arm {
    let cfg = Config::from_toml_str(&format!(
        r#"
        [backends.{name}]
        kind = "openai"
        auth = "chatgpt"
        base_url = "{base}/v1"
        request_timeout_secs = 10

        [casts.{name}]
        synth = "{name}/gpt-6.1-sol"
        "#
    ))
    .expect("config loads");
    let backend = cfg.resolve_backend(name).unwrap().clone();
    let slot = cfg
        .resolve_cast(name)
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap()
        .clone();
    Arm::from_slot(&backend, &slot, ModelRole::Synth, &cfg.defaults).expect("arm builds")
}

fn grant(backend: &str, base: &str, access: &str, expires_in: i64) -> Grant {
    Grant {
        backend: backend.to_string(),
        auth: AuthProfile::Chatgpt,
        client_id: Some("oaiapp_test".into()),
        token_url: format!("{base}/oauth/token"),
        access_token: access.to_string(),
        refresh_token: Some("rt-1".into()),
        expires_at: Some(kaibo::oauth::grant::now() + expires_in),
        account: Some("a@example.com".into()),
        scopes: None,
    }
}

#[tokio::test]
async fn a_plan_request_carries_the_plan_shape_and_the_stored_token() {
    let store = store().await;
    let (base, seen) = serve(vec![sse_answer("391")]).await;
    store
        .put_grant(&grant("plan-shape", &base, "at-1", 3600))
        .await
        .unwrap();

    let (answer, usage) = oneshot(
        "what is 17 * 23?",
        &[],
        &plan_arm("plan-shape", &base),
        &PhaseContext::default(),
    )
    .await
    .expect("the plan route answers");
    assert_eq!(answer, "391");
    assert_eq!(usage.input_tokens, Some(40));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let req = &seen[0];
    assert_eq!(req.path, "/v1/responses");
    assert_eq!(
        req.bearer().as_deref(),
        Some("at-1"),
        "the grant's token, not a placeholder"
    );
    let body = req.json();
    assert_eq!(body["stream"], true, "the plan route only streams: {body}");
    assert_eq!(body["store"], false, "{body}");
    for rejected in [
        "max_output_tokens",
        "temperature",
        "top_p",
        "metadata",
        "user",
        "previous_response_id",
    ] {
        assert!(
            body.get(rejected).is_none(),
            "the plan route rejects `{rejected}`: {body}"
        );
    }
    assert_eq!(body["model"], "gpt-6.1-sol");
    assert_eq!(
        body["reasoning"]["effort"], "high",
        "the per-role effort ships: {body}"
    );
    let input = body["input"].as_array().expect("input items");
    assert!(
        input.iter().all(|i| i["role"] != "system"),
        "no system-role input items; the preamble goes in `instructions`: {body}"
    );
    // rig's own ChatGPT identity is dropped.
    assert!(!req.headers.contains("originator:"), "{}", req.headers);
    assert!(
        !req.headers.contains("chatgpt-account-id:"),
        "{}",
        req.headers
    );
}

#[tokio::test]
async fn an_expiring_token_is_refreshed_before_the_request() {
    let store = store().await;
    let (base, seen) = serve(vec![token_reply("at-2", "rt-2"), sse_answer("ok")]).await;
    // Inside the five-minute margin: refreshed before use.
    store
        .put_grant(&grant("plan-expiring", &base, "at-1", 60))
        .await
        .unwrap();

    let (answer, _) = oneshot(
        "q",
        &[],
        &plan_arm("plan-expiring", &base),
        &PhaseContext::default(),
    )
    .await
    .expect("answers after a refresh");
    assert_eq!(answer, "ok");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].path, "/oauth/token");
    let form = seen[0].form();
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "rt-1");
    assert_eq!(form["client_id"], "oaiapp_test");
    assert_eq!(form["resource"], "https://api.openai.com/v1");
    assert!(!form.contains_key("scope"), "SIWC: omit scope on refresh");
    assert_eq!(seen[1].bearer().as_deref(), Some("at-2"));

    let stored = store.get_grant("plan-expiring").await.unwrap().unwrap();
    assert_eq!(stored.access_token, "at-2");
    assert_eq!(
        stored.refresh_token.as_deref(),
        Some("rt-2"),
        "the rotated refresh token is kept"
    );
}

#[tokio::test]
async fn a_401_is_refreshed_and_sent_once_more() {
    let store = store().await;
    let (base, seen) = serve(vec![
        (
            "401 Unauthorized",
            "application/json",
            r#"{"error":{"message":"expired"}}"#.into(),
        ),
        token_reply("at-new", "rt-2"),
        sse_answer("ok"),
    ])
    .await;
    store
        .put_grant(&grant("plan-401", &base, "at-old", 3600))
        .await
        .unwrap();

    let (answer, _) = oneshot(
        "q",
        &[],
        &plan_arm("plan-401", &base),
        &PhaseContext::default(),
    )
    .await
    .expect("answers after one refresh");
    assert_eq!(answer, "ok");
    let seen = seen.lock().unwrap();
    let paths: Vec<_> = seen.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(paths, ["/v1/responses", "/oauth/token", "/v1/responses"]);
    assert_eq!(seen[0].bearer().as_deref(), Some("at-old"));
    assert_eq!(seen[2].bearer().as_deref(), Some("at-new"));
}

#[tokio::test]
async fn concurrent_callers_share_one_refresh() {
    let store = store().await;
    let mut replies = vec![token_reply("at-2", "rt-2")];
    for _ in 0..4 {
        replies.push(sse_answer("ok"));
    }
    let (base, seen) = serve(replies).await;
    store
        .put_grant(&grant("plan-concurrent", &base, "at-1", 0))
        .await
        .unwrap();

    let arm = Arc::new(plan_arm("plan-concurrent", &base));
    // Spawn all four before awaiting any, so they overlap: the token endpoint holds
    // its reply (see `serve`), and the other three arrive while the refresh is open.
    let calls: Vec<_> = (0..4)
        .map(|_| {
            let arm = arm.clone();
            tokio::spawn(async move { oneshot("q", &[], &arm, &PhaseContext::default()).await })
        })
        .collect();
    for call in calls {
        call.await.unwrap().expect("every caller answers");
    }
    let seen = seen.lock().unwrap();
    let refreshes = seen.iter().filter(|s| s.path == "/oauth/token").count();
    assert_eq!(
        refreshes, 1,
        "a rotating refresh token must be spent once: {seen:?}"
    );
    assert!(seen
        .iter()
        .filter(|s| s.path == "/v1/responses")
        .all(|s| s.bearer().as_deref() == Some("at-2")));
}

#[tokio::test]
async fn a_dead_grant_is_removed_with_a_login_next_step() {
    let store = store().await;
    let (base, _seen) = serve(vec![(
        "400 Bad Request",
        "application/json",
        r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#.into(),
    )])
    .await;
    store
        .put_grant(&grant("plan-dead", &base, "at-1", 0))
        .await
        .unwrap();

    let err = oneshot(
        "q",
        &[],
        &plan_arm("plan-dead", &base),
        &PhaseContext::default(),
    )
    .await
    .expect_err("a revoked grant cannot answer");
    let msg = format!("{err:#}");
    assert!(msg.contains("kaibo login plan-dead"), "{msg}");
    assert!(
        store.get_grant("plan-dead").await.unwrap().is_none(),
        "the dead grant is gone"
    );
}

#[tokio::test]
async fn no_sign_in_names_kaibo_login() {
    store().await;
    let (base, seen) = serve(vec![]).await;
    let err = oneshot(
        "q",
        &[],
        &plan_arm("plan-none", &base),
        &PhaseContext::default(),
    )
    .await
    .expect_err("no grant, no request");
    let msg = format!("{err:#}");
    assert!(msg.contains("kaibo login plan-none"), "{msg}");
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing is sent without a credential"
    );
}

#[tokio::test]
async fn the_plan_usage_limit_is_not_retried() {
    let store = store().await;
    let (base, seen) = serve(vec![
        (
            "429 Too Many Requests",
            "application/json",
            r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit"}}"#
                .into(),
        ),
        sse_answer("should not be reached"),
    ])
    .await;
    store
        .put_grant(&grant("plan-limit", &base, "at-1", 3600))
        .await
        .unwrap();

    let err = oneshot(
        "q",
        &[],
        &plan_arm("plan-limit", &base),
        &PhaseContext::default(),
    )
    .await
    .expect_err("the limit stops the call");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ChatGPT") && msg.contains("Usage"),
        "names the plan limit: {msg}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "a spent plan is not asked again"
    );
}

#[tokio::test]
async fn an_unavailable_plan_is_retried() {
    let store = store().await;
    let (base, seen) = serve(vec![
        (
            "503 Service Unavailable",
            "application/json",
            r#"{"error":{"code":"subscription_sharing_usage_unavailable"}}"#.into(),
        ),
        sse_answer("ok"),
    ])
    .await;
    store
        .put_grant(&grant("plan-503", &base, "at-1", 3600))
        .await
        .unwrap();
    let (answer, _) = oneshot(
        "q",
        &[],
        &plan_arm("plan-503", &base),
        &PhaseContext::default(),
    )
    .await
    .expect("the retry answers");
    assert_eq!(answer, "ok");
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// kaibo's own client builder: ring installed, as every kaibo client is.
fn client() -> reqwest::Client {
    kaibo::tls::https_client(std::time::Duration::from_secs(10)).unwrap()
}

/// An unsigned JWT carrying `claims`, the shape an ID token has on the wire.
fn id_token(claims: Value) -> String {
    use base64::Engine as _;
    let enc = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    format!(
        "{}.{}.sig",
        enc(br#"{"alg":"RS256"}"#),
        enc(claims.to_string().as_bytes())
    )
}

/// Act as the browser: wait for kaibo's URL, then follow the provider's redirect to the
/// callback with `code` and the URL's own state, plus an issued client id.
async fn browse(url_rx: tokio::sync::oneshot::Receiver<String>, queue: Queue) -> (String, String) {
    let url = url::Url::parse(&url_rx.await.unwrap()).unwrap();
    let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    // The token endpoint will mint an ID token for exactly this attempt.
    queue.lock().unwrap().push_back((
        "200 OK",
        "application/json",
        json!({
            "access_token": "at-login", "refresh_token": "rt-login", "expires_in": 3600,
            "token_type": "Bearer", "scope": "openid email resource.invoke",
            "id_token": id_token(json!({
                "iss": "https://auth.openai.com", "aud": "oaiapp_issued", "nonce": q["nonce"],
                "exp": kaibo::oauth::grant::now() + 600, "sub": "user-1", "email": "a@example.com"
            })),
        })
        .to_string(),
    ));
    let callback = format!(
        "{}?code=the-code&state={}&client_id=oaiapp_issued",
        q["redirect_uri"], q["state"]
    );
    let body = client()
        .get(&callback)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    (body, q["ext_agent_host_id"].clone())
}

#[tokio::test]
async fn login_registers_exchanges_and_stores_a_grant() {
    let store = store().await;
    let (base, seen, queue) = serve_queue(vec![]).await;
    let http = client();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let browser = tokio::spawn(browse(rx, queue));

    let grant = kaibo::oauth::sign_in(
        &kaibo::oauth::CHATGPT,
        "plan-login",
        store,
        &http,
        &format!("{base}/oauth/token"),
        std::time::Duration::from_secs(10),
        |url, _port| tx.send(url.to_string()).unwrap(),
    )
    .await
    .expect("the sign-in completes");
    let (page, host_id) = browser.await.unwrap();
    assert!(page.contains("received the sign-in"), "{page}");

    assert_eq!(grant.client_id.as_deref(), Some("oaiapp_issued"));
    assert_eq!(grant.account.as_deref(), Some("a@example.com"));
    assert_eq!(store.get_grant("plan-login").await.unwrap(), Some(grant));
    assert_eq!(
        store.host_id().await.unwrap(),
        host_id,
        "one stable host id per machine"
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let form = seen[0].form();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "the-code");
    assert_eq!(form["client_id"], "oaiapp_issued");
    assert_eq!(form["resource"], "https://api.openai.com/v1");
    assert!(
        form["redirect_uri"].starts_with("http://127.0.0.1:"),
        "{form:?}"
    );
    assert_eq!(form["code_verifier"].len(), 43);
}

#[tokio::test]
async fn a_tampered_id_token_fails_the_login_and_stores_nothing() {
    let store = store().await;
    let (base, _seen, queue) = serve_queue(vec![]).await;
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    // A browser whose token endpoint answers with someone else's nonce.
    let browser = tokio::spawn(async move {
        let url = url::Url::parse(&rx.await.unwrap()).unwrap();
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        queue.lock().unwrap().push_back((
            "200 OK",
            "application/json",
            json!({"access_token": "at", "refresh_token": "rt", "expires_in": 3600,
                   "id_token": id_token(json!({"iss": "https://auth.openai.com", "aud": "oaiapp_x",
                       "nonce": "not-this-attempt", "exp": kaibo::oauth::grant::now() + 600}))})
            .to_string(),
        ));
        let callback = format!(
            "{}?code=c&state={}&client_id=oaiapp_x",
            q["redirect_uri"], q["state"]
        );
        let _ = client().get(&callback).send().await;
    });
    let err = kaibo::oauth::sign_in(
        &kaibo::oauth::CHATGPT,
        "plan-tampered",
        store,
        &client(),
        &format!("{base}/oauth/token"),
        std::time::Duration::from_secs(10),
        |url, _| tx.send(url.to_string()).unwrap(),
    )
    .await
    .expect_err("a nonce mismatch is refused");
    browser.await.unwrap();
    assert!(format!("{err:#}").contains("nonce"), "{err:#}");
    assert!(store.get_grant("plan-tampered").await.unwrap().is_none());
}

#[tokio::test]
async fn models_lists_a_signed_in_backend_with_its_token_and_plan_visibility() {
    let store = store().await;
    let (base, seen) = serve(vec![(
        "200 OK",
        "application/json",
        // The plan route's catalog, as observed live 2026-11-15: a `models` array
        // keyed by `slug`, not the Platform's `data`/`id`.
        json!({"models": [
            {"slug": "gpt-6.1-sol", "display_name": "GPT-6.1 Sol", "visibility": "list",
             "context_window": 500000, "supported_in_api": true},
            {"slug": "internal-only", "visibility": "hide", "supported_in_api": true},
            {"slug": "codex-only", "visibility": "list", "supported_in_api": false},
        ]})
        .to_string(),
    )])
    .await;
    store
        .put_grant(&grant("plan-models", &base, "at-models", 3600))
        .await
        .unwrap();
    let cfg = Config::from_toml_str(&format!(
        "[backends.plan-models]\nkind = \"openai\"\nauth = \"chatgpt\"\nbase_url = \"{base}/v1\"\n"
    ))
    .unwrap();
    let backend = cfg.resolve_backend("plan-models").unwrap();
    let fetcher = kaibo::discover::HttpModelListFetcher::new(std::time::Duration::from_secs(10));
    let models = kaibo::discover::discover_models(backend, &fetcher)
        .await
        .expect("lists");
    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["gpt-6.1-sol"],
        "only `visibility: list` models are callable"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].path, "/v1/models");
    assert_eq!(seen[0].bearer().as_deref(), Some("at-models"));
}

#[tokio::test]
async fn models_refreshes_once_when_the_plan_refuses_the_token() {
    let store = store().await;
    let (base, seen) = serve(vec![
        ("401 Unauthorized", "application/json", json!({"error": {"code": "invalid_token"}}).to_string()),
        token_reply("at-new", "rt-new"),
        (
            "200 OK",
            "application/json",
            json!({"models": [{"slug": "gpt-6.1-sol", "visibility": "list"}]}).to_string(),
        ),
    ])
    .await;
    // Not due for refresh by its clock: only the 401 says the token is bad.
    store
        .put_grant(&grant("plan-models-401", &base, "at-old", 3600))
        .await
        .unwrap();
    let cfg = Config::from_toml_str(&format!(
        "[backends.plan-models-401]\nkind = \"openai\"\nauth = \"chatgpt\"\nbase_url = \"{base}/v1\"\n"
    ))
    .unwrap();
    let backend = cfg.resolve_backend("plan-models-401").unwrap();
    let fetcher = kaibo::discover::HttpModelListFetcher::new(std::time::Duration::from_secs(10));
    let models = kaibo::discover::discover_models(backend, &fetcher)
        .await
        .expect("lists after one refresh");
    assert_eq!(models.len(), 1);
    let seen = seen.lock().unwrap();
    let paths: Vec<_> = seen.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(paths, ["/v1/models", "/oauth/token", "/v1/models"]);
    assert_eq!(seen[0].bearer().as_deref(), Some("at-old"));
    assert_eq!(seen[2].bearer().as_deref(), Some("at-new"));
}

#[tokio::test]
async fn openrouter_sign_in_mints_and_stores_a_key() {
    let store = store().await;
    let (base, seen) = serve(vec![(
        "200 OK",
        "application/json",
        json!({"key": "sk-or-minted", "user_id": "user_or"}).to_string(),
    )])
    .await;
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let browser = tokio::spawn(async move {
        let url = url::Url::parse(&rx.await.unwrap()).unwrap();
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        // OpenRouter redirects to callback_url with `?code=` appended.
        let callback = format!("{}?code=or-code", q["callback_url"]);
        // `localhost` in the URL; the listener answers on both loopback families.
        client()
            .get(&callback)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    let grant = kaibo::oauth::sign_in(
        &kaibo::oauth::OPENROUTER,
        "or-login",
        store,
        &client(),
        &format!("{base}/api/v1/auth/keys"),
        std::time::Duration::from_secs(10),
        |url, _| tx.send(url.to_string()).unwrap(),
    )
    .await
    .expect("the key mint completes");
    assert!(browser.await.unwrap().contains("received the sign-in"));
    assert_eq!(grant.access_token, "sk-or-minted");
    assert_eq!(grant.refresh_token, None);
    assert_eq!(grant.expires_at, None, "a minted key does not expire");

    let seen = seen.lock().unwrap().clone();
    let body = seen[0].json();
    assert_eq!(seen[0].path, "/api/v1/auth/keys");
    assert_eq!(body["code"], "or-code");
    assert_eq!(body["code_challenge_method"], "S256");
    assert_eq!(body["code_verifier"].as_str().unwrap().len(), 43);

    // The stored key is what a request sends, with nothing to refresh.
    let source = kaibo::oauth::TokenSource::new(
        "or-login",
        AuthProfile::OpenRouter,
        store.clone(),
        client(),
    );
    assert_eq!(source.bearer().await.unwrap(), "sk-or-minted");
}

#[tokio::test]
async fn a_plan_limit_inside_the_stream_names_the_plan() {
    let store = store().await;
    let failed = json!({"type": "response.failed", "response": {
        "id": "resp_1", "status": "failed", "output": [],
        "error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "usage limit reached"}}});
    let body = format!("event: response.failed\ndata: {failed}\n\n");
    let (base, _seen) = serve(vec![("200 OK", "text/event-stream", body)]).await;
    store
        .put_grant(&grant("plan-midstream", &base, "at-1", 3600))
        .await
        .unwrap();
    let err = oneshot(
        "q",
        &[],
        &plan_arm("plan-midstream", &base),
        &PhaseContext::default(),
    )
    .await
    .expect_err("a failed stream is an error");
    let msg = format!("{err:#}");
    // The failure renderer recognizes a spent plan only by this exact code, read from
    // the same `{err:#}` string, so the code itself has to survive the stream path.
    assert!(
        msg.contains(kaibo::completion_retry::PLAN_USAGE_LIMIT_CODE),
        "the plan-limit code must reach the renderer: {msg}"
    );
    eprintln!("mid-stream plan limit surfaces as: {msg}");
}

/// A streamed Responses turn that calls `run_kaish` with `script`.
fn sse_tool_call(script: &str) -> Reply {
    let args = json!({ "script": script }).to_string();
    let item = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "run_kaish",
                      "arguments": args, "status": "completed"});
    let events = [
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "run_kaish", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": args}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": "resp_t", "object": "response",
               "status": "completed", "model": "gpt-6.1-sol", "output": [item],
               "usage": {"input_tokens": 30, "output_tokens": 5, "total_tokens": 35}}}),
    ];
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    ("200 OK", "text/event-stream", body)
}

#[tokio::test]
async fn a_consult_tool_loop_runs_on_the_plan_route() {
    let store = store().await;
    let (base, seen) = serve(vec![
        sse_tool_call("cat -n src/foo.rs"),
        sse_answer("target_marker is at src/foo.rs:1"),
    ])
    .await;
    store
        .put_grant(&grant("plan-loop", &base, "at-1", 3600))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/foo.rs"), "fn target_marker() {}\n").unwrap();

    let out = kaibo::consult::consult(
        "Where is target_marker defined?",
        None,
        dir.path(),
        None,
        &plan_arm("plan-loop", &base),
        &kaibo::consult::ConsultConfig::default(),
        None,
    )
    .await
    .expect("the plan route drives the tool loop");
    assert!(out.answer.contains("src/foo.rs:1"), "{}", out.answer);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "{seen:?}");
    let first = seen[0].json();
    let tools = first["tools"].as_array().expect("the loop offers tools");
    assert!(tools.iter().any(|t| t["name"] == "run_kaish"), "{first}");
    let second = seen[1].json();
    for rejected in [
        "max_output_tokens",
        "temperature",
        "top_p",
        "previous_response_id",
    ] {
        assert!(
            second.get(rejected).is_none(),
            "`{rejected}` on a loop turn: {second}"
        );
    }
    let input = second["input"].as_array().unwrap();
    let output = input
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("the tool result rides in `input`, stateless");
    assert!(
        output["output"].to_string().contains("target_marker"),
        "{output}"
    );
}

#[tokio::test]
async fn a_grant_from_another_sign_in_is_not_sent() {
    // The backend's `auth` changed since this grant was stored: an OpenRouter key must
    // never reach OpenAI.
    let store = store().await;
    let (base, seen) = serve(vec![sse_answer("should not be reached")]).await;
    let mut other = grant("plan-switched", &base, "sk-or-old", 3600);
    other.auth = AuthProfile::OpenRouter;
    other.expires_at = None;
    store.put_grant(&other).await.unwrap();
    let err = oneshot(
        "q",
        &[],
        &plan_arm("plan-switched", &base),
        &PhaseContext::default(),
    )
    .await
    .expect_err("a foreign grant is not this backend's credential");
    assert!(
        format!("{err:#}").contains("kaibo login plan-switched"),
        "{err:#}"
    );
    assert!(seen.lock().unwrap().is_empty(), "nothing sent");
}
