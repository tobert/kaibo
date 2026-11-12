//! The web client — DeepSeek's Search and Fetch APIs, driven directly rather than
//! through rig.
//!
//! These are plain JSON endpoints, not completion routes: `POST /v1/tools/search` takes a
//! query and returns ranked results, `POST /v1/tools/fetch` takes one URL and returns the
//! page as Markdown. DeepSeek runs both on its own servers, so kaibo dials only
//! `api.deepseek.com`, the same host its DeepSeek completions already reach. **kaibo
//! never opens a URL a model chose.** A fetch target travels as a JSON field to DeepSeek
//! and DeepSeek reads it, so a model naming `http://127.0.0.1:…` (kaibo's own HTTP
//! transport, a metadata service, anything on the operator's network) reaches nothing of
//! the operator's. That property is why this route was chosen over a kaibo-side fetch,
//! and the reason the fetch URL is validated here only to give the model a clear error.
//!
//! Wire shapes and limits are from DeepSeek's reference (api-docs.deepseek.com,
//! `/api/search`, `/api/fetch`, `/guides/search`, read 2026-11-06): a query is 1 to 128
//! characters, `count` 1 to 50, a fetch URL at most 8192 characters with no userinfo.
//! Errors use the OpenAI error envelope. 408, 429 and 5xx are retryable; any other 4xx
//! is not. Each successful call is billed ($0.75 per 1,000 per endpoint); an empty search
//! result and a failure are not.
//!
//! The client is independent of the tools a model calls ([`WebSearch`]/[`WebFetch`], in
//! `tools.rs`) so a later route (an operator CLI, a different provider) can reuse it.

pub mod native;
mod tools;

use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub use tools::{SourceKind, WebFetch, WebSearch, WebSession, WebSource, WebUsage};

/// The per-request deadline for a web call. A search or fetch answers in seconds, so it
/// gets its own bound rather than the backend's `request_timeout`, which is sized for a
/// thinking model (900 s by default). With two retries, one stalled call costs at most
/// about three minutes of a call's `call_deadline`, not three quarters of an hour.
pub const WEB_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A provider error body longer than this many characters is cut before it reaches a
/// model: a proxy's HTML error page is noise in a tool result.
const MAX_ERROR_CHARS: usize = 500;

/// The search provider a session calls. One variant today; another provider (Exa, say)
/// is a new variant with its own client, and every phase, budget, and record above it
/// stays the same. The rule each variant keeps: the provider reads the page on its own
/// servers, so kaibo never opens a URL a model chose.
#[derive(Debug)]
pub enum WebApi {
    DeepSeek(WebClient),
}

impl WebApi {
    pub async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchResult>, WebError> {
        match self {
            Self::DeepSeek(c) => c.search(req).await,
        }
    }

    pub async fn fetch(&self, url: &str) -> Result<FetchedPage, WebError> {
        match self {
            Self::DeepSeek(c) => c.fetch(url).await,
        }
    }
}

/// DeepSeek's Search endpoint.
pub const DEEPSEEK_SEARCH_URL: &str = "https://api.deepseek.com/v1/tools/search";
/// DeepSeek's Fetch endpoint.
pub const DEEPSEEK_FETCH_URL: &str = "https://api.deepseek.com/v1/tools/fetch";

/// A search query is at most this many characters.
pub const MAX_QUERY_CHARS: usize = 128;
/// A search returns at most this many results.
pub const MAX_RESULTS: u32 = 50;
/// A fetch URL is at most this many characters.
pub const MAX_URL_CHARS: usize = 8192;

/// Retry delays for a retryable status (408, 429, 5xx). Two retries, then the error
/// reaches the model, which can choose another query or answer from what it has.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1500)];

/// The Search request body.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchRequest {
    pub query: String,
    pub count: u32,
    /// The question behind the query. DeepSeek uses it to understand the query better.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ori_question: Option<String>,
}

