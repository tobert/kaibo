//! `save_artifact` — a one-way delivery tunnel from the inner model team to the caller.
//!
//! The problem it solves: a `consult` that produces *bulk* output (a corpus of shell
//! commands to fuzz a parser with, a per-file inventory, a generated fixture) has only
//! one channel today — the answer text. That channel is the caller's context window,
//! and a driver that dumps ten kilobytes into it has spent the caller's budget on
//! material the caller may only want to *store*.
//!
//! So the model gets a second channel: hand kaibo the bytes, get back a digest. The
//! caller sees footer lines naming each `kaibo://cas/<digest>` and **chooses** whether
//! to read them. One way, by construction: the model can write into the store and can
//! never read, list, or probe it (see "What the model cannot do" below).
//!
//! # Why this does not weaken the read-only invariant
//!
//! The invariant text (`AGENTS.md`) already carves the shape this fills: "anything
//! further that must *record* or *emit* is its own individually-gated mediated tool,
//! never a general filesystem escape hatch." This is that tool, and it holds the line
//! the same way [`crate::cas`] does — by *shape*, not policy:
//!
//! - **The write is unaimable.** The tool's whole input is `content` (the bytes) plus a
//!   `label` and a `format`. There is no `path`, no `from`, no destination of any kind,
//!   and there never will be: the address is the content's own hash. A parameter naming
//!   a filesystem location was designed, argued, and **dropped permanently** (2026-08-05)
//!   — inline `content` is the only input path this tool ever gets.
//! - **It reaches disk only through [`crate::cas::Cas::put`]**, the existing blessed
//!   write surface. No new `std::fs` call site exists in this module, so
//!   `tests/no_write_path.rs` stays pinned at its four blessed lines.
//! - **kaish is untouched.** The shell the model drives has no new builtin, no mount, no
//!   knowledge that this store exists. `src/sandbox.rs` did not change.
//!
//! # What the model cannot do
//!
//! The tool returns one digest and nothing else. It never says whether the content was
//! new or already present, because that difference is an **existence oracle**: the CAS
//! spans every project this kaibo has ever served, so "was this already here?" answered
//! for arbitrary bytes would let one project's model team probe another's artifacts.
//! Same content in, same line out. There is no read verb, no list verb, and no
//! `kaibo://cas` access from inside the loop — retrieval is operator surface only.
//!
//! # Caps refuse; they never truncate
//!
//! A rejected call has already cost the caller the output tokens for the whole payload,
//! so every limit is stated in the tool's own description (from these constants, never a
//! hand-copied number that can drift) and every refusal names the cap, the actual size,
//! and the way out. Truncating instead would hand back a digest for content that is not
//! what the model wrote, which is the silent corruption this codebase refuses.
//!
//! The per-call ledger lives on one [`ArtifactSink`], built per MCP call, so "8 artifacts
//! per call" means the call the caller made — not per turn, and not per sweep.

use std::sync::{Arc, Mutex};

use rig_agent::tool::{Tool, ToolExecutionError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cas::{Digest, Extension, MediaStore, Provenance};

/// The most bytes one artifact may carry.
///
/// A backstop, not a working limit: `content` rides in tool-call arguments, which are
/// completion tokens, so a synth's `max_tokens` binds long before this does (a 32K
/// output budget is roughly 100 KB of text at best). It exists so a model that somehow
/// can emit more cannot land a megabyte-plus object in one call.
pub const MAX_ARTIFACT_BYTES: usize = 1 << 20;

/// The most artifacts one MCP call may save. Counted across the whole call, so a driver
/// cannot spread a flood over many turns.
pub const MAX_ARTIFACTS_PER_CALL: usize = 8;

/// The most bytes one MCP call may save in total, across every artifact.
pub const MAX_TOTAL_BYTES_PER_CALL: usize = 1 << 23;

/// One call's limits. Ships as [`Caps::default`] — the three constants above — and is a
/// field on the sink rather than three `const` reads scattered through the checks, so the
/// refusals, the tool description, and the admission logic all read the *same* numbers.
/// That is also what lets a test drive a boundary without allocating megabytes
/// (mirroring `ViewImage::with_max_bytes`); it is deliberately not an operator knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub per_artifact_bytes: usize,
    pub artifacts_per_call: usize,
    pub total_bytes_per_call: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            per_artifact_bytes: MAX_ARTIFACT_BYTES,
            artifacts_per_call: MAX_ARTIFACTS_PER_CALL,
            total_bytes_per_call: MAX_TOTAL_BYTES_PER_CALL,
        }
    }
}

