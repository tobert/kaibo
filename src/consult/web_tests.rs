//! Offline tests for the web wiring in the consult engine: which phase gets
//! `web_search`/`web_fetch`, which preamble gets the web directive, and that the
//! `research` loop really reaches a web API and returns the explorer's report. The model
//! is scripted (`ScriptedClient`); the web API is a local socket, so no request leaves
//! the machine.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::config::{ConsultConfig, ExploreConfig, PhaseContext, ResearchConfig};
use super::engine::{consult_with, explore_with, research_with, Arm};
use super::prompts::ReportReader;
use super::shaping::ModelCaps;
use crate::test_support::{
    has_tool, preamble, text_response, tool_call_response, transcript_text, ScriptedClient,
};
use crate::web::{WebApi, WebClient, WebSession};

const SYNTH: &str = "capable-synth";
const EXPLORER: &str = "cheap-explorer";

fn arm(client: &ScriptedClient, model: &str) -> Arm {
    Arm::new(
        client.clone(),
        model,
        16384,
        None,
        ModelCaps {
            vision: false,
            tool_result_images: true,
        },
    )
}

/// A web API on a local socket: `/search` answers one result, `/fetch` one page.
/// Serves until the test ends.
///
/// This whole file is test-only (`#[cfg(test)] mod web_tests` in `mod.rs`). The
/// attribute is repeated here because `tests/no_write_path.rs` reads source text and
/// takes everything after the first `#[cfg(test)]` as test code; the socket write below
/// answers a test's HTTP request and touches no filesystem.
#[cfg(test)]
async fn web_api() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len: usize = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let body = if head.starts_with("POST /search") {
                    r#"{"results":[{"title":"Turso notes","url":"https://example.com/turso","snippet":"s","summary":"WEB_SUMMARY","authority":3}]}"#
                } else {
                    r##"{"title":"Turso notes","content":"# Turso\nWEB_PAGE_BODY"}"##
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.ok();
                sock.shutdown().await.ok();
            });
        }
    });
    format!("http://{addr}")
}

async fn session() -> Arc<WebSession> {
    let base = web_api().await;
    let client = WebClient::new(
        format!("{base}/search"),
        format!("{base}/fetch"),
        "k",
        Duration::from_secs(10),
    )
    .unwrap();
    WebSession::new(WebApi::DeepSeek(client), "deepseek", 10, 10)
}

/// An offline session for the toolset tests, which never call the web.
fn idle_session() -> Arc<WebSession> {
    let client = WebClient::new(
        "http://127.0.0.1:9/s",
        "http://127.0.0.1:9/f",
        "k",
        Duration::from_millis(100),
    )
    .unwrap();
    WebSession::new(WebApi::DeepSeek(client), "deepseek", 10, 10)
}

fn consult_cfg(web: Option<Arc<WebSession>>) -> ConsultConfig {
    ConsultConfig {
        explore: ExploreConfig {
            web,
            ..ExploreConfig::default()
        },
        ..ConsultConfig::default()
    }
}

/// The research loop searches, fetches, and returns the explorer's report verbatim; the
/// session records the page as a source and counts both calls.
#[tokio::test]
async fn research_searches_fetches_and_reports() {
    let client = ScriptedClient::builder()
        .on_model(EXPLORER, |req| {
            assert!(has_tool(req, "web_search") && has_tool(req, "web_fetch"));
            assert!(!has_tool(req, "run_kaish"), "research mounts no project");
            assert!(
                preamble(req)
                    .unwrap()
                    .starts_with("You are the explorer. An agent working outside kaibo asked a question that the web"),
                "research runs on its own preamble"
            );
            let seen = transcript_text(req);
            if !seen.contains("WEB_SUMMARY") {
                Ok(tool_call_response(
                    "s1",
                    "web_search",
                    json!({ "query": "turso MP-WAL" }),
                ))
            } else if !seen.contains("WEB_PAGE_BODY") {
                Ok(tool_call_response(
                    "f1",
                    "web_fetch",
                    json!({ "url": "https://example.com/turso" }),
                ))
            } else {
                Ok(text_response(
                    "REPORT: MP-WAL per https://example.com/turso",
                ))
            }
        })
        .build();
    let web = session().await;
    let cfg = ResearchConfig {
        phase: PhaseContext::default(),
        max_turns: 10,
        web: Arc::clone(&web),
    };
    let (report, _) = research_with(
        "is MP-WAL safe?",
        Some("we pin turso 0.8"),
        &arm(&client, EXPLORER),
        &cfg,
    )
    .await
    .unwrap();
    assert_eq!(report, "REPORT: MP-WAL per https://example.com/turso");
    let first = &client.requests_for(EXPLORER)[0];
    let asked = first.user_text.clone();
    assert!(
        asked.contains("we pin turso 0.8"),
        "context rides the first turn: {asked}"
    );
    let usage = web.usage();
    assert_eq!((usage.searches, usage.fetches), (1, 1));
    assert_eq!(web.sources()[0].url, "https://example.com/turso");
}

