//! The explorer phase: a model drives the read-only kaish sandbox to investigate
//! a question, then reports what it found.
//!
//! It sees exactly one tool — [`RunKaish`] (`run_kaish`) — and uses kaish's
//! builtins and pipelines for all reading, searching, and parsing. Writes,
//! `git`/`touch`, and external commands are refused by [`crate::sandbox`].
//!
//! v0 is single-phase: the explorer answers directly. The synthesizer phase
//! (curated report → second model) lands next, per the dpal pattern.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use rig::client::CompletionClient;
use rig::completion::{Prompt, ToolDefinition};
use rig::providers::anthropic;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use crate::kaish_syntax::{kaish_syntax_core, run_kaish_tool_description};
use crate::sandbox::{KaishOutput, KaishWorker};

/// System prompt for the explorer. Composes the shared [`kaish_syntax_core`] (so
/// the shell idioms and exit-code contract never drift) with the explorer's task.
pub fn explorer_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You are a code explorer. {core}\n\n\
         Work iteratively: look, then narrow. When you have enough, answer the \
         question directly and concretely, citing concrete paths and `file:line` \
         where you can."
    )
}

/// Tunables for one explore pass.
#[derive(Debug, Clone)]
pub struct ExploreConfig {
    /// Provider model id (passed verbatim to the API).
    pub model: String,
    /// Max tool-loop turns before the model must conclude.
    pub max_turns: usize,
    /// Max output tokens per model turn.
    pub max_tokens: u64,
}

impl Default for ExploreConfig {
    fn default() -> Self {
        // A cheap, fast model is the right default for exploration grunt-work.
        // (rig 0.34's bundled CLAUDE_* consts point at retired 3.x ids — use a
        // current alias directly.)
        Self {
            model: "claude-haiku-4-5".to_string(),
            max_turns: 12,
            max_tokens: 4096,
        }
    }
}

/// The single tool the explorer drives: run a kaish script in the sandbox.
pub struct RunKaish {
    worker: KaishWorker,
}

impl RunKaish {
    pub fn new(worker: KaishWorker) -> Self {
        Self { worker }
    }
}

#[derive(Debug, Deserialize)]
pub struct RunKaishArgs {
    /// The kaish script to run.
    pub script: String,
}

/// Infrastructural failure of the tool itself (the worker is gone). A *script*
/// that exits non-zero is NOT this — that's normal output handed back to the model.
#[derive(Debug)]
pub struct RunKaishError(String);

impl std::fmt::Display for RunKaishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run_kaish failed: {}", self.0)
    }
}

impl std::error::Error for RunKaishError {}

impl Tool for RunKaish {
    const NAME: &'static str = "run_kaish";
    type Error = RunKaishError;
    type Args = RunKaishArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: run_kaish_tool_description(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "kaish script to execute, e.g. `rg -n TODO src | head`"
                    }
                },
                "required": ["script"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let out = self
            .worker
            .run(args.script)
            .await
            .map_err(|e| RunKaishError(e.to_string()))?;
        Ok(format_output(&out))
    }
}

/// Render an execution as the flat text the model (or an MCP caller) reads back.
pub(crate) fn format_output(o: &KaishOutput) -> String {
    let mut s = format!("exit: {}\n", o.code);
    if !o.stdout.is_empty() {
        s.push_str("--- stdout ---\n");
        s.push_str(&o.stdout);
        if !o.stdout.ends_with('\n') {
            s.push('\n');
        }
    }
    if !o.stderr.is_empty() {
        s.push_str("--- stderr ---\n");
        s.push_str(&o.stderr);
        if !o.stderr.ends_with('\n') {
            s.push('\n');
        }
    }
    s
}

/// Run one explore pass: spin a read-only kaish over `root`, let an Anthropic
/// model investigate `question` through `run_kaish`, and return its answer.
pub async fn explore(
    question: &str,
    root: impl Into<PathBuf>,
    api_key: &str,
    cfg: &ExploreConfig,
) -> Result<String> {
    let worker = KaishWorker::spawn(root)?;

    let client =
        anthropic::Client::new(api_key).map_err(|e| anyhow!("anthropic client init: {e}"))?;

    let agent = client
        .agent(cfg.model.as_str())
        .preamble(&explorer_preamble())
        .max_tokens(cfg.max_tokens)
        .tool(RunKaish::new(worker))
        .build();

    agent
        .prompt(question)
        .max_turns(cfg.max_turns)
        .await
        .map_err(|e| anyhow!("explorer loop failed: {e}"))
}