/// The formats a model may ask for, and the on-disk container each maps to. Kept as a
/// table rather than a `match` so the tool description, the schema `enum`, and the
/// refusal message all render from the same list and cannot drift apart.
///
/// This is a *serving* concern, not a safety boundary — a model can put anything inside
/// a `.txt`, and the byte caps are the real limit. What it buys is a retrieval that says
/// something true about the bytes: `kaibo://cas/<digest>` stamps a mime from this table.
pub const FORMATS: &[(&str, Extension)] = &[
    ("text", Extension::Txt),
    ("jsonl", Extension::Jsonl),
    ("markdown", Extension::Md),
];

/// The format names, for the schema description and the coercion note.
fn format_names() -> Vec<&'static str> {
    FORMATS.iter().map(|(name, _)| *name).collect()
}

/// Resolve a `format` ask to a container. **Assume text**: a name outside [`FORMATS`]
/// resolves to [`Extension::Txt`] rather than refusing — the content arrived as a JSON
/// string, so it IS UTF-8 text whatever the model called it, and `text/plain` is a true
/// label where a refusal would burn the whole payload's output tokens to enforce a mime
/// vocabulary. `format` is a hint, not a gate; the binary paths belong to `generate`.
/// `None` when the ask was unknown lets the caller *state* the coercion in its result —
/// stated, never silent.
pub fn resolve_format(asked: Option<&str>) -> (Extension, Option<String>) {
    let asked = asked.map(str::trim).filter(|f| !f.is_empty());
    match asked {
        None => (Extension::Txt, None),
        Some(name) => FORMATS
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, ext)| (*ext, None))
            .unwrap_or((Extension::Txt, Some(name.to_string()))),
    }
}

/// The most bytes a `label` may carry, and it is not a courtesy limit.
///
/// The label is the one model-authored string that reaches the caller *outside* the
/// content — it is rendered into the structured footer beside a URI. Two things follow.
/// Unbounded, it is metadata that rides around the byte caps: a model could deliver
/// kilobytes per save in a field nothing counts. And unvalidated, it is an injection
/// surface: a newline lets a model forge extra numbered entries or a `path:` line in the
/// footer, so the caller reads artifacts kaibo never wrote. Hence a modest ceiling and a
/// hard refusal of any control character, checked before anything touches the store.
///
/// 200 bytes is a full descriptive sentence and nowhere near a payload.
pub const MAX_LABEL_BYTES: usize = 200;

/// Who authored an artifact, resolved once per MCP call from the cast and slot actually
/// running. Recorded into the sidecar the *first* write of a given content produces —
/// housekeeping metadata that makes an object self-describing when an operator reaches
/// it **by address**. See [`ArtifactSink::save`] on what that record does and does not
/// claim.
#[derive(Debug, Clone)]
pub struct ArtifactAuthor {
    /// The question this consult is answering. Recorded as the sidecar's `prompt`, the
    /// same slot a `generate` sidecar gives the image prompt: what was asked for.
    pub prompt: String,
    /// The authoring model's id, `backend/model` as the arm resolved it.
    pub model: String,
    /// The cast whose team is running.
    pub cast: String,
    /// The reasoning slot the author filled — `synth` today, since only the consult
    /// driver loop can save.
    pub slot: &'static str,
    /// The consult session, when the call carried one.
    pub session: Option<String>,
}