/// A team consult with the web: each delegated sweep gets both web tools and the
/// directive; the driver gets `web_fetch` alone (one known page) and the note that its
/// explorer searches.
#[tokio::test]
async fn a_team_consult_gives_search_to_the_sweep_and_fetch_to_the_driver() {
    let client = ScriptedClient::builder()
        .on_model(SYNTH, |req| {
            assert!(
                !has_tool(req, "web_search"),
                "searching stays with the sweep"
            );
            assert!(
                has_tool(req, "web_fetch"),
                "the driver can read one known page"
            );
            assert!(
                preamble(req)
                    .unwrap()
                    .ends_with("your next turn is that answer, written out in full."),
                "the obligation is restated after the web note"
            );
            assert!(
                preamble(req)
                    .unwrap()
                    .contains("This call also gives your explorer the web"),
                "the driver is told its explorer can search"
            );
            if !transcript_text(req).contains("REPORT") {
                Ok(tool_call_response(
                    "e1",
                    "explore",
                    json!({ "question": "q" }),
                ))
            } else {
                Ok(text_response("ANSWER"))
            }
        })
        .on_model(EXPLORER, |req| {
            assert!(has_tool(req, "run_kaish"));
            assert!(has_tool(req, "web_search") && has_tool(req, "web_fetch"));
            assert!(
                preamble(req)
                    .unwrap()
                    .contains("This call also gives you the web"),
                "the sweep carries the web directive"
            );
            Ok(text_response("REPORT"))
        })
        .build();
    let dir = tempdir().unwrap();
    let out = consult_with(
        "q",
        dir.path(),
        Some(&arm(&client, EXPLORER)),
        &arm(&client, SYNTH),
        &consult_cfg(Some(idle_session())),
    )
    .await
    .unwrap();
    assert_eq!(out.answer, "ANSWER");
    assert_eq!(client.requests_for(EXPLORER).len(), 1, "the sweep ran");
}

/// A solo consult with the web: the driver searches itself.
#[tokio::test]
async fn a_solo_consult_driver_gets_the_web_itself() {
    let client = ScriptedClient::builder()
        .on_model(SYNTH, |req| {
            assert!(has_tool(req, "run_kaish"));
            assert!(has_tool(req, "web_search") && has_tool(req, "web_fetch"));
            let p = preamble(req).unwrap();
            assert!(
                p.contains("In your answer, cite each web fact by its URL"),
                "{p}"
            );
            Ok(text_response("ANSWER"))
        })
        .build();
    let dir = tempdir().unwrap();
    consult_with(
        "q",
        dir.path(),
        None,
        &arm(&client, SYNTH),
        &consult_cfg(Some(idle_session())),
    )
    .await
    .unwrap();
}

