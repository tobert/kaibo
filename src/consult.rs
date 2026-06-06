//! `consult` and the seams it's composed from, across providers.
//!
//! One primitive — [`run_phase`]: a model + preamble + an injected toolset, run as
//! a bounded tool loop. Every tool on the surface is that loop wearing different
//! clothes:
//!
//! - [`explore`] — a cheap model · `{run_kaish}` → a curated report.
//! - [`synthesize`] — a capable model · `{run_kaish}` · optional context → an answer.
//! - [`consult`] — a capable model · `{run_kaish, explore′}` → a cited answer. No
//!   rigid explorer→synth hand-off: the capable model decides when to delegate a
//!   broad sweep to the cheap [`RunExplore`] sub-agent vs. read a span directly.
//!
//! Provider choice (Anthropic / DeepSeek / Gemini / Lemonade) only changes which
//! client is constructed (see `with_provider_client!`); the loop is shared
//! generically via [`CompletionClient`]. Each tool gets its own fresh
//! [`KaishWorker`] (a kernel rooted at the project), and so does every `explore′`
//! delegation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rig::client::CompletionClient;
use rig::completion::{Prompt, ToolDefinition};
use rig::providers::{anthropic, deepseek, gemini, openai};
use rig::tool::{Tool, ToolDyn};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::credentials::{self, Provider};
use crate::explorer::RunKaish;
use crate::kaish_syntax::KAISH_SYNTAX_CORE;
use crate::sandbox::KaishWorker;

/// Construct the rig client for `$provider` (loading its key, or the base URL for
/// the local one) and bind it to `$client` for `$body`. The four provider client
/// types differ, so this is the single place those arms live — `consult`,
/// `explore`, and `synthesize` all dispatch through it. `$body` runs inside the
/// arm, so it may `.await` and use `?`.
macro_rules! with_provider_client {
    ($provider:expr, |$client:ident| $body:expr) => {{
        match $provider {
            Provider::Anthropic => {
                let key = credentials::load($provider)?;
                let $client = anthropic::Client::new(&key)
                    .map_err(|e| anyhow!("anthropic client init: {e}"))?;
                $body
            }
            Provider::DeepSeek => {
                let key = credentials::load($provider)?;
                let $client = deepseek::Client::new(&key)
                    .map_err(|e| anyhow!("deepseek client init: {e}"))?;
                $body
            }
            Provider::Gemini => {
                let key = credentials::load($provider)?;
                let $client = gemini::Client::new(&key)
                    .map_err(|e| anyhow!("gemini client init: {e}"))?;
                $body
            }
            Provider::Lemonade => {
                // Local OpenAI-compatible server: point rig's completions client at
                // the lemonade base URL. The bearer token is required-but-ignored
                // (lemonade does no auth), so any non-empty string serves.
                let base_url = credentials::lemonade_base_url();
                let $client = openai::CompletionsClient::builder()
                    .api_key("lemonade")
                    .base_url(&base_url)
                    .build()
                    .map_err(|e| anyhow!("lemonade client init at {base_url}: {e}"))?;
                $body
            }
        }
    }};
}

/// Explorer preamble: gather and organize evidence, don't conclude. Composes the
/// shared [`KAISH_SYNTAX_CORE`] so the shell idioms and exit-code contract are
/// stated in exactly one place.
pub fn report_preamble() -> String {
    format!(
        "You are a code explorer. {KAISH_SYNTAX_CORE}\n\n\
         Your job is NOT to write a polished answer. Investigate the question, then \
         produce a CURATED REPORT for a synthesizer who will write the final answer: \
         list the relevant files with `file:line` locations, quote the short key \
         snippets verbatim, and note what each means for the question. Be precise and \
         evidence-first; omit filler. The synthesizer trusts your citations, so make \
         them exact."
    )
}

/// Synth preamble: answer from the report, reach for tools only to fill a gap.
pub const SYNTH_PREAMBLE: &str = "\
You answer a question about a codebase. You are given the user's question and a \
CURATED REPORT from an explorer who already investigated the READ-ONLY project. \
Write the final answer, grounded in the report and citing concrete `file:line`.