/// One artifact this call saved, as the caller's footer renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedArtifact {
    /// The address: 64 lowercase hex, the content's own SHA-256.
    pub digest: String,
    /// The mime the `kaibo://cas/<digest>` resource will stamp on these bytes — read
    /// back from the **store** after the write, never the format this save asked for.
    /// The two can differ: identical bytes saved as `jsonl` when they are already held
    /// as `txt` land at the same address, and the store's answer is `txt`. Rendering the
    /// request instead would advertise a mime the resource will not serve and a path
    /// that does not exist. See [`MediaStore::extension_for`].
    pub mime: &'static str,
    /// Size of the content, in bytes.
    pub bytes: usize,
    /// The model's one-line description, validated (bounded, no control characters —
    /// see [`MAX_LABEL_BYTES`]) before it reaches this struct, because it is rendered
    /// into the structured footer.
    pub label: String,
    /// True when the bytes are durably stored but their provenance sidecar could not be
    /// written ([`crate::cas::CasError::ProvenanceNotRecorded`]). The artifact still
    /// belongs in the footer — it is real and retrievable — so the ledger carries it and
    /// the footer says the record is missing rather than pretending it is there.
    pub provenance_missing: bool,
}

/// A refusal, always loud, always naming the way out. Every variant carries the cap it
/// hit *and* the actual value, because a model that only learns "too big" has to guess
/// the next size and pay for the whole payload again to find out.
#[derive(Debug)]
pub enum SaveError {
    /// One artifact's `content` is past [`Caps::per_artifact_bytes`].
    TooLarge { cap: usize, actual: usize },
    /// This call has already saved [`Caps::artifacts_per_call`] artifacts.
    TooMany { cap: usize },
    /// This artifact would push the call past [`Caps::total_bytes_per_call`].
    TotalExceeded {
        cap: usize,
        used: usize,
        actual: usize,
    },
    /// `label` came in blank. The label is what the caller reads beside the URI, so an
    /// artifact with no description is one the caller cannot act on.
    MissingLabel,
    /// `label` is past [`MAX_LABEL_BYTES`], or carries a control character (a newline
    /// especially — see that constant for why the footer cannot take one).
    BadLabel { cap: usize, actual: usize },
    /// The bytes are stored and reachable, but their provenance sidecar is not — the
    /// store's own [`crate::cas::CasError::ProvenanceNotRecorded`]. An error, because
    /// the housekeeping record the operator prunes by is missing, but emphatically NOT
    /// "nothing was saved": the digest names durable content and rides the footer.
    StoredWithoutProvenance { digest: String },
    /// The store refused the write (capacity, I/O, a corrupt object at this address).
    ///
    /// Holds the **typed** error rather than its rendered text, because the two audiences
    /// need opposite things from it. The operator wants everything — the CAS path, the
    /// store's current usage, the OS error — and gets it on the tracing log at the sink.
    /// The model gets a sanitized sentence, because [`crate::cas::CasError`]'s own
    /// `Display` carries filesystem paths (kaibo's XDG data dir, which the model has no
    /// business learning) and, for a capacity refusal, `current_bytes` of a store shared
    /// across every project this kaibo has served. That number is a side channel: it
    /// tells a model how much other projects' work is sitting in the store, and watching
    /// it move across calls tells it more.
    Store(crate::cas::CasError),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::TooLarge { cap, actual } => write!(
                f,
                "This artifact is {actual} bytes and the limit is {cap} bytes per \
                 artifact. Nothing was saved. Split the content into several smaller \
                 artifacts and save each one."
            ),
            SaveError::TooMany { cap } => write!(
                f,
                "This call has already saved {cap} artifacts, which is the limit for one \
                 call. Nothing was saved. Report what you have saved so far in your \
                 answer, and leave the rest for a follow-up call."
            ),
            SaveError::TotalExceeded { cap, used, actual } => write!(
                f,
                "This call has saved {used} bytes and this artifact adds {actual} more, \
                 which is past the limit of {cap} bytes for one call. Nothing was saved. \
                 Save a smaller selection, and report the rest in your answer."
            ),
            SaveError::MissingLabel => write!(
                f,
                "`label` was empty. Nothing was saved. The caller reads the label beside \
                 the artifact's URI, so give one short line saying what this artifact \
                 holds, and save again."
            ),
            SaveError::BadLabel { cap, actual } => write!(
                f,
                "`label` is {actual} bytes and must be one single line of at most {cap} \
                 bytes, with no line breaks. Nothing was saved. Shorten it to one plain \
                 sentence naming what this artifact holds, and save again. Put the detail \
                 in your answer, where there is room for it."
            ),
            SaveError::StoredWithoutProvenance { digest } => write!(
                f,
                "The artifact WAS saved and the caller will receive it at {}{digest}. \
                 kaibo could not record its housekeeping metadata beside it, so name the \
                 URI in your answer and describe what the artifact covers, so the caller \
                 knows what it holds.",
                crate::cas::CAS_URI_PREFIX
            ),
            // Sanitized on purpose — see `SaveError::Store`. Two kinds, because they call
            // for different next moves, and neither carries a number or a path.
            SaveError::Store(crate::cas::CasError::CapacityExceeded { .. }) => write!(
                f,
                "kaibo's artifact store has no room for this artifact, so nothing was \
                 saved. A smaller artifact may still fit. Report what you found in your \
                 answer instead, and say that saving was refused for lack of room."
            ),
            SaveError::Store(_) => write!(
                f,
                "kaibo's artifact store refused this write, so nothing was saved. Report \
                 what you found in your answer instead, and say that saving was refused."
            ),
        }
    }
}

