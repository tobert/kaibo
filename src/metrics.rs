//! GenAI metrics — the signal that carries no content.
//!
//! The second half of the telemetry split. [`crate::otel_filter`] made *traces* safe
//! by filtering attributes on the way out; metrics are safe by **construction**, and
//! the difference matters. A filter is a policy: it holds because an allowlist is
//! correct and stays correct as rig's attributes drift. This signal needs no
//! allowlist, because no metric in the GenAI semantic conventions carries a
//! content-bearing attribute at all — every one of them is a model id, a provider
//! name, a token type, an error type, an agent name, or a tool name. That was
//! confirmed by reading `model/gen-ai/metrics.yaml` and the attribute groups it
//! references, not assumed from the metric names.
//!
//! So an operator can run `traces = false, metrics = true` and export kaibo's
//! behavior — spend, latency, delegation, tool use — while no prompt, completion, or
//! source line can leave the process by this road even in principle.
//!
//! ## What kaibo measures, and where it already knew it
//!
//! Nothing here is new bookkeeping; each instrument reads a number kaibo already had.
//!
//! | Metric | Recorded at | The number was already in |
//! |---|---|---|
//! | `gen_ai.client.token.usage` | [`record_completion`] | the provider's `Usage` on every turn |
//! | `gen_ai.client.operation.duration` | [`record_completion`] | — (timed around the call) |
//! | `gen_ai.invoke_agent.duration` | [`record_agent_invocation`] | — (timed around the phase) |
//! | `gen_ai.invoke_agent.inference_calls` | [`record_agent_invocation`] | `CompletionLog::len` |
//! | `gen_ai.invoke_agent.tool_calls` | [`record_agent_invocation`] | `TurnRecord::tool_calls`, summed |
//! | `gen_ai.execute_tool.duration` | [`record_tool_execution`] | — (timed around the tool) |
//!
//! `inference_calls` and `tool_calls` fall out of [`crate::completion_watch`] for
//! free, which is why they cost one addition each rather than a counter threaded
//! through the loop. `tool_calls` is the one Amy asked for by name: it answers "did
//! the consult driver actually delegate a sweep, or did it read all 203 turns
//! itself" — the question a $4 OpenRouter consult raised, and which until now needed
//! OTLP span archaeology to answer.
//!
//! ## Counting rules that are easy to get wrong
//!
//! Straight from `metrics.yaml:188-225`, because two of them are counterintuitive:
//!
//! - Both agent call-count metrics are scoped to **one invocation**, and both
//!   **include failed calls**. A phase that died at turn 90 still reports 90.
//! - Both **exclude calls made by sub-agents**. kaibo's delegated `explore′` sweep is
//!   a sub-agent invocation: it records its own `inference_calls` against its own
//!   name, and the driver counts the delegation as **one tool call**. That is what
//!   makes each call counted exactly once across the tree, and it is why the driver's
//!   `tool_calls` is the delegation answer rather than a mixed total.
//! - `tool_calls` counts **client-side** tool calls only — tools kaibo executes.
//!   A provider-side built-in (web search, code execution) is not ours to count.
//!
//! ## Off costs nothing
//!
//! The instruments hang off the **global** meter provider. With metrics disabled
//! kaibo installs none, so `global::meter` yields the SDK's no-op provider and every
//! `record` here is a virtual call that returns — no allocation, no attribute set
//! built, nothing buffered. That is the same shape the traces layer gets from having
//! no subscriber attached, and it keeps the default run free of a telemetry tax.

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::Histogram;
use opentelemetry::{global, KeyValue};

use crate::credentials::ProviderKind;