You also have the `run_kaish` tool (read-only kaish shell) as a FALLBACK: use it \
sparingly to fetch or confirm a precise span the report pointed to but didn't fully \
quote. Do not re-explore from scratch — the report is primary, the tool is a \
backstop for a specific gap.";

/// Token budget for model "thinking"/reasoning, for the providers that expose a
/// request-time toggle. Sized well under [`ConsultConfig`]'s `max_tokens` so the
/// reasoning never starves the actual answer (a thinking model that spends its
/// whole budget reasoning returns empty content — we saw exactly that on Gemma).
/// Anthropic additionally *requires* `max_tokens > budget_tokens`.
pub const THINKING_BUDGET: u64 = 8192;

/// Provider-specific request params that turn **thinking on**, or `None` when the
/// provider reasons without a switch.
///
/// - **Anthropic** — a top-level `thinking` block; rig flattens `additional_params`
///   straight into the Messages request.
/// - **Gemini** — `generationConfig.thinkingConfig` (camelCase; rig parses this
///   into a typed `GenerationConfig`, so the shape must be exact). Note: Gemini 3
///   models take `thinkingLevel` instead of `thinkingBudget` — if a default model
///   id moves to a 3.x line this may need to switch (tracked in `docs/issues.md`).
/// - **DeepSeek** — reasoner models (`*-pro`) emit `reasoning_content` on their own;
///   there is no request toggle. `None`.
/// - **Lemonade/Gemma** — the local server already reasons (`--reasoning-format
///   auto`); nothing to send. `None`.
pub fn thinking_params(provider: Provider) -> Option<Value> {
    match provider {
        Provider::Anthropic => Some(json!({
            "thinking": { "type": "enabled", "budget_tokens": THINKING_BUDGET }
        })),
        Provider::Gemini => Some(json!({
            "generationConfig": {
                "thinkingConfig": {
                    "thinkingBudget": THINKING_BUDGET,
                    "includeThoughts": true
                }
            }
        })),
        Provider::DeepSeek | Provider::Lemonade => None,
    }
}

/// Default (explorer, synth) model ids per provider. Values drift — see the
/// `provider-model-ids` note for the source-of-truth configs they track.
pub fn default_models(provider: Provider) -> (&'static str, &'static str) {
    match provider {
        // (cheap explorer, capable synth)
        Provider::Anthropic => ("claude-haiku-4-5", "claude-sonnet-4-6"),
        Provider::DeepSeek => ("deepseek-v4-flash", "deepseek-v4-pro"),
        // Gemini: LITE explorer; flash (not pro) synth — pro is API-flaky.
        Provider::Gemini => ("gemini-flash-lite-latest", "gemini-3.5-flash"),
        // Local lemonade/Gemma: the small E4B drives the tool-heavy exploration,
        // the 26B MoE writes the answer. Both carry the `tool-calling` label.
        Provider::Lemonade => ("Gemma-4-E4B-it-GGUF", "Gemma-4-26B-A4B-it-GGUF"),
    }
}

/// Tunables for a two-phase consult. Model ids are optional overrides; when unset
/// the provider's [`default_models`] are used.
#[derive(Debug, Clone)]
pub struct ConsultConfig {
    pub explorer_model: Option<String>,
    pub synth_model: Option<String>,
    pub explorer_max_turns: usize,
    pub synth_max_turns: usize,
    pub max_tokens: u64,
}

impl Default for ConsultConfig {
    fn default() -> Self {
        Self {
            explorer_model: None,
            synth_model: None,
            // explorer_max_turns bounds each cheap explore′ sweep — let it rip.
            // synth_max_turns now bounds the recomposed consult's *whole* driver
            // loop (it delegates sweeps AND reads spans), so the old 8 was far too
            // low — a multi-part question blew it. ≥100 gives the driver room.
            explorer_max_turns: 50,
            synth_max_turns: 100,
            // Generous output headroom by design: few high-value turns, not long
            // chats, and **thinking is on** — reasoning eats this budget, so it must
            // sit well above THINKING_BUDGET or the answer gets truncated to empty.
            max_tokens: 16384,
        }
    }
}

