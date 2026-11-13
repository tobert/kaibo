//! Provider-native web tools: the search and fetch a provider runs inside its own
//! completion, as opposed to the kaibo-run [`super::WebSearch`]/[`super::WebFetch`].
//!
//! When a web-enabled phase runs on an arm whose provider has server-side web tools,
//! kaibo declares those tools on the request instead of its own (Amy, 2026-11-06:
//! native by default). The trust property is the same as the DeepSeek route: the
//! provider reads each page on its own servers, and kaibo opens nothing.
//!
//! The tool definitions ride each request's `additional_params.tools` (a rig
//! `RequestPatch` set per turn by the engine's `NativeWebHook`), which every rig wire
//! merges with the function tools (`raw_tools`). Setting them per request is what lets
//! the cap follow the call's shared budget as it is spent. What kaibo
//! must do itself is account for them: the searches happen inside the provider's
//! response, never through a kaibo tool call. [`NativeTally::read`] takes them off each
//! raw response, provider-agnostic by spelling the way `completion_watch` reads a finish
//! reason, so an unknown shape tallies zero instead of failing a call.
//!
//! Gemini (probed 2026-11-13) declares `google_search` and `url_context`. Two facts
//! shape it. First, built-in tools beside function declarations need
//! `toolConfig.includeServerSideToolInvocations`; the reply then carries each server
//! call as a `toolCall`/`toolResponse` part pair, which rig keeps as opaque parts and
//! replays verbatim, thought signatures included. Second, Gemini takes no use cap, so
//! kaibo enforces the shared budget by leaving a spent tool off the next request; one
//! turn can run past it, and the tally records what it really ran. Pages Gemini found by
//! search come back as `vertexaisearch.cloud.google.com/grounding-api-redirect/...`
//! links titled with a bare domain, so they list as provider-cited, not read.
//!
//! Versions are pinned to the basic tools: Anthropic's `web_search_20250305` and
//! `web_fetch_20250910`. The newer versions default to running search from code
//! execution (dynamic filtering), a second server loop kaibo has not tested; the basic
//! tools are called directly and are supported on every current Claude model.

use serde_json::{json, Value};

use crate::credentials::ProviderKind;
use super::tools::{SourceKind, WebSource};

/// Approximate token cap on one natively fetched page: about the 40,000 characters a
/// kaibo-run fetch is cut at ([`super::tools::MAX_PAGE_CHARS`]).
const NATIVE_FETCH_TOKENS: u64 = 10_000;

/// Results one native search returns, where the provider takes the knob.
const NATIVE_RESULTS: u64 = 8;

/// The engine OpenRouter's search and fetch run on. Named rather than left at OpenRouter's
/// default `auto`, which switches to a model's own provider search where one exists
/// (GPT-5+, Claude 4+, Gemini 3+, Grok 4+): that search ignores `max_results` and may not
/// count as a tool call, which breaks the fetch count [`NativeTally::read`] derives. Exa
/// runs the same way on every model, so what kaibo sends and counts holds on all of them.
const OPENROUTER_ENGINE: &str = "exa";

/// A provider whose completions can run web tools server-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeWeb {
    /// Anthropic Messages: `web_search` and `web_fetch` server tools.
    Anthropic,
    /// OpenRouter: the `openrouter:web_search` and `openrouter:web_fetch` server tools.
    OpenRouter,
    /// Gemini API: the `google_search` and `url_context` built-in tools.
    Gemini,
}

impl NativeWeb {
    /// The native web tools for a backend kind, or `None` when it has none.
    pub fn for_kind(kind: ProviderKind) -> Option<Self> {
        match kind {
            ProviderKind::Anthropic => Some(Self::Anthropic),
            ProviderKind::OpenRouter => Some(Self::OpenRouter),
            ProviderKind::Gemini => Some(Self::Gemini),
            _ => None,
        }
    }

