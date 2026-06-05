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

use crate::credentials::{self, Provider};
use crate::explorer::RunKaish;
use crate::sandbox::KaishWorker;

/// Explorer preamble: gather and organize evidence, don't conclude.
pub const REPORT_PREAMBLE: &str = "\
You are a code explorer. You investigate a project on a READ-ONLY filesystem by \
calling the `run_kaish` tool, which runs a kaish (sh-like) script and returns its \
exit code, stdout, and stderr. You have kaish's builtins and pipelines: ls, cat, \
head, grep, rg, find, jq, awk, cut, sort, wc, diff, tree, and more — compose them \
with pipes and `$(...)`. Each call starts at the project root. Writes, `git`, \
`touch`, and external commands are refused.

Your job is NOT to write a polished answer. Investigate the question, then produce \
a CURATED REPORT for a synthesizer who will write the final answer: list the \
relevant files with `file:line` locations, quote the short key snippets verbatim, \
and note what each means for the question. Be precise and evidence-first; omit \
filler. The synthesizer trusts your citations, so make them exact.";

/// Synth preamble: answer from the report, reach for tools only to fill a gap.
pub const SYNTH_PREAMBLE: &str = "\
You answer a question about a codebase. You are given the user's question and a \
CURATED REPORT from an explorer who already investigated the READ-ONLY project. \
Write the final answer, grounded in the report and citing concrete `file:line`.

You also have the `run_kaish` tool (read-only kaish shell) as a FALLBACK: use it \
sparingly to fetch or confirm a precise span the report pointed to but didn't fully \
quote. Do not re-explore from scratch — the report is primary, the tool is a \
backstop for a specific gap.";

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
            max_tokens: 4096,
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
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let worker = KaishWorker::spawn(root)?;
    let agent = client
        .agent(model)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .tool(RunKaish::new(worker))
        .build();
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
) -> Result<ConsultOutput>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let report = run_phase(
        client,
        explorer_model,
        REPORT_PREAMBLE,
        cfg.max_tokens,
        root,
        question.to_string(),
        cfg.explorer_max_turns,
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

    // Keyed providers load a key; the local one is reached by base URL instead,
    // so credential loading is per-arm rather than shared up front.
    match provider {
        Provider::Anthropic => {
            let key = credentials::load(provider)?;
            let client =
                anthropic::Client::new(&key).map_err(|e| anyhow!("anthropic client init: {e}"))?;
            consult_with(&client, question, &root, &explorer_model, &synth_model, cfg).await
        }
        Provider::DeepSeek => {
            let key = credentials::load(provider)?;
            let client =
                deepseek::Client::new(&key).map_err(|e| anyhow!("deepseek client init: {e}"))?;
            consult_with(&client, question, &root, &explorer_model, &synth_model, cfg).await
        }
        Provider::Gemini => {
            let key = credentials::load(provider)?;
            let client =
                gemini::Client::new(&key).map_err(|e| anyhow!("gemini client init: {e}"))?;
            consult_with(&client, question, &root, &explorer_model, &synth_model, cfg).await
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
            consult_with(&client, question, &root, &explorer_model, &synth_model, cfg).await
        }
    }
}
