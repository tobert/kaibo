//! One span per tool call. Wrap any phase tool so every invocation emits a `tool`
//! span carrying `gen_ai.tool.name`, a short `gen_ai.tool.arguments` summary, and an
//! ok/err `outcome` (the span's own duration is the call's latency). This is what
//! lets telemetry answer *which* tool the model called *and with what* — the
//! granularity the read-tool and orientation A/Bs both needed (the tool name alone
//! couldn't separate a discovery `glob` from a `cat -n` read inside `run_kaish`).
//!
//! The `outcome` only says whether the tool *itself* worked — for `run_kaish` that's
//! always `ok`, because a non-zero *script* exit is normal output handed to the model,
//! not a tool failure (see `explorer.rs`). So the span carries two more optional fields
//! a tool may fill about its result: `kaish.exit_code` (the script's exit — `3` is the
//! head+tail truncation, `124` a timeout, `126` blocked) and `kaish.output_bytes` (the
//! delivered stdout size). Recorded by `run_kaish` via [`record_kaish_result`]; left
//! empty (and thus unexported) by every other tool. This is what lets a trace tell a
//! `cat -n` that *truncated* and forced narrow re-reads from one the model chose to
//! slice — the read-size question the explorer A/Bs turn on.
//!
//! It's *our* span on *our* tools, independent of what rig or a given provider
//! instruments, so it lands the same on every backend. [`traced`] is where a typed
//! tool becomes the erased [`DynamicTool`] rig dispatches — the seam that still sees
//! raw JSON args, so it can summarize the call — and it is otherwise transparent:
//! name, description, parameters, output, and errors all pass straight through.
//! Because the erasure is ours, the arg-parse happens inside the span too, so a
//! malformed-args refusal is reported as `outcome = error` rather than never
//! appearing — the honest reading of "did this call work".

use std::sync::Arc;

use rig_agent::tool::{
    tool_definition, DynamicTool, IntoToolOutput, Tool, ToolContext, ToolExecutionError, ToolOutput,
};
use rig_core::wasm_compat::WasmBoxedFuture;
use serde_json::Value;
use tracing::{field, info_span, Instrument, Span};

/// Parse a dynamic tool's JSON args into the wrapped tool's typed `Args`.
///
/// `null` and `{}` both mean "no arguments" — a no-argument tool is routinely called
/// either way, so treating only one as valid yields a tool that fails for some models
/// and not others.
fn parse_args<A>(args: Value) -> Result<A, ToolExecutionError>
where
    A: for<'de> serde::Deserialize<'de>,
{
    let args = if args.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        args
    };
    serde_json::from_value(args).map_err(|error| {
        ToolExecutionError::invalid_args(format!("failed to parse tool arguments: {error}"))
            .with_source(error)
    })
}

/// Record a `run_kaish` script's result onto the enclosing `tool` span — its exit
/// code and delivered stdout size. Called from inside [`RunKaish::call`](crate::explorer),
/// which runs under the [`traced`] wrapper's span, so [`Span::current`] is that span
/// and its pre-declared `kaish.*` fields fill in. Off the wrapped path (e.g. a direct
/// worker call with no `tool` span current) this is a harmless no-op — best-effort
/// observability, never load-bearing. The field names live *here*, with their
/// declaration, so a caller can't silently mistype one.
pub(crate) fn record_kaish_result(exit_code: i64, output_bytes: usize) {
    let span = Span::current();
    span.record("kaish.exit_code", exit_code);
    span.record("kaish.output_bytes", output_bytes as u64);
}

/// Erase a typed tool into a span-wrapped [`DynamicTool`] — the toolset-assembly
/// drop-in, so every tool in a phase's toolset is traced uniformly. The returned tool
/// is what rig registers and dispatches, so the span brackets exactly the work rig
/// attributes to this call.
pub fn traced<T: Tool + 'static>(tool: T) -> DynamicTool {
    let definition = tool_definition(&tool);
    let name = definition.name.clone();
    let tool = Arc::new(tool);
    DynamicTool::new(
        definition.name,
        definition.description,
        definition.parameters,
        move |context: &mut ToolContext, args: Value| {
            let tool = Arc::clone(&tool);
            let name = name.clone();
            // Summarize before the args are moved into the call. `outcome` is filled
            // after, so one span carries which tool, with what, and whether it worked;
            // the span's start/end bracket the latency.
            let summary = arg_summary(&args.to_string());
            Box::pin(async move {
                let span = info_span!(
                    "tool",
                    "gen_ai.tool.name" = %name,
                    "gen_ai.tool.arguments" = %summary,
                    outcome = field::Empty,
                    // Filled by `run_kaish` (via `record_kaish_result`) from inside the
                    // instrumented call — `Span::current()` is this span there. Every
                    // other tool leaves them empty, so they don't export.
                    "kaish.exit_code" = field::Empty,
                    "kaish.output_bytes" = field::Empty,
                );
                async move {
                    let result = run_traced(tool.as_ref(), context, args).await;
                    Span::current().record("outcome", if result.is_ok() { "ok" } else { "error" });
                    result
                }
                .instrument(span)
                .await
            }) as WasmBoxedFuture<'_, Result<ToolOutput, ToolExecutionError>>
        },
    )
}