impl SearchRequest {
    /// Build a request, refusing what DeepSeek would refuse before a request is sent.
    /// `count` is clamped to 1..=[`MAX_RESULTS`]. A question over the query limit is cut,
    /// not refused, because it is only a hint.
    pub fn new(query: &str, count: u32, ori_question: Option<&str>) -> Result<Self, WebError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(WebError::Refused(
                "web_search refused an empty query. Pass the words to search for, such as \
                 `turso 0.8 MP-WAL changelog`."
                    .into(),
            ));
        }
        let chars = query.chars().count();
        if chars > MAX_QUERY_CHARS {
            return Err(WebError::Refused(format!(
                "web_search refused a query of {chars} characters: the search API accepts at \
                 most {MAX_QUERY_CHARS}. Search for the key terms only, such as a crate name, \
                 a version, and an error message."
            )));
        }
        let ori_question = ori_question
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| q.chars().take(MAX_QUERY_CHARS).collect());
        Ok(Self {
            query: query.to_string(),
            count: count.clamp(1, MAX_RESULTS),
            ori_question,
        })
    }
}

/// One search result. Every field DeepSeek marks required is still defaulted here, so a
/// result missing one reads as empty rather than failing the whole search.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct SearchResult {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    /// The longer text DeepSeek writes for a model to read, about 600 to 800 characters.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub site_name: Option<String>,
    /// RFC 3339, UTC.
    #[serde(default)]
    pub date: Option<String>,
    /// -1 (unknown) to 4. 3 and up is highly authoritative.
    #[serde(default)]
    pub authority: Option<i64>,
    /// 0 to 3. 1.5 and up is high quality.
    #[serde(default)]
    pub rerank_score: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

/// A fetched page.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FetchedPage {
    /// Empty when the page has no title.
    #[serde(default)]
    pub title: String,
    /// The page as Markdown.
    pub content: String,
}

/// Why a web call failed. The `Display` text reaches a model as a tool result, so each
/// variant names what failed, why, and what to do next.
#[derive(Debug, Clone, PartialEq)]
pub enum WebError {
    /// kaibo refused the input before sending it.
    Refused(String),
    /// The request never got an HTTP answer (connect failure, timeout).
    Transport(String),
    /// DeepSeek answered with an error status.
    Provider { status: u16, message: String },
    /// DeepSeek answered 200 with a body kaibo could not read.
    Unreadable(String),
}

impl WebError {
    /// Whether a retry of the same request can succeed.
    fn retryable(&self) -> bool {
        match self {
            Self::Provider { status, .. } => *status == 408 || *status == 429 || *status >= 500,
            Self::Transport(_) => true,
            Self::Refused(_) | Self::Unreadable(_) => false,
        }
    }
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) => f.write_str(why),
            Self::Transport(e) => write!(
                f,
                "the web API did not answer ({e}). Try once more, or answer from the \
                 evidence you have and say the web was unreachable."
            ),
            Self::Provider {
                status: 402,
                message,
            } => write!(
                f,
                "the web API refused the call: the DeepSeek account has no balance \
                 (402: {message}). Answer from the evidence you have and say web access \
                 is unavailable."
            ),
            Self::Provider {
                status: 403,
                message,
            } => write!(
                f,
                "the web API refused to read this page under its security policy (403: \
                 {message}). Choose a different source for the same fact."
            ),
            Self::Provider {
                status: 404,
                message,
            } => write!(
                f,
                "the web API could not read usable content at this address (404: \
                 {message}). The page may be gone, need JavaScript, or be a format it \
                 cannot read. Choose a different source, or search for the page's title."
            ),
            Self::Provider { status, message } => write!(
                f,
                "the web API returned HTTP {status}: {message}. Change the request, or \
                 answer from the evidence you have and say this call failed."
            ),
            Self::Unreadable(e) => write!(
                f,
                "the web API answered with a body kaibo could not read ({e}). Try a \
                 different query or page."
            ),
        }
    }
}