/// Advisory bucket boundaries, taken from the rendered conventions
/// (`docs/gen-ai/gen-ai-metrics.md`) rather than invented.
///
/// They are **not** in `metrics.yaml` — the YAML defines the instrument, the rendered
/// doc carries the `ExplicitBucketBoundaries` line — so anyone re-deriving these from
/// the model files alone will not find them. Using the published ones is what lets a
/// backend compare kaibo's histograms against every other GenAI instrumentation's.
mod buckets {
    /// Tokens: powers of four to 64M, so a 16k prompt and a 4M cache read land in
    /// different buckets.
    pub const TOKEN_USAGE: &[f64] = &[
        1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0,
        4194304.0, 16777216.0, 67108864.0,
    ];
    /// One provider call, in seconds — doubling from 10ms to ~82s.
    pub const CLIENT_OPERATION: &[f64] = &[
        0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
    ];
    /// A whole agent invocation, in seconds — doubling from 100ms to ~410s. Wider
    /// than a single call because a consult is many calls plus tool time; the 21-minute
    /// OpenRouter consult lands in the top bucket, which is the right shape for a
    /// runaway.
    pub const AGENT_DURATION: &[f64] = &[
        0.1, 0.2, 0.4, 0.8, 1.6, 3.2, 6.4, 12.8, 25.6, 51.2, 102.4, 204.8, 409.6,
    ];
    /// Calls per invocation — powers of two to 128. kaibo's turn caps sit inside this
    /// range, so a phase pinned at its cap is visible as pile-up in one bucket.
    pub const CALL_COUNT: &[f64] = &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0];
    /// One tool execution, in seconds. Same ladder as a client call: a `run_kaish`
    /// grep and a delegated sweep differ by orders of magnitude, and both fit.
    pub const TOOL_DURATION: &[f64] = &[
        0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
    ];
}

/// The conventions' name for a provider, which is not always kaibo's.
///
/// `gen_ai.provider.name` is an **open** enum: the spec fixes a spelling for the
/// providers it knows and lets instrumentation name the rest. Two of kaibo's kinds
/// need the translation, and getting it wrong would file kaibo's numbers under a name
/// no other instrumentation uses — the whole value of a shared convention.
///
/// - Gemini is `gcp.gemini` in the spec, never `gemini`.
/// - OpenRouter has no spelling in the spec at all (it is a gateway, not a model
///   vendor), so kaibo's own `openrouter` stands. A reader sees the gateway, which is
///   the honest answer: the upstream model behind one OpenRouter slug varies per call
///   and kaibo is not told which one served it.
///
/// The media kinds never reach here — they generate images through kaibo's own
/// facades, not a rig completion model, so no client metric is recorded for them.
fn provider_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Gemini => "gcp.gemini",
        other => other.canonical_name(),
    }
}

/// The six instruments, built once. Cached in a `OnceLock` rather than rebuilt per
/// call because instrument creation walks the provider's registry — cheap, but not
/// free, and these are recorded on every turn of every phase.
///
/// **Bound at first use.** An OTel `Meter` captures the global provider as it stands
/// when the meter is made, so these instruments answer to whatever was installed the
/// first time anything recorded. In the binary that is unambiguous — `main` runs
/// `telemetry::init` before it serves a single request — but it means installing a
/// provider *after* a record is a no-op, which is why the tests below build their own
/// `Instruments` from a test meter instead of racing the global.
struct Instruments {
    token_usage: Histogram<u64>,
    operation_duration: Histogram<f64>,
    agent_duration: Histogram<f64>,
    inference_calls: Histogram<u64>,
    tool_calls: Histogram<u64>,
    tool_duration: Histogram<f64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

/// Build the instruments against whatever meter provider is installed *now*.
///
/// Called lazily on first record, which is after `main` has installed the provider (or
/// decided not to). With metrics off that is the no-op provider and these become no-op
/// instruments — built once, then free forever.
fn instruments() -> &'static Instruments {
    INSTRUMENTS.get_or_init(|| Instruments::new(&global::meter("kaibo")))
}

impl Instruments {
    /// Build the six against a given meter. Split from the global cache so a test can
    /// hold its own set and read what was recorded, and so the boundaries live in
    /// exactly one place for both.
    fn new(meter: &opentelemetry::metrics::Meter) -> Self {
        Instruments {
            token_usage: meter
                .u64_histogram("gen_ai.client.token.usage")
                .with_description("Number of input and output tokens used.")
                .with_unit("{token}")
                .with_boundaries(buckets::TOKEN_USAGE.to_vec())
                .build(),
            operation_duration: meter
                .f64_histogram("gen_ai.client.operation.duration")
                .with_description("GenAI operation duration.")
                .with_unit("s")
                .with_boundaries(buckets::CLIENT_OPERATION.to_vec())
                .build(),
            agent_duration: meter
                .f64_histogram("gen_ai.invoke_agent.duration")
                .with_description("The end-to-end duration of a single agent invocation.")
                .with_unit("s")
                .with_boundaries(buckets::AGENT_DURATION.to_vec())
                .build(),
            inference_calls: meter
                .u64_histogram("gen_ai.invoke_agent.inference_calls")
                .with_description(
                    "The number of inference calls an agent makes during a single invocation.",
                )
                .with_unit("{inference_call}")
                .with_boundaries(buckets::CALL_COUNT.to_vec())
                .build(),
            tool_calls: meter
                .u64_histogram("gen_ai.invoke_agent.tool_calls")
                .with_description(
                    "The number of tool calls an agent makes during a single invocation.",
                )
                .with_unit("{tool_call}")
                .with_boundaries(buckets::CALL_COUNT.to_vec())
                .build(),
            tool_duration: meter
                .f64_histogram("gen_ai.execute_tool.duration")
                .with_description("The duration of a single tool execution.")
                .with_unit("s")
                .with_boundaries(buckets::TOOL_DURATION.to_vec())
                .build(),
        }
    }
}