impl std::error::Error for SaveError {}

/// The per-MCP-call ledger and the one place bytes reach the store.
///
/// Built by the server when all three keys hold (the operator enabled artifacts, the
/// caller asked for them on this call, and the media CAS is live), handed to the consult
/// driver's toolset, and read back after the loop to render the caller's footer. Its
/// lifetime **is** the call, which is what makes the per-call caps mean the call.
#[derive(Debug)]
pub struct ArtifactSink {
    store: Arc<MediaStore>,
    author: ArtifactAuthor,
    caps: Caps,
    ledger: Mutex<Ledger>,
}

#[derive(Debug, Default)]
struct Ledger {
    total_bytes: usize,
    saved: Vec<SavedArtifact>,
}

impl ArtifactSink {
    pub fn new(store: Arc<MediaStore>, author: ArtifactAuthor) -> Self {
        Self::with_caps(store, author, Caps::default())
    }

    /// A sink with non-shipping limits — the seam a boundary test drives without
    /// allocating megabytes, and the reason every refusal renders its cap from the sink
    /// rather than from a constant. Production builds one through
    /// [`new`](Self::new).
    pub fn with_caps(store: Arc<MediaStore>, author: ArtifactAuthor, caps: Caps) -> Self {
        Self {
            store,
            author,
            caps,
            ledger: Mutex::new(Ledger::default()),
        }
    }

    /// This sink's limits — what the tool description states, so the numbers a model
    /// reads are the numbers the admission check enforces.
    pub fn caps(&self) -> Caps {
        self.caps
    }

    /// Everything this call saved, oldest first — the footer's input.
    pub fn saved(&self) -> Vec<SavedArtifact> {
        self.ledger
            .lock()
            .expect("artifact ledger poisoned")
            .saved
            .clone()
    }

    /// The caller-facing footer for what this call saved. Empty when nothing was saved.
    pub fn footer(&self) -> String {
        artifact_footer(&self.saved(), &self.store)
    }