    /// The name the footer uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
            Self::Gemini => "gemini",
        }
    }

    /// Request fields beside `tools` that this provider's web tools need, merged into
    /// the same `additional_params` patch. Gemini refuses built-in tools next to
    /// function declarations without `includeServerSideToolInvocations`.
    pub fn request_params(self) -> Option<(&'static str, Value)> {
        match self {
            Self::Gemini => Some((
                "toolConfig",
                json!({ "includeServerSideToolInvocations": true }),
            )),
            Self::Anthropic | Self::OpenRouter => None,
        }
    }

    /// Whether this provider counts `max_uses` across a whole provider turn (Anthropic:
    /// a request whose last message holds only tool results continues the turn) rather
    /// than per request (OpenRouter). The per-request cap kaibo sends follows it.
    pub fn counts_per_turn(self) -> bool {
        matches!(self, Self::Anthropic)
    }

    /// The tool definitions for one request, capped at `searches` and `fetches` uses. A
    /// cap of 0 omits that tool.
    pub fn tool_defs(self, searches: usize, fetches: usize) -> Vec<Value> {
        let mut defs = Vec::new();
        match self {
            Self::Anthropic => {
                if searches > 0 {
                    defs.push(json!({
                        "type": "web_search_20250305",
                        "name": "web_search",
                        "max_uses": searches,
                    }));
                }
                if fetches > 0 {
                    defs.push(json!({
                        "type": "web_fetch_20250910",
                        "name": "web_fetch",
                        "max_uses": fetches,
                        "citations": { "enabled": true },
                        "max_content_tokens": NATIVE_FETCH_TOKENS,
                    }));
                }
            }
            Self::OpenRouter => {
                if searches > 0 {
                    defs.push(json!({
                        "type": "openrouter:web_search",
                        "parameters": {
                            "engine": OPENROUTER_ENGINE,
                            "max_uses": searches,
                            "max_results": NATIVE_RESULTS,
                        },
                    }));
                }
                if fetches > 0 {
                    defs.push(json!({
                        "type": "openrouter:web_fetch",
                        "parameters": {
                            "engine": OPENROUTER_ENGINE,
                            "max_uses": fetches,
                            "max_content_tokens": NATIVE_FETCH_TOKENS,
                        },
                    }));
                }
            }
            // No use cap exists, so a cap of 0 leaving the tool off is the whole budget.
            Self::Gemini => {
                if searches > 0 {
                    defs.push(json!({ "google_search": {} }));
                }
                if fetches > 0 {
                    defs.push(json!({ "url_context": {} }));
                }
            }
        }
        defs
    }
}

/// What one response's native web tools did: request counts and the pages read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeTally {
    pub searches: usize,
    pub fetches: usize,
    /// Each page the response fetched or cited, in order, marked by which.
    pub sources: Vec<WebSource>,
}