/// Who is running a phase — the identity its metrics are attributed to.
///
/// Carried rather than looked up because the one place that knows both facts is cast
/// resolution (`Arm::from_slot`), and the places that measure are the phase loop and
/// the completion wrapper several layers below it.
///
/// `Option<PhaseIdentity>` on an arm, and absent means **no provider call is
/// happening**: the offline scripted client the test harness injects through
/// `Arm::new` runs the real loop against an in-process responder. Recording a
/// microsecond "provider call" for that would poison every latency histogram it
/// touched, so an arm without an identity records nothing. Live arms always have one —
/// `Arm::from_slot` is the single live construction point and sets it there.
#[derive(Debug, Clone, Copy)]
pub struct PhaseIdentity {
    /// The provider kind behind this arm — translated to the conventions' spelling by
    /// [`provider_name`].
    pub provider: ProviderKind,
    /// This phase's agent name: the cast role it fills, `"synth"` or `"explorer"`.
    ///
    /// That is the distinction the agent metrics exist to draw. A consult driver is
    /// the synth; a delegated `explore′` sweep is an explorer running its own
    /// invocation. Reading `gen_ai.invoke_agent.tool_calls` split by this name is how
    /// "did the driver delegate, or did it read everything itself" gets answered
    /// without opening a single trace.
    pub agent: &'static str,
}

/// Who made one provider call — [`PhaseIdentity`] plus the model that served it.
#[derive(Debug, Clone, Copy)]
pub struct CallIdent<'a> {
    /// The provider kind behind this arm.
    pub provider: ProviderKind,
    /// The model id as configured, e.g. `deepseek-v4-pro`. Reported as
    /// `gen_ai.request.model`: kaibo asks for a specific id and gets it, and rig's
    /// normalized response does not carry the served id separately.
    pub model: &'a str,
}

/// Record one provider call: its duration, and the tokens it reported.
///
/// `usage` is rig's normalized report for this single completion. A provider that
/// reported nothing yields a zero-valued `Usage`, and zero-valued token counts are
/// **not** recorded — the conventions say to report usage only when the count is
/// readily available, and a recorded zero would drag every average down while
/// claiming a call genuinely used no tokens.
///
/// `error` is the `error.type` for a failed call (the conventions' own attribute), and
/// `None` for a call that returned normally. A failed call still records its duration:
/// a provider that takes 30 seconds to fail is exactly what an operator needs to see.
pub fn record_completion(
    ident: CallIdent<'_>,
    duration: Duration,
    usage: Option<&rig_core::completion::Usage>,
    error: Option<&str>,
) {
    record_completion_on(instruments(), ident, duration, usage, error);
}

fn record_completion_on(
    inst: &Instruments,
    ident: CallIdent<'_>,
    duration: Duration,
    usage: Option<&rig_core::completion::Usage>,
    error: Option<&str>,
) {
    let provider = provider_name(ident.provider);

    let mut attrs = vec![
        KeyValue::new("gen_ai.provider.name", provider),
        KeyValue::new("gen_ai.request.model", ident.model.to_string()),
        KeyValue::new("gen_ai.operation.name", "chat"),
    ];
    if let Some(kind) = error {
        attrs.push(KeyValue::new("error.type", kind.to_string()));
    }
    inst.operation_duration
        .record(duration.as_secs_f64(), &attrs);

    let Some(usage) = usage else { return };
    // Token type is a REQUIRED attribute on this metric, so the two directions are
    // two records against one instrument, never one record of a total.
    let token_attrs = |token_type: &'static str| {
        vec![
            KeyValue::new("gen_ai.provider.name", provider),
            KeyValue::new("gen_ai.request.model", ident.model.to_string()),
            KeyValue::new("gen_ai.operation.name", "chat"),
            KeyValue::new("gen_ai.token.type", token_type),
        ]
    };
    if usage.input_tokens > 0 {
        inst.token_usage
            .record(usage.input_tokens, &token_attrs("input"));
    }
    if usage.output_tokens > 0 {
        inst.token_usage
            .record(usage.output_tokens, &token_attrs("output"));
    }
}