/// Without the web, nothing changes: no web tool anywhere, no directive.
#[tokio::test]
async fn a_consult_without_the_web_has_no_web_tools_or_directive() {
    let client = ScriptedClient::builder()
        .on_model(SYNTH, |req| {
            assert!(!has_tool(req, "web_search"));
            assert!(!preamble(req).unwrap().contains("the web"));
            if !transcript_text(req).contains("REPORT") {
                Ok(tool_call_response(
                    "e1",
                    "explore",
                    json!({ "question": "q" }),
                ))
            } else {
                Ok(text_response("ANSWER"))
            }
        })
        .on_model(EXPLORER, |req| {
            assert!(!has_tool(req, "web_search") && !has_tool(req, "web_fetch"));
            assert!(!preamble(req).unwrap().contains("gives you the web"));
            Ok(text_response("REPORT"))
        })
        .build();
    let dir = tempdir().unwrap();
    consult_with(
        "q",
        dir.path(),
        Some(&arm(&client, EXPLORER)),
        &arm(&client, SYNTH),
        &consult_cfg(None),
    )
    .await
    .unwrap();
}

/// `deliberate`'s dossier stage runs through `explore_with`; with the web on, its
/// explorer gets the tools and the directive.
#[tokio::test]
async fn a_dossier_survey_with_the_web_gets_the_tools() {
    let client = ScriptedClient::builder()
        .on_model(EXPLORER, |req| {
            assert!(has_tool(req, "run_kaish"));
            assert!(has_tool(req, "web_search") && has_tool(req, "web_fetch"));
            assert!(preamble(req)
                .unwrap()
                .contains("This call also gives you the web"));
            Ok(text_response("DOSSIER"))
        })
        .build();
    let dir = tempdir().unwrap();
    let cfg = ExploreConfig {
        web: Some(idle_session()),
        ..ExploreConfig::default()
    };
    let (dossier, _) = explore_with(
        "q",
        dir.path().to_path_buf(),
        &arm(&client, EXPLORER),
        &cfg,
        &[],
        None,
        ReportReader::SynthesisAgent,
    )
    .await
    .unwrap();
    assert_eq!(dossier, "DOSSIER");
}

// --- provider-native web tools ----------------------------------------------

use crate::test_support::with_raw;
use crate::web::native::NativeWeb;