impl NativeTally {
    /// Read a raw provider response. Anthropic reports counts under
    /// `usage.server_tool_use` and pages as `web_fetch_tool_result` blocks and
    /// `web_search_result_location` citations; OpenRouter reports searches and all
    /// executed tool calls under `usage.server_tool_use_details`, and pages as
    /// `url_citation` annotations that do not say whether a page was fetched. Any other
    /// shape reads as zero.
    pub fn read(raw: &Value) -> Self {
        let usage = &raw["usage"];
        let count = |v: &Value, key: &str| v[key].as_u64().unwrap_or(0) as usize;
        let details = &usage["server_tool_use_details"];
        let or_searches = count(details, "web_search_requests");
        // OpenRouter has no fetch counter: `web_search_requests` is its only per-tool
        // count, and `tool_calls_executed` counts every server tool call that produced a
        // result, a call refused past `max_uses` included. kaibo declares only search and
        // fetch there, so the calls that were not searches are the fetches. A future
        // `web_fetch_requests` is taken as given. This holds because kaibo names the Exa
        // engine ([`OPENROUTER_ENGINE`]): OpenRouter's docs say a provider-native search,
        // which its default `auto` engine picks on some models, may count in
        // `web_search_requests` without a tool call, and this would undercount by that many.
        let or_fetches = match details.get("web_fetch_requests") {
            Some(_) => count(details, "web_fetch_requests"),
            None => count(details, "tool_calls_executed").saturating_sub(or_searches),
        };
        let mut tally = Self {
            searches: count(&usage["server_tool_use"], "web_search_requests") + or_searches,
            fetches: count(&usage["server_tool_use"], "web_fetch_requests") + or_fetches,
            sources: Vec::new(),
        };
        let mut add = |url: Option<&str>, title: Option<&str>, kind: SourceKind| {
            if let Some(url) = url.filter(|u| !u.is_empty()) {
                let title = title.unwrap_or("");
                WebSource::merge_into(&mut tally.sources, WebSource::new(url, title, kind));
            }
        };
        // Anthropic: content blocks.
        for block in raw["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                // Only a successful fetch is a page read; an error block is skipped even
                // if a later shape adds its URL.
                Some("web_fetch_tool_result") if block["content"]["type"] == "web_fetch_result" => {
                    let result = &block["content"];
                    add(
                        result["url"].as_str(),
                        result["content"]["title"].as_str(),
                        SourceKind::Fetched,
                    );
                }
                Some("text") => {
                    for c in block["citations"].as_array().into_iter().flatten() {
                        if c["type"] == "web_search_result_location" {
                            add(c["url"].as_str(), c["title"].as_str(), SourceKind::SearchResult);
                        }
                    }
                }
                _ => {}
            }
        }
        // Gemini: server-side `toolCall` parts carry each search's queries and each
        // fetch's URLs; with no function tools declared there are no such parts, and
        // `groundingMetadata.webSearchQueries` is the only count. Pages read are
        // `urlContextMetadata` entries; search results are `groundingChunks`.
        for cand in raw["candidates"].as_array().into_iter().flatten() {
            let mut calls = (0, 0, false);
            for part in cand["content"]["parts"].as_array().into_iter().flatten() {
                let call = &part["toolCall"];
                let n = |key: &str| call["args"][key].as_array().map_or(1, |a| a.len().max(1));
                match call["toolType"].as_str() {
                    Some("GOOGLE_SEARCH_WEB") => (calls.0, calls.2) = (calls.0 + n("queries"), true),
                    Some("URL_CONTEXT") => (calls.1, calls.2) = (calls.1 + n("urls"), true),
                    _ => {}
                }
            }
            let grounding = &cand["groundingMetadata"];
            let urls = cand["urlContextMetadata"]["urlMetadata"].as_array();
            if calls.2 {
                tally.searches += calls.0;
                tally.fetches += calls.1;
            } else {
                tally.searches += grounding["webSearchQueries"].as_array().map_or(0, Vec::len);
                tally.fetches += urls.map_or(0, Vec::len);
            }
            for u in urls.into_iter().flatten() {
                let url = u["retrievedUrl"].as_str();
                let ok = u["urlRetrievalStatus"] == "URL_RETRIEVAL_STATUS_SUCCESS";
                // A redirect link is a search result Gemini read; its real address is
                // unknown, so it lists with the provider-cited pages below.
                let redirect = url.is_some_and(is_grounding_redirect);
                if ok {
                    let (kind, title) = if redirect {
                        (SourceKind::Unattributed, Some(GROUNDING_REDIRECT_TITLE))
                    } else {
                        (SourceKind::Fetched, None)
                    };
                    add(url, title, kind);
                }
            }
            for chunk in grounding["groundingChunks"].as_array().into_iter().flatten() {
                let web = &chunk["web"];
                add(web["uri"].as_str(), web["title"].as_str(), SourceKind::Unattributed);
            }
        }
        // OpenRouter (OpenAI chat shape): message annotations.
        for choice in raw["choices"].as_array().into_iter().flatten() {
            for a in choice["message"]["annotations"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if a["type"] == "url_citation" {
                    // An annotation does not say whether the provider fetched the page.
                    let c = &a["url_citation"];
                    add(c["url"].as_str(), c["title"].as_str(), SourceKind::Unattributed);
                }
            }
        }
        tally
    }

    /// Fold another response's tally into this one.
    pub fn absorb(&mut self, other: NativeTally) {
        self.searches += other.searches;
        self.fetches += other.fetches;
        for source in other.sources {
            WebSource::merge_into(&mut self.sources, source);
        }
    }
}