impl std::error::Error for WebError {}

/// Pull the message out of the `{"error": {"message"}}` envelope, falling back to the raw
/// body so nothing the provider said is dropped.
fn provider_message(body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.trim().to_string());
    if message.chars().count() <= MAX_ERROR_CHARS {
        return message;
    }
    let cut: String = message.chars().take(MAX_ERROR_CHARS).collect();
    format!("{cut}… [cut at {MAX_ERROR_CHARS} characters]")
}

/// Check a fetch URL. DeepSeek dials it, not kaibo (see the module doc), so this exists
/// to give the model a clear refusal instead of a provider 400.
pub fn check_fetch_url(url: &str) -> Result<String, WebError> {
    let url = url.trim();
    let chars = url.chars().count();
    if chars > MAX_URL_CHARS {
        return Err(WebError::Refused(format!(
            "web_fetch refused a URL of {chars} characters: the fetch API accepts at most \
             {MAX_URL_CHARS}. Fetch the page without its query string."
        )));
    }
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        WebError::Refused(format!(
            "web_fetch refused {url:?}: it is not a URL ({e}). Pass a full address such as \
             `https://docs.rs/turso/latest/turso/`."
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(WebError::Refused(format!(
            "web_fetch refused {url:?}: only http and https addresses with a host can be \
             fetched. Pass a web address such as `https://docs.rs/turso/latest/turso/`."
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(WebError::Refused(
            "web_fetch refused a URL that carries a username or password. Fetch the public \
             address without credentials."
                .into(),
        ));
    }
    Ok(url.to_string())
}

/// A client for one Search/Fetch endpoint pair and key.
pub struct WebClient {
    search_url: String,
    fetch_url: String,
    key: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for WebClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never reaches a log.
        f.debug_struct("WebClient")
            .field("search_url", &self.search_url)
            .field("fetch_url", &self.fetch_url)
            .finish_non_exhaustive()
    }
}

impl WebClient {
    /// Production passes [`DEEPSEEK_SEARCH_URL`] and [`DEEPSEEK_FETCH_URL`]; a test passes
    /// a local listener.
    pub fn new(
        search_url: impl Into<String>,
        fetch_url: impl Into<String>,
        key: impl Into<String>,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            search_url: search_url.into(),
            fetch_url: fetch_url.into(),
            key: key.into(),
            http: crate::tls::api_client_no_redirect(timeout).context("web http client")?,
        })
    }

    /// The DeepSeek endpoints.
    pub fn deepseek(key: impl Into<String>, timeout: Duration) -> anyhow::Result<Self> {
        Self::new(DEEPSEEK_SEARCH_URL, DEEPSEEK_FETCH_URL, key, timeout)
    }

    /// Run one search. An empty result list is a success.
    pub async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchResult>, WebError> {
        let body = self.post(&self.search_url, req).await?;
        serde_json::from_str::<SearchResponse>(&body)
            .map(|r| r.results)
            .map_err(|e| WebError::Unreadable(e.to_string()))
    }

    /// Fetch one page as Markdown. `url` should already have passed [`check_fetch_url`].
    pub async fn fetch(&self, url: &str) -> Result<FetchedPage, WebError> {
        let body = self
            .post(&self.fetch_url, &serde_json::json!({ "url": url }))
            .await?;
        serde_json::from_str::<FetchedPage>(&body).map_err(|e| WebError::Unreadable(e.to_string()))
    }

    /// POST a JSON body, retrying a retryable failure, and return the 200 body.
    async fn post(&self, url: &str, body: &impl Serialize) -> Result<String, WebError> {
        let mut delays = RETRY_DELAYS.iter();
        loop {
            match self.post_once(url, body).await {
                Err(e) if e.retryable() => match delays.next() {
                    Some(d) => tokio::time::sleep(*d).await,
                    None => return Err(e),
                },
                other => return other,
            }
        }
    }

    async fn post_once(&self, url: &str, body: &impl Serialize) -> Result<String, WebError> {
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await
            .map_err(|e| WebError::Transport(e.without_url().to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| WebError::Transport(e.without_url().to_string()))?;
        if !status.is_success() {
            return Err(WebError::Provider {
                status: status.as_u16(),
                message: provider_message(&text),
            });
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_is_trimmed_and_count_clamped() {
        let r = SearchRequest::new("  rig provider tools ", 500, Some("why")).unwrap();
        assert_eq!(r.query, "rig provider tools");
        assert_eq!(r.count, MAX_RESULTS);
        assert_eq!(SearchRequest::new("q", 0, None).unwrap().count, 1);
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({"query": "rig provider tools", "count": 50, "ori_question": "why"})
        );
    }

    #[test]
    fn an_empty_or_long_query_is_refused_before_sending() {
        let e = SearchRequest::new("   ", 5, None).unwrap_err();
        assert!(e.to_string().contains("empty query"), "{e}");
        let long = "x".repeat(MAX_QUERY_CHARS + 1);
        let e = SearchRequest::new(&long, 5, None).unwrap_err();
        assert!(e.to_string().contains("at most 128"), "{e}");
        assert!(SearchRequest::new(&"x".repeat(MAX_QUERY_CHARS), 5, None).is_ok());
    }

    #[test]
    fn a_long_question_hint_is_cut_not_refused() {
        let q = "y".repeat(400);
        let r = SearchRequest::new("q", 5, Some(&q)).unwrap();
        assert_eq!(r.ori_question.unwrap().chars().count(), MAX_QUERY_CHARS);
        assert!(SearchRequest::new("q", 5, Some("  "))
            .unwrap()
            .ori_question
            .is_none());
    }

    #[test]
    fn fetch_urls_are_checked() {
        assert!(check_fetch_url("https://docs.rs/rig-core").is_ok());
        assert!(check_fetch_url("http://example.com/a?b=c").is_ok());
        for bad in [
            "docs.rs/rig-core",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "https://user:pw@example.com/",
        ] {
            assert!(
                matches!(check_fetch_url(bad), Err(WebError::Refused(_))),
                "{bad} should be refused"
            );
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        let e = check_fetch_url(&long).unwrap_err();
        assert!(e.to_string().contains("at most 8192"), "{e}");
    }

    #[test]
    fn retryable_follows_the_provider_guidance() {
        let p = |status| WebError::Provider {
            status,
            message: String::new(),
        };
        for s in [408, 429, 500, 504] {
            assert!(p(s).retryable(), "{s}");
        }
        for s in [400, 401, 402, 403, 404, 413] {
            assert!(!p(s).retryable(), "{s}");
        }
        assert!(!WebError::Refused(String::new()).retryable());
    }

    #[test]
    fn provider_message_reads_the_openai_envelope() {
        assert_eq!(
            provider_message(r#"{"error":{"message":"bad key","type":"x","param":null}}"#),
            "bad key"
        );
        assert_eq!(provider_message(" plain text "), "plain text");
        let page = "x".repeat(MAX_ERROR_CHARS * 3);
        let cut = provider_message(&page);
        assert!(cut.ends_with("[cut at 500 characters]"), "{cut}");
        assert!(cut.chars().count() < MAX_ERROR_CHARS + 40);
    }

    #[test]
    fn a_result_missing_fields_reads_as_empty() {
        let r: SearchResponse =
            serde_json::from_str(r#"{"results":[{"url":"https://a"}]}"#).unwrap();
        assert_eq!(r.results[0].url, "https://a");
        assert_eq!(r.results[0].title, "");
    }

    #[test]
    fn the_debug_form_never_shows_the_key() {
        let c =
            WebClient::new("http://s", "http://f", "sk-secret", Duration::from_secs(1)).unwrap();
        assert!(!format!("{c:?}").contains("sk-secret"));
    }
}
