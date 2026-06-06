//! Two-phase `consult` (the dpal pattern), across providers.
//!
//! 1. **Explore** — a cheap model drives the read-only kaish sandbox via
//!    `run_kaish` and writes a *curated report*: relevant files, `file:line`
//!    locations, short quoted snippets. It does NOT write the final answer.
//! 2. **Synthesize** — a stronger model writes the answer grounded in that
//!    report, with `run_kaish` available as a *fallback* to fetch a precise span
//!    the report pointed to but didn't fully quote.
//!
//! Provider choice (Anthropic / DeepSeek / Gemini) only changes which client is
//! constructed; the phase logic is shared generically via [`CompletionClient`].
//! Each phase gets its own fresh [`KaishWorker`] (kernel rooted at the project),
//! so the synth starts at the root rather than wherever the explorer wandered.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::providers::{anthropic, deepseek, gemini, openai};
use serde_json::{json, Value};

use crate::credentials::{self, Provider};
use crate::explorer::RunKaish;
use crate::kaish_syntax::KAISH_SYNTAX_CORE;
use crate::sandbox::KaishWorker;

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
            // The explorer is cheap and tool-heavy — let it rip. The synth is the
            // expensive model and only needs a few fallback fetches, so keep it lean.
            explorer_max_turns: 50,
            synth_max_turns: 8,
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

/// One model phase: spawn a fresh read-only kernel, build an agent with the
/// `run_kaish` tool, and run its bounded tool loop. Generic over the provider.
async fn run_phase<C>(
    client: &C,
    model: &str,
    preamble: &str,
    max_tokens: u64,
    root: &Path,
    user_prompt: String,
    max_turns: usize,
    thinking: Option<&Value>,
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let worker = KaishWorker::spawn(root)?;
    let mut builder = client
        .agent(model)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .tool(RunKaish::new(worker));
    // Thinking on (both phases) where the provider takes a request-time toggle.
    if let Some(params) = thinking {
        builder = builder.additional_params(params.clone());
    }
    let agent = builder.build();
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

/// Run both phases against an already-constructed provider client.
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
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let report = run_phase(
        client,
        explorer_model,
        &report_preamble(),
        cfg.max_tokens,
        root,
        question.to_string(),
        cfg.explorer_max_turns,
        thinking,
    )
    .await
    .context("explore phase")?;

    let answer = run_phase(
        client,
        synth_model,
        SYNTH_PREAMBLE,
        cfg.max_tokens,
        root,
        synth_user_prompt(question, &report),
        cfg.synth_max_turns,
        thinking,
    )
    .await
    .context("synth phase")?;

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

    // Keyed providers load a key; the local one is reached by base URL instead,
    // so credential loading is per-arm rather than shared up front.
    match provider {
        Provider::Anthropic => {
            let key = credentials::load(provider)?;
            let client =
                anthropic::Client::new(&key).map_err(|e| anyhow!("anthropic client init: {e}"))?;
            consult_with(&client, question, &root, &explorer_model, &synth_model, cfg, thinking)
                .await
        }
        Provider::DeepSeek => {
            let key = credentials::load(provider)?;
            let client =
                deepseek::Client::new(&key).map_err(|e| anyhow!("deepseek client init: {e}"))?;
            consult_with(&client, question, &root, &explorer_model, &synth_model, cfg, thinking)
                .await
        }
        Provider::Gemini => {
            let key = credentials::load(provider)?;
            let client =
                gemini::Client::new(&key).map_err(|e| anyhow!("gemini client init: {e}"))?;
            consult_with(&client, question, &root, &explorer_model, &synth_model, cfg, thinking)
                .await
        }
        Provider::Lemonade => {
            // Local OpenAI-compatible server: point rig's completions client at
            // the lemonade base URL. The bearer token is required-but-ignored —
            // lemonade does no auth — so any non-empty string serves.
            let base_url = credentials::lemonade_base_url();
            let client = openai::CompletionsClient::builder()
                .api_key("lemonade")
                .base_url(&base_url)
                .build()
                .map_err(|e| anyhow!("lemonade client init at {base_url}: {e}"))?;
            consult_with(&client, question, &root, &explorer_model, &synth_model, cfg, thinking)
                .await
        }
    }
}