/// The title a Gemini redirect link lists under when Gemini names nothing better. Gemini
/// reports no title for a search result it read, and the link hides the real address.
const GROUNDING_REDIRECT_TITLE: &str = "a Google Search result (Gemini hides its address)";

/// Whether `url` is one of Gemini's grounding redirect links, which hide the real page
/// address behind Google's host.
fn is_grounding_redirect(url: &str) -> bool {
    url.starts_with("https://vertexaisearch.cloud.google.com/grounding-api-redirect/")
}

/// One error a native web tool returned, as the provider reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeToolError {
    pub tool: &'static str,
    /// The provider's `error_code`, such as `url_not_in_prior_context`.
    pub code: String,
    /// The address a refused `web_fetch` asked for, read from the matching
    /// `server_tool_use` block. It tells a refused search-result address from one the
    /// model typed itself.
    pub url: Option<String>,
}

/// The errors a raw Anthropic response's native web tools returned. The model reads
/// these and often misreports them (a refused URL described as a spent budget), so
/// kaibo logs the provider's own code. Any other shape reads as none.
pub fn tool_errors(raw: &Value) -> Vec<NativeToolError> {
    let blocks: Vec<&Value> = raw["content"].as_array().into_iter().flatten().collect();
    let requested_url = |id: &Value| {
        blocks
            .iter()
            .find(|b| b["type"] == "server_tool_use" && b["id"] == *id)
            .and_then(|b| b["input"]["url"].as_str())
            .map(str::to_string)
    };
    let mut out = Vec::new();
    for block in &blocks {
        let tool = match block["type"].as_str() {
            Some("web_fetch_tool_result") => "web_fetch",
            Some("web_search_tool_result") => "web_search",
            _ => continue,
        };
        let c = &block["content"];
        let is_error = matches!(
            c["type"].as_str(),
            Some("web_fetch_tool_error" | "web_fetch_tool_result_error" | "web_search_tool_result_error")
        );
        if is_error {
            out.push(NativeToolError {
                tool,
                code: c["error_code"].as_str().unwrap_or("unknown").to_string(),
                url: (tool == "web_fetch")
                    .then(|| requested_url(&block["tool_use_id"]))
                    .flatten(),
            });
        }
    }
    out
}

