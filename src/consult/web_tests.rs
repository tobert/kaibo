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
use crate::web::{WebClient, WebSession};

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
    WebSession::new(client, "deepseek", 10, 10)
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
    WebSession::new(client, "deepseek", 10, 10)
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
                    .starts_with("You are the explorer, researching"),
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

/// A team consult with the web: each delegated sweep gets the web tools and the
/// directive; the driver gets neither tool, only the note that its explorer can search.
#[tokio::test]
async fn a_team_consult_gives_the_web_to_the_sweep_not_the_driver() {
    let client = ScriptedClient::builder()
        .on_model(SYNTH, |req| {
            assert!(!has_tool(req, "web_search") && !has_tool(req, "web_fetch"));
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
