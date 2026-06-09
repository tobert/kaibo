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
//! Provider choice (Anthropic / DeepSeek / Gemini / OpenAI) only changes which
//! client is constructed (see `with_provider_client!`); the loop is shared
//! generically via [`CompletionClient`]. Each tool gets its own fresh
//! [`KaishWorker`] (a kernel rooted at the project), and so does every `explore′`
//! delegation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rig::client::CompletionClient;
use rig::completion::message::{ToolChoice, UserContent};
use rig::completion::{Message, Prompt, PromptError, ToolDefinition};
use rig::providers::{anthropic, deepseek, gemini, openai};
use rig::tool::{Tool, ToolDyn};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Profile;
use crate::credentials::ProviderKind;
use crate::explorer::RunKaish;
use crate::kaish_syntax::kaish_syntax_core;
use crate::progress::{NullSink, PhaseEvent, ProgressSink};
use crate::sandbox::{KaishWorker, SandboxConfig};
use crate::session::{QaTurn, SessionStore};

/// Construct the rig client for `$profile` (resolving its key, plus base URL for an
/// OpenAI-compatible one) and bind it to `$client` for `$body`. The kind selects the
/// client type; the *profile* carries the endpoint, key source, and models — so two
/// `openai` profiles build two distinct clients. This is the single place those four
/// client types live — `consult`, `explore`, and `synthesize` all dispatch through
/// it. `$body` runs inside the arm, so it may `.await` and use `?`.
macro_rules! with_provider_client {
    ($profile:expr, |$client:ident| $body:expr) => {{
        // Bind once so `$profile` is evaluated a single time even though each arm
        // also reads it (key resolution, base URL).
        let profile = $profile;
        // One HTTP backend, shared by whichever client the kind selects, carrying
        // the per-request deadline. rig exposes no native timeout and its prompt
        // loop is non-streaming, so a provider that connects but never responds
        // would hang the whole call with no other brake — the 2026-06-06 wedge
        // (~29 min; docs/issues.md). `timeout` bounds a single completion;
        // `connect_timeout` fails a dead endpoint fast (capped at the deadline so a
        // sub-10s profile timeout still dominates). Injected via rig's
        // `.http_client(..)`; only one match arm runs, so the move is exclusive.
        let http = reqwest::Client::builder()
            .timeout(profile.request_timeout)
            .connect_timeout(profile.request_timeout.min(Duration::from_secs(10)))
            .build()
            .map_err(|e| anyhow!("http client init: {e}"))?;
        match profile.kind {
            ProviderKind::Anthropic => {
                let key = profile.resolve_key()?;
                let $client = anthropic::Client::builder()
                    .api_key(&key)
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("anthropic client init: {e}"))?;
                $body
            }
            ProviderKind::DeepSeek => {
                let key = profile.resolve_key()?;
                let $client = deepseek::Client::builder()
                    .api_key(&key)
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("deepseek client init: {e}"))?;
                $body
            }
            ProviderKind::Gemini => {
                let key = profile.resolve_key()?;
                let $client = gemini::Client::builder()
                    .api_key(&key)
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("gemini client init: {e}"))?;
                $body
            }
            ProviderKind::Openai => {
                // Any OpenAI-compatible endpoint, addressed by the profile's base
                // URL. The key is optional for a keyless profile: `resolve_key`
                // returns the configured key or a placeholder the server ignores.
                let base_url = profile.resolved_base_url();
                let key = profile.resolve_key()?;
                let $client = openai::CompletionsClient::builder()
                    .api_key(&key)
                    .base_url(&base_url)
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("openai client init at {base_url}: {e}"))?;
                $body
            }
        }
    }};
}

/// Explorer preamble: gather and organize evidence, don't conclude. Composes the
/// shared [`kaish_syntax_core`] so the shell idioms and exit-code contract are
/// stated in exactly one place.
pub fn report_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You are a code explorer. {core}\n\n\
         Your job is NOT to write a polished answer. Investigate the question, then \
         produce a CURATED REPORT for a synthesizer who will write the final answer: \
         list the relevant files with `file:line` locations, quote the short key \
         snippets verbatim, and note what each means for the question. Be precise and \
         evidence-first; omit filler. The synthesizer trusts your citations, so make \
         them exact."
    )
}

/// Token budget for model "thinking"/reasoning, for the providers that expose a
/// request-time toggle. Sized well under [`ConsultConfig`]'s `max_tokens` so the
/// reasoning never starves the actual answer (a thinking model that spends its
/// whole budget reasoning returns empty content — we saw exactly that on Gemma).
/// Anthropic additionally *requires* `max_tokens > budget_tokens`.
pub const THINKING_BUDGET: u64 = 8192;

/// Does this Gemini id belong to the *pure* 3-line (e.g. `gemini-3-pro-preview`),
/// which takes `thinkingLevel` rather than the 2.5-era `thinkingBudget`?
///
/// The boundary is **empirical, not nominal**: `gemini-3.5-flash` *accepted*
/// `thinkingBudget` in the 2026-06-06 live test, so switching it to `thinkingLevel`
/// would be a silent regression of a working default. We only flip the ids the
/// official API + gemini-cli confirm want a level — `gemini-3-…` — and leave the
/// `3.5` minor line (and 2.x) on budget. Any new id past these wants a live probe,
/// not a guess. See `docs/issues.md` "Per-model request shaping".
fn is_gemini3_level(model: &str) -> bool {
    model == "gemini-3" || model.starts_with("gemini-3-")
}

/// Per-(provider, model) request params that turn **thinking on**, or `None` when the
/// model reasons without a switch. Model-aware: within Gemini, the 3-line takes
/// `thinkingLevel` while 2.5/3.5 take `thinkingBudget` (mutually exclusive — rig's
/// typed `ThinkingConfig` carries both fields). Usually reached via [`Dialect`],
/// which binds the kind+budget and resolves each phase against its own model.
///
/// - **Anthropic** — a top-level `thinking` block; rig flattens `additional_params`
///   straight into the Messages request.
/// - **Gemini** — `generationConfig.thinkingConfig` (camelCase; rig parses this
///   into a typed `GenerationConfig`, so the shape must be exact). `thinkingLevel`
///   for the 3-line ([`is_gemini3_level`]), else `thinkingBudget`.
/// - **DeepSeek** — the V4 hybrids (`deepseek-v4-flash`/`-pro`) toggle thinking at
///   request time: top-level `thinking.type` + `reasoning_effort`. rig flattens
///   `additional_params` into the body, so both land top-level; rig also round-trips
///   the response `reasoning_content` back on outgoing turns, so tool-call loops
///   don't trip DeepSeek's "echo the CoT or 400" rule. The budget doesn't apply —
///   DeepSeek controls depth by effort level, not a token budget.
/// - **OpenAI** — the generic OpenAI-compatible path; the local Gemma default
///   already reasons (`--reasoning-format auto`) and there's no portable toggle
///   across arbitrary endpoints, so nothing to send. `None`.
pub fn thinking_params(kind: ProviderKind, model: &str, budget: u64) -> Option<Value> {
    match kind {
        ProviderKind::Anthropic => Some(json!({
            "thinking": { "type": "enabled", "budget_tokens": budget }
        })),
        ProviderKind::Gemini => {
            // 3-line: thinkingLevel (mutually exclusive with budget). "high" matches
            // gemini-cli's investigator; rig deserializes it to ThinkingLevel::High.
            let thinking_config = if is_gemini3_level(model) {
                json!({ "thinkingLevel": "high", "includeThoughts": true })
            } else {
                json!({ "thinkingBudget": budget, "includeThoughts": true })
            };
            Some(json!({ "generationConfig": { "thinkingConfig": thinking_config } }))
        }
        ProviderKind::DeepSeek => Some(json!({
            // Explicit-on (it's the V4 default, but say it so intent survives a default
            // flip). "high" is the documented sweet spot; "max" is reserved for heavy
            // agent runs (a possible per-role tune — docs/issues.md).
            "thinking": { "type": "enabled" },
            "reasoning_effort": "high"
        })),
        ProviderKind::Openai => None,
    }
}