impl ConsultConfig {
    /// Resolve (explorer, synth) model ids for `provider`, applying overrides.
    pub fn resolved_models(&self, provider: Provider) -> (String, String) {
        let (de, ds) = default_models(provider);
        (
            self.explorer_model.clone().unwrap_or_else(|| de.to_string()),
            self.synth_model.clone().unwrap_or_else(|| ds.to_string()),
        )
    }
}

/// The result of a consult: the final answer plus the explorer's report (kept so
/// callers can inspect/debug the hand-off, and for future session storage).
#[derive(Debug, Clone)]
pub struct ConsultOutput {
    pub answer: String,
    pub report: String,
}

/// Build the synth's user prompt from the question and the explorer's report.
///
/// Pure and offline-testable: this is the entire explorer→synth hand-off, and
/// getting the framing right matters, so it's worth pinning in a test.
pub fn synth_user_prompt(question: &str, report: &str) -> String {
    format!(
        "Question:\n{question}\n\n\
         Explorer's curated report:\n{report}\n\n\
         Using the report (and the run_kaish fallback only if a specific detail is \
         missing), write the final answer to the question."
    )
}

/// One model loop, parameterized by its toolset: build an agent with `preamble`,
/// hand it `tools`, and run its bounded tool loop. Generic over the provider.
///
/// The toolset is injected (not hardcoded to `run_kaish`) so the same loop is the
/// primitive behind every tool on the surface — `explore` ({run_kaish}),
/// `synthesize` ({run_kaish}), and the recomposed `consult` ({run_kaish,
/// explore′}). Heterogeneous tools erase to `Box<dyn ToolDyn>`, so this stays
/// monomorphic in its toolset. The caller owns each tool's worker lifetime.
#[allow(clippy::too_many_arguments)] // each arg is a distinct, named loop input
pub(crate) async fn run_phase<C>(
    client: &C,
    model: &str,
    preamble: &str,
    max_tokens: u64,
    user_prompt: String,
    max_turns: usize,
    thinking: Option<&Value>,
    tools: Vec<Box<dyn ToolDyn>>,
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let mut builder = client
        .agent(model)
        .preamble(preamble)
        .max_tokens(max_tokens);
    // Thinking on (both phases) where the provider takes a request-time toggle.
    if let Some(params) = thinking {
        builder = builder.additional_params(params.clone());
    }
    let agent = builder.tools(tools).build();
    agent
        .prompt(user_prompt)
        .max_turns(max_turns)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            // rig treats hitting the turn cap as a fatal error (no partial result).
            // Make it actionable rather than opaque.
            if msg.contains("max turn") {
                anyhow!(
                    "model used all {max_turns} tool turns without concluding — raise \
                     the turn cap or narrow the question ({msg})"
                )
            } else {
                anyhow!("model loop failed: {msg}")
            }
        })
}

/// `explore′` — the explorer unit wrapped as a rig [`Tool`] the consult loop can
/// call. Its `call` runs a *nested* agent: a cheap model driving `{run_kaish}`
/// over a fresh kernel, returning a curated report. This is what lets the capable
/// `consult` model delegate a broad repo sweep instead of reading every span
/// itself.
///
/// `!Send` care (an invariant): the nested kernel stays on its `KaishWorker`
/// thread and never crosses the `.await` here — only the `Send` worker handle
/// does — so `call`'s future is `Send`, as rig requires. `tests/explore_send.rs`
/// pins this at compile time.
pub struct RunExplore<C> {
    client: C,
    model: String,
    max_tokens: u64,
    max_turns: usize,
    thinking: Option<Value>,
    root: PathBuf,
    /// Every delegated report is appended here, so the caller can surface what the
    /// sweeps found (the recomposed `consult`'s `report`) and a test can observe
    /// that a delegation actually happened.
    reports: Arc<Mutex<Vec<String>>>,
}