/// The native tool definitions on a request's `additional_params.tools`, by `type`.
fn native_types(req: &rig_core::completion::CompletionRequest) -> Vec<String> {
    req.additional_params
        .as_ref()
        .and_then(|p| p.get("tools"))
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|d| d["type"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn native_arm(client: &ScriptedClient, model: &str) -> Arm {
    let mut a = arm(client, model);
    a.native_web = Some(NativeWeb::Anthropic);
    a
}

/// A native research arm declares the provider's tools instead of kaibo's, and its use
/// is tallied off the raw response into the one session: counts, footer, and sources.
#[tokio::test]
async fn native_research_declares_provider_tools_and_tallies_the_response() {
    let client = ScriptedClient::builder()
        .on_model(EXPLORER, |req| {
            assert_eq!(
                native_types(req),
                vec!["web_search_20250305", "web_fetch_20250910"]
            );
            assert!(!has_tool(req, "web_search") && !has_tool(req, "web_fetch"));
            Ok(with_raw(
                text_response("REPORT per https://a"),
                json!({
                    "stop_reason": "end_turn",
                    "content": [
                        {"type": "web_fetch_tool_result", "tool_use_id": "s", "content": {
                            "type": "web_fetch_result", "url": "https://a",
                            "content": {"title": "A"}}}
                    ],
                    "usage": {"server_tool_use": {"web_search_requests": 2, "web_fetch_requests": 1}}
                }),
            ))
        })
        .build();
    let web = idle_session();
    let cfg = ResearchConfig {
        phase: PhaseContext::default(),
        max_turns: 5,
        web: Arc::clone(&web),
    };
    let (report, _) = research_with("q", None, &native_arm(&client, EXPLORER), &cfg)
        .await
        .unwrap();
    assert_eq!(report, "REPORT per https://a");
    assert_eq!(
        (web.usage().searches, web.usage().fetches),
        (2, 1),
        "native use counts against the one budget"
    );
    assert_eq!(web.remaining(), (8, 9));
    assert_eq!(web.sources()[0].url, "https://a");
    assert_eq!(
        web.footer(),
        "web: 2 searches, 1 fetch via anthropic (native)"
    );
}

/// A Gemini arm declares `google_search` and `url_context` beside its function tools,
/// with the `toolConfig` flag Gemini requires for that mix, and its use is tallied off
/// the `toolCall` parts and `urlContextMetadata` of a GenerateContent reply (shapes from
/// a live probe, 2026-11-13).
#[tokio::test]
async fn gemini_research_declares_built_in_tools_and_tallies_the_reply() {
    let client = ScriptedClient::builder()
        .on_model(EXPLORER, |req| {
            let params = req.additional_params.as_ref().expect("native params");
            assert_eq!(
                params["tools"],
                json!([{"google_search": {}}, {"url_context": {}}]),
                "{params}"
            );
            assert_eq!(
                params["toolConfig"],
                json!({"includeServerSideToolInvocations": true}),
                "{params}"
            );
            assert!(!has_tool(req, "web_search") && !has_tool(req, "web_fetch"));
            Ok(with_raw(
                text_response("REPORT per https://turso.tech/blog/turso-0.8.0"),
                json!({"candidates": [{
                    "content": {"role": "model", "parts": [
                        {"toolCall": {"toolType": "URL_CONTEXT",
                            "args": {"urls": ["https://turso.tech/blog/turso-0.8.0"]}, "id": "c1"}},
                        {"toolCall": {"toolType": "GOOGLE_SEARCH_WEB",
                            "args": {"queries": ["turso mvcc bug", "turso 0.8 issue"]}, "id": "c2"}},
                        {"toolResponse": {"toolType": "URL_CONTEXT", "id": "c1", "response": {}}},
                        {"text": "REPORT per https://turso.tech/blog/turso-0.8.0"}
                    ]},
                    "finishReason": "STOP",
                    "urlContextMetadata": {"urlMetadata": [{
                        "retrievedUrl": "https://turso.tech/blog/turso-0.8.0",
                        "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_SUCCESS"}]},
                    "groundingMetadata": {"groundingChunks": [{"web": {
                        "uri": "https://vertexaisearch.cloud.google.com/grounding-api-redirect/AB",
                        "title": "github.com"}}]}
                }]}),
            ))
        })
        .build();
    let web = idle_session();
    let cfg = ResearchConfig {
        phase: PhaseContext::default(),
        max_turns: 5,
        web: Arc::clone(&web),
    };
    let mut gemini = arm(&client, EXPLORER);
    gemini.native_web = Some(NativeWeb::Gemini);
    research_with("q", None, &gemini, &cfg).await.unwrap();
    assert_eq!((web.usage().searches, web.usage().fetches), (2, 1));
    let sources = web.sources();
    assert_eq!(sources[0].url, "https://turso.tech/blog/turso-0.8.0");
    assert_eq!(sources[0].fetched_flag(), Some(true));
    assert_eq!(sources[1].title, "github.com");
    assert_eq!(sources[1].fetched_flag(), None, "a redirect link is provider-cited");
    assert_eq!(web.footer(), "web: 2 searches, 1 fetch via gemini (native)");
}

/// `[web] native = false` keeps a native-capable arm on kaibo's tools.
#[tokio::test]
async fn native_off_keeps_kaibo_tools() {
    let client = ScriptedClient::builder()
        .on_model(EXPLORER, |req| {
            assert!(native_types(req).is_empty());
            assert!(has_tool(req, "web_search") && has_tool(req, "web_fetch"));
            Ok(text_response("REPORT"))
        })
        .build();
    let api = crate::web::WebApi::DeepSeek(
        WebClient::new(
            "http://127.0.0.1:9/s",
            "http://127.0.0.1:9/f",
            "k",
            Duration::from_millis(100),
        )
        .unwrap(),
    );
    let web = WebSession::with(Some(api), false, "deepseek", 10, 10);
    let cfg = ResearchConfig {
        phase: PhaseContext::default(),
        max_turns: 5,
        web,
    };
    research_with("q", None, &native_arm(&client, EXPLORER), &cfg)
        .await
        .unwrap();
}

/// A team consult with a native synth and a kaibo-run explorer: the driver declares the
/// provider's fetch tool alone, the sweep gets kaibo's two tools.
#[tokio::test]
async fn a_native_team_driver_gets_only_the_provider_fetch_tool() {
    let client = ScriptedClient::builder()
        .on_model(SYNTH, |req| {
            assert_eq!(native_types(req), vec!["web_fetch_20250910"]);
            assert!(
                !has_tool(req, "web_fetch"),
                "no kaibo fetch beside the native one"
            );
            if !transcript_text(req).contains("REPORT") {
                Ok(tool_call_response(
                    "e1",
                    "explore",
                    json!({ "question": "q" }),
                ))
            } else {
                Ok(text_response("ANSWER"))
            }
        })
        .on_model(EXPLORER, |req| {
            assert!(native_types(req).is_empty());
            assert!(has_tool(req, "web_search") && has_tool(req, "web_fetch"));
            Ok(text_response("REPORT"))
        })
        .build();
    let dir = tempdir().unwrap();
    let out = consult_with(
        "q",
        dir.path(),
        Some(&arm(&client, EXPLORER)),
        &native_arm(&client, SYNTH),
        &consult_cfg(Some(idle_session())),
    )
    .await
    .unwrap();
    assert_eq!(out.answer, "ANSWER");
}

/// An Anthropic `pause_turn` (a server-tool turn the provider stopped early) does not end
/// the phase on its fragment: the loop asks the model to continue.
#[tokio::test]
async fn a_paused_turn_is_continued_not_returned() {
    let client = ScriptedClient::builder()
        .on_model(EXPLORER, |req| {
            if transcript_text(req).contains("Your web search paused") {
                Ok(text_response("FULL REPORT"))
            } else {
                Ok(with_raw(
                    text_response("partial"),
                    json!({"stop_reason": "pause_turn", "content": []}),
                ))
            }
        })
        .build();
    let cfg = ResearchConfig {
        phase: PhaseContext::default(),
        max_turns: 5,
        web: idle_session(),
    };
    let (report, _) = research_with("q", None, &native_arm(&client, EXPLORER), &cfg)
        .await
        .unwrap();
    assert_eq!(report, "FULL REPORT");
    assert_eq!(client.requests_for(EXPLORER).len(), 2);
}

/// The native cap follows the budget request by request. Turn one spends 3 searches;
/// turn two's request is capped at what the call has left, which OpenRouter counts per
/// request (7) and Anthropic counts per provider turn, so its cap adds back this phase's
/// own 3 (10).
#[tokio::test]
async fn the_native_cap_follows_the_shared_budget_across_turns() {
    for (kind, second_cap) in [(NativeWeb::OpenRouter, 7), (NativeWeb::Anthropic, 10)] {
        let client = ScriptedClient::builder()
            .on_model(EXPLORER, move |req| {
                let tools = req
                    .additional_params
                    .as_ref()
                    .and_then(|p| p.get("tools"))
                    .cloned()
                    .unwrap_or_default();
                let cap = |t: &serde_json::Value| {
                    t.get("max_uses")
                        .or_else(|| t["parameters"].get("max_uses"))
                        .and_then(|v| v.as_u64())
                };
                let search_cap = tools.as_array().and_then(|a| a.first()).and_then(cap);
                if !transcript_text(req).contains("hi") {
                    assert_eq!(search_cap, Some(10), "{kind:?} first request");
                    Ok(with_raw(
                        tool_call_response("k1", "run_kaish", json!({ "script": "echo hi" })),
                        json!({
                            "usage": {
                                "server_tool_use": {"web_search_requests": 3},
                                "server_tool_use_details": {"web_search_requests": 0}
                            }
                        }),
                    ))
                } else {
                    assert_eq!(search_cap, Some(second_cap), "{kind:?} second request");
                    Ok(text_response("DOSSIER"))
                }
            })
            .build();
        let mut a = arm(&client, EXPLORER);
        a.native_web = Some(kind);
        let web = idle_session();
        let dir = tempdir().unwrap();
        let cfg = ExploreConfig {
            web: Some(Arc::clone(&web)),
            ..ExploreConfig::default()
        };
        explore_with(
            "q",
            dir.path().to_path_buf(),
            &a,
            &cfg,
            &[],
            None,
            ReportReader::SynthesisAgent,
        )
        .await
        .unwrap();
        assert_eq!(web.remaining(), (7, 10), "{kind:?}");
    }
}