/// The phase a request is for. Temperature is role-sensitive: the explorer gathers
/// exact citations (cold), the synth composes the answer (a touch warmer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Explorer,
    Synth,
}

/// All of a request's model-shaping params merged into one `additional_params` blob:
/// thinking (per [`thinking_params`]) plus sampling. Per provider, sampling lives
/// where that wire format puts it — under `generationConfig` for Gemini (camelCase
/// `topP`), top-level for Anthropic/DeepSeek/OpenAI (rig flattens it into the body).
/// `None` only when nothing at all is set (no thinking, no sampling).
pub fn request_params(
    kind: ProviderKind,
    model: &str,
    budget: u64,
    temperature: Option<f64>,
    top_p: Option<f64>,
) -> Option<Value> {
    let mut params = thinking_params(kind, model, budget).unwrap_or_else(|| json!({}));
    let obj = params.as_object_mut().expect("thinking_params yields an object or {}");
    let mut wrote = !obj.is_empty();

    if kind == ProviderKind::Gemini {
        if temperature.is_some() || top_p.is_some() {
            let gc = obj
                .entry("generationConfig")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("generationConfig is an object");
            if let Some(t) = temperature {
                gc.insert("temperature".into(), json!(t));
            }
            if let Some(p) = top_p {
                gc.insert("topP".into(), json!(p));
            }
            wrote = true;
        }
    } else {
        if let Some(t) = temperature {
            obj.insert("temperature".into(), json!(t));
            wrote = true;
        }
        if let Some(p) = top_p {
            obj.insert("top_p".into(), json!(p));
            wrote = true;
        }
    }

    wrote.then_some(params)
}

/// How kaibo shapes requests for the models of one [`Profile`] — the single home for
/// per-model request tuning. Resolved once from a profile, then asked for *each
/// phase's own* model and role: a profile whose explorer and synth straddle a
/// capability line (a Gemini 2.x flash explorer with a Gemini 3 synth, say) gets each
/// arm fit correctly, rather than one shape computed once and shared.
#[derive(Debug, Clone)]
pub struct Dialect {
    kind: ProviderKind,
    thinking_budget: u64,
    explorer_temperature: Option<f64>,
    synth_temperature: Option<f64>,
    top_p: Option<f64>,
}

impl Dialect {
    /// Bind a kind and a thinking budget with no sampling overrides (the seam tests
    /// reach for, and any path that only cares about thinking).
    pub fn new(kind: ProviderKind, thinking_budget: u64) -> Self {
        Self {
            kind,
            thinking_budget,
            explorer_temperature: None,
            synth_temperature: None,
            top_p: None,
        }
    }

    /// The usual path: take the kind, budget, and sampling a resolved profile carries.
    pub fn from_profile(profile: &Profile) -> Self {
        Self {
            kind: profile.kind,
            thinking_budget: profile.thinking_budget,
            explorer_temperature: Some(profile.explorer_temperature),
            synth_temperature: Some(profile.synth_temperature),
            top_p: profile.top_p.into(),
        }
    }

    /// The full `additional_params` for `model` in `role`, or `None` when there's
    /// nothing to send. The per-phase resolution point.
    pub fn request_params(&self, model: &str, role: Role) -> Option<Value> {
        let temperature = match role {
            Role::Explorer => self.explorer_temperature,
            Role::Synth => self.synth_temperature,
        };
        request_params(self.kind, model, self.thinking_budget, temperature, self.top_p)
    }
}

/// Per-call loop tunables for a phase. Models, `max_tokens`, and the thinking budget
/// now live on the [`Profile`] (they track the provider/model); what remains here
/// are the loop bounds the caller may dial per request, plus the resolved
/// `max_tokens` the phase passes straight through to the API.
#[derive(Debug, Clone)]
pub struct ConsultConfig {
    /// Bounds each cheap `explore′` sweep — it's cheap, let it rip.
    pub explorer_max_turns: usize,
    /// Bounds the recomposed consult's *whole* driver loop (it delegates sweeps AND
    /// reads spans), so it must be generous — a multi-part question blew the old 8.
    pub synth_max_turns: usize,
    /// Output headroom, resolved from the profile. **Thinking is on**, so reasoning
    /// eats this budget — it must sit well above the profile's thinking budget or the
    /// answer gets truncated to empty.
    pub max_tokens: u64,
    /// Read-only sandbox limits applied to every kaish worker this phase spawns.
    pub sandbox: SandboxConfig,
    /// Where the phase's liveness goes: each delegated sweep and direct kaish read
    /// emits a [`PhaseEvent`] here. The server installs an adapter that renders these
    /// as MCP progress notifications when the caller asked for them; otherwise it's
    /// [`NullSink`], a no-op — so a stateless one-shot is byte-for-byte its old self.
    /// It rides on `ConsultConfig` because that's the one bundle already threaded into
    /// every phase fn and the toolset builders.
    pub progress: Arc<dyn ProgressSink>,
}

impl Default for ConsultConfig {
    fn default() -> Self {
        let d = crate::config::Defaults::default();
        Self {
            explorer_max_turns: d.explorer_max_turns,
            synth_max_turns: d.synth_max_turns,
            max_tokens: d.max_tokens,
            sandbox: SandboxConfig::default(),
            progress: Arc::new(NullSink),
        }
    }
}

/// The result of a consult: the final answer plus the explorer's report (kept so
/// callers can inspect/debug the hand-off, and for future session storage).
#[derive(Debug, Clone)]
pub struct ConsultOutput {
    pub answer: String,
    pub report: String,
}

/// The forced-finish instruction we append when a phase exhausts its turn cap.
/// Deliberately repeated front and back: weaker/local models latch onto the most
/// recent instruction, and a model that just spent every turn calling tools needs
/// firm, redundant steering to stop and write. Positive framing where it counts
/// ("write your full response now") bracketed by the hard constraint ("no more
/// tools"), per the `positive-prompt-framing` discipline.
const FINALIZE_NOTE: &str = "\
STOP — you have reached your research limit and may not call any more tools. Using \
only the evidence you have already gathered in this conversation, write your \
COMPLETE final response now, with its concrete `file:line` citations. Do not call \
any tool. Do not ask to continue. Write the full answer (or curated report) from \
what you already have — right now.";

/// Build the forced final turn from the transcript rig hands back at the turn cap.
///
/// Returns `(history, prompt)` for one more constrained completion: the prompt is
/// the conversation's last message with [`FINALIZE_NOTE`] appended (so the model
/// reads the "answer now" instruction last), and `history` is everything before it.
///
/// At the cap the transcript's last message is almost always the user's
/// tool-results turn (the loop broke just as the model was about to call yet
/// another tool), so the note rides along inside that same user message — we never
/// emit two user turns back to back, which some providers reject. If the transcript
/// somehow ends on an assistant turn, the note becomes a fresh trailing user turn
/// instead (valid after an assistant message). Pure and offline-testable.
fn finalize_prompt(mut chat_history: Vec<Message>) -> (Vec<Message>, Message) {
    match chat_history.pop() {
        Some(Message::User { mut content }) => {
            content.push(UserContent::text(FINALIZE_NOTE));
            (chat_history, Message::User { content })
        }
        Some(other) => {
            chat_history.push(other);
            (chat_history, Message::user(FINALIZE_NOTE))
        }
        None => (Vec::new(), Message::user(FINALIZE_NOTE)),
    }
}

