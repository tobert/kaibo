//! Provider-native web tools: the search and fetch a provider runs inside its own
//! completion, as opposed to the kaibo-run [`super::WebSearch`]/[`super::WebFetch`].
//!
//! When a web-enabled phase runs on an arm whose provider has server-side web tools,
//! kaibo declares those tools on the request instead of its own (Amy, 2026-11-06:
//! native by default). The trust property is the same as the DeepSeek route: the
//! provider reads each page on its own servers, and kaibo opens nothing.
//!
//! The tool definitions ride the arm's `additional_params.tools`, which every rig wire
//! merges with the function tools (`raw_tools`), so no rig seam is needed. What kaibo
//! must do itself is account for them: the searches happen inside the provider's
//! response, never through a kaibo tool call. [`NativeTally::read`] takes them off each
//! raw response, provider-agnostic by spelling the way `completion_watch` reads a finish
//! reason, so an unknown shape tallies zero instead of failing a call.
//!
//! Versions are pinned to the basic tools: Anthropic's `web_search_20250305` and
//! `web_fetch_20250910`. The newer versions default to running search from code
//! execution (dynamic filtering), a second server loop kaibo has not tested; the basic
//! tools are called directly and are supported on every current Claude model.

use serde_json::{json, Value};

use crate::credentials::ProviderKind;

/// Approximate token cap on one natively fetched page: about the 40,000 characters a
/// kaibo-run fetch is cut at ([`super::tools::MAX_PAGE_CHARS`]).
const NATIVE_FETCH_TOKENS: u64 = 10_000;

/// Results one native search returns, where the provider takes the knob.
const NATIVE_RESULTS: u64 = 8;

/// A provider whose completions can run web tools server-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeWeb {
    /// Anthropic Messages: `web_search` and `web_fetch` server tools.
    Anthropic,
    /// OpenRouter: the `openrouter:web_search` and `openrouter:web_fetch` server tools.
    OpenRouter,
}

impl NativeWeb {
    /// The native web tools for a backend kind, or `None` when it has none.
    pub fn for_kind(kind: ProviderKind) -> Option<Self> {
        match kind {
            ProviderKind::Anthropic => Some(Self::Anthropic),
            ProviderKind::OpenRouter => Some(Self::OpenRouter),
            _ => None,
        }
    }

    /// The name the footer uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
        }
    }

    /// The tool definitions for one phase, capped at `searches` and `fetches` uses. A
    /// cap of 0 omits that tool. The caps are `max_uses`, which both providers count
    /// per turn, and a kaibo phase is one provider turn (its later requests carry only
    /// tool results), so the cap holds for the phase.
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
                        "parameters": { "max_uses": searches, "max_results": NATIVE_RESULTS },
                    }));
                }
                if fetches > 0 {
                    defs.push(json!({
                        "type": "openrouter:web_fetch",
                        "parameters": {
                            "max_uses": fetches,
                            "max_content_tokens": NATIVE_FETCH_TOKENS,
                        },
                    }));
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
    /// `(url, title)` of each page the response fetched or cited, in order.
    pub sources: Vec<(String, String)>,
}

impl NativeTally {
    /// Read a raw provider response. Anthropic reports counts under
    /// `usage.server_tool_use` and pages as `web_fetch_tool_result` blocks and
    /// `web_search_result_location` citations; OpenRouter reports counts under
    /// `usage.server_tool_use_details` and pages as `url_citation` annotations. Any other
    /// shape reads as zero.
    pub fn read(raw: &Value) -> Self {
        let usage = &raw["usage"];
        let count = |v: &Value, key: &str| v[key].as_u64().unwrap_or(0) as usize;
        let mut tally = Self {
            searches: count(&usage["server_tool_use"], "web_search_requests")
                + count(&usage["server_tool_use_details"], "web_search_requests"),
            fetches: count(&usage["server_tool_use"], "web_fetch_requests")
                + count(&usage["server_tool_use_details"], "web_fetch_requests"),
            sources: Vec::new(),
        };
        let mut add = |url: Option<&str>, title: Option<&str>| {
            if let Some(url) = url.filter(|u| !u.is_empty()) {
                let title = title.unwrap_or("").to_string();
                if !tally.sources.iter().any(|(u, _)| u == url) {
                    tally.sources.push((url.to_string(), title));
                }
            }
        };
        // Anthropic: content blocks.
        for block in raw["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("web_fetch_tool_result") => {
                    let result = &block["content"];
                    add(result["url"].as_str(), result["content"]["title"].as_str());
                }
                Some("text") => {
                    for c in block["citations"].as_array().into_iter().flatten() {
                        if c["type"] == "web_search_result_location" {
                            add(c["url"].as_str(), c["title"].as_str());
                        }
                    }
                }
                _ => {}
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
                    let c = &a["url_citation"];
                    add(c["url"].as_str(), c["title"].as_str());
                }
            }
        }
        tally
    }

    /// Fold another response's tally into this one.
    pub fn absorb(&mut self, other: NativeTally) {
        self.searches += other.searches;
        self.fetches += other.fetches;
        for (url, title) in other.sources {
            if !self.sources.iter().any(|(u, _)| *u == url) {
                self.sources.push((url, title));
            }
        }
    }
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
        for kind in [
            ProviderKind::DeepSeek,
            ProviderKind::Gemini,
            ProviderKind::Openai,
        ] {
            assert_eq!(NativeWeb::for_kind(kind), None, "{kind:?}");
        }
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
                    {"type": "char_location", "document_title": "B"}]}
            ],
            "usage": {"server_tool_use": {"web_search_requests": 2, "web_fetch_requests": 1}}
        });
        let t = NativeTally::read(&raw);
        assert_eq!((t.searches, t.fetches), (2, 1));
        assert_eq!(
            t.sources,
            vec![
                ("https://b".into(), "B".into()),
                ("https://a".into(), "A".into())
            ]
        );
        assert!(!is_paused(&raw));
        assert!(is_paused(&json!({"stop_reason": "pause_turn"})));
    }

    #[test]
    fn an_openrouter_response_tallies_annotations() {
        let raw = json!({
            "choices": [{"message": {"content": "x", "annotations": [
                {"type": "url_citation", "url_citation": {"url": "https://c", "title": "C"}}]}}],
            "usage": {"server_tool_use_details": {"web_search_requests": 1}}
        });
        let t = NativeTally::read(&raw);
        assert_eq!((t.searches, t.fetches), (1, 0));
        assert_eq!(t.sources, vec![("https://c".into(), "C".into())]);
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