    /// Validate, admit against this call's budget, store, and record.
    ///
    /// Order is the contract: every check runs *before* [`crate::cas::Cas::put`], so a
    /// refusal stores nothing. The ledger lock spans the admission and the write so two
    /// concurrent tool calls in one turn cannot both admit against the same remaining
    /// budget; nothing here awaits, so the lock is never held across a suspension point.
    ///
    /// The return value is the digest and nothing more. Whether these bytes were already
    /// in the store is deliberately unobservable — see the module doc.
    ///
    /// # The sidecar is first-writer-wins, and it is housekeeping, not an audit trail
    ///
    /// The provenance sidecar is written with `create_new` and an existing one is left
    /// alone, so the record beside a given content describes its **first** write. Save
    /// content another call already stored and the sidecar keeps that call's cast, model,
    /// label, and session — or names `generate`, if a provider rendered those exact bytes
    /// first. This save still enters *this* call's ledger and this call's footer, so the
    /// caller sees the label it was given; only the on-disk record is the older one.
    ///
    /// That is fine because of what the sidecar is for: **housekeeping metadata that
    /// makes an object self-describing to whoever holds its address**. It is read the way
    /// everything in this store is read — by digest, one lookup — and it is what lets
    /// [`crate::cas::Cas::entry_for`] name an object's format in a single stat. It is
    /// deliberately not a per-save audit record and must not be described as one: a
    /// content-addressed store that never rewrites structurally cannot hold one write's
    /// worth of metadata per save. Where a durable per-call record is wanted, the honest
    /// sources are the tool-call telemetry every save emits (a traced `save_artifact`
    /// span) and the session the answer was recorded into, whose persisted text carries
    /// this call's digests.
    pub fn save(
        &self,
        label: &str,
        content: &str,
        format: Option<&str>,
    ) -> Result<Digest, SaveError> {
        let label = label.trim();
        if label.is_empty() {
            return Err(SaveError::MissingLabel);
        }
        // Bounded and single-line, checked before the store is touched: the label is
        // rendered into the structured footer, so a newline forges footer entries and an
        // unbounded one is payload the byte caps never see. See `MAX_LABEL_BYTES`.
        if label.len() > MAX_LABEL_BYTES || label.chars().any(char::is_control) {
            return Err(SaveError::BadLabel {
                cap: MAX_LABEL_BYTES,
                actual: label.len(),
            });
        }
        // Assume text: an unknown format name resolves to Txt instead of refusing —
        // see `resolve_format` for the argument. The coercion is surfaced by the tool's
        // `call`, which runs the same resolution to build its note.
        let (ext, _coerced) = resolve_format(format);
        let bytes = content.as_bytes();
        if bytes.len() > self.caps.per_artifact_bytes {
            return Err(SaveError::TooLarge {
                cap: self.caps.per_artifact_bytes,
                actual: bytes.len(),
            });
        }

        let mut ledger = self.ledger.lock().expect("artifact ledger poisoned");
        if ledger.saved.len() >= self.caps.artifacts_per_call {
            return Err(SaveError::TooMany {
                cap: self.caps.artifacts_per_call,
            });
        }
        if ledger.total_bytes + bytes.len() > self.caps.total_bytes_per_call {
            return Err(SaveError::TotalExceeded {
                cap: self.caps.total_bytes_per_call,
                used: ledger.total_bytes,
                actual: bytes.len(),
            });
        }

        let provenance = Provenance {
            prompt: self.author.prompt.clone(),
            model: self.author.model.clone(),
            cast: self.author.cast.clone(),
            timestamp: now_epoch_secs(),
            mime: ext.mime().to_string(),
            seed: None,
            tool: Some(SaveArtifact::NAME.to_string()),
            slot: Some(self.author.slot.to_string()),
            label: Some(label.to_string()),
            session: self.author.session.clone(),
        };
        // Three outcomes, not two. The middle one — bytes stored, provenance not — is
        // still a save, so it enters the ledger and rides the footer; denying it would
        // orphan durable content behind a message claiming nothing happened.
        let (digest, provenance_missing) = match self.store.put(bytes, ext, &provenance) {
            Ok(digest) => (digest, false),
            Err(crate::cas::CasError::ProvenanceNotRecorded { digest, cause }) => {
                tracing::warn!(
                    digest = %digest,
                    cause = %cause,
                    "artifact stored without its provenance sidecar — the bytes are \
                     durable and retrievable, the housekeeping record is not"
                );
                let parsed = crate::cas::Digest::from_hex(&digest)
                    .expect("the store renders its own digests in canonical hex");
                (parsed, true)
            }
            Err(e) => {
                // The operator gets the whole typed error, paths and usage included; the
                // model gets `SaveError`'s sanitized rendering. See `SaveError::Store`.
                tracing::warn!(error = %e, "artifact store refused a save_artifact write");
                return Err(SaveError::Store(e));
            }
        };

        // What the artifact IS, per the store — not what this save asked for. They differ
        // whenever identical content is already held under another container format, and
        // the footer must agree with the resource read and the on-disk path. A `None`
        // here is not reachable through a successful put; treat it as loud-but-recoverable
        // rather than panicking on the caller's paid-for save.
        let stored_ext = self.store.extension_for(&digest).unwrap_or_else(|| {
            tracing::warn!(
                digest = %digest.to_hex(),
                "artifact stored but the store cannot name its format — falling back to \
                 the requested one for the footer"
            );
            ext
        });

        ledger.total_bytes += bytes.len();
        ledger.saved.push(SavedArtifact {
            digest: digest.to_hex(),
            mime: stored_ext.mime(),
            bytes: bytes.len(),
            label: label.to_string(),
            provenance_missing,
        });
        if provenance_missing {
            return Err(SaveError::StoredWithoutProvenance {
                digest: digest.to_hex(),
            });
        }
        Ok(digest)
    }
}

