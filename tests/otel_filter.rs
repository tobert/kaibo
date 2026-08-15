//! The canary: a distinctive string planted everywhere a span can carry content,
//! run through the real SDK export path, and asserted absent.
//!
//! This is the test the whole filter exists to pass. The unit tests in
//! `src/otel_filter.rs` check the policy's *decisions*; this one checks that those
//! decisions are actually applied to a span the SDK produced, on every surface a
//! span has — attributes, event attributes, and the error status description.
//!
//! **Written as a differential.** Asserting only "the marker is absent" would pass
//! just as well if the test were looking in the wrong place, or if no span were
//! exported at all. So every case runs twice: once redacted, where the marker must
//! be gone, and once with `capture_content = true`, where it must be *present*. The
//! second half is what proves the first half was looking somewhere real.
//!
//! It drives the OpenTelemetry SDK directly rather than through the `tracing`
//! bridge, deliberately. A `tracing` subscriber installed in one test poisons
//! callsite interest for others in the same binary, and that flake would make a
//! security test intermittently green for the wrong reason. The bridge is covered
//! by construction instead: `tracing_opentelemetry` maps a field *name* to an
//! attribute *key* unchanged, so a `gen_ai.prompt` field arrives as a
//! `gen_ai.prompt` attribute and meets the same allowlist. What this file proves is
//! that the allowlist is enforced on the way out, which is the half that could
//! silently stop working.

use opentelemetry::trace::{Span, Status, Tracer, TracerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

use kaibo::otel_filter::{AttributePolicy, Filtered};

/// A string that appears nowhere else in the codebase, so finding it anywhere in an
/// exported span means it came from the payload we planted.
const MARKER: &str = "CANARY-7f3a91-SECRET-SOURCE-LINE";

/// Emit one span carrying `MARKER` on every content-bearing surface, and return
/// what the exporter actually received.
fn export_a_span_full_of_content(capture_content: bool) -> Vec<SpanData> {
    let sink = InMemorySpanExporter::default();
    let policy = AttributePolicy::new(capture_content, &[]);
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(Filtered::new(sink.clone(), policy))
        .build();
    let tracer = provider.tracer("canary");

    let mut span = tracer
        .span_builder("chat gpt-5.6")
        .with_attributes([
            // Content, in rig's two spellings plus the current conventions.
            KeyValue::new("gen_ai.prompt", MARKER),
            KeyValue::new("gen_ai.completion", MARKER),
            KeyValue::new("gen_ai.input.messages", MARKER),
            KeyValue::new("gen_ai.output.messages", MARKER),
            KeyValue::new("gen_ai.system_instructions", MARKER),
            KeyValue::new("gen_ai.tool.call.arguments", MARKER),
            KeyValue::new("gen_ai.tool.call.result", MARKER),
            KeyValue::new("gen_ai.tool.arguments", MARKER),
            // An attribute nobody has blessed — the shape of a future rig bump.
            KeyValue::new("rig.some.new.payload", MARKER),
            // Metadata, which must survive either way.
            KeyValue::new("gen_ai.request.model", "gpt-5.6-terra"),
            KeyValue::new("gen_ai.usage.input_tokens", 1234_i64),
            KeyValue::new("kaish.exit_code", 0_i64),
        ])
        .start(&tracer);

    // Surface 2: an event, the way one of kaibo's own log lines arrives.
    span.add_event(
        "log",
        vec![
            KeyValue::new("message", format!("running kaish: cat -n {MARKER}")),
            KeyValue::new("kaish.output_bytes", 4096_i64),
        ],
    );

    // Surface 3: the error status description, where a provider body lands.
    span.set_status(Status::error(format!("provider said: {MARKER}")));
    span.end();

    provider.force_flush().expect("flush");
    sink.get_finished_spans().expect("spans were exported")
}

/// Every place the marker could hide in an exported span, rendered as text.
fn everything_in(spans: &[SpanData]) -> String {
    let mut out = String::new();
    for s in spans {
        out.push_str(&s.name);
        out.push('\n');
        for kv in &s.attributes {
            out.push_str(kv.key.as_str());
            out.push('=');
            out.push_str(&kv.value.as_str());
            out.push('\n');
        }
        for e in s.events.iter() {
            out.push_str(&e.name);
            out.push('\n');
            for kv in &e.attributes {
                out.push_str(kv.key.as_str());
                out.push('=');
                out.push_str(&kv.value.as_str());
                out.push('\n');
            }
        }
        if let Status::Error { description } = &s.status {
            out.push_str(description);
            out.push('\n');
        }
    }
    out
}

/// The canary. With content redacted, the marker reaches the exporter nowhere —
/// not on an attribute, not on an event, not in the error description.
#[test]
fn no_content_reaches_the_exporter_when_redacted() {
    let spans = export_a_span_full_of_content(false);
    assert_eq!(spans.len(), 1, "exactly one span was exported");
    let haystack = everything_in(&spans);
    assert!(
        !haystack.contains(MARKER),
        "content leaked past the filter. Exported span was:\n{haystack}"
    );
}

/// The other half of the differential: with the opt-in on, the marker IS there.
/// Without this, the test above would pass on an empty span or a broken search.
#[test]
fn the_same_content_does_reach_the_exporter_when_opted_in() {
    let spans = export_a_span_full_of_content(true);
    let haystack = everything_in(&spans);
    assert!(
        haystack.contains(MARKER),
        "opting in must actually export content — otherwise the redaction test \
         proves nothing. Exported span was:\n{haystack}"
    );
}

/// Redaction is not a blanket: the numbers an operator watches survive it. A filter
/// that dropped everything would pass the canary and be useless.
#[test]
fn metadata_survives_redaction() {
    let spans = export_a_span_full_of_content(false);
    let haystack = everything_in(&spans);
    for expected in [
        "gen_ai.request.model=gpt-5.6-terra",
        "gen_ai.usage.input_tokens=1234",
        "kaish.exit_code=0",
        "kaish.output_bytes=4096",
    ] {
        assert!(
            haystack.contains(expected),
            "{expected} is metadata and must survive redaction; got:\n{haystack}"
        );
    }
    // The span name and the fact of the error both survive — the skeleton stays.
    assert!(haystack.contains("chat gpt-5.6"), "the span name survives");
    assert!(
        matches!(spans[0].status, Status::Error { .. }),
        "the error is still an error; only its prose is dropped"
    );
}

/// An attribute nobody blessed is dropped even though it is not on the content
/// list — the fail-closed property, stated as an end-to-end fact rather than a
/// policy unit test. This is what protects us on a rig bump.
#[test]
fn an_unblessed_attribute_never_reaches_the_wire() {
    let spans = export_a_span_full_of_content(false);
    let haystack = everything_in(&spans);
    assert!(
        !haystack.contains("rig.some.new.payload"),
        "an attribute absent from the allowlist must not be exported; got:\n{haystack}"
    );
}
