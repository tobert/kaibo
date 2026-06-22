//! One span per tool call. Wrap any phase tool so every invocation emits a `tool`
//! span carrying `gen_ai.tool.name`, a short `gen_ai.tool.arguments` summary, and an
//! ok/err `outcome` (the span's own duration is the call's latency). This is what
//! lets telemetry answer *which* tool the model called *and with what* — the
//! granularity the read-tool and orientation A/Bs both needed (the tool name alone
//! couldn't separate a discovery `glob` from a `cat -n` read inside `run_kaish`).
//!
//! It's *our* span on *our* tools, independent of what rig or a given provider
//! instruments, so it lands the same on every backend. The wrapper sits at the
//! `ToolDyn` seam (where `call` carries the raw JSON args) so it can summarize the
//! call; it is otherwise transparent — name, definition, output, and errors pass
//! straight through.

use rig_core::completion::ToolDefinition;
use rig_core::tool::{Tool, ToolDyn, ToolError};
use rig_core::wasm_compat::WasmBoxedFuture;
use tracing::{field, info_span, Instrument, Span};

/// Wraps a boxed [`ToolDyn`], emitting a `tool` span per `call`. Delegates the rest.
pub struct Traced {
    inner: Box<dyn ToolDyn>,
}

impl ToolDyn for Traced {
    fn name(&self) -> String {
        self.inner.name()
    }

    fn definition<'a>(&'a self, prompt: String) -> WasmBoxedFuture<'a, ToolDefinition> {
        self.inner.definition(prompt)
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        let name = self.inner.name();
        // Summarize before the args are moved into the call. `outcome` is filled
        // after, so one span carries which tool, with what, and whether it worked;
        // the span's start/end bracket the latency.
        let summary = arg_summary(&args);
        Box::pin(async move {
            let span = info_span!(
                "tool",
                "gen_ai.tool.name" = %name,
                "gen_ai.tool.arguments" = %summary,
                outcome = field::Empty,
            );
            async {
                let result = self.inner.call(args).await;
                Span::current().record("outcome", if result.is_ok() { "ok" } else { "error" });
                result
            }
            .instrument(span)
            .await
        })
    }
}

/// Box a tool as a span-wrapped [`ToolDyn`] — the toolset-assembly drop-in for
/// `Box::new(tool)`, so every tool in a phase's toolset is traced uniformly.
pub fn traced<T: Tool + 'static>(tool: T) -> Box<dyn ToolDyn> {
    Box::new(Traced {
        inner: Box::new(tool),
    })
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
    use serde::Deserialize;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;
    use tracing_subscriber::Layer;

    /// Serializes the span-capturing tests so their `set_default` installs and
    /// teardowns — which mutate *process-global* tracing state (the callsite interest
    /// cache, the live-dispatcher set, the global max-level) via
    /// `Dispatch::new` → `callsite::register_dispatch` — never interleave with each
    /// other. Belt to the [`force_multi_dispatcher`] suspenders below.
    static CAPTURE_SERIAL: Mutex<()> = Mutex::new(());

    /// A leaked, permanently-registered second dispatcher, established once.
    ///
    /// The flake this kills: `info_span!("tool")` registers its callsite *lazily*, on
    /// first hit. While tracing's `has_just_one` fast path holds — true whenever ≤1
    /// dispatcher is registered, which is exactly our case since each test installs a
    /// single subscriber — that first registration computes the callsite's interest
    /// from **the registering thread's current default**, not from the installed
    /// subscriber. So when a no-subscriber `consult` test (this binary is full of them)
    /// wins the race to first-touch the `tool` callsite during a capture test's window,
    /// it caches `Interest::never()` against `NoSubscriber`, gating the span off — an
    /// empty capture, the ~5% full-suite flake. Serializing the two capture tests can't
    /// prevent that: the poisoning thread is a *third* test with no subscriber.
    ///
    /// Holding a second registered dispatcher forever forces `has_just_one` false, so
    /// every callsite registration instead consults the registered-dispatcher set —
    /// which contains a span-enabling registry — regardless of which thread triggers
    /// it. It is never a thread's default, so it receives no events; it exists only to
    /// keep the registration path honest. Leaked deliberately: it must outlive every
    /// test in the process.
    fn force_multi_dispatcher() {
        use std::sync::OnceLock;
        static KEEPALIVE: OnceLock<()> = OnceLock::new();
        KEEPALIVE.get_or_init(|| {
            let keep = tracing::Dispatch::new(tracing_subscriber::registry());
            std::mem::forget(keep);
        });
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
        async fn definition(&self, _prompt: String) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "echo".into(),
                parameters: json!({}),
            }
        }
        async fn call(&self, args: Self::Args) -> Result<String, EchoErr> {
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

    /// One closed span as captured: (span name, tool name, arguments, outcome).
    type CapturedSpan = (String, String, String, String);

    /// Captures each [`CapturedSpan`] as its span closes.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<CapturedSpan>>>);

    #[derive(Default)]
    struct Grab {
        tool: Option<String>,
        args: Option<String>,
        outcome: Option<String>,
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
    struct Stored {
        tool: String,
        args: String,
        outcome: String,
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
            });
        }
        fn on_record(
            &self,
            id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            ctx: Context<'_, S>,
        ) {
            let mut g = Grab::default();
            values.record(&mut g);
            if let (Some(span), Some(o)) = (ctx.span(id), g.outcome) {
                if let Some(st) = span.extensions_mut().get_mut::<Stored>() {
                    st.outcome = o;
                }
            }
        }
        fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
            let span = ctx.span(&id).unwrap();
            let name = span.name().to_string();
            // Pull owned values out before the extensions borrow ends, then push.
            let row = span
                .extensions()
                .get::<Stored>()
                .map(|st| (name, st.tool.clone(), st.args.clone(), st.outcome.clone()));
            if let Some(row) = row {
                self.0.lock().unwrap().push(row);
            }
        }
    }

    /// Drive an async body to completion on a private current-thread runtime while
    /// holding [`CAPTURE_SERIAL`]. Sync (not `#[tokio::test]`) so the ordering guard
    /// isn't held across an `.await`, and `block_on` polls the future on *this* thread
    /// — the one whose `set_default` is in scope, so the span routes to our capture.
    fn serialized_capture<F: std::future::Future>(body: F) {
        let _serial = CAPTURE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        force_multi_dispatcher();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(body);
    }

    /// A success emits a `tool` span tagged with the tool's name, an args summary,
    /// and outcome=ok; the output passes through.
    #[test]
    fn emits_a_tool_span_with_name_args_and_outcome() {
        serialized_capture(async {
            let cap = Capture::default();
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);

            let tool = traced(Echo);
            let out = tool.call(r#"{"msg":"hi"}"#.to_string()).await.unwrap();
            assert!(out.contains("hi"), "output passes through: {out}");

            drop(_g);
            let spans = cap.0.lock().unwrap().clone();
            assert!(
                spans.iter().any(|(n, tool, args, oc)| n == "tool"
                    && tool == "echo"
                    && args.contains("hi")
                    && oc == "ok"),
                "a `tool` span with name/args/outcome: {spans:?}"
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

            let tool = traced(Echo);
            assert!(tool.call(r#"{"msg":"boom"}"#.to_string()).await.is_err());

            drop(_g);
            let spans = cap.0.lock().unwrap().clone();
            assert!(
                spans
                    .iter()
                    .any(|(n, tool, _args, oc)| n == "tool" && tool == "echo" && oc == "error"),
                "a `tool` span tagged outcome=error: {spans:?}"
            );
        });
    }
}