impl<C> RunExplore<C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: C,
        model: impl Into<String>,
        max_tokens: u64,
        max_turns: usize,
        thinking: Option<Value>,
        root: impl Into<PathBuf>,
        reports: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            client,
            model: model.into(),
            max_tokens,
            max_turns,
            thinking,
            root: root.into(),
            reports,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RunExploreArgs {
    /// The question or sub-question to investigate across the repo.
    pub question: String,
}

/// The nested explore loop failed (the sub-agent errored or its worker died).
#[derive(Debug)]
pub struct RunExploreError(String);

impl std::fmt::Display for RunExploreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "explore failed: {}", self.0)
    }
}

impl std::error::Error for RunExploreError {}

impl<C> Tool for RunExplore<C>
where
    C: CompletionClient + Send + Sync,
    C::CompletionModel: 'static,
{
    const NAME: &'static str = "explore";
    type Error = RunExploreError;
    type Args = RunExploreArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Delegate a broad sweep to a fast investigator that rips \
                through the repo on a read-only kaish shell and reports back with \
                concrete `file:line` citations. Give it a focused question; use it \
                to cover breadth, and read precise spans yourself with `run_kaish`."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "the question or sub-question to investigate"
                    }
                },
                "required": ["question"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // A fresh kernel per call (the §2.1 cost note: a KaishWorker per explore′).
        let worker = KaishWorker::spawn(&self.root).map_err(|e| RunExploreError(e.to_string()))?;
        let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(RunKaish::new(worker))];
        // Reuse the one loop — explore′ is just run_phase with the explorer model.
        let report = run_phase(
            &self.client,
            &self.model,
            &report_preamble(),
            self.max_tokens,
            args.question,
            self.max_turns,
            self.thinking.as_ref(),
            tools,
        )
        .await
        .map_err(|e| RunExploreError(format!("{e:#}")))?;
        // Lock poisoning means another delegation panicked — surface it, don't mask.
        self.reports
            .lock()
            .expect("explore report sink poisoned")
            .push(report.clone());
        Ok(report)
    }
}

/// One explore loop against an already-constructed client: a cheap model drives
/// `{run_kaish}` over a fresh kernel rooted at `root`, returning a curated report.
async fn explore_with<C>(
    client: &C,
    model: &str,
    root: &Path,
    cfg: &ConsultConfig,
    thinking: Option<&Value>,
    question: &str,
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let tools: Vec<Box<dyn ToolDyn>> =
        vec![Box::new(RunKaish::new(KaishWorker::spawn(root)?))];
    run_phase(
        client,
        model,
        &report_preamble(),
        cfg.max_tokens,
        question.to_string(),
        cfg.explorer_max_turns,
        thinking,
        tools,
    )
    .await
}

/// The `explore` unit: a cheap model drives `{run_kaish}` over `root` and returns
/// a curated report. The standalone seam behind the MCP `explore` tool — multi-
/// provider, unlike the legacy Anthropic-only [`crate::explorer::explore`].
pub async fn explore(
    question: &str,
    root: impl Into<PathBuf>,
    provider: Provider,
    cfg: &ConsultConfig,
) -> Result<String> {
    let root = root.into();
    let (explorer_model, _) = cfg.resolved_models(provider);
    let thinking = thinking_params(provider);
    let thinking = thinking.as_ref();

    with_provider_client!(provider, |client| {
        explore_with(&client, &explorer_model, &root, cfg, thinking, question).await
    })
}

/// Build the standalone `synthesize` user prompt. Pure and offline-testable.
///
/// With `context`, frame it as primary evidence to ground in (question first, then
/// context). With no context — or whitespace-only — steer the model to investigate
/// directly via `run_kaish` rather than guess, so the answer stays grounded either
/// way.
pub fn synthesize_user_prompt(question: &str, context: Option<&str>) -> String {
    match context.map(str::trim).filter(|c| !c.is_empty()) {
        Some(context) => format!(
            "Question:\n{question}\n\n\
             Context (supplied material — typically a curated explorer report or \
             pasted source):\n{context}\n\n\
             Answer the question, grounded in the context above. Use the `run_kaish` \
             tool to verify a citation or fetch a precise span the context points to \
             but doesn't fully quote; cite concrete `file:line`."
        ),
        None => format!(
            "Question:\n{question}\n\n\
             No context was supplied. Investigate the project yourself with the \
             `run_kaish` tool (a read-only kaish shell) and answer from what you find, \
             citing concrete `file:line`."
        ),
    }
}

