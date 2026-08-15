//! The span-attribute allowlist: kaibo exports the attributes named here and drops
//! every other one.
//!
//! # Why a filter and not a call-site rule
//!
//! Everywhere else kaibo controls what it records, the rule is "never build the
//! thing you must not send". That is not available here. The attributes carrying
//! prompts, completions, and tool payloads are emitted by **rig**, not by kaibo —
//! `gen_ai.prompt`, `gen_ai.completion`, `gen_ai.input.messages`,
//! `gen_ai.output.messages`, `gen_ai.system_instructions`,
//! `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result` all come from inside the
//! dependency. kaibo cannot decline to set them; it can only decline to export them.
//!
//! So the guard goes at the export step, which kaibo does own: a transparent wrapper
//! around the `SpanExporter`, the same shape `completion_watch::Watched` and
//! `wire_repair::Repaired` take around their traits. Every span the SDK produces
//! passes through [`Filtered::export`] before it leaves.
//!
//! # An allowlist, because the failure directions are not symmetric
//!
//! A denylist fails **open** on a dependency bump: rig adds one new content
//! attribute, kaibo has never heard of it, and the next `cargo update` starts
//! shipping source code to a collector with nothing in the diff to notice. An
//! allowlist fails **closed**: the new attribute is simply not exported until
//! someone blesses it by name, and the cost of being wrong is a missing field on a
//! dashboard. That is the same reasoning `tests/no_write_path.rs` uses when it pins
//! an exact blessed count rather than enumerating what is forbidden.
//!
//! The list is built from the OpenTelemetry GenAI semantic conventions, whose own
//! rule is the one kaibo follows: *"OpenTelemetry instrumentations SHOULD NOT
//! capture them by default, but SHOULD provide an option for users to opt in"*
//! (`gen-ai-spans.md`, and its pattern 1 — "[Default] Don't record instructions,
//! inputs, or outputs").
//!
//! # Three surfaces, not one
//!
//! A span carries content in three places, and an allowlist that covered only the
//! first would leak through the other two:
//!
//! 1. **Span attributes** — the obvious one.
//! 2. **Event attributes.** `tracing` events inside a span become span events, and
//!    kaibo's own log lines are events: `running kaish: cat -n src/auth.rs` names a
//!    real path and a real command. Event attributes go through the same allowlist.
//!    An event whose attributes all drop keeps its name and timestamp — the record
//!    that something happened is not content, so it stays.
//! 3. **The error status description.** A failed provider call puts the response
//!    body there, and a failed tool call can put a source snippet there. The
//!    semantic conventions draw exactly this line — `error.type` is a safe,
//!    low-cardinality enum and is on the allowlist, while `exception.message`
//!    carries a sensitivity warning — so kaibo keeps the type and drops the prose.
//!
//! Span **names** are left alone deliberately: the conventions define them as
//! `{operation} {model}` / `execute_tool {tool_name}` shapes, which are identifiers
//! rather than payloads. `tests/otel_filter.rs`'s canary asserts that stays true by
//! searching names along with everything else.

use std::collections::BTreeSet;

use opentelemetry::trace::Status;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};

/// Attributes kaibo exports by default: identifiers, counts, timings, and outcomes.
///
/// Every entry is here because it describes *shape* rather than content — a model
/// id, a token count, an exit code. Sorted by origin so a reader can tell which are
/// the conventions' and which are kaibo's own.
pub const SAFE_ATTRIBUTES: &[&str] = &[
    // --- GenAI conventions: the operation ---
    "gen_ai.operation.name",
    "gen_ai.provider.name",
    "gen_ai.system",
    "gen_ai.conversation.id",
    "gen_ai.output.type",
    "gen_ai.agent.name",
    "gen_ai.agent.id",
    "gen_ai.agent.description",
    "gen_ai.workflow.name",
    // --- GenAI conventions: the request knobs (no payload) ---
    "gen_ai.request.model",
    "gen_ai.request.max_tokens",
    "gen_ai.request.temperature",
    "gen_ai.request.top_p",
    "gen_ai.request.top_k",
    "gen_ai.request.seed",
    "gen_ai.request.stream",
    "gen_ai.request.choice.count",
    "gen_ai.request.frequency_penalty",
    "gen_ai.request.presence_penalty",
    "gen_ai.request.reasoning.level",
    "gen_ai.request.previous_response.id",
    // --- GenAI conventions: the response shape ---
    "gen_ai.response.id",
    "gen_ai.response.model",
    "gen_ai.response.finish_reasons",
    "gen_ai.response.time_to_first_chunk",
    // --- GenAI conventions: usage ---
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    "gen_ai.usage.reasoning_tokens",
    "gen_ai.usage.reasoning.output_tokens",
    "gen_ai.usage.cache_read.input_tokens",
    "gen_ai.usage.cache_creation.input_tokens",
    "gen_ai.usage.tool_use_prompt_tokens",
    "gen_ai.token.type",
    // --- GenAI conventions: tools, identity only ---
    "gen_ai.tool.name",
    "gen_ai.tool.type",
    "gen_ai.tool.call.id",
    // `gen_ai.tool.description` carries a sensitivity warning in the conventions but
    // is `Recommended`, not `Opt-In` — a real inconsistency for a general
    // instrumentation. It is safe HERE for a reason specific to kaibo: our tool
    // descriptions are published product text, written to be read by a calling
    // model. Blessed deliberately, not by oversight.
    "gen_ai.tool.description",
    // --- Errors: the type, never the prose ---
    "error.type",
    // --- Server identity ---
    "server.address",
    "server.port",
    // --- kaibo's own span fields ---
    // From `tool_span.rs`: the shell's outcome and the SIZE of what it printed,
    // never what it printed. `gen_ai.tool.arguments` is deliberately absent — it is
    // kaibo's summary of the kaish script, which names paths.
    "kaish.exit_code",
    "kaish.output_bytes",
    "outcome",
    // The team that answered. A cast name is operator vocabulary from config, and a
    // slot ref is `backend/model-id` — both identifiers.
    "cast",
    "model",
    "slot",
    "phase",
    "role",
    // Loop shape: how many turns, how many delegations. The numbers that answer
    // "did the driver actually delegate", with no payload.
    "turns",
    "turn",
    "max_turns",
    "finish_reason",
];