/// The erased call itself: parse, dispatch, then normalize both halves of the typed
/// result — output through [`IntoToolOutput`](rig_agent::tool::IntoToolOutput), error
/// through the tool's own `map_error`, so a tool that classifies its failures keeps
/// that classification.
async fn run_traced<T: Tool>(
    tool: &T,
    context: &mut ToolContext,
    args: Value,
) -> Result<ToolOutput, ToolExecutionError> {
    let args = parse_args::<T::Args>(args)?;
    match tool.call(context, args).await {
        Ok(output) => output.into_tool_output(),
        Err(error) => Err(tool.map_error(error)),
    }
}

/// A short, span-friendly summary of a tool call's JSON args: the most informative
/// field — the `run_kaish` `script`, a `path`, an `explore` `question` — else the
/// raw JSON, truncated. Read-only project args carry no secrets; the cap just keeps
/// a long script from bloating the span. This is the field that turns "16 run_kaish
/// calls" into "1 was `glob`, 15 were `cat -n`/`grep`".
fn arg_summary(args: &str) -> String {
    const MAX_CHARS: usize = 200;
    let preview = serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| {
            ["script", "path", "question", "prompt"]
                .iter()
                .find_map(|k| v.get(k).and_then(|x| x.as_str()).map(str::to_string))
        })
        .unwrap_or_else(|| args.to_string());
    if preview.chars().count() <= MAX_CHARS {
        preview
    } else {
        let cut: String = preview.chars().take(MAX_CHARS).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serialized_capture;
    use rig_agent::tool::{ToolResult, ToolSet};
    use serde::Deserialize;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;
    use tracing_subscriber::Layer;

    /// Dispatch a span-wrapped tool the way rig does — through a registered
    /// [`ToolSet`], the public execution surface. This is the production path, not a
    /// stand-in: rig's erased `call` is private.
    async fn dispatch(tool: DynamicTool, args: &str) -> ToolResult {
        let name = tool.name().to_string();
        let tools = ToolSet::from_dynamic_tools(vec![tool]);
        let mut context = ToolContext::default();
        tools.execute(&name, args, &mut context).await
    }

    /// A trivial tool to wrap: echoes its `msg`, or errors on "boom".
    struct Echo;
    #[derive(Deserialize)]
    struct EchoArgs {
        msg: String,
    }
    #[derive(Debug)]
    struct EchoErr;
    impl std::fmt::Display for EchoErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "echo error")
        }
    }
    impl std::error::Error for EchoErr {}
    impl Tool for Echo {
        const NAME: &'static str = "echo";
        type Error = EchoErr;
        type Args = EchoArgs;
        type Output = String;
        fn description(&self) -> String {
            "echo".into()
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn call(&self, _ctx: &mut ToolContext, args: Self::Args) -> Result<String, EchoErr> {
            if args.msg == "boom" {
                Err(EchoErr)
            } else {
                Ok(args.msg)
            }
        }
    }

    /// The summary pulls the informative field (so a `run_kaish` span shows the
    /// *script*), falls back to raw JSON, and truncates a long value.
    #[test]
    fn arg_summary_picks_the_informative_field() {
        assert_eq!(
            arg_summary(r#"{"script":"glob -a '**/*'"}"#),
            "glob -a '**/*'"
        );
        assert_eq!(arg_summary(r#"{"path":"src/lib.rs"}"#), "src/lib.rs");
        // No known field → the raw JSON (still useful, just not distilled).
        assert_eq!(arg_summary(r#"{"msg":"hi"}"#), r#"{"msg":"hi"}"#);
        // Long script is truncated with an ellipsis.
        let long = format!(r#"{{"script":"{}"}}"#, "x".repeat(500));
        let s = arg_summary(&long);
        assert!(
            s.chars().count() <= 201,
            "truncated: {} chars",
            s.chars().count()
        );
        assert!(s.ends_with('…'), "marked truncated: {s}");
    }

    /// One closed span as captured.
    #[derive(Clone)]
    struct CapturedSpan {
        name: String,
        tool: String,
        args: String,
        outcome: String,
        kaish_exit: Option<i64>,
        kaish_bytes: Option<u64>,
    }

    /// Captures each [`CapturedSpan`] as its span closes.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<CapturedSpan>>>);

    #[derive(Default)]
    struct Grab {
        tool: Option<String>,
        args: Option<String>,
        outcome: Option<String>,
        kaish_exit: Option<i64>,
        kaish_bytes: Option<u64>,
    }
    impl tracing::field::Visit for Grab {
        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
            match f.name() {
                "gen_ai.tool.name" => self.tool = Some(v.to_string()),
                "gen_ai.tool.arguments" => self.args = Some(v.to_string()),
                "outcome" => self.outcome = Some(v.to_string()),
                _ => {}
            }
        }
        // `run_kaish`'s result fields arrive as native numbers, not strings.
        fn record_i64(&mut self, f: &tracing::field::Field, v: i64) {
            if f.name() == "kaish.exit_code" {
                self.kaish_exit = Some(v);
            }
        }
        fn record_u64(&mut self, f: &tracing::field::Field, v: u64) {
            if f.name() == "kaish.output_bytes" {
                self.kaish_bytes = Some(v);
            }
        }
        // `%summary`/`%name` record via Display, which arrives as a debug value.
        fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
            let s = format!("{v:?}");
            let s = s.trim_matches('"').to_string();
            match f.name() {
                "gen_ai.tool.name" => self.tool = Some(s),
                "gen_ai.tool.arguments" => self.args = Some(s),
                "outcome" => self.outcome = Some(s),
                _ => {}
            }
        }
    }
    #[derive(Default)]
    struct Stored {
        tool: String,
        args: String,
        outcome: String,
        kaish_exit: Option<i64>,
        kaish_bytes: Option<u64>,
    }

    impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::Id,
            ctx: Context<'_, S>,
        ) {
            let mut g = Grab::default();
            attrs.record(&mut g);
            ctx.span(id).unwrap().extensions_mut().insert(Stored {
                tool: g.tool.unwrap_or_default(),
                args: g.args.unwrap_or_default(),
                outcome: g.outcome.unwrap_or_default(),
                kaish_exit: g.kaish_exit,
                kaish_bytes: g.kaish_bytes,
            });
        }
        fn on_record(
            &self,
            id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            ctx: Context<'_, S>,
        ) {
            // `outcome` and the `kaish.*` result fields all land here (recorded after
            // the span opens), so merge whichever this record carried.
            let mut g = Grab::default();
            values.record(&mut g);
            if let Some(span) = ctx.span(id) {
                if let Some(st) = span.extensions_mut().get_mut::<Stored>() {
                    if let Some(o) = g.outcome {
                        st.outcome = o;
                    }
                    if g.kaish_exit.is_some() {
                        st.kaish_exit = g.kaish_exit;
                    }
                    if g.kaish_bytes.is_some() {
                        st.kaish_bytes = g.kaish_bytes;
                    }
                }
            }
        }
        fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
            let span = ctx.span(&id).unwrap();
            let name = span.name().to_string();
            // Pull owned values out before the extensions borrow ends, then push.
            let row = span.extensions().get::<Stored>().map(|st| CapturedSpan {
                name,
                tool: st.tool.clone(),
                args: st.args.clone(),
                outcome: st.outcome.clone(),
                kaish_exit: st.kaish_exit,
                kaish_bytes: st.kaish_bytes,
            });
            if let Some(row) = row {
                self.0.lock().unwrap().push(row);
            }
        }
    }

    /// A success emits a `tool` span tagged with the tool's name, an args summary,
    /// and outcome=ok; the output passes through.
    #[test]
    fn emits_a_tool_span_with_name_args_and_outcome() {
        serialized_capture(async {
            let cap = Capture::default();
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);

            let result = dispatch(traced(Echo), r#"{"msg":"hi"}"#).await;
            assert!(result.is_success(), "echo succeeded: {result:?}");
            let out = result.output().render();
            assert!(out.contains("hi"), "output passes through: {out}");

            drop(_g);
            let spans = cap.0.lock().unwrap().clone();
            assert!(
                spans.iter().any(|s| s.name == "tool"
                    && s.tool == "echo"
                    && s.args.contains("hi")
                    && s.outcome == "ok"),
                "a `tool` span with name/args/outcome"
            );
            // A non-kaish tool never fills the kaish result fields.
            assert!(
                spans
                    .iter()
                    .all(|s| s.kaish_exit.is_none() && s.kaish_bytes.is_none()),
                "echo must leave the kaish result fields empty"
            );
        });
    }

    /// An error still emits the span, tagged outcome=error.
    #[test]
    fn emits_an_error_outcome_when_the_tool_fails() {
        serialized_capture(async {
            let cap = Capture::default();
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);

            let result = dispatch(traced(Echo), r#"{"msg":"boom"}"#).await;
            assert!(result.is_error(), "the tool's own failure: {result:?}");

            drop(_g);
            let spans = cap.0.lock().unwrap().clone();
            assert!(
                spans
                    .iter()
                    .any(|s| s.name == "tool" && s.tool == "echo" && s.outcome == "error"),
                "a `tool` span tagged outcome=error"
            );
        });
    }

    /// Malformed args are refused *inside* the span, so the trace shows the attempt
    /// and tags it `outcome = error`. [`traced`] owns the arg-parse, which is where a
    /// failed call could quietly start being reported as a success.
    #[test]
    fn malformed_args_are_refused_inside_the_span() {
        serialized_capture(async {
            let cap = Capture::default();
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);

            // `msg` is required and an integer is not a string.
            let result = dispatch(traced(Echo), r#"{"msg":7}"#).await;
            assert!(
                result.is_error(),
                "unparseable args fail the call: {result:?}"
            );

            drop(_g);
            let spans = cap.0.lock().unwrap().clone();
            assert!(
                spans
                    .iter()
                    .any(|s| s.name == "tool" && s.tool == "echo" && s.outcome == "error"),
                "the refused call still emits its span, tagged outcome=error"
            );
        });
    }

    /// `record_kaish_result`, called from inside a wrapped tool's `call`, tags the
    /// enclosing `tool` span with the script's exit code and delivered size — the
    /// half `outcome` can't carry (a non-zero script exit is normal output, not a
    /// tool error). A truncated read is `exit_code = 3`.
    #[test]
    fn run_kaish_records_exit_code_and_output_bytes() {
        /// A stand-in for `run_kaish`: reports a truncated read the way the real tool
        /// does, then returns normally (a truncation is `Ok`, never a tool error).
        struct TruncatedRead;
        #[derive(Deserialize)]
        struct NoArgs {}
        #[derive(Debug)]
        struct Never;
        impl std::fmt::Display for Never {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "never")
            }
        }
        impl std::error::Error for Never {}
        impl Tool for TruncatedRead {
            const NAME: &'static str = "run_kaish";
            type Error = Never;
            type Args = NoArgs;
            type Output = String;
            fn description(&self) -> String {
                "run_kaish".into()
            }
            fn parameters(&self) -> serde_json::Value {
                json!({})
            }
            async fn call(
                &self,
                _ctx: &mut ToolContext,
                _args: Self::Args,
            ) -> Result<String, Never> {
                super::record_kaish_result(3, 4321);
                Ok("exit: 3\n".into())
            }
        }

        serialized_capture(async {
            let cap = Capture::default();
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);

            // `null` args, not `{}` — a no-argument tool is routinely called that way,
            // and [`parse_args`] has to mean the same thing by both.
            let result = dispatch(traced(TruncatedRead), "null").await;
            assert!(result.is_success(), "null args mean no args: {result:?}");

            drop(_g);
            let spans = cap.0.lock().unwrap().clone();
            assert!(
                spans.iter().any(|s| s.name == "tool"
                    && s.kaish_exit == Some(3)
                    && s.kaish_bytes == Some(4321)),
                "the tool span must carry kaish.exit_code=3 and kaish.output_bytes=4321"
            );
        });
    }

    /// End-to-end teeth: a real `RunKaish` over a real worker whose output cap is
    /// tiny, reading a file that overflows it, must surface `exit_code = 3` on the
    /// span — the whole point is a trace can see a *truncated* read. Proves the
    /// wiring from `RunKaish::call` through `record_kaish_result` to the span, not
    /// just the recorder in isolation.
    #[test]
    fn a_truncated_read_surfaces_exit_3_end_to_end() {
        use crate::explorer::RunKaish;
        use crate::sandbox::{KaishWorker, SandboxConfig};

        serialized_capture(async {
            let dir = tempfile::tempdir().unwrap();
            // Comfortably past the 64-byte cap below, so `cat -n` truncates.
            std::fs::write(dir.path().join("big.txt"), "x\n".repeat(200)).unwrap();
            let worker = KaishWorker::spawn_with(
                dir.path(),
                SandboxConfig {
                    output_limit_bytes: 64,
                    ..SandboxConfig::default()
                },
            )
            .unwrap();

            let cap = Capture::default();
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);

            let result = dispatch(
                traced(RunKaish::new(worker)),
                r#"{"script":"cat -n big.txt"}"#,
            )
            .await;
            assert!(result.is_success(), "the script ran: {result:?}");
            let out = result.output().render();
            assert!(out.contains("exit: 3"), "the read truncated: {out}");

            drop(_g);
            let spans = cap.0.lock().unwrap().clone();
            assert!(
                spans
                    .iter()
                    .any(|s| s.name == "tool" && s.tool == "run_kaish" && s.kaish_exit == Some(3)),
                "a truncated run_kaish read must record kaish.exit_code=3 on its span"
            );
        });
    }
}