/// Epoch seconds for a sidecar timestamp. Local to this module so the sink needs nothing
/// from the server layer; a pre-1970 clock reads as 0 rather than panicking.
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The `save_artifact` tool, as the inner model team sees it.
pub struct SaveArtifact {
    sink: Arc<ArtifactSink>,
}

impl SaveArtifact {
    pub fn new(sink: Arc<ArtifactSink>) -> Self {
        Self { sink }
    }
}

#[derive(Debug, Deserialize)]
pub struct SaveArtifactArgs {
    /// One short line saying what this artifact holds.
    pub label: String,
    /// The bytes themselves.
    pub content: String,
    /// A format hint (`text`, `jsonl`, `markdown`); anything else, or nothing, is text.
    #[serde(default)]
    pub format: Option<String>,
}

impl Tool for SaveArtifact {
    const NAME: &'static str = "save_artifact";
    type Error = SaveError;
    type Args = SaveArtifactArgs;
    type Output = String;

    /// Keep the refusal model-visible: rig's default redacts a tool error to a
    /// kind-level "the tool failed", and every [`SaveError`] exists to tell the model
    /// the cap, the actual size, and what to do next. A model that only learns "it
    /// failed" pays the whole payload again to guess.
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string()).with_source(error)
    }

    fn description(&self) -> String {
        // Rendered from the sink's own caps, never a hand-copied number. A rejected save
        // has already cost the caller the output tokens for the whole payload, so
        // discover-by-failing is uniquely expensive here and a stale figure is worse than
        // none.
        let Caps {
            per_artifact_bytes,
            artifacts_per_call,
            total_bytes_per_call,
        } = self.sink.caps();
        format!(
            "Save bulk content you have written into kaibo's artifact store. You get back \
             an address, and the caller receives that address with your answer.\n\n\
             The saved artifact is the delivery vehicle for bulk content. Your final \
             answer is the report of the work: name the artifact by its URI so the caller \
             can retrieve it, describe what it covers and how it is organized, and quote \
             one or two representative entries where they illustrate the coverage. The \
             full content reaches the caller through the artifact.\n\n\
             Use this for material the caller wants to keep or to run: a generated \
             corpus, a long inventory, a report, a source file, a fixture. Any UTF-8 \
             content ships as text unless `jsonl` or `markdown` fits it better. Reach \
             for it when the content is bulk. Keep your reasoning and your conclusions \
             in the answer itself.\n\n\
             Limits for one call: {per_artifact_bytes} bytes per artifact, \
             {artifacts_per_call} artifacts, {total_bytes_per_call} bytes in total. A \
             save past a limit is refused and stores nothing, and the refusal says which \
             limit it hit. Split large content into several artifacts to stay inside \
             them. Write `label` as one single line of at most {MAX_LABEL_BYTES} bytes."
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "one single line saying what this artifact holds; the \
                                    caller reads it beside the artifact's URI"
                },
                "content": {
                    "type": "string",
                    "description": "the content to save, written out in full"
                },
                "format": {
                    "type": "string",
                    "description": format!(
                        "a hint for the stored mime: {}; anything else, or nothing, \
                         stores as text",
                        format_names().join(" | ")
                    )
                }
            },
            "required": ["label", "content"]
        })
    }

    async fn call(
        &self,
        _ctx: &mut rig_agent::tool::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let digest = self
            .sink
            .save(&args.label, &args.content, args.format.as_deref())?;
        // Exactly this line for exactly these bytes, every time. A word about whether the
        // content was new would answer "is this already in the store?" for arbitrary bytes,
        // across every project this kaibo serves — see the module doc's existence-oracle note.
        // The coercion note depends only on the *request*, never on store state, so it
        // keeps that property.
        let (_, coerced) = resolve_format(args.format.as_deref());
        let note = match coerced {
            Some(asked) => format!(
                " (`format` {:?} is not a name kaibo knows, so it stored as text/plain — \
                 the formats are {})",
                asked,
                format_names().join(", ")
            ),
            None => String::new(),
        };
        Ok(format!(
            "Saved{note}. The caller will receive this artifact with your answer, at \
             {}{}. Name that URI in your answer and describe what the artifact covers.",
            crate::cas::CAS_URI_PREFIX,
            digest.to_hex()
        ))
    }
}