/// Attributes that carry content, exported only when the operator opts in.
///
/// Split out rather than merged into one big list so the diff that adds content to
/// the wire is legible, and so [`AttributePolicy::describe`] can say what changed.
/// The conventions mark most of these `Opt-In`; the two `gen_ai.prompt` /
/// `gen_ai.completion` entries are rig's older spelling of the same thing, and are
/// listed because rig still emits both.
pub const CONTENT_ATTRIBUTES: &[&str] = &[
    "gen_ai.prompt",
    "gen_ai.completion",
    "gen_ai.input.messages",
    "gen_ai.output.messages",
    "gen_ai.system_instructions",
    "gen_ai.tool.definitions",
    "gen_ai.tool.call.arguments",
    "gen_ai.tool.call.result",
    // kaibo's own: a summary of the kaish script the model ran.
    "gen_ai.tool.arguments",
    // `tracing`'s event body — kaibo's own log lines, which name paths and commands.
    "message",
];

/// The attributes this process exports. Everything else is dropped.
#[derive(Debug, Clone)]
pub struct AttributePolicy {
    allow: BTreeSet<String>,
    content: bool,
}

impl AttributePolicy {
    /// The default policy: the safe set, plus any extra names the operator listed.
    ///
    /// `capture_content` folds in [`CONTENT_ATTRIBUTES`] wholesale — the spec-named
    /// master switch. `extra` is the finer knob: individual semantic-convention
    /// names, so an operator who wants completions but not prompts can say so
    /// without turning everything on.
    pub fn new(capture_content: bool, extra: &[String]) -> Self {
        let mut allow: BTreeSet<String> = SAFE_ATTRIBUTES.iter().map(|s| s.to_string()).collect();
        if capture_content {
            allow.extend(CONTENT_ATTRIBUTES.iter().map(|s| s.to_string()));
        }
        allow.extend(extra.iter().cloned());
        Self {
            allow,
            content: capture_content,
        }
    }

    /// Is this attribute exported?
    pub fn allows(&self, key: &str) -> bool {
        self.allow.contains(key)
    }

    /// Whether the error status description survives. Tied to content capture: a
    /// provider's error body and a failed tool's output both land there.
    pub fn allows_status_description(&self) -> bool {
        self.content
    }

    /// One line naming what leaves this process, for the startup log and
    /// `kaibo://config` — an operator can read the state without reading config back.
    ///
    /// Both spellings state the count, because the number is the part an operator can
    /// check against a change they just made.
    pub fn describe(&self) -> String {
        if self.content {
            format!(
                "content exported — {} attributes, including prompts and responses",
                self.allow.len()
            )
        } else {
            format!(
                "content redacted — {} attributes, no prompts or responses",
                self.allow.len()
            )
        }
    }

    /// Drop every attribute this policy does not allow, in place.
    pub fn scrub(&self, span: &mut SpanData) {
        span.attributes.retain(|kv| self.allows(kv.key.as_str()));
        for event in &mut span.events.events {
            event.attributes.retain(|kv| self.allows(kv.key.as_str()));
        }
        if !self.allows_status_description() {
            if let Status::Error { .. } = span.status {
                // Keep the fact of the error — `error.type` carries the kind — and
                // drop the prose, which is where a provider body or a source
                // snippet would be.
                span.status = Status::Error {
                    description: "".into(),
                };
            }
        }
    }
}

