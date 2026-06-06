//! The MCP server surface: one `consult` tool over the two-phase pipeline.
//!
//! stdio only — like kaish-mcp, kaibo must never bind a socket: it can read a
//! user's filesystem, so the transport pipe is the security boundary.

use std::path::PathBuf;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    AnnotateAble, CallToolResult, Content, Implementation, ListResourcesResult,
    PaginatedRequestParams, ProtocolVersion, RawResource, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::ErrorData as McpError;
use rmcp::{tool, tool_handler, tool_router, RoleServer};
use serde::Deserialize;

use crate::consult::{consult, explore, synthesize, ConsultConfig};
use crate::credentials::Provider;
use crate::explorer::format_output;
use crate::kaish_syntax::kaish_syntax_resource;
use crate::sandbox::KaishWorker;

/// The one resource kaibo serves: the verbose kaish cheatsheet.
const KAISH_SYNTAX_URI: &str = "kaibo://kaish-syntax";

/// Which tools to advertise. All on by default; each `--no-<tool>` flips one off.
///
/// Composes to any posture: `{explore:false, synthesize:false}` ≈ the original
/// consult-only surface; only `run_kaish` on ≈ "no code leaves the box, kaibo as a
/// pure read-only shell". A server with *all* off is a misconfiguration — refused
/// at startup (see `main`), not represented as a valid state here.
#[derive(Debug, Clone, Copy)]
pub struct ToolGating {
    pub consult: bool,
    pub explore: bool,
    pub synthesize: bool,
    pub run_kaish: bool,
}

impl Default for ToolGating {
    fn default() -> Self {
        Self { consult: true, explore: true, synthesize: true, run_kaish: true }
    }
}

impl ToolGating {
    /// True iff every tool is disabled — the zero-tool server we refuse to start.
    pub fn all_disabled(&self) -> bool {
        !self.consult && !self.explore && !self.synthesize && !self.run_kaish
    }
}

/// Arguments to the `consult` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConsultInput {
    /// The question to investigate about the project.
    pub question: String,

    /// Absolute path to the project to explore. Optional only if the server was
    /// launched with a default `--root`.
    #[serde(default)]
    pub path: Option<String>,

    /// Provider: "anthropic" (default), "deepseek", or "gemini".
    #[serde(default)]
    pub provider: Option<String>,

    /// Override the explorer (investigation) model id.
    #[serde(default)]
    pub explorer_model: Option<String>,

    /// Override the synthesizer (final-answer) model id.
    #[serde(default)]
    pub synth_model: Option<String>,

    /// Max tool-loop turns for the explorer (default 50 — it's cheap, let it rip).
    #[serde(default)]
    pub explorer_max_turns: Option<usize>,

    /// Max tool-loop turns for the synth fallback fetches (default 8).
    #[serde(default)]
    pub synth_max_turns: Option<usize>,
}

/// Arguments to the `explore` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExploreInput {
    /// The question to investigate about the project.
    pub question: String,

    /// Absolute path to the project. Optional only if the server was launched
    /// with a default `--root`.
    #[serde(default)]
    pub path: Option<String>,

    /// Provider: "anthropic" (default), "deepseek", "gemini", or "lemonade".
    #[serde(default)]
    pub provider: Option<String>,

    /// Override the explorer model id.
    #[serde(default)]
    pub model: Option<String>,

    /// Max tool-loop turns for the explorer (default 50 — it's cheap, let it rip).
    #[serde(default)]
    pub max_turns: Option<usize>,
}

/// Arguments to the `synthesize` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SynthesizeInput {
    /// The question to answer.
    pub question: String,

    /// Optional context to ground the answer in — typically an `explore` report or
    /// pasted source. When absent, the model investigates via `run_kaish`.
    #[serde(default)]
    pub context: Option<String>,

    /// Absolute path to the project. Optional only if the server was launched
    /// with a default `--root`.
    #[serde(default)]
    pub path: Option<String>,

    /// Provider: "anthropic" (default), "deepseek", "gemini", or "lemonade".
    #[serde(default)]
    pub provider: Option<String>,

    /// Override the synthesizer (capable) model id.
    #[serde(default)]
    pub model: Option<String>,
}

/// Arguments to the `run_kaish` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunKaishInput {
    /// The kaish (sh-like) script to run against the read-only project.
    pub script: String,

    /// Absolute path to the project. Optional only if the server was launched
    /// with a default `--root`. Each call starts at this root — there is no
    /// persistent cwd across calls.
    #[serde(default)]
    pub path: Option<String>,
}

