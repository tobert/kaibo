//! The explorer phase: a model drives the read-only kaish sandbox to investigate
//! a question, then reports what it found.
//!
//! It sees exactly one tool — [`RunKaish`] (`run_kaish`) — and uses kaish's
//! builtins and pipelines for all reading, searching, and parsing. Writes,
//! `git`/`touch`, and external commands are refused by [`crate::sandbox`].

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use crate::kaish_syntax::run_kaish_tool_description;
use crate::sandbox::{KaishOutput, KaishWorker};

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
