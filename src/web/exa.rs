//! Exa's Search and Contents APIs, behind the same [`WebClient`] and [`WebApi`] the
//! DeepSeek route uses.
//!
//! `POST {base}/search` takes a query and returns ranked results; `POST {base}/contents`
//! takes URLs and returns each page's text. Exa reads every page on its own servers, so
//! the rule in the parent module holds: **kaibo never opens a URL a model chose.**
//!
//! Wire facts, from live probes on 2026-11-13:
//! - Exa accepts `Authorization: Bearer <key>` as well as `x-api-key`, so the shared
//!   client's bearer auth carries it.
//! - `site:` inside the query narrows results, as it does on DeepSeek, so the
//!   `web_search` description stays true on both.
//! - `contents.highlights` with a `query` returns the passages that answer that query,
//!   at no charge beyond the search ($0.007 per search of up to 10 results).
//!   kaibo passes the model's `question` there, the same hint DeepSeek takes as
//!   `ori_question`.
//! - `/contents` answers 200 even when a page cannot be read: the failure is in
//!   `statuses[].error` (`httpStatusCode`, `tag`), and `results` is empty for that URL.
//!   Each page read costs $0.001.
//! - Errors are `{"error": "<text>", "tag": "<TAG>"}`, not the OpenAI envelope.

use serde::Deserialize;
use serde_json::{json, Value};

use super::{FetchedPage, SearchRequest, SearchResult, WebClient, WebError};

/// The environment variable that overrides the key file.
pub const EXA_KEY_ENV_VAR: &str = "EXA_API_KEY";
/// The key file's name within `$HOME`.
pub const EXA_KEY_FILE_NAME: &str = ".exa-key";
/// Exa's API root. A backend's `base_url` replaces it, which is how tests aim the
/// client at a local listener.
pub const EXA_API_BASE: &str = "https://api.exa.ai";

/// Characters of highlight text per result. Near the length of a DeepSeek summary, so a
/// model reads results of the same size from either API.
const HIGHLIGHT_CHARS: u32 = 800;

/// The Search request body for `req`.
pub(crate) fn search_body(req: &SearchRequest) -> Value {
    let mut highlights = json!({ "maxCharacters": HIGHLIGHT_CHARS });
    if let Some(q) = &req.ori_question {
        highlights["query"] = json!(q);
    }
    json!({
        "query": req.query,
        "numResults": req.count,
        "type": "auto",
        "contents": { "highlights": highlights },
    })
}

/// The Contents request body for one URL. The full page text is asked for: kaibo's
/// own page limits cut it, and say so to the model, which a provider-side cut would not.
pub(crate) fn fetch_body(url: &str) -> Value {
    json!({ "urls": [url], "text": true })
}

#[derive(Debug, Deserialize)]
struct ExaSearchResponse {
    #[serde(default)]
    results: Vec<ExaResult>,
}

#[derive(Debug, Deserialize)]
struct ExaContentsResponse {
    #[serde(default)]
    results: Vec<ExaResult>,
    #[serde(default)]
    statuses: Vec<ExaStatus>,
}

/// One result, from either endpoint. Every field is defaulted, so a result missing one
/// reads as empty rather than failing the whole call.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
    #[serde(default)]
    highlights: Vec<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExaStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<ExaStatusError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaStatusError {
    #[serde(default)]
    http_status_code: Option<u16>,
    #[serde(default)]
    tag: Option<String>,
}

/// Read a Search response into kaibo's result shape. Exa names no site, so the host
/// stands in for it; the highlights become the summary a model reads.
pub(crate) fn parse_search(body: &str) -> Result<Vec<SearchResult>, WebError> {
    let resp: ExaSearchResponse =
        serde_json::from_str(body).map_err(|e| WebError::Unreadable(e.to_string()))?;
    Ok(resp
        .results
        .into_iter()
        .map(|r| SearchResult {
            site_name: reqwest::Url::parse(&r.url).ok().and_then(|u| {
                u.host_str()
                    .map(|h| h.trim_start_matches("www.").to_string())
            }),
            title: r.title.unwrap_or_default(),
            summary: r
                .highlights
                .iter()
                .map(|h| h.trim())
                .filter(|h| !h.is_empty())
                .collect::<Vec<_>>()
                .join(" … "),
            url: r.url,
            date: r.published_date,
            ..SearchResult::default()
        })
        .collect())
}