/// kaibo's MCP handler. Cheap to clone (rmcp clones it per request).
#[derive(Clone)]
pub struct KaiboHandler {
    default_root: Option<PathBuf>,
    default_provider: Provider,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl KaiboHandler {
    pub fn new(
        default_root: Option<PathBuf>,
        default_provider: Provider,
        gating: ToolGating,
    ) -> Self {
        // `#[tool_router]` gathers every #[tool] method at compile time; gating is a
        // runtime choice, so build the full router and drop the disabled routes by
        // name. (The methods stay compiled — no dead code — they're just not
        // advertised or callable.)
        let mut tool_router = Self::tool_router();
        if !gating.consult {
            tool_router.remove_route("consult");
        }
        if !gating.explore {
            tool_router.remove_route("explore");
        }
        if !gating.synthesize {
            tool_router.remove_route("synthesize");
        }
        if !gating.run_kaish {
            tool_router.remove_route("run_kaish");
        }
        Self {
            default_root,
            default_provider,
            tool_router,
        }
    }

    /// Tool names this handler advertises, after gating. For tests/diagnostics.
    pub fn advertised_tools(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }

    /// Resolve a call's project root: the explicit `path`, else the server's
    /// `--root`. A call with neither is a parameter error, not a silent default.
    fn resolve_root(&self, path: Option<String>) -> Result<PathBuf, McpError> {
        match path {
            Some(p) => Ok(PathBuf::from(p)),
            None => self.default_root.clone().ok_or_else(|| {
                McpError::invalid_params(
                    "no `path` provided and the server has no default --root",
                    None,
                )
            }),
        }
    }

    /// Resolve a call's provider: the explicit string (parsed), else the server's
    /// default. An unrecognized provider string is a parameter error.
    fn parse_provider(&self, provider: Option<String>) -> Result<Provider, McpError> {
        match provider {
            Some(s) => s
                .parse::<Provider>()
                .map_err(|e| McpError::invalid_params(e.to_string(), None)),
            None => Ok(self.default_provider),
        }
    }

    #[tool(
        description = "Investigate a question about a codebase and return a grounded, \
            cited answer. A cheap explorer model reads the project through a read-only \
            kaish shell (cat/grep/rg/find/jq/pipelines), writes a curated report, then a \
            stronger model synthesizes the final answer. Read-only: it never modifies the \
            project. Args: question (required), path (project dir; optional if the server \
            has a default root), provider (anthropic|deepseek|gemini), and optional \
            explorer_model / synth_model overrides."
    )]
    async fn consult(
        &self,
        Parameters(input): Parameters<ConsultInput>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        let provider = self.parse_provider(input.provider)?;

        let defaults = ConsultConfig::default();
        let cfg = ConsultConfig {
            explorer_model: input.explorer_model,
            synth_model: input.synth_model,
            explorer_max_turns: input.explorer_max_turns.unwrap_or(defaults.explorer_max_turns),
            synth_max_turns: input.synth_max_turns.unwrap_or(defaults.synth_max_turns),
            ..defaults
        };

        let out = consult(&input.question, root, provider, &cfg)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(out.answer)]))
    }

    #[tool(
        description = "Investigate a question about a codebase and return a curated \
            report citing concrete `file:line` locations. A fast, cheap model reads \
            the project through a read-only kaish shell (cat/grep/rg/find/pipelines) \
            and reports what it found — relevant files, line numbers, key snippets. \
            This is the explorer seam on its own: it gathers evidence, it does not \
            write a polished final answer (use `consult` for that). Read-only: it \
            never modifies the project. Args: question (required), path (project dir; \
            optional if the server has a default root), provider \
            (anthropic|deepseek|gemini|lemonade), and optional model / max_turns \
            overrides."
    )]
    async fn explore(
        &self,
        Parameters(input): Parameters<ExploreInput>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        let provider = self.parse_provider(input.provider)?;

        let defaults = ConsultConfig::default();
        let cfg = ConsultConfig {
            explorer_model: input.model,
            explorer_max_turns: input.max_turns.unwrap_or(defaults.explorer_max_turns),
            ..defaults
        };

        let report = explore(&input.question, root, provider, &cfg)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(report)]))
    }

    #[tool(
        description = "Answer a question about a codebase with a capable model, \
            grounded in optional supplied context (typically an `explore` report or \
            pasted source). With context, the model treats it as primary evidence and \
            uses a read-only kaish shell to verify or fill precise gaps; without \
            context, it investigates directly. This is the synthesizer seam on its \
            own — a real outside opinion you can seed with material `explore` or you \
            gathered. Read-only. Args: question (required), context (optional), path \
            (project dir; optional with a default root), provider \
            (anthropic|deepseek|gemini|lemonade), and an optional model override."
    )]
    async fn synthesize(
        &self,
        Parameters(input): Parameters<SynthesizeInput>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        let provider = self.parse_provider(input.provider)?;

        let defaults = ConsultConfig::default();
        let cfg = ConsultConfig {
            synth_model: input.model,
            ..defaults
        };

        let answer = synthesize(&input.question, input.context.as_deref(), root, provider, &cfg)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(answer)]))
    }

    #[tool(
        description = "Run a kaish (sh-like) script against the read-only project; \
            returns exit code + stdout + stderr. Browse code with line numbers: \
            `cat -n FILE`, `rg -n PATTERN`, `cat -n FILE | sed -n '40,80p'`; compose \
            builtins with pipes (grep/jq/awk/find/...). Read-only: writes and external \
            commands are refused (exit 126 = blocked by the sandbox; a script killed \
            for running too long exits 124). Each call starts at the project root — \
            there is no persistent cwd. Full reference: the `kaibo://kaish-syntax` \
            resource. Args: script (required), path (project dir; optional if the \
            server has a default root)."
    )]
    pub async fn run_kaish(
        &self,
        Parameters(input): Parameters<RunKaishInput>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;

        // A fresh worker (and kernel) per call: stateless, starts at root, and the
        // !Send kernel stays on its own thread so this future stays Send.
        let worker = KaishWorker::spawn(&root)
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        let out = worker
            .run(input.script)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(format_output(&out))]))
    }
}