/// Record one agent invocation — a whole phase, from its first turn to its answer.
///
/// `inference_calls` and `tool_calls` follow the conventions' counting rules quoted in
/// the module docs: this invocation's own calls, failed ones included, sub-agent calls
/// excluded. kaibo satisfies the sub-agent rule by construction rather than by
/// filtering — a delegated `explore′` sweep runs its own [`record_agent_invocation`]
/// under its own agent name, and the driver sees the delegation only as the one tool
/// call it made.
pub fn record_agent_invocation(
    agent: &str,
    model: &str,
    duration: Duration,
    inference_calls: u64,
    tool_calls: u64,
    error: Option<&str>,
) {
    record_agent_invocation_on(
        instruments(),
        agent,
        model,
        duration,
        inference_calls,
        tool_calls,
        error,
    );
}

#[allow(clippy::too_many_arguments)] // one argument per recorded attribute or value
fn record_agent_invocation_on(
    inst: &Instruments,
    agent: &str,
    model: &str,
    duration: Duration,
    inference_calls: u64,
    tool_calls: u64,
    error: Option<&str>,
) {
    let mut attrs = vec![
        KeyValue::new("gen_ai.agent.name", agent.to_string()),
        KeyValue::new("gen_ai.request.model", model.to_string()),
    ];
    if let Some(kind) = error {
        attrs.push(KeyValue::new("error.type", kind.to_string()));
    }
    inst.agent_duration.record(duration.as_secs_f64(), &attrs);

    // The call counts take the agent name alone, which is what the conventions list
    // for them. Keeping the model off these two is deliberate: "how many turns does a
    // consult driver take" is a question about the phase, and mixing models into the
    // series would split it per cast for no gain the duration metric doesn't already
    // give.
    let count_attrs = [KeyValue::new("gen_ai.agent.name", agent.to_string())];
    inst.inference_calls.record(inference_calls, &count_attrs);
    inst.tool_calls.record(tool_calls, &count_attrs);
}

/// Record one tool execution — a `run_kaish` shell call, a delegated `explore′` sweep,
/// a `view_image` read.
///
/// The delegated sweep shows up here *and* as its own agent invocation, which is not
/// double counting: they measure different things. This says how long the driver
/// waited; the sweep's own invocation says what it did while the driver waited.
pub fn record_tool_execution(tool: &str, duration: Duration, error: Option<&str>) {
    record_tool_execution_on(instruments(), tool, duration, error);
}