/// Read a Contents response for `url`. A page Exa could not read comes back as a 200
/// with an error status, which becomes the same [`WebError::Provider`] a DeepSeek
/// failure would, so the model reads the same guidance for a dead page from either API.
pub(crate) fn parse_fetch(body: &str, url: &str) -> Result<FetchedPage, WebError> {
    let resp: ExaContentsResponse =
        serde_json::from_str(body).map_err(|e| WebError::Unreadable(e.to_string()))?;
    if let Some(page) = resp.results.into_iter().next() {
        return Ok(FetchedPage {
            title: page.title.unwrap_or_default(),
            content: page.text.unwrap_or_default(),
        });
    }
    let failed = resp.statuses.into_iter().find(|s| s.status != "success");
    let (status, tag) = match failed.and_then(|s| s.error) {
        Some(e) => (
            e.http_status_code.unwrap_or(502),
            e.tag.unwrap_or_else(|| "no error tag".into()),
        ),
        None => (502, "no page and no status".into()),
    };
    Err(WebError::Provider {
        status,
        message: format!("Exa could not read {url}: {tag}"),
    })
}

impl WebClient {
    /// A client for Exa at `base_url` (normally [`EXA_API_BASE`]).
    pub fn exa(
        key: impl Into<String>,
        base_url: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let base = base_url.trim_end_matches('/');
        Self::new(
            format!("{base}/search"),
            format!("{base}/contents"),
            key,
            timeout,
        )
    }

    /// Run one Exa search. An empty result list is a success.
    pub async fn exa_search(&self, req: &SearchRequest) -> Result<Vec<SearchResult>, WebError> {
        let body = self.post(&self.search_url, &search_body(req)).await?;
        parse_search(&body)
    }