/// Standalone synth preamble: interactive, with `run_kaish` as a first-class
/// investigation tool (not just a fallback). Composes the shared
/// [`KAISH_SYNTAX_CORE`] so the shell idioms and exit-code contract don't drift.
pub fn synthesize_preamble() -> String {
    format!(
        "You answer a question about a codebase. {KAISH_SYNTAX_CORE}\n\n\
         You may be given CONTEXT — a curated explorer report or pasted material. \
         When context is present, treat it as primary evidence and ground your answer \
         in it, using `run_kaish` to verify a citation or fetch a precise span it \
         points to. When context is thin or absent, investigate directly with \
         `run_kaish` and answer from what you find. Either way, cite concrete \
         `file:line` locations so every claim traces back to evidence."
    )
}

/// One synth loop against an already-constructed client: a capable model answers
/// `user_prompt` with `{run_kaish}` available over a fresh kernel rooted at `root`.
async fn synthesize_with<C>(
    client: &C,
    model: &str,
    root: &Path,
    cfg: &ConsultConfig,
    thinking: Option<&Value>,
    user_prompt: String,
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let tools: Vec<Box<dyn ToolDyn>> =
        vec![Box::new(RunKaish::new(KaishWorker::spawn(root)?))];
    run_phase(
        client,
        model,
        &synthesize_preamble(),
        cfg.max_tokens,
        user_prompt,
        cfg.synth_max_turns,
        thinking,
        tools,
    )
    .await
}

/// The standalone `synthesize` seam: a capable model answers `question`, grounded
/// in an optional caller-supplied `context` (typically an `explore` report or
/// pasted material), with `run_kaish` to verify or fill a precise gap. Defaults to
/// the capable synth model — a real outside opinion, not the cheap explorer.
pub async fn synthesize(
    question: &str,
    context: Option<&str>,
    root: impl Into<PathBuf>,
    provider: Provider,
    cfg: &ConsultConfig,
) -> Result<String> {
    let root = root.into();
    let (_, synth_model) = cfg.resolved_models(provider);
    let thinking = thinking_params(provider);
    let thinking = thinking.as_ref();
    let user_prompt = synthesize_user_prompt(question, context);

    with_provider_client!(provider, |client| {
        synthesize_with(&client, &synth_model, &root, cfg, thinking, user_prompt).await
    })
}

/// The recomposed `consult` driver: one capable model, two tools. Composes the
/// shared [`KAISH_SYNTAX_CORE`] (for `run_kaish`) and frames `explore` as the way
/// to cover breadth. Positive framing on purpose — weaker/local models loop on
/// blanket prohibitions, so reinforce the grounded behavior we want.
pub fn consult_preamble() -> String {
    format!(
        "You answer a question about a codebase, grounded in evidence and citing \
         concrete `file:line`. {KAISH_SYNTAX_CORE}\n\n\
         You also have a second tool, `explore`: it delegates a broad sweep to a \
         fast investigator that rips through the repo and reports back with \
         `file:line` citations. Reach for `explore` to cover breadth — find where a \
         thing lives, gather the relevant files — and use `run_kaish` to read a \
         precise span yourself and confirm a detail. Build your answer from what \
         they return: quote the key snippet, name its `file:line`, and let the \
         evidence carry the claim. Where the evidence settles the question, answer \
         it fully; where it reaches its edge, say so and name what would close the gap."
    )
}