/// One model loop, parameterized by its toolset: build an agent with `preamble`,
/// hand it the tools `make_tools` builds, and run its bounded tool loop. Generic
/// over the provider.
///
/// The toolset is injected via a *factory* (not a prebuilt `Vec`, and not hardcoded
/// to `run_kaish`) so the same loop is the primitive behind every tool on the
/// surface — `explore` ({run_kaish}), `synthesize` ({run_kaish}), and the
/// recomposed `consult` ({run_kaish, explore′}). The factory matters because of the
/// turn-cap recovery below: a fresh toolset is built for the forced final turn, and
/// `Box<dyn ToolDyn>` can't be cloned, so we rebuild rather than share. Each call
/// spawns its own `KaishWorker`(s); the caller owns their lifetime.
///
/// **Turn-cap recovery.** When the model uses every turn without concluding, rig
/// 0.34 returns `MaxTurnsError` carrying the *full transcript* (not the opaque
/// failure the old code mapped to an error). Rather than discard all that work, we
/// run one final constrained turn via [`finalize_after_max_turns`]: the tools stay
/// declared so the accumulated tool_use/tool_result history stays valid, but
/// `ToolChoice::None` forbids new calls — the model must answer from what it has.
#[allow(clippy::too_many_arguments)] // each arg is a distinct, named loop input
pub(crate) async fn run_phase<C, F>(
    client: &C,
    model: &str,
    preamble: &str,
    max_tokens: u64,
    user_prompt: String,
    max_turns: usize,
    thinking: Option<&Value>,
    progress: &dyn ProgressSink,
    make_tools: F,
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
    F: Fn() -> Result<Vec<Box<dyn ToolDyn>>>,
{
    let mut builder = client
        .agent(model)
        .preamble(preamble)
        .max_tokens(max_tokens);
    // Thinking on (both phases) where the provider takes a request-time toggle.
    if let Some(params) = thinking {
        builder = builder.additional_params(params.clone());
    }
    let agent = builder.tools(make_tools()?).build();
    match agent.prompt(user_prompt).max_turns(max_turns).await {
        Ok(answer) => Ok(answer),
        Err(PromptError::MaxTurnsError { chat_history, .. }) => {
            // The loop hit its cap and is about to write a forced final answer —
            // tell the caller, so a watching client sees "wrapping up" not silence.
            progress.emit(PhaseEvent::TurnCapReached);
            finalize_after_max_turns(
                client,
                model,
                preamble,
                max_tokens,
                thinking,
                make_tools()?,
                *chat_history,
                max_turns,
            )
            .await
        }
        Err(e) => Err(anyhow!("model loop failed: {e}")),
    }
}

/// The forced final turn after a phase hit its turn cap: replay the partial
/// transcript and make the model write its answer now, with tools declared (so the
/// history validates) but [`ToolChoice::None`] forbidding any further call. See
/// [`run_phase`]'s recovery note and [`finalize_prompt`].
#[allow(clippy::too_many_arguments)] // mirrors run_phase's loop inputs
async fn finalize_after_max_turns<C>(
    client: &C,
    model: &str,
    preamble: &str,
    max_tokens: u64,
    thinking: Option<&Value>,
    tools: Vec<Box<dyn ToolDyn>>,
    chat_history: Vec<Message>,
    max_turns: usize,
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let (history, prompt) = finalize_prompt(chat_history);
    let mut builder = client
        .agent(model)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .tool_choice(ToolChoice::None);
    if let Some(params) = thinking {
        builder = builder.additional_params(params.clone());
    }
    let agent = builder.tools(tools).build();
    // max_turns(1): one constrained completion. With tools forbidden the model can't
    // loop, so a single round is enough — and if a provider ignores ToolChoice::None
    // and still calls a tool, we surface that rather than recurse.
    agent
        .prompt(prompt)
        .with_history(history)
        .max_turns(1)
        .await
        .map_err(|e| {
            anyhow!(
                "model used all {max_turns} turns, and the forced final-answer turn \
                 also failed to conclude: {e}"
            )
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
    /// Sandbox limits for the fresh kernel each delegated sweep spawns.
    sandbox: SandboxConfig,
    /// Every delegated report is appended here, so the caller can surface what the
    /// sweeps found (the recomposed `consult`'s `report`) and a test can observe
    /// that a delegation actually happened.
    reports: Arc<Mutex<Vec<String>>>,
    /// Liveness for the sweep: brackets each delegation with start/finish, and is
    /// handed to the nested kernel's `run_kaish` so the sub-agent's own reads show
    /// through too (a delegated sweep is where a long consult spends its silence).
    progress: Arc<dyn ProgressSink>,
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
        sandbox: SandboxConfig,
        reports: Arc<Mutex<Vec<String>>>,
        progress: Arc<dyn ProgressSink>,
    ) -> Self {
        Self {
            client,
            model: model.into(),
            max_tokens,
            max_turns,
            thinking,
            root: root.into(),
            sandbox,
            reports,
            progress,
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
        // Bracket the delegation: the start carries the sub-question, the finish fires
        // on both success and failure (the `?` below short-circuits, so emit it before
        // unwrapping the result).
        self.progress.emit(PhaseEvent::SweepStarted { question: args.question.clone() });
        // Reuse the one loop — explore′ is just run_phase with the explorer model.
        // A fresh kernel per worker build (the §2.1 cost note: a KaishWorker per
        // explore′; run_phase may build a second for the turn-cap recovery turn).
        // The sub-agent's `run_kaish` carries the same sink, so its reads surface too.
        let result = run_phase(
            &self.client,
            &self.model,
            &report_preamble(),
            self.max_tokens,
            args.question,
            self.max_turns,
            self.thinking.as_ref(),
            self.progress.as_ref(),
            || -> Result<Vec<Box<dyn ToolDyn>>> {
                Ok(vec![Box::new(RunKaish::with_progress(
                    KaishWorker::spawn_with(&self.root, self.sandbox.clone())?,
                    self.progress.clone(),
                ))])
            },
        )
        .await;
        self.progress.emit(PhaseEvent::SweepFinished);
        let report = result.map_err(|e| RunExploreError(format!("{e:#}")))?;
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
    run_phase(
        client,
        model,
        &report_preamble(),
        cfg.max_tokens,
        question.to_string(),
        cfg.explorer_max_turns,
        thinking,
        cfg.progress.as_ref(),
        || -> Result<Vec<Box<dyn ToolDyn>>> {
            Ok(vec![Box::new(RunKaish::with_progress(
                KaishWorker::spawn_with(root, cfg.sandbox.clone())?,
                cfg.progress.clone(),
            ))])
        },
    )
    .await
}

/// The `explore` unit: a cheap model drives `{run_kaish}` over `root` and returns
/// a curated report. The standalone seam behind the MCP `explore` tool — built on
/// the generalized [`run_phase`] and multi-provider, so it works for any profile.
pub async fn explore(
    question: &str,
    root: impl Into<PathBuf>,
    profile: &Profile,
    cfg: &ConsultConfig,
) -> Result<String> {
    let root = root.into();
    // Single-model phase: resolve thinking against the explorer's own model.
    let thinking =
        Dialect::from_profile(profile).request_params(&profile.explorer_model, Role::Explorer);
    let thinking = thinking.as_ref();

    with_provider_client!(profile, |client| {
        explore_with(&client, &profile.explorer_model, &root, cfg, thinking, question).await
    })
}

/// Build the standalone `synthesize` user prompt. Pure and offline-testable.
///
/// With `context`, frame it as trusted starting evidence — a grounded `file:line`
/// rarely needs re-deriving — and steer the model to reach for `run_kaish` when it
/// needs *more* than the context gives (a span, a whole file, a detail left open),
/// not to re-verify what's likely right (question first, then context). With no
/// context — or whitespace-only — steer the model to investigate directly via
/// `run_kaish` rather than guess, so the answer stays grounded either way.
pub fn synthesize_user_prompt(question: &str, context: Option<&str>) -> String {
    match context.map(str::trim).filter(|c| !c.is_empty()) {
        Some(context) => format!(
            "Question:\n{question}\n\n\
             Context (supplied material — typically a curated explorer report or \
             pasted source):\n{context}\n\n\
             Answer the question, grounded in concrete `file:line` evidence. The \
             context is your starting evidence — when it cites a `file:line`, trust \
             it; a grounded citation rarely needs re-deriving. Reach for the \
             `run_kaish` tool when you need more than the context gives you: a span \
             it references but doesn't quote, a whole file or a large span when you \
             need the full picture, a detail it left open, or anything the question \
             asks that it didn't cover. If the code you read and the context \
             genuinely disagree, the code wins — the code is the only ground truth. \
             Cite concrete `file:line` for every claim."
        ),
        None => format!(
            "Question:\n{question}\n\n\
             No context was supplied. Investigate the project yourself with the \
             `run_kaish` tool (a read-only kaish shell) and answer from what you find, \
             citing concrete `file:line`."
        ),
    }
}

/// Standalone synth preamble: interactive, framing supplied context as trusted
/// starting evidence and `run_kaish` as the way to *get more* when the context
/// isn't enough — including whole files and large spans. Composes the shared
/// [`kaish_syntax_core`] so the shell idioms and exit-code contract don't drift.
pub fn synthesize_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You answer a question about a codebase, grounded in evidence and citing \
         concrete `file:line`. {core}\n\n\
         You may be given CONTEXT — a curated explorer report or pasted material. \
         Treat it as your starting evidence: when it cites a concrete `file:line`, \
         trust it — a grounded citation rarely needs re-deriving. The `run_kaish` \
         tool is yours to drive directly whenever the context isn't enough: fetch a \
         span it references but doesn't quote, read a whole file or a large span when \
         you need the full picture, chase a detail it left open, or investigate \
         anything the question reaches that the context didn't cover. Getting more \
         evidence when you need it is the normal move; you're never limited to what \
         you were handed. Where the code you read and the context genuinely disagree, \
         the code wins — the code is the only ground truth that matters. Ground every \
         claim in a concrete `file:line`."
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
    run_phase(
        client,
        model,
        &synthesize_preamble(),
        cfg.max_tokens,
        user_prompt,
        cfg.synth_max_turns,
        thinking,
        cfg.progress.as_ref(),
        || -> Result<Vec<Box<dyn ToolDyn>>> {
            Ok(vec![Box::new(RunKaish::with_progress(
                KaishWorker::spawn_with(root, cfg.sandbox.clone())?,
                cfg.progress.clone(),
            ))])
        },
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
    profile: &Profile,
    cfg: &ConsultConfig,
) -> Result<String> {
    let root = root.into();
    // Single-model phase: resolve thinking against the synth's own model.
    let thinking =
        Dialect::from_profile(profile).request_params(&profile.synth_model, Role::Synth);
    let thinking = thinking.as_ref();
    let user_prompt = synthesize_user_prompt(question, context);

    with_provider_client!(profile, |client| {
        synthesize_with(&client, &profile.synth_model, &root, cfg, thinking, user_prompt).await
    })
}

/// Build the consult driver's user prompt from the question and any prior session
/// turns. Pure and offline-testable: this framing is the whole of the multi-turn
/// hand-off, so it's worth pinning in a test.
///
/// With **no** history this is exactly the bare question — a stateless consult is
/// byte-for-byte unchanged. With history, prepend the prior `(question, answer)`
/// pairs as conversation context and steer the model to re-confirm any span a prior
/// answer cited: the exploration runs fresh every turn (we never replay the stored
/// report — it'd be stale), so the code is the ground truth, not the old answer.
pub fn consult_user_prompt(question: &str, history: &[QaTurn]) -> String {
    if history.is_empty() {
        return question.to_string();
    }
    let mut prompt = String::from(
        "This is a continuing conversation about the same codebase. Earlier turns, \
         oldest first:\n\n",
    );
    for (i, turn) in history.iter().enumerate() {
        prompt.push_str(&format!("[Turn {}]\nQ: {}\nA: {}\n\n", i + 1, turn.question, turn.answer));
    }
    prompt.push_str(&format!(
        "Use the earlier turns for context and continuity. Investigate fresh and \
         re-confirm any `file:line` an earlier answer cited before you rely on it — \
         the code is the ground truth, not the prior answer. Now answer the current \
         question:\n\n{question}"
    ));
    prompt
}

/// The recomposed `consult` driver: one capable model, two tools. Composes the
/// shared [`kaish_syntax_core`] (for `run_kaish`) and frames `explore` as the way
/// to cover breadth. Positive framing on purpose — weaker/local models loop on
/// blanket prohibitions, so reinforce the grounded behavior we want.
pub fn consult_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You answer a question about a codebase, grounded in evidence and citing \
         concrete `file:line`. {core}\n\n\
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
    dialect: &Dialect,
    reports: Arc<Mutex<Vec<String>>>,
) -> Result<Vec<Box<dyn ToolDyn>>>
where
    C: CompletionClient + Clone + Send + Sync + 'static,
    C::CompletionModel: 'static,
{
    // run_kaish for precise reads by the consult model itself — carries the sink so
    // the driver's own reads show up as progress alongside the delegated sweeps'.
    let worker = KaishWorker::spawn_with(root, cfg.sandbox.clone())?;
    // explore′ for delegated breadth: same explore unit, wrapped as a tool, pointed
    // at the cheap explorer model. Bounded by explorer_max_turns per sweep; no cap
    // on how many times consult may delegate (Amy's call — watch real behavior).
    // Thinking resolved against the explorer's own model, which may differ in
    // generation from the synth driver's.
    let explore = RunExplore::new(
        client.clone(),
        explorer_model,
        cfg.max_tokens,
        cfg.explorer_max_turns,
        dialect.request_params(explorer_model, Role::Explorer),
        root,
        cfg.sandbox.clone(),
        reports,
        cfg.progress.clone(),
    );
    Ok(vec![
        Box::new(RunKaish::with_progress(worker, cfg.progress.clone())),
        Box::new(explore),
    ])
}

/// Run a `consult` against an already-constructed provider client.
///
/// One loop, two tools — no rigid explorer→synth hand-off. The capable model
/// decides when to delegate a sweep to the cheap `explore′` vs. read a span
/// directly with `run_kaish`. `ConsultOutput.report` aggregates whatever the
/// `explore′` sweeps returned (empty if the model read everything itself).
async fn consult_with<C>(
    client: &C,
    user_prompt: &str,
    root: &Path,
    explorer_model: &str,
    synth_model: &str,
    cfg: &ConsultConfig,
    dialect: &Dialect,
) -> Result<ConsultOutput>
where
    C: CompletionClient + Clone + Send + Sync + 'static,
    C::CompletionModel: 'static,
{
    let reports = Arc::new(Mutex::new(Vec::<String>::new()));
    // Per-phase: the driver runs the synth model/role, the sweeps the explorer
    // model/role — each gets thinking + sampling fit to *itself*, not one shape
    // computed once and shared.
    let synth_thinking = dialect.request_params(synth_model, Role::Synth);

    let answer = run_phase(
        client,
        synth_model,
        &consult_preamble(),
        cfg.max_tokens,
        user_prompt.to_string(),
        cfg.synth_max_turns,
        synth_thinking.as_ref(),
        cfg.progress.as_ref(),
        // Rebuilt per call (main loop, and again if run_phase forces a final turn);
        // every build shares the one `reports` sink so all explore′ sweeps aggregate.
        || consult_tools(client, explorer_model, root, cfg, dialect, reports.clone()),
    )
    .await
    .context("consult loop")?;

    let report = reports
        .lock()
        .expect("explore report sink poisoned")
        .join("\n\n---\n\n");
    Ok(ConsultOutput { answer, report })
}

/// A consult turn's session binding: the store and the session id. `None` is a
/// stateless one-shot — no prior turns replayed, nothing recorded. `Some` makes it
/// multi-turn: replay this session's history into the prompt, record the answer.
pub type Session<'a> = (&'a SessionStore, &'a str);

/// One sessioned (or stateless) consult turn against an already-constructed client.
///
/// This is the whole multi-turn glue, made generic so it's driven offline by a mock
/// client in tests (the public [`consult`] builds the real client and wraps this):
/// read the session's prior turns → frame the prompt with them → run the consult →
/// record the answer. The exploration always runs fresh; only the lean
/// `(question, answer)` pairs are replayed. Recording happens *after* a successful
/// turn (`?` short-circuits a failure), so a failed consult never poisons the thread
/// with a half-answer the next turn would treat as established context.
#[allow(clippy::too_many_arguments)] // mirrors consult_with's loop inputs plus the session
pub(crate) async fn consult_session_turn<C>(
    client: &C,
    session: Option<Session<'_>>,
    question: &str,
    root: &Path,
    explorer_model: &str,
    synth_model: &str,
    cfg: &ConsultConfig,
    dialect: &Dialect,
) -> Result<ConsultOutput>
where
    C: CompletionClient + Clone + Send + Sync + 'static,
    C::CompletionModel: 'static,
{
    let history = match session {
        Some((store, id)) => store.history(id),
        None => Vec::new(),
    };
    let user_prompt = consult_user_prompt(question, &history);

    let out =
        consult_with(client, &user_prompt, root, explorer_model, synth_model, cfg, dialect).await?;

    if let Some((store, id)) = session {
        store.record(id, QaTurn::new(question, out.answer.clone()));
    }
    Ok(out)
}

/// Run a consult against `root` using `profile`.
///
/// Resolves the profile's key (env var or key-file) and takes its models, token
/// budget, and thinking budget; `cfg` carries the per-call loop bounds. `session`
/// binds this turn to a multi-turn thread (replay prior turns, record this one) or is
/// `None` for a stateless one-shot. The session seeds the driver's prompt but never
/// the exploration, which always runs fresh. See [`consult_session_turn`].
pub async fn consult(
    question: &str,
    root: impl Into<PathBuf>,
    profile: &Profile,
    cfg: &ConsultConfig,
    session: Option<Session<'_>>,
) -> Result<ConsultOutput> {
    let root = root.into();
    // Two phases, two models: pass the dialect down so each resolves thinking against
    // its own model (the driver vs the cheap explorer can straddle a capability line).
    let dialect = Dialect::from_profile(profile);

    with_provider_client!(profile, |client| {
        consult_session_turn(
            &client,
            session,
            question,
            &root,
            &profile.explorer_model,
            &profile.synth_model,
            cfg,
            &dialect,
        )
        .await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        has_tool, is_finalize_turn, provider_error, text_response, tool_call_response,
        transcript_text, RecordingSink, ScriptedClient,
    };
    use crate::session::SessionStore;
    use std::fs;
    use std::num::NonZeroUsize;
    use tempfile::tempdir;

    fn store() -> SessionStore {
        SessionStore::new(NonZeroUsize::new(4).unwrap())
    }

    /// A dialect that emits no thinking params — for the tests that exercise the loop
    /// wiring (report aggregation, sessions, turn caps) and don't care about thinking.
    /// `openai` reasons without a request toggle, so `thinking()` is always `None`.
    fn no_thinking() -> Dialect {
        Dialect::new(ProviderKind::Openai, 0)
    }

    /// A driver that answers immediately (no tools), echoing the current question into
    /// its answer so a later turn's replayed history is easy to spot. Keeps the
    /// session tests focused on the glue, not the loop. `consult_user_prompt` puts the
    /// current question last, so the final non-empty line is it.
    fn echo_client(model: &str) -> ScriptedClient {
        ScriptedClient::builder()
            .on_model(model, |req| {
                let shown = transcript_text(req);
                let question =
                    shown.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                Ok(text_response(format!("ANSWER[{question}]")))
            })
            .build()
    }

    /// A project root with one real file carrying a known marker, so the kaish reads
    /// in the e2e below hit real bytes — not a stub.
    fn project_with_marker() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/foo.rs"), "fn target_marker() {}\n").unwrap();
        dir
    }

    /// The load-bearing e2e: a scripted consult that delegates a sweep to `explore′`,
    /// reads a span itself, and answers — driving the *real* loop end to end with no
    /// network. This proves what the offline wiring test below cannot: the driver's
    /// `explore` tool call actually runs the nested explorer agent (which itself runs
    /// real kaish), and its report aggregates into `ConsultOutput.report`. If
    /// delegation silently broke, `report` would come back empty and this fails.
    #[tokio::test]
    async fn consult_delegates_to_explore_and_aggregates_the_report() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        const REPORT: &str = "EXPLORER_REPORT: src/foo.rs:1 fn target_marker";

        let client = ScriptedClient::builder()
            // The consult driver: delegate first, then read a span itself, then answer.
            // Content-driven, so it's robust to the loop's turn structure: it decides
            // from what it has already been shown, not from a call counter.
            .on_model(SYNTH, |req| {
                assert!(has_tool(req, "run_kaish"), "driver must have run_kaish");
                assert!(has_tool(req, "explore"), "driver must have explore′");
                let seen = transcript_text(req);
                if !seen.contains("EXPLORER_REPORT") {
                    // Haven't delegated yet → delegate a broad sweep.
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "where is target_marker defined?" }),
                    ))
                } else if !seen.contains("target_marker() {}") {
                    // Report in hand, but confirm the span directly via run_kaish.
                    Ok(tool_call_response(
                        "t-read",
                        "run_kaish",
                        json!({ "script": "cat -n src/foo.rs" }),
                    ))
                } else {
                    // Have the report and the confirmed span → answer.
                    Ok(text_response(
                        "ANSWER: target_marker is defined at src/foo.rs:1.",
                    ))
                }
            })
            // The explorer sub-agent: run real kaish once, then write its report.
            .on_model(EXPLORER, |req| {
                // Only run_kaish — the explorer has no nested explore′.
                assert!(has_tool(req, "run_kaish"), "explorer must have run_kaish");
                assert!(!has_tool(req, "explore"), "explorer must NOT nest explore′");
                let seen = transcript_text(req);
                if !seen.contains("target_marker") {
                    Ok(tool_call_response(
                        "t-rg",
                        "run_kaish",
                        json!({ "script": "rg -n target_marker src" }),
                    ))
                } else {
                    Ok(text_response(REPORT))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        let out = consult_with(
            &client,
            "Where is target_marker defined?",
            dir.path(),
            EXPLORER,
            SYNTH,
            &cfg,
            &no_thinking(),
        )
        .await
        .expect("scripted consult should succeed");

        // The driver concluded with its final answer.
        assert!(
            out.answer.contains("target_marker is defined at src/foo.rs:1"),
            "answer should be the driver's final text, got: {:?}",
            out.answer
        );
        // The teeth: the explorer's report aggregated into ConsultOutput.report. A
        // non-empty report here means the `explore` tool call genuinely drove the
        // nested explorer agent and the reports sink collected it.
        assert!(
            out.report.contains("EXPLORER_REPORT"),
            "explorer's report must aggregate into ConsultOutput.report, got: {:?}",
            out.report
        );

        // And the routing held: the cheap model saw the *report* preamble (explorer
        // role), the capable model saw the *consult* preamble (driver role).
        let explorer_reqs = client.requests_for(EXPLORER);
        assert!(!explorer_reqs.is_empty(), "explorer model was actually invoked");
        assert!(
            explorer_reqs[0].preamble.as_deref().unwrap_or("").contains("code explorer"),
            "explorer got the report preamble: {:?}",
            explorer_reqs[0].preamble
        );
        let synth_reqs = client.requests_for(SYNTH);
        assert!(
            synth_reqs[0].preamble.as_deref().unwrap_or("").contains("second tool, `explore`"),
            "driver got the consult preamble: {:?}",
            synth_reqs[0].preamble
        );
    }

    /// Progress reaches the *deep* loop. The same delegate-then-read flow as the e2e
    /// above, but driven through a [`RecordingSink`] on `ConsultConfig`: the sink must
    /// see the sweep bracket (start/finish), the nested explorer's own `run_kaish`
    /// read, and the driver's direct `run_kaish` read. This is the teeth for the whole
    /// threading job — were the sink dropped anywhere between `ConsultConfig` and the
    /// tools (or not forwarded into the nested explorer), one of these would be missing.
    #[tokio::test]
    async fn progress_events_reach_the_sweep_and_both_kaish_reads() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                let seen = transcript_text(req);
                if !seen.contains("EXPLORER_REPORT") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "where is target_marker defined?" }),
                    ))
                } else if !seen.contains("target_marker() {}") {
                    Ok(tool_call_response(
                        "t-read",
                        "run_kaish",
                        json!({ "script": "cat -n src/foo.rs" }),
                    ))
                } else {
                    Ok(text_response("ANSWER: src/foo.rs:1"))
                }
            })
            .on_model(EXPLORER, |req| {
                // Branch on tool *output* (run_kaish prefixes "exit:"), not on the
                // question text — the sub-question itself contains "target_marker", so
                // a content check on that would skip the read we're here to observe.
                if !transcript_text(req).contains("exit:") {
                    Ok(tool_call_response(
                        "t-rg",
                        "run_kaish",
                        json!({ "script": "rg -n target_marker src" }),
                    ))
                } else {
                    Ok(text_response("EXPLORER_REPORT: src/foo.rs:1"))
                }
            })
            .build();

        let dir = project_with_marker();
        let sink = Arc::new(RecordingSink::default());
        let cfg = ConsultConfig { progress: sink.clone(), ..ConsultConfig::default() };

        consult_with(
            &client,
            "Where is target_marker?",
            dir.path(),
            EXPLORER,
            SYNTH,
            &cfg,
            &no_thinking(),
        )
        .await
        .expect("scripted consult should succeed");

        let events = sink.events();
        assert!(
            events.contains(&PhaseEvent::SweepStarted {
                question: "where is target_marker defined?".into()
            }),
            "the delegation must announce its start: {events:?}"
        );
        assert!(
            events.contains(&PhaseEvent::SweepFinished),
            "the delegation must announce its finish: {events:?}"
        );
        assert!(
            events.contains(&PhaseEvent::KaishRun { script: "rg -n target_marker src".into() }),
            "the nested explorer's read must surface (sink threaded into the sub-agent): {events:?}"
        );
        assert!(
            events.contains(&PhaseEvent::KaishRun { script: "cat -n src/foo.rs".into() }),
            "the driver's own direct read must surface: {events:?}"
        );
        // Ordering sanity: the sweep starts before its nested read, which precedes the
        // sweep finishing — the bracket actually brackets.
        let pos = |want: &PhaseEvent| events.iter().position(|e| e == want).unwrap();
        let start = pos(&PhaseEvent::SweepStarted {
            question: "where is target_marker defined?".into(),
        });
        let nested = pos(&PhaseEvent::KaishRun { script: "rg -n target_marker src".into() });
        let finish = pos(&PhaseEvent::SweepFinished);
        assert!(start < nested && nested < finish, "sweep must bracket its nested read: {events:?}");
    }

    /// A stateless consult (default `ConsultConfig`) emits to the [`NullSink`] — no
    /// panic, no observable effect. The opt-out path stays a true no-op.
    #[tokio::test]
    async fn the_default_sink_is_a_silent_no_op() {
        const SYNTH: &str = "synth";
        let client = echo_client(SYNTH);
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();
        // No token, no recording sink — just prove the default path runs clean.
        consult_with(&client, "q", dir.path(), "explorer", SYNTH, &cfg, &no_thinking())
            .await
            .unwrap();
    }

    /// Multi-sweep: a driver that delegates to `explore′` more than once must
    /// aggregate every report into `ConsultOutput.report`, joined by the `---`
    /// separator. The single-delegation e2e can't see this — one report makes any
    /// join string look right.
    #[tokio::test]
    async fn multiple_sweeps_aggregate_into_one_report_joined_by_separator() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                // Delegate twice (distinguishable sub-questions), then answer. Count
                // the reports already gathered to decide which step we're on.
                let sweeps = transcript_text(req).matches("REPORT-").count();
                match sweeps {
                    0 => Ok(tool_call_response(
                        "s1",
                        "explore",
                        json!({ "question": "find the sandbox" }),
                    )),
                    1 => Ok(tool_call_response(
                        "s2",
                        "explore",
                        json!({ "question": "find the kaish syntax" }),
                    )),
                    _ => Ok(text_response("ANSWER from both sweeps")),
                }
            })
            .on_model(EXPLORER, |req| {
                // Each sweep answers immediately with a distinguishable report, keyed
                // off its sub-question (which is the explorer's whole prompt).
                if transcript_text(req).contains("sandbox") {
                    Ok(text_response("REPORT-SANDBOX: src/sandbox.rs:1"))
                } else {
                    Ok(text_response("REPORT-KAISH: src/kaish_syntax.rs:1"))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        let out =
            consult_with(&client, "two-part question", dir.path(), EXPLORER, SYNTH, &cfg, &no_thinking())
                .await
                .unwrap();

        assert!(out.report.contains("REPORT-SANDBOX"), "first sweep present: {:?}", out.report);
        assert!(out.report.contains("REPORT-KAISH"), "second sweep present: {:?}", out.report);
        assert_eq!(
            out.report.matches("---").count(),
            1,
            "exactly one `---` between two reports, got: {:?}",
            out.report
        );
        assert_eq!(
            client.requests_for(EXPLORER).len(),
            2,
            "the driver must have delegated two distinct sweeps"
        );
    }

    /// A dying `explore′` sweep must not sink the whole consult: the driver sees the
    /// failure in its transcript and answers from what it has, the report sink stays
    /// empty, and — the teeth for the harness's record-before-respond promise — the
    /// failed explorer request is still logged.
    #[tokio::test]
    async fn a_failed_sweep_surfaces_and_the_driver_recovers() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                // Delegate once; once the failure has come back, answer from direct work.
                if transcript_text(req).contains("simulated provider outage") {
                    Ok(text_response("ANSWER: explore failed, answered from direct reads"))
                } else {
                    Ok(tool_call_response("s1", "explore", json!({ "question": "find it" })))
                }
            })
            .on_model(EXPLORER, |_req| Err(provider_error("simulated provider outage")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        let out = consult_with(&client, "a question", dir.path(), EXPLORER, SYNTH, &cfg, &no_thinking())
            .await
            .expect("a failed sweep must not fail the whole consult");

        // The driver only reaches this answer by *seeing* the error in its transcript
        // (its branch requires it) — so the answer text proves the failure surfaced.
        assert!(
            out.answer.contains("answered from direct reads"),
            "driver should recover after the sweep failed, got: {:?}",
            out.answer
        );
        // The sweep errored before `RunExplore` pushed to the sink, so the report is
        // empty — distinct from a sweep that ran and found nothing.
        assert!(out.report.is_empty(), "a failed sweep contributes no report: {:?}", out.report);
        // Record-before-respond: the failing explorer call was still captured.
        assert!(
            !client.requests_for(EXPLORER).is_empty(),
            "the failed explorer request must still be logged"
        );
    }

    /// Turn-cap recovery, offline: a model that never stops calling tools must still
    /// yield an answer. We cap the loop low and script a driver that *always* calls a
    /// tool — until it's shown `ToolChoice::None` (the forced finalize turn), where it
    /// writes its answer. This proves `run_phase` → `finalize_after_max_turns` turns a
    /// `MaxTurnsError` into a real answer from the partial transcript, a path the live
    /// tests can only hit by luck.
    #[tokio::test]
    async fn turn_cap_forces_a_final_answer_from_partial_work() {
        const SYNTH: &str = "synth";
        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if is_finalize_turn(req) {
                    // Forbidden from calling tools — answer from what we have.
                    Ok(text_response("FORCED FINAL ANSWER: src/foo.rs:1"))
                } else {
                    // Keep burning turns; never conclude on our own.
                    Ok(tool_call_response("t", "run_kaish", json!({ "script": "cat src/foo.rs" })))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig { synth_max_turns: 2, ..ConsultConfig::default() };

        let out = consult_with(
            &client,
            "A question the model never finishes answering",
            dir.path(),
            "explorer-unused",
            SYNTH,
            &cfg,
            &no_thinking(),
        )
        .await
        .expect("turn-cap recovery should still produce an answer");

        assert!(
            out.answer.contains("FORCED FINAL ANSWER"),
            "the forced finalize turn must produce the answer, got: {:?}",
            out.answer
        );
        // And the recovery path actually ran: some request carried ToolChoice::None.
        let finalize = client
            .requests_for(SYNTH)
            .into_iter()
            .find(|r| r.tool_choice == Some(ToolChoice::None))
            .expect("a forced finalize turn (ToolChoice::None) must have been issued");
        // The teeth: the finalize turn must carry the *partial work* — the run_kaish
        // results the driver accumulated before the cap — not a blank history. The
        // answer is hardcoded in the responder, so without this a regression that fed
        // `finalize_after_max_turns` an empty transcript would still pass.
        assert!(
            finalize.transcript.contains("target_marker"),
            "finalize turn must replay the accumulated tool work, got transcript: {:?}",
            finalize.transcript
        );
    }

    /// Thinking params must reach *every* model call — the consult driver and each
    /// nested `explore′`. All other tests pass `None` for thinking, so a regression
    /// that dropped `additional_params` in `run_phase`, or stopped `RunExplore`
    /// forwarding `self.thinking` to its nested loop, would slip through. These
    /// shapes are provider-specific and have already drifted once (`docs/issues.md`).
    #[tokio::test]
    async fn thinking_params_reach_both_the_driver_and_every_sweep() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        // Anthropic dialect: both phases resolve to the same top-level `thinking`
        // block (Anthropic ignores the model id). The mock doesn't interpret it — we
        // only assert it survives the plumbing into *every* request, unchanged.
        let dialect = Dialect::new(ProviderKind::Anthropic, 4096);
        let expected = json!({ "thinking": { "type": "enabled", "budget_tokens": 4096 } });

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if transcript_text(req).contains("REPORT") {
                    Ok(text_response("ANSWER"))
                } else {
                    Ok(tool_call_response("s1", "explore", json!({ "question": "find it" })))
                }
            })
            .on_model(EXPLORER, |_req| Ok(text_response("REPORT: src/foo.rs:1")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        consult_with(&client, "q", dir.path(), EXPLORER, SYNTH, &cfg, &dialect)
            .await
            .unwrap();

        // Both roles were actually exercised (so the loop below isn't vacuous)...
        assert!(!client.requests_for(SYNTH).is_empty(), "driver ran");
        assert!(!client.requests_for(EXPLORER).is_empty(), "a sweep ran");
        // ...and every request carried the thinking shape, unchanged.
        for r in client.requests() {
            assert_eq!(
                r.additional_params.as_ref(),
                Some(&expected),
                "model {:?} must carry the thinking params, got: {:?}",
                r.model,
                r.additional_params
            );
        }
    }

    /// The per-phase payoff: when a Gemini profile's synth and explorer straddle the
    /// 3-line capability boundary, each request must carry the thinking shape fit to
    /// *its own* model — the driver `thinkingLevel`, the sweep `thinkingBudget`. A
    /// regression that resolved thinking once (per profile) and shared it — the old
    /// `consult.rs:815` shape — would put one model's params on the other's request.
    #[tokio::test]
    async fn each_phase_gets_thinking_fit_to_its_own_model() {
        const SYNTH: &str = "gemini-3-pro-preview"; // 3-line → thinkingLevel
        const EXPLORER: &str = "gemini-2.5-flash"; // 2.5 → thinkingBudget
        let dialect = Dialect::new(ProviderKind::Gemini, 4096);

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if transcript_text(req).contains("REPORT") {
                    Ok(text_response("ANSWER"))
                } else {
                    Ok(tool_call_response("s1", "explore", json!({ "question": "find it" })))
                }
            })
            .on_model(EXPLORER, |_req| Ok(text_response("REPORT: src/foo.rs:1")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        consult_with(&client, "q", dir.path(), EXPLORER, SYNTH, &cfg, &dialect)
            .await
            .unwrap();

        let tc = |r: &crate::test_support::RecordedRequest| {
            r.additional_params.as_ref().unwrap()["generationConfig"]["thinkingConfig"].clone()
        };
        for r in client.requests_for(SYNTH) {
            let cfg = tc(&r);
            assert_eq!(cfg["thinkingLevel"], "high", "3-line driver wants a level");
            assert!(cfg.get("thinkingBudget").is_none(), "level and budget are exclusive");
        }
        for r in client.requests_for(EXPLORER) {
            let cfg = tc(&r);
            assert_eq!(cfg["thinkingBudget"], 4096, "2.5 explorer wants a budget");
            assert!(cfg.get("thinkingLevel").is_none(), "level and budget are exclusive");
        }
    }

    /// `run_phase` builds the toolset twice — once for the main loop, again for the
    /// forced finalize turn. The `reports` sink is created once in `consult_with` and
    /// `clone()`d into each build, so a sweep that completed before the cap must
    /// survive into the final `ConsultOutput.report`. Delegate once, then burn turns
    /// on `run_kaish` until the cap forces a finalize (the second build) — the first
    /// sweep's report must still be there.
    #[tokio::test]
    async fn a_sweeps_report_survives_the_finalize_toolset_rebuild() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if is_finalize_turn(req) {
                    Ok(text_response("FINAL"))
                } else if !transcript_text(req).contains("REPORT-E") {
                    // First turn: delegate one sweep (pushes to the shared sink).
                    Ok(tool_call_response("s1", "explore", json!({ "question": "find it" })))
                } else {
                    // Already swept; keep burning turns without re-delegating, so the
                    // cap fires and forces the second toolset build.
                    Ok(tool_call_response("k", "run_kaish", json!({ "script": "cat src/foo.rs" })))
                }
            })
            .on_model(EXPLORER, |_req| Ok(text_response("REPORT-E: src/foo.rs:1")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig { synth_max_turns: 2, ..ConsultConfig::default() };

        let out = consult_with(&client, "q", dir.path(), EXPLORER, SYNTH, &cfg, &no_thinking())
            .await
            .unwrap();

        // The teeth: were `reports` rebuilt per `make_tools` call instead of shared,
        // the pre-cap sweep would be lost and this would be empty.
        assert!(
            out.report.contains("REPORT-E"),
            "the pre-cap sweep's report must survive the finalize rebuild, got: {:?}",
            out.report
        );
        assert_eq!(
            client.requests_for(EXPLORER).len(),
            1,
            "exactly one sweep was delegated (the rest burned run_kaish)"
        );
        assert!(
            client.requests_for(SYNTH).iter().any(|r| r.tool_choice == Some(ToolChoice::None)),
            "the cap must have forced a finalize turn (the second toolset build)"
        );
        assert!(out.answer.contains("FINAL"), "finalize produced the answer");
    }

    /// Session glue, end to end and offline: a second turn must *see* the first
    /// turn's `(question, answer)` pair in its prompt, and both turns must accumulate
    /// in the store. This is the `server.consult` history→consult→record dance,
    /// now `consult_session_turn`, driven by a mock — the seam the live `#[ignore]`d
    /// tests couldn't pin without a real model.
    #[tokio::test]
    async fn a_second_turn_replays_the_first_turns_pair_and_records() {
        const SYNTH: &str = "synth";
        let client = echo_client(SYNTH);
        let sessions = store();
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();
        let sid = "thread-1";

        // Turn 1.
        let out1 = consult_session_turn(
            &client,
            Some((&sessions, sid)),
            "Q1 what is kaish",
            dir.path(),
            "explorer",
            SYNTH,
            &cfg,
            &no_thinking(),
        )
        .await
        .unwrap();
        assert_eq!(out1.answer, "ANSWER[Q1 what is kaish]");
        assert_eq!(
            sessions.history(sid),
            vec![QaTurn::new("Q1 what is kaish", "ANSWER[Q1 what is kaish]")],
            "turn 1 must be recorded"
        );

        // Turn 2.
        let out2 = consult_session_turn(
            &client,
            Some((&sessions, sid)),
            "Q2 who calls it",
            dir.path(),
            "explorer",
            SYNTH,
            &cfg,
            &no_thinking(),
        )
        .await
        .unwrap();

        // The teeth: turn 2's request carried turn 1's Q and A into the prompt.
        let turn2_req = &client.requests_for(SYNTH)[1];
        assert!(
            turn2_req.user_text.contains("Q1 what is kaish"),
            "turn 2 must replay turn 1's question: {:?}",
            turn2_req.user_text
        );
        assert!(
            turn2_req.user_text.contains("ANSWER[Q1 what is kaish]"),
            "turn 2 must replay turn 1's answer: {:?}",
            turn2_req.user_text
        );
        assert_eq!(out2.answer, "ANSWER[Q2 who calls it]");
        assert_eq!(sessions.history(sid).len(), 2, "both turns accumulate in the thread");
    }

    /// A failed turn must NOT record — a half-answer can't be allowed to poison the
    /// thread as established context the next turn would trust. (The invariant the
    /// `server.rs:325` comment used to assert only in prose.)
    #[tokio::test]
    async fn a_failed_turn_does_not_record() {
        const SYNTH: &str = "synth";
        let client = ScriptedClient::builder()
            .on_model(SYNTH, |_req| Err(provider_error("scripted failure")))
            .build();
        let sessions = store();
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();
        let sid = "doomed";

        let result = consult_session_turn(
            &client,
            Some((&sessions, sid)),
            "Q that fails",
            dir.path(),
            "explorer",
            SYNTH,
            &cfg,
            &no_thinking(),
        )
        .await;

        assert!(result.is_err(), "a provider error must surface, not be swallowed");
        assert!(
            sessions.history(sid).is_empty(),
            "a failed turn must leave the thread untouched, got: {:?}",
            sessions.history(sid)
        );
    }

    /// A stateless turn (`session: None`) records nothing and replays nothing — the
    /// one-shot path stays byte-for-byte its pre-session self.
    #[tokio::test]
    async fn a_stateless_turn_records_nothing() {
        const SYNTH: &str = "synth";
        let client = echo_client(SYNTH);
        let sessions = store();
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();

        let out = consult_session_turn(
            &client, None, "lone question", dir.path(), "explorer", SYNTH, &cfg, &no_thinking(),
        )
        .await
        .unwrap();

        assert_eq!(out.answer, "ANSWER[lone question]");
        assert_eq!(sessions.session_count(), 0, "a stateless turn creates no session");
    }

    /// The recomposed consult must drive BOTH tools: a direct `run_kaish` and the
    /// delegated `explore′`. Pin the wiring offline — no model, just the toolset.
    #[test]
    fn consult_toolset_has_both_run_kaish_and_explore() {
        let dir = tempdir().unwrap();
        // The scripted client satisfies the same trait bounds with no network and no
        // key-format requirement — so this stays a pure toolset-wiring test, not a
        // hostage to rig's anthropic constructor.
        let client = ScriptedClient::builder().build();
        let cfg = ConsultConfig::default();
        let reports = Arc::new(Mutex::new(Vec::new()));

        let tools = consult_tools(&client, "explorer-model", dir.path(), &cfg, &no_thinking(), reports)
            .expect("building the consult toolset should succeed");

        let names: Vec<String> = tools.iter().map(|t| t.name()).collect();
        assert!(names.iter().any(|n| n == "run_kaish"), "missing run_kaish, got {names:?}");
        assert!(names.iter().any(|n| n == "explore"), "missing explore′, got {names:?}");
    }

    /// Collect the text blocks of a user message (for asserting the finalize note).
    fn user_text(msg: &Message) -> String {
        match msg {
            Message::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!("expected a user message, got {msg:?}"),
        }
    }

    /// The usual cap shape: the transcript ends on the user's tool-results turn. The
    /// finalize note must ride *inside* that last user turn (not a new one — back-to-
    /// back user turns break some providers), and history must shrink by exactly it.
    #[test]
    fn finalize_folds_note_into_trailing_user_turn() {
        let history = vec![
            Message::user("Original question"),
            Message::assistant("calling a tool"),
            Message::user("tool results"),
        ];
        let (rest, prompt) = finalize_prompt(history);

        // The trailing user turn becomes the prompt and carries both its original
        // content and the appended note.
        let text = user_text(&prompt);
        assert!(text.contains("tool results"), "original content kept: {text}");
        assert!(text.contains(FINALIZE_NOTE), "note appended: {text}");
        // History is everything before it — the trailing turn was consumed, not duplicated.
        assert_eq!(rest.len(), 2, "trailing user turn consumed into the prompt");
    }

    /// Defensive shape: if the transcript ends on an assistant turn, we must not
    /// mutate it — the note becomes a fresh trailing user turn (valid after an
    /// assistant message) and the assistant turn stays in history.
    #[test]
    fn finalize_adds_user_turn_when_transcript_ends_on_assistant() {
        let history = vec![Message::user("Q"), Message::assistant("partial thoughts")];
        let (rest, prompt) = finalize_prompt(history);

        assert!(user_text(&prompt).contains(FINALIZE_NOTE), "note is the new user turn");
        assert_eq!(rest.len(), 2, "assistant turn kept in history");
        assert!(
            matches!(rest.last(), Some(Message::Assistant { .. })),
            "assistant turn preserved at the tail"
        );
    }

    /// No session history ⇒ the prompt is *exactly* the bare question. This pins the
    /// promise that a stateless consult is byte-for-byte its pre-session behavior.
    #[test]
    fn empty_history_yields_the_bare_question() {
        assert_eq!(consult_user_prompt("Where is the sandbox enforced?", &[]),
                   "Where is the sandbox enforced?");
    }

    /// With history, every prior turn appears, the current question appears, and the
    /// turns precede the current question (the model reads context before the ask).
    #[test]
    fn history_is_replayed_before_the_current_question_in_order() {
        let history = vec![
            QaTurn::new("What is kaish?", "A read-only shell (src/sandbox.rs)."),
            QaTurn::new("Who calls it?", "consult drives it (src/consult.rs)."),
        ];
        let prompt = consult_user_prompt("And explore?", &history);

        for needle in [
            "What is kaish?",
            "A read-only shell (src/sandbox.rs).",
            "Who calls it?",
            "consult drives it (src/consult.rs).",
            "And explore?",
        ] {
            assert!(prompt.contains(needle), "prompt must carry {needle:?}:\n{prompt}");
        }
        // Ordering: the first prior turn comes before the second, and both come
        // before the current question.
        let first = prompt.find("What is kaish?").unwrap();
        let second = prompt.find("Who calls it?").unwrap();
        let current = prompt.find("And explore?").unwrap();
        assert!(first < second, "turns must be oldest-first");
        assert!(second < current, "history must precede the current question");
    }
}
