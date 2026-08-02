//! The explorer phase: a model drives the read-only kaish sandbox to investigate
//! a question, then reports what it found.
//!
//! It sees exactly one tool — [`RunKaish`] (`run_kaish`) — and uses kaish's
//! builtins and pipelines for all reading, searching, and parsing. Writes,
//! `git`/`touch`, and external commands are refused by [`crate::sandbox`].

use std::sync::Arc;

use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use serde::Deserialize;
use serde_json::json;

use crate::kaish_syntax::run_kaish_tool_description;
use crate::progress::{NullSink, PhaseEvent, ProgressSink};
use crate::sandbox::{KaishOutput, KaishWorker};

/// The single tool the explorer drives: run a kaish script in the sandbox.
pub struct RunKaish {
    worker: KaishWorker,
    /// Where a "ran a kaish script" beat goes. Each `call` is real forward motion in
    /// an otherwise-silent loop, so it's the natural progress heartbeat. [`NullSink`]
    /// when the caller wants no progress.
    progress: Arc<dyn ProgressSink>,
}

impl RunKaish {
    /// A `run_kaish` tool with no progress surface — the bare construction for paths
    /// that don't report (and the obvious default).
    pub fn new(worker: KaishWorker) -> Self {
        Self::with_progress(worker, Arc::new(NullSink))
    }

    /// A `run_kaish` tool that announces each script it runs to `progress`. The loop
    /// builds it with the phase's sink so a long, quiet investigation still shows
    /// life on the MCP wire.
    pub fn with_progress(worker: KaishWorker, progress: Arc<dyn ProgressSink>) -> Self {
        Self { worker, progress }
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

    /// Keep the failure text model-visible: rig's default would redact it to a
    /// kind-level "the tool failed", and this message names what the shell refused —
    /// the only thing telling the model how to reshape its script. `with_source` keeps
    /// the concrete error for operator diagnostics and downcasting.
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string()).with_source(error)
    }

    fn description(&self) -> String {
        run_kaish_tool_description()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "kaish script to execute, e.g. `grep -rn TODO src | head`"
                }
            },
            "required": ["script"]
        })
    }

    async fn call(
        &self,
        _ctx: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        // Announce the read before running it — the beat fires even if the script
        // then errors, which is exactly the liveness a stuck call wants to show.
        self.progress.emit(PhaseEvent::KaishRun {
            script: args.script.clone(),
        });
        let out = self
            .worker
            .run(args.script)
            .await
            .map_err(|e| RunKaishError(e.to_string()))?;
        // Tag the enclosing `tool` span with the script's exit code and delivered
        // size — the `outcome` field can't, since a non-zero script exit is a normal
        // result, not a tool error. This is what lets a trace see a truncated read.
        crate::tool_span::record_kaish_result(out.code, out.stdout.len());
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