    /// Read one page through Exa. `url` should already have passed `check_fetch_url`.
    pub async fn exa_fetch(&self, url: &str) -> Result<FetchedPage, WebError> {
        let body = self.post(&self.fetch_url, &fetch_body(url)).await?;
        parse_fetch(&body, url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_body_carries_the_query_count_and_question() {
        let req = SearchRequest::new("turso MVCC site:docs.rs", 8, Some("is MVCC stable")).unwrap();
        assert_eq!(
            search_body(&req),
            json!({
                "query": "turso MVCC site:docs.rs",
                "numResults": 8,
                "type": "auto",
                "contents": {"highlights": {"maxCharacters": 800, "query": "is MVCC stable"}},
            })
        );
        let bare = search_body(&SearchRequest::new("q", 3, None).unwrap());
        assert!(
            bare["contents"]["highlights"].get("query").is_none(),
            "{bare}"
        );
    }

    #[test]
    fn a_search_response_reads_into_results() {
        let body = r#"{"requestId":"r","results":[
            {"id":"https://www.turso.tech/blog/0.8","url":"https://www.turso.tech/blog/0.8",
             "title":"Turso 0.8","publishedDate":"2026-07-01T00:00:00.000Z",
             "highlights":["Concurrent writes.\n", "  ", "MVCC is beta."]},
            {"url":"https://a.example/x"}
        ],"costDollars":{"total":0.007}}"#;
        let r = parse_search(body).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Turso 0.8");
        assert_eq!(r[0].site_name.as_deref(), Some("turso.tech"));
        assert_eq!(r[0].date.as_deref(), Some("2026-07-01T00:00:00.000Z"));
        assert_eq!(r[0].summary, "Concurrent writes. … MVCC is beta.");
        assert_eq!(r[0].authority, None);
        assert_eq!(r[1].title, "");
        assert_eq!(r[1].summary, "");
        assert!(parse_search(r#"{"results":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn a_contents_response_reads_into_a_page() {
        let body = r##"{"results":[{"url":"https://a.example/","title":"A","text":"# A\nbody"}],
            "statuses":[{"id":"https://a.example/","status":"success","source":"cached"}]}"##;
        let page = parse_fetch(body, "https://a.example/").unwrap();
        assert_eq!(page.title, "A");
        assert_eq!(page.content, "# A\nbody");
    }

    /// A dead page is a 200 with an error status. It must reach the model as a provider
    /// error with Exa's status code, so the 404 guidance ("choose a different source")
    /// fires the way it does for DeepSeek.
    #[test]
    fn a_failed_status_becomes_a_provider_error() {
        let body = r#"{"results":[],"statuses":[{"id":"https://gone.example/","status":"error",
            "error":{"httpStatusCode":404,"tag":"CRAWL_NOT_FOUND"}}]}"#;
        let e = parse_fetch(body, "https://gone.example/").unwrap_err();
        assert_eq!(
            e,
            WebError::Provider {
                status: 404,
                message: "Exa could not read https://gone.example/: CRAWL_NOT_FOUND".into()
            }
        );
        assert!(e.to_string().contains("Choose a different source"), "{e}");
        let e = parse_fetch(r#"{"results":[],"statuses":[]}"#, "https://x.example/").unwrap_err();
        assert!(matches!(e, WebError::Provider { status: 502, .. }), "{e:?}");
    }

    /// An Exa stand-in on a local socket that records each request (head and body) and
    /// answers `/search` and `/contents` with Exa's shapes.
    async fn exa_listener() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let log = log.clone();
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
                    let req = String::from_utf8_lossy(&buf).to_string();
                    let body = if req.starts_with("POST /search") {
                        r#"{"results":[{"url":"https://turso.tech/blog/0.8","title":"Turso 0.8","highlights":["EXA_HIGHLIGHT"]}]}"#
                    } else {
                        r#"{"results":[{"url":"https://turso.tech/blog/0.8","title":"Turso 0.8","text":"EXA_PAGE_TEXT"}],"statuses":[{"id":"https://turso.tech/blog/0.8","status":"success"}]}"#
                    };
                    log.lock().unwrap().push(req);
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
        (format!("http://{addr}"), seen)
    }

    /// The whole route: a session on an Exa client sends Exa's paths and bodies with the
    /// key as a bearer token, formats the highlights as the result summary, and records
    /// the fetched page as a source read.
    #[tokio::test]
    async fn a_session_on_exa_searches_and_fetches_through_exa_shapes() {
        use crate::progress::NullSink;
        let (base, seen) = exa_listener().await;
        let client =
            WebClient::exa("exa-test-key", &base, std::time::Duration::from_secs(10)).unwrap();
        let session = super::super::WebSession::new(super::super::WebApi::Exa(client), "exa", 5, 5);

        let results = session
            .search("turso 0.8", Some("what changed"), &NullSink)
            .await
            .unwrap();
        assert!(results.contains("EXA_HIGHLIGHT"), "{results}");
        assert!(results.contains("turso.tech"), "{results}");
        let page = session
            .fetch("https://turso.tech/blog/0.8", &NullSink)
            .await
            .unwrap();
        assert!(page.contains("EXA_PAGE_TEXT"), "{page}");
        assert_eq!(session.usage().searches, 1);
        assert_eq!(session.usage().fetches, 1);
        assert!(session.footer().contains("via exa"), "{}", session.footer());

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].starts_with("POST /search "), "{}", seen[0]);
        assert!(seen[1].starts_with("POST /contents "), "{}", seen[1]);
        for req in seen.iter() {
            assert!(
                req.to_ascii_lowercase()
                    .contains("authorization: bearer exa-test-key"),
                "{req}"
            );
        }
        assert!(seen[0].contains(r#""numResults":8"#), "{}", seen[0]);
        assert!(seen[0].contains(r#""query":"what changed""#), "{}", seen[0]);
        assert!(
            seen[1].contains(r#""urls":["https://turso.tech/blog/0.8"]"#),
            "{}",
            seen[1]
        );
    }

    #[test]
    fn the_client_builds_both_routes_from_one_base() {
        let c = WebClient::exa(
            "k",
            "http://127.0.0.1:9/",
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(c.search_url, "http://127.0.0.1:9/search");
        assert_eq!(c.fetch_url, "http://127.0.0.1:9/contents");
    }
}