/// Whether a raw response is an Anthropic server-tool turn the provider paused before
/// it finished (`stop_reason: "pause_turn"`). rig maps it to a clean stop, which would
/// end kaibo's loop on a partial turn.
pub fn is_paused(raw: &Value) -> bool {
    raw["stop_reason"] == "pause_turn"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_providers_with_server_tools_are_native() {
        assert_eq!(
            NativeWeb::for_kind(ProviderKind::Anthropic),
            Some(NativeWeb::Anthropic)
        );
        assert_eq!(
            NativeWeb::for_kind(ProviderKind::OpenRouter),
            Some(NativeWeb::OpenRouter)
        );
        assert_eq!(
            NativeWeb::for_kind(ProviderKind::Gemini),
            Some(NativeWeb::Gemini)
        );
        for kind in [
            ProviderKind::DeepSeek,
            ProviderKind::GeminiMedia,
            ProviderKind::Openai,
        ] {
            assert_eq!(NativeWeb::for_kind(kind), None, "{kind:?}");
        }
    }

    /// Gemini has no use cap: a spent budget leaves the tool off, and only Gemini asks
    /// for the `toolConfig` flag.
    #[test]
    fn gemini_tools_follow_the_budget_and_ask_for_server_side_invocations() {
        let g = NativeWeb::Gemini;
        assert_eq!(
            g.tool_defs(3, 2),
            vec![json!({"google_search": {}}), json!({"url_context": {}})]
        );
        assert_eq!(g.tool_defs(0, 2), vec![json!({"url_context": {}})]);
        assert!(g.tool_defs(0, 0).is_empty());
        assert_eq!(
            g.request_params(),
            Some(("toolConfig", json!({"includeServerSideToolInvocations": true})))
        );
        assert_eq!(NativeWeb::Anthropic.request_params(), None);
        assert_eq!(NativeWeb::OpenRouter.request_params(), None);
    }

    /// With no function tools declared, Gemini sends no `toolCall` parts, so the counts
    /// come from the metadata. A failed page read is not a source, and a redirect link
    /// Gemini did read lists as provider-cited.
    #[test]
    fn a_gemini_reply_without_tool_call_parts_is_counted_from_its_metadata() {
        let raw = json!({"candidates": [{
            "content": {"parts": [{"text": "answer"}]},
            "groundingMetadata": {
                "webSearchQueries": ["a", "b", "c"],
                "groundingChunks": [{"web": {"uri": "https://vertexaisearch.cloud.google.com/grounding-api-redirect/X", "title": "turso.tech"}}]
            },
            "urlContextMetadata": {"urlMetadata": [
                {"retrievedUrl": "https://ok.example/", "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_SUCCESS"},
                {"retrievedUrl": "https://gone.example/", "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_ERROR"},
                {"retrievedUrl": "https://vertexaisearch.cloud.google.com/grounding-api-redirect/Y", "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_SUCCESS"}
            ]}
        }]});
        let t = NativeTally::read(&raw);
        assert_eq!((t.searches, t.fetches), (3, 3));
        assert_eq!(t.sources[1].title, GROUNDING_REDIRECT_TITLE);
        assert_eq!(t.sources[2].title, "turso.tech");
        let urls: Vec<(&str, Option<bool>)> =
            t.sources.iter().map(|s| (s.url.as_str(), s.fetched_flag())).collect();
        assert_eq!(
            urls,
            vec![
                ("https://ok.example/", Some(true)),
                ("https://vertexaisearch.cloud.google.com/grounding-api-redirect/Y", None),
                ("https://vertexaisearch.cloud.google.com/grounding-api-redirect/X", None),
            ]
        );
    }

    #[test]
    fn tool_errors_name_the_provider_code_and_skip_successes() {
        let raw = json!({ "content": [
            { "type": "server_tool_use", "id": "a", "name": "web_fetch",
              "input": { "url": "https://typed.test/guess" } },
            { "type": "web_fetch_tool_result", "tool_use_id": "a",
              "content": { "type": "web_fetch_tool_error", "error_code": "url_not_allowed",
                           "url": "https://typed.test/guess" } },
            { "type": "web_fetch_tool_result", "tool_use_id": "b",
              "content": { "type": "web_fetch_result", "url": "https://x.test/",
                           "content": { "title": "X" } } },
            { "type": "web_search_tool_result", "tool_use_id": "c",
              "content": { "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" } },
            { "type": "web_search_tool_result", "tool_use_id": "d", "content": [] },
            { "type": "text", "text": "done" }
        ]});
        assert_eq!(
            tool_errors(&raw),
            vec![
                NativeToolError {
                    tool: "web_fetch",
                    code: "url_not_allowed".into(),
                    url: Some("https://typed.test/guess".into()),
                },
                NativeToolError {
                    tool: "web_search",
                    code: "max_uses_exceeded".into(),
                    url: None,
                },
            ]
        );
        assert!(tool_errors(&json!({ "choices": [] })).is_empty());
        // A refused fetch is never a page read, even when its error block names the URL.
        assert_eq!(
            NativeTally::read(&raw).sources,
            vec![WebSource::fetched("https://x.test/", "X")]
        );
    }

    #[test]
    fn tool_defs_pin_the_basic_versions_and_omit_a_zero_cap() {
        let defs = NativeWeb::Anthropic.tool_defs(3, 2);
        assert_eq!(defs[0]["type"], "web_search_20250305");
        assert_eq!(defs[0]["max_uses"], 3);
        assert_eq!(defs[1]["type"], "web_fetch_20250910");
        assert_eq!(defs[1]["max_uses"], 2);
        assert_eq!(NativeWeb::Anthropic.tool_defs(0, 2).len(), 1);
        assert!(NativeWeb::Anthropic.tool_defs(0, 0).is_empty());
        let or = NativeWeb::OpenRouter.tool_defs(4, 0);
        assert_eq!(or.len(), 1);
        assert_eq!(or[0]["type"], "openrouter:web_search");
        assert_eq!(or[0]["parameters"]["max_uses"], 4);
        // The engine is named, never OpenRouter's `auto`, on both tools.
        let both = NativeWeb::OpenRouter.tool_defs(1, 1);
        assert_eq!(both.len(), 2);
        for def in &both {
            assert_eq!(def["parameters"]["engine"], "exa", "{def}");
        }
    }

    #[test]
    fn an_anthropic_response_tallies_counts_fetches_and_citations() {
        let raw = json!({
            "stop_reason": "end_turn",
            "content": [
                {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "s1", "content": [
                    {"type": "web_search_result", "url": "https://a", "title": "A"}]},
                {"type": "web_fetch_tool_result", "tool_use_id": "s2", "content": {
                    "type": "web_fetch_result", "url": "https://b",
                    "content": {"type": "document", "title": "B"}}},
                {"type": "text", "text": "x", "citations": [
                    {"type": "web_search_result_location", "url": "https://a", "title": "A"},
                    {"type": "web_search_result_location", "url": "https://b", "title": "B"},
                    {"type": "char_location", "document_title": "B"}]}
            ],
            "usage": {"server_tool_use": {"web_search_requests": 2, "web_fetch_requests": 1}}
        });
        let t = NativeTally::read(&raw);
        assert_eq!((t.searches, t.fetches), (2, 1));
        // A fetched page stays fetched when the answer also cites it as a search result;
        // a result cited and never fetched is marked so.
        assert_eq!(
            t.sources,
            vec![WebSource::fetched("https://b", "B"), WebSource::cited("https://a", "A")]
        );
        assert!(!is_paused(&raw));
        assert!(is_paused(&json!({"stop_reason": "pause_turn"})));
    }

    #[test]
    fn an_openrouter_response_tallies_annotations() {
        // The shape a live Qwen research run returned (2026-11-12): three searches and
        // fourteen executed calls, so eleven fetches, one of them refused past the cap.
        let raw = json!({
            "choices": [{"message": {"content": "x", "annotations": [
                {"type": "url_citation", "url_citation": {"url": "https://c", "title": "C"}}]}}],
            "usage": {"server_tool_use_details": {
                "web_search_requests": 3, "tool_calls_requested": 14, "tool_calls_executed": 14}}
        });
        let t = NativeTally::read(&raw);
        assert_eq!((t.searches, t.fetches), (3, 11));
        assert_eq!(t.sources, vec![WebSource::unattributed("https://c", "C")]);

        // Searches alone: no fetches. A fetch counter, if OpenRouter adds one, wins.
        let only = |details| NativeTally::read(&json!({"usage": {"server_tool_use_details": details}}));
        let t = only(json!({"web_search_requests": 2, "tool_calls_executed": 2}));
        assert_eq!((t.searches, t.fetches), (2, 0));
        let t = only(json!({"web_search_requests": 1, "web_fetch_requests": 2, "tool_calls_executed": 9}));
        assert_eq!((t.searches, t.fetches), (1, 2));
    }

    #[test]
    fn an_unknown_shape_tallies_zero() {
        assert_eq!(NativeTally::read(&Value::Null), NativeTally::default());
        assert_eq!(
            NativeTally::read(&json!({"usage": {"input_tokens": 5}})),
            NativeTally::default()
        );
    }
}