/// A [`SpanExporter`] that scrubs every batch before handing it on.
///
/// Transparent by design: it owns no transport and makes no export decisions, so
/// swapping the inner exporter for an in-memory one in a test exercises the real
/// filter rather than a stand-in.
#[derive(Debug)]
pub struct Filtered<E> {
    inner: E,
    policy: AttributePolicy,
}

impl<E> Filtered<E> {
    /// Wrap an exporter in a policy.
    pub fn new(inner: E, policy: AttributePolicy) -> Self {
        Self { inner, policy }
    }
}

impl<E: SpanExporter> SpanExporter for Filtered<E> {
    async fn export(&self, mut batch: Vec<SpanData>) -> OTelSdkResult {
        for span in &mut batch {
            self.policy.scrub(span);
        }
        self.inner.export(batch).await
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two lists must not overlap: a name in both would be exported by default
    /// while reading as gated, which is the one way this design fails quietly.
    #[test]
    fn the_safe_and_content_lists_are_disjoint() {
        let safe: BTreeSet<_> = SAFE_ATTRIBUTES.iter().collect();
        let content: BTreeSet<_> = CONTENT_ATTRIBUTES.iter().collect();
        let both: Vec<_> = safe.intersection(&content).collect();
        assert!(
            both.is_empty(),
            "an attribute cannot be both safe and gated: {both:?}"
        );
    }

    /// Every attribute rig is known to emit for content is gated. A rig bump can add
    /// to this list, so it is pinned by name rather than inferred.
    #[test]
    fn every_content_attribute_rig_emits_is_gated() {
        for name in [
            "gen_ai.prompt",
            "gen_ai.completion",
            "gen_ai.input.messages",
            "gen_ai.output.messages",
            "gen_ai.system_instructions",
            "gen_ai.tool.call.arguments",
            "gen_ai.tool.call.result",
        ] {
            assert!(
                CONTENT_ATTRIBUTES.contains(&name),
                "{name} is emitted by rig and carries content — it must be gated"
            );
            assert!(
                !AttributePolicy::new(false, &[]).allows(name),
                "{name} must not be exported by default"
            );
        }
    }

    /// The default policy keeps the numbers an operator actually watches.
    #[test]
    fn the_default_policy_keeps_metadata() {
        let p = AttributePolicy::new(false, &[]);
        for name in [
            "gen_ai.request.model",
            "gen_ai.usage.input_tokens",
            "gen_ai.usage.output_tokens",
            "gen_ai.response.finish_reasons",
            "gen_ai.tool.name",
            "kaish.exit_code",
            "kaish.output_bytes",
            "error.type",
            "cast",
        ] {
            assert!(p.allows(name), "{name} is metadata and should survive");
        }
    }

    /// An unknown attribute is dropped — the fail-closed property, which is the
    /// whole reason this is an allowlist. A rig bump that adds
    /// `gen_ai.something.new` must not reach the wire.
    #[test]
    fn an_unknown_attribute_is_dropped() {
        let p = AttributePolicy::new(false, &[]);
        assert!(!p.allows("gen_ai.something.new"));
        assert!(!p.allows("rig.internal.prompt_text"));
    }

    /// Opting in adds the content set and nothing else — it is not a bypass.
    #[test]
    fn opting_in_adds_content_but_stays_an_allowlist() {
        let p = AttributePolicy::new(true, &[]);
        assert!(p.allows("gen_ai.input.messages"));
        assert!(
            !p.allows("gen_ai.something.new"),
            "capture_content is not a wildcard"
        );
    }

    /// A named extra is admitted without turning the whole content set on — the
    /// finer knob, for an operator who wants one field.
    #[test]
    fn an_extra_name_is_admitted_alone() {
        let p = AttributePolicy::new(false, &["gen_ai.output.messages".to_string()]);
        assert!(p.allows("gen_ai.output.messages"));
        assert!(
            !p.allows("gen_ai.input.messages"),
            "naming one attribute must not admit its siblings"
        );
    }

    /// The startup line says which mode is live, in words an operator can act on, and
    /// states the count either way — the number is what someone checks against a
    /// change they just made.
    #[test]
    fn describe_names_the_mode_and_the_count() {
        let off = AttributePolicy::new(false, &[]).describe();
        assert!(off.contains("redacted"), "got: {off}");
        assert!(
            off.contains("no prompts or responses"),
            "the redacted line must say what is absent, not only that it is redacted: {off}"
        );
        let on = AttributePolicy::new(true, &[]).describe();
        assert!(
            on.contains("including prompts and responses"),
            "the opted-in line must name what is now leaving: {on}"
        );
        for line in [&off, &on] {
            assert!(
                line.chars().any(|c| c.is_ascii_digit()),
                "both spellings state the attribute count: {line}"
            );
        }
    }
}