#[tool_handler]
impl rmcp::ServerHandler for KaiboHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            // Identify as kaibo, not rmcp (from_build_env reports the rmcp crate).
            server_info: Implementation {
                name: "kaibo".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("kaibo (解剖)".to_string()),
                description: None,
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "kaibo (解剖) — ask `consult` a question about a codebase. kaibo explores \
                 the project read-only through a kaish shell and returns a cited answer. \
                 It never modifies files and cannot run external commands."
                    .to_string(),
            ),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: kaibo_resources(),
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        read_kaibo_resource(&request.uri)
    }
}

/// The resources kaibo advertises. Pure (no `self`, no transport) so the dispatch
/// is unit-testable without fabricating a `RequestContext`.
fn kaibo_resources() -> Vec<rmcp::model::Resource> {
    let resource = RawResource {
        mime_type: Some("text/markdown".to_string()),
        description: Some(
            "kaish read-only shell cheatsheet: line-number browsing idioms, \
             builtins, and the exit-code contract."
                .to_string(),
        ),
        ..RawResource::new(KAISH_SYNTAX_URI, "kaish syntax")
    };
    vec![resource.no_annotation()]
}

/// Read one kaibo resource by URI. An unknown URI is a not-found error, not a
/// silent empty result — a caller asking for the wrong thing should hear so.
fn read_kaibo_resource(uri: &str) -> Result<ReadResourceResult, McpError> {
    if uri != KAISH_SYNTAX_URI {
        return Err(McpError::resource_not_found(
            format!("unknown resource: {uri}"),
            None,
        ));
    }
    Ok(ReadResourceResult {
        contents: vec![ResourceContents::text(kaish_syntax_resource(), KAISH_SYNTAX_URI)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_kaish_syntax_resource() {
        let uris: Vec<String> = kaibo_resources()
            .into_iter()
            .map(|r| r.raw.uri)
            .collect();
        assert!(
            uris.iter().any(|u| u == KAISH_SYNTAX_URI),
            "list_resources must advertise {KAISH_SYNTAX_URI}, got {uris:?}"
        );
    }

    #[test]
    fn reads_the_kaish_syntax_resource_with_the_idioms_and_codes() {
        let result = read_kaibo_resource(KAISH_SYNTAX_URI).expect("known uri must read");
        let text = match &result.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            other => panic!("expected text contents, got {other:?}"),
        };
        for needle in ["cat -n", "rg", "read-only", "126", "124"] {
            assert!(text.contains(needle), "resource must mention {needle:?}");
        }
    }

    #[test]
    fn unknown_resource_uri_is_an_error() {
        assert!(
            read_kaibo_resource("kaibo://nope").is_err(),
            "an unknown URI must be a not-found error, not an empty success"
        );
    }
}
