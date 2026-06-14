//! One span per tool call. Wrap any phase tool so every invocation emits a `tool`
//! span carrying `gen_ai.tool.name` and an ok/err `outcome` (the span's own
//! duration is the call's latency). This is what lets telemetry answer *which* tool
//! the model actually called — the question the `read`-tool spike could only infer
//! from trace content (docs/issues.md "No per-tool-call span").
//!
//! It's *our* span on *our* tools, independent of what rig or a given provider
//! instruments, so it lands the same on every backend. The wrapper is otherwise
//! transparent: name, definition, args, output, and errors pass straight through.

use rig_core::completion::ToolDefinition;
use rig_core::tool::{Tool, ToolDyn};
use tracing::{field, info_span, Instrument, Span};

/// Wraps a [`Tool`], emitting a `tool` span per `call`. Delegates everything else.
pub struct Traced<T> {
    inner: T,
}

impl<T: Tool> Traced<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: Tool> Tool for Traced<T> {
    const NAME: &'static str = T::NAME;
    type Error = T::Error;
    type Args = T::Args;
    type Output = T::Output;

    async fn definition(&self, prompt: String) -> ToolDefinition {
        self.inner.definition(prompt).await
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // `outcome` is left empty and filled after the call so one span carries both
        // "which tool" and "did it succeed"; the span's start/end bracket the latency.
        let span = info_span!("tool", "gen_ai.tool.name" = T::NAME, outcome = field::Empty);
        async {
            let result = self.inner.call(args).await;
            Span::current().record("outcome", if result.is_ok() { "ok" } else { "error" });
            result
        }
        .instrument(span)
        .await
    }
}

/// Box a tool as a span-wrapped [`ToolDyn`] — the toolset-assembly drop-in for
/// `Box::new(tool)`, so every tool in a phase's toolset is traced uniformly.
pub fn traced<T: Tool + 'static>(tool: T) -> Box<dyn ToolDyn> {
    Box::new(Traced::new(tool))
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

    /// Captures (span name, gen_ai.tool.name, outcome) for each span opened/closed.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<(String, String, String)>>>);

    struct FieldGrab {
        tool: Option<String>,
        outcome: Option<String>,
    }
    impl tracing::field::Visit for FieldGrab {
        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
            match f.name() {
                "gen_ai.tool.name" => self.tool = Some(v.to_string()),
                "outcome" => self.outcome = Some(v.to_string()),
                _ => {}
            }
        }
        fn record_debug(&mut self, _f: &tracing::field::Field, _v: &dyn std::fmt::Debug) {}
    }

    impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
        // Record on close so the `outcome` (set after the inner call) is present.
        fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
            let span = ctx.span(&id).unwrap();
            let name = span.name().to_string();
            // The tool name is an at-creation field; pull it from the span extensions
            // the registry stores. Simpler: re-record from a stored visitor.
            let ext = span.extensions();
            if let Some(g) = ext.get::<Grabbed>() {
                self.0
                    .lock()
                    .unwrap()
                    .push((name, g.tool.clone(), g.outcome.clone()));
            }
        }
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::Id,
            ctx: Context<'_, S>,
        ) {
            let mut g = FieldGrab {
                tool: None,
                outcome: None,
            };
            attrs.record(&mut g);
            let span = ctx.span(id).unwrap();
            span.extensions_mut().insert(Grabbed {
                tool: g.tool.unwrap_or_default(),
                outcome: g.outcome.unwrap_or_default(),
            });
        }
        fn on_record(
            &self,
            id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            ctx: Context<'_, S>,
        ) {
            let mut g = FieldGrab {
                tool: None,
                outcome: None,
            };
            values.record(&mut g);
            if let Some(span) = ctx.span(id) {
                if let Some(stored) = span.extensions_mut().get_mut::<Grabbed>() {
                    if let Some(o) = g.outcome {
                        stored.outcome = o;
                    }
                }
            }
        }
    }
    struct Grabbed {
        tool: String,
        outcome: String,
    }

    /// A success emits a `tool` span tagged with the tool's name and outcome=ok; the
    /// output passes through unchanged.
    #[tokio::test]
    async fn emits_a_named_tool_span_on_success() {
        let cap = Capture::default();
        let sub = tracing_subscriber::registry().with(cap.clone());
        let _g = tracing::subscriber::set_default(sub);

        // Disambiguate: `Traced<T>` carries both `Tool::call` and the blanket
        // `ToolDyn::call`. We exercise the typed `Tool::call` the wrapper defines.
        let out = Tool::call(&Traced::new(Echo), EchoArgs { msg: "hi".into() })
            .await
            .unwrap();
        assert_eq!(out, "hi", "output passes through the wrapper");

        drop(_g); // close the subscriber's view; spans already closed at await end
        let spans = cap.0.lock().unwrap().clone();
        assert!(
            spans
                .iter()
                .any(|(n, tool, oc)| n == "tool" && tool == "echo" && oc == "ok"),
            "a `tool` span tagged gen_ai.tool.name=echo outcome=ok: {spans:?}"
        );
    }

    /// An error still emits the span, tagged outcome=error — so a failing tool is
    /// visible, not silent.
    #[tokio::test]
    async fn emits_an_error_outcome_when_the_tool_fails() {
        let cap = Capture::default();
        let sub = tracing_subscriber::registry().with(cap.clone());
        let _g = tracing::subscriber::set_default(sub);

        let err = Tool::call(&Traced::new(Echo), EchoArgs { msg: "boom".into() }).await;
        assert!(err.is_err(), "error passes through");

        drop(_g);
        let spans = cap.0.lock().unwrap().clone();
        assert!(
            spans
                .iter()
                .any(|(n, tool, oc)| n == "tool" && tool == "echo" && oc == "error"),
            "a `tool` span tagged outcome=error: {spans:?}"
        );
    }
}