fn record_tool_execution_on(
    inst: &Instruments,
    tool: &str,
    duration: Duration,
    error: Option<&str>,
) {
    let mut attrs = vec![KeyValue::new("gen_ai.tool.name", tool.to_string())];
    if let Some(kind) = error {
        attrs.push(KeyValue::new("error.type", kind.to_string()));
    }
    inst.tool_duration.record(duration.as_secs_f64(), &attrs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_is_reported_under_the_conventions_spelling() {
        // The one translation that would silently file kaibo's numbers under a name no
        // other instrumentation uses. `gemini` is kaibo's name; `gcp.gemini` is the
        // spec's, and the spec wins on the wire.
        assert_eq!(provider_name(ProviderKind::Gemini), "gcp.gemini");
    }

    #[test]
    fn a_provider_the_spec_does_not_name_keeps_kaibos_own() {
        // OpenRouter is a gateway, absent from the spec's enum — which is open, so our
        // canonical name is the honest answer rather than a forced fit into someone
        // else's.
        assert_eq!(provider_name(ProviderKind::OpenRouter), "openrouter");
        assert_eq!(provider_name(ProviderKind::Anthropic), "anthropic");
        assert_eq!(provider_name(ProviderKind::DeepSeek), "deepseek");
        assert_eq!(provider_name(ProviderKind::Openai), "openai");
    }

    #[test]
    fn recording_without_a_meter_provider_is_a_no_op_not_a_panic() {
        // The default run: metrics off, no provider installed, and every record here
        // must be a cheap return. If this ever panics or blocks, telemetry-off stopped
        // being free.
        let ident = CallIdent {
            provider: ProviderKind::DeepSeek,
            model: "deepseek-v4-pro",
        };
        record_completion(ident, Duration::from_millis(10), None, None);
        record_agent_invocation(
            "synth",
            "deepseek-v4-pro",
            Duration::from_secs(1),
            5,
            2,
            None,
        );
        record_tool_execution("run_kaish", Duration::from_millis(3), Some("timeout"));
    }

    // --- Recording, against a real reader -----------------------------------------
    //
    // These build their own `Instruments` from a test meter rather than installing a
    // global provider. Two reasons, both structural: the global is process-wide, so a
    // test that installed one would leak into every other test in this binary; and the
    // instruments bind to whichever provider was live at first use, so racing for that
    // slot would make the suite order-dependent. See `Instruments`.

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::AggregatedMetrics;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    /// One recorded point: the metric's name, its attributes as (key, value) pairs, and
    /// the value asserted on — a summed count for the integer histograms, a point count
    /// for the duration ones (wall-clock cannot be pinned).
    type Point = (String, Vec<(String, String)>, u64);

    /// A provider whose measurements can be read back, plus the instruments feeding it.
    fn reader() -> (SdkMeterProvider, InMemoryMetricExporter, Instruments) {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let instruments = Instruments::new(&provider.meter("kaibo"));
        (provider, exporter, instruments)
    }

    /// Every (metric name, attributes, recorded sum) the exporter saw, flattened —
    /// enough to assert both that a metric was recorded and what it was labelled with.
    fn collected(
        provider: &SdkMeterProvider,
        exporter: &InMemoryMetricExporter,
    ) -> Vec<Point> {
        provider.force_flush().expect("flush the test reader");
        let mut out = Vec::new();
        for rm in exporter.get_finished_metrics().expect("read metrics") {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    let name = m.name().to_string();
                    let points: Vec<_> = match m.data() {
                        AggregatedMetrics::U64(d) => match d {
                            opentelemetry_sdk::metrics::data::MetricData::Histogram(h) => h
                                .data_points()
                                .map(|p| {
                                    (
                                        p.attributes()
                                            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                                            .collect::<Vec<_>>(),
                                        p.sum(),
                                    )
                                })
                                .collect(),
                            _ => Vec::new(),
                        },
                        AggregatedMetrics::F64(d) => match d {
                            opentelemetry_sdk::metrics::data::MetricData::Histogram(h) => h
                                .data_points()
                                // Durations are asserted on presence and labels, not
                                // value — a wall-clock number cannot be pinned.
                                .map(|p| {
                                    (
                                        p.attributes()
                                            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                                            .collect::<Vec<_>>(),
                                        p.count(),
                                    )
                                })
                                .collect(),
                            _ => Vec::new(),
                        },
                        AggregatedMetrics::I64(_) => Vec::new(),
                    };
                    for (attrs, value) in points {
                        out.push((name.clone(), attrs, value));
                    }
                }
            }
        }
        out
    }

    fn find<'a>(rows: &'a [Point], metric: &str) -> Vec<&'a Point> {
        rows.iter().filter(|(n, _, _)| n == metric).collect()
    }

    fn attr(row: &Point, key: &str) -> Option<String> {
        row.1
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
    }

    #[test]
    fn token_usage_splits_input_and_output_into_two_records() {
        // `gen_ai.token.type` is a REQUIRED attribute, so a single record of a total
        // would be unreadable — a backend could not tell prompt spend from completion
        // spend, which is the split that makes the metric worth having.
        let (provider, exporter, inst) = reader();
        let usage = rig_core::completion::Usage {
            input_tokens: 1200,
            output_tokens: 340,
            ..Default::default()
        };
        record_completion_on(
            &inst,
            CallIdent {
                provider: ProviderKind::DeepSeek,
                model: "deepseek-v4-pro",
            },
            Duration::from_millis(900),
            Some(&usage),
            None,
        );

        let rows = collected(&provider, &exporter);
        let usage_rows = find(&rows, "gen_ai.client.token.usage");
        assert_eq!(usage_rows.len(), 2, "one record per direction: {rows:?}");

        let input = usage_rows
            .iter()
            .find(|r| attr(r, "gen_ai.token.type").as_deref() == Some("input"))
            .expect("an input-token record");
        assert_eq!(input.2, 1200, "input tokens recorded verbatim");
        assert_eq!(
            attr(input, "gen_ai.provider.name").as_deref(),
            Some("deepseek"),
        );
        assert_eq!(
            attr(input, "gen_ai.request.model").as_deref(),
            Some("deepseek-v4-pro"),
        );

        let output = usage_rows
            .iter()
            .find(|r| attr(r, "gen_ai.token.type").as_deref() == Some("output"))
            .expect("an output-token record");
        assert_eq!(output.2, 340, "output tokens recorded verbatim");
    }

    #[test]
    fn a_provider_that_reported_no_tokens_records_no_token_usage() {
        // rig hands back a zero-valued `Usage` when the provider reported nothing. The
        // conventions say to report usage only when the count is readily available, and
        // a recorded zero is worse than an absent one: it claims a call genuinely used
        // no tokens and drags every average toward zero.
        let (provider, exporter, inst) = reader();
        record_completion_on(
            &inst,
            CallIdent {
                provider: ProviderKind::Anthropic,
                model: "claude-sonnet-4-6",
            },
            Duration::from_millis(50),
            Some(&rig_core::completion::Usage::default()),
            None,
        );

        let rows = collected(&provider, &exporter);
        assert!(
            find(&rows, "gen_ai.client.token.usage").is_empty(),
            "a zero-valued Usage records no tokens: {rows:?}"
        );
        assert_eq!(
            find(&rows, "gen_ai.client.operation.duration").len(),
            1,
            "the call still happened, so its duration is still recorded: {rows:?}"
        );
    }

    #[test]
    fn a_failed_call_records_its_duration_and_its_error_class() {
        // The number an operator most wants when a provider degrades is how long it
        // took to fail. Dropping failures would hide exactly that.
        let (provider, exporter, inst) = reader();
        record_completion_on(
            &inst,
            CallIdent {
                provider: ProviderKind::Openai,
                model: "gpt-5.6-terra",
            },
            Duration::from_secs(30),
            None,
            Some("http_error"),
        );

        let rows = collected(&provider, &exporter);
        let duration = find(&rows, "gen_ai.client.operation.duration");
        assert_eq!(duration.len(), 1, "a failed call is still recorded: {rows:?}");
        assert_eq!(
            attr(duration[0], "error.type").as_deref(),
            Some("http_error"),
            "the failure class rides the record"
        );
    }

    #[test]
    fn the_driver_and_the_sweep_are_separate_series() {
        // The reason this whole signal was prioritized. A consult driver that delegated
        // twice and an explorer sweep that ran twenty turns must be readable apart —
        // `tool_calls` on the synth answers "did it delegate", and it only answers that
        // if the sweep's own turns are counted against the explorer instead.
        let (provider, exporter, inst) = reader();
        record_agent_invocation_on(
            &inst,
            "synth",
            "deepseek-v4-pro",
            Duration::from_secs(120),
            8,
            2,
            None,
        );
        record_agent_invocation_on(
            &inst,
            "explorer",
            "deepseek-v4-flash",
            Duration::from_secs(40),
            20,
            19,
            None,
        );

        let rows = collected(&provider, &exporter);
        let tool_calls = find(&rows, "gen_ai.invoke_agent.tool_calls");
        let synth = tool_calls
            .iter()
            .find(|r| attr(r, "gen_ai.agent.name").as_deref() == Some("synth"))
            .expect("a synth series");
        let explorer = tool_calls
            .iter()
            .find(|r| attr(r, "gen_ai.agent.name").as_deref() == Some("explorer"))
            .expect("an explorer series");
        assert_eq!(synth.2, 2, "the driver's own tool calls — its delegations");
        assert_eq!(explorer.2, 19, "the sweep's own reads, on its own series");

        let inference = find(&rows, "gen_ai.invoke_agent.inference_calls");
        let synth_turns = inference
            .iter()
            .find(|r| attr(r, "gen_ai.agent.name").as_deref() == Some("synth"))
            .expect("a synth series");
        assert_eq!(
            synth_turns.2, 8,
            "the driver's turns exclude the sweep's twenty"
        );
    }

    #[test]
    fn a_tool_execution_carries_its_name_and_failure_class() {
        let (provider, exporter, inst) = reader();
        record_tool_execution_on(&inst, "run_kaish", Duration::from_millis(12), None);
        record_tool_execution_on(&inst, "view_image", Duration::from_millis(5), Some("timeout"));

        let rows = collected(&provider, &exporter);
        let tools = find(&rows, "gen_ai.execute_tool.duration");
        assert_eq!(tools.len(), 2, "one series per (tool, error) pair: {rows:?}");
        let failed = tools
            .iter()
            .find(|r| attr(r, "gen_ai.tool.name").as_deref() == Some("view_image"))
            .expect("the failed tool's series");
        assert_eq!(attr(failed, "error.type").as_deref(), Some("timeout"));
    }
}