/// Build the recomposed `consult` toolset: `{run_kaish, explore′}`. Factored out so
/// the wiring (both tools present, explore′ pointed at the cheap model) is
/// unit-testable without a live model. `reports` collects each `explore′` sweep.
fn consult_tools<C>(
    client: &C,
    explorer_model: &str,
    root: &Path,
    cfg: &ConsultConfig,
    thinking: Option<&Value>,
    reports: Arc<Mutex<Vec<String>>>,
) -> Result<Vec<Box<dyn ToolDyn>>>
where
    C: CompletionClient + Clone + Send + Sync + 'static,
    C::CompletionModel: 'static,
{
    // run_kaish for precise reads by the consult model itself.
    let worker = KaishWorker::spawn(root)?;
    // explore′ for delegated breadth: same explore unit, wrapped as a tool, pointed
    // at the cheap explorer model. Bounded by explorer_max_turns per sweep; no cap
    // on how many times consult may delegate (Amy's call — watch real behavior).
    let explore = RunExplore::new(
        client.clone(),
        explorer_model,
        cfg.max_tokens,
        cfg.explorer_max_turns,
        thinking.cloned(),
        root,
        reports,
    );
    Ok(vec![Box::new(RunKaish::new(worker)), Box::new(explore)])
}

/// Run a `consult` against an already-constructed provider client.
///
/// One loop, two tools — no rigid explorer→synth hand-off. The capable model
/// decides when to delegate a sweep to the cheap `explore′` vs. read a span
/// directly with `run_kaish`. `ConsultOutput.report` aggregates whatever the
/// `explore′` sweeps returned (empty if the model read everything itself).
async fn consult_with<C>(
    client: &C,
    question: &str,
    root: &Path,
    explorer_model: &str,
    synth_model: &str,
    cfg: &ConsultConfig,
    thinking: Option<&Value>,
) -> Result<ConsultOutput>
where
    C: CompletionClient + Clone + Send + Sync + 'static,
    C::CompletionModel: 'static,
{
    let reports = Arc::new(Mutex::new(Vec::<String>::new()));
    let tools = consult_tools(client, explorer_model, root, cfg, thinking, reports.clone())?;

    let answer = run_phase(
        client,
        synth_model,
        &consult_preamble(),
        cfg.max_tokens,
        question.to_string(),
        cfg.synth_max_turns,
        thinking,
        tools,
    )
    .await
    .context("consult loop")?;

    let report = reports
        .lock()
        .expect("explore report sink poisoned")
        .join("\n\n---\n\n");
    Ok(ConsultOutput { answer, report })
}

/// Run a two-phase consult against `root` using `provider`.
///
/// Loads the provider's key (env var or key-file) and resolves model ids from
/// `cfg` (falling back to [`default_models`]).
pub async fn consult(
    question: &str,
    root: impl Into<PathBuf>,
    provider: Provider,
    cfg: &ConsultConfig,
) -> Result<ConsultOutput> {
    let root = root.into();
    let (explorer_model, synth_model) = cfg.resolved_models(provider);
    // Thinking on, both phases, where the provider takes a request-time toggle.
    let thinking = thinking_params(provider);
    let thinking = thinking.as_ref();

    with_provider_client!(provider, |client| {
        consult_with(&client, question, &root, &explorer_model, &synth_model, cfg, thinking).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// The recomposed consult must drive BOTH tools: a direct `run_kaish` and the
    /// delegated `explore′`. Pin the wiring offline — no model, just the toolset.
    #[test]
    fn consult_toolset_has_both_run_kaish_and_explore() {
        let dir = tempdir().unwrap();
        // Construction is offline (no network): the key is never validated here.
        let client = anthropic::Client::new("test-key").unwrap();
        let cfg = ConsultConfig::default();
        let reports = Arc::new(Mutex::new(Vec::new()));

        let tools = consult_tools(&client, "explorer-model", dir.path(), &cfg, None, reports)
            .expect("building the consult toolset should succeed");

        let names: Vec<String> = tools.iter().map(|t| t.name()).collect();
        assert!(names.iter().any(|n| n == "run_kaish"), "missing run_kaish, got {names:?}");
        assert!(names.iter().any(|n| n == "explore"), "missing explore′, got {names:?}");
    }
}