/// The caller-facing footer: what this call saved, appended to the consult answer.
/// Empty (and appends nothing) when the loop saved nothing, so a consult that never
/// reached for the tool reads byte-for-byte as it always did.
///
/// Mirrors `generate`'s per-artifact lines — same URI, same parenthetical, same
/// `path:` continuation in disk mode — because they are the same retrieval story and a
/// caller should not have to learn two renderings.
/// Append a call's artifact footer to its answer. `None` (no sink, so no
/// `save_artifact` was ever offered) and an unused sink both return `answer` untouched,
/// which is what keeps a consult that never saved anything byte-for-byte its old self.
pub fn with_artifacts(answer: String, sink: Option<&ArtifactSink>) -> String {
    match sink {
        Some(sink) => answer + &sink.footer(),
        None => answer,
    }
}

fn artifact_footer(saved: &[SavedArtifact], store: &MediaStore) -> String {
    if saved.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\n\n---\nSaved {} artifact{} (retrieve by reading the resource URI):",
        saved.len(),
        if saved.len() == 1 { "" } else { "s" }
    );
    for (i, a) in saved.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. {}{} ({}, {} bytes)\n   {}",
            i + 1,
            crate::cas::CAS_URI_PREFIX,
            a.digest,
            a.mime,
            a.bytes,
            a.label
        ));
        if let Some(path) = Digest::from_hex(&a.digest)
            .ok()
            .and_then(|d| store.path_for(&d))
        {
            out.push_str(&format!("\n   path: {}", path.display()));
        }
        if a.provenance_missing {
            out.push_str(
                "\n   note: kaibo could not write this artifact's housekeeping metadata \
                 beside it; the content itself is stored and readable at the URI above.",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::MemoryCas;

    fn sink() -> ArtifactSink {
        ArtifactSink::new(
            Arc::new(MediaStore::Memory(MemoryCas::new(None))),
            ArtifactAuthor {
                prompt: "generate a fuzz corpus".into(),
                model: "deepseek/deepseek-v4-pro".into(),
                cast: "deepseek".into(),
                slot: "synth",
                session: Some("s-1".into()),
            },
        )
    }

    /// The description renders LIVE cap values. A hand-copied number drifts the moment a
    /// constant moves, and a model that reads a stale cap pays the whole payload to find
    /// out — which is exactly why the caps are stated at all.
    #[test]
    fn the_description_states_the_live_caps() {
        let desc = SaveArtifact::new(Arc::new(sink())).description();
        for n in [
            MAX_ARTIFACT_BYTES.to_string(),
            MAX_ARTIFACTS_PER_CALL.to_string(),
            MAX_TOTAL_BYTES_PER_CALL.to_string(),
        ] {
            assert!(desc.contains(&n), "description must state {n}, got: {desc}");
        }
    }

    /// The schema names no filesystem location, source or destination — the property the
    /// whole design rests on. A future parameter that does must fail here first.
    #[test]
    fn the_schema_has_no_path_parameter_of_any_kind() {
        let params = SaveArtifact::new(Arc::new(sink())).parameters();
        let props = params["properties"].as_object().expect("an object schema");
        let mut names: Vec<&String> = props.keys().collect();
        names.sort();
        assert_eq!(
            names,
            vec!["content", "format", "label"],
            "save_artifact takes bytes, a label, and a format. Nothing else."
        );
    }
}
