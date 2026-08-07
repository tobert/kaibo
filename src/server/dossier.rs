//! Keeping the dossier a `deliberate` call builds.
//!
//! `deliberate` runs an explorer sweep to build a dossier, then hands that dossier to an
//! offline synth. Until now the dossier was consumed invisibly: the caller paid for a
//! sweep that could run hundreds of thousands of tokens, waited minutes for it, and never
//! saw the evidence the synth actually reasoned over. Two things were impossible — to
//! **peek** (audit what reached the synth when its answer looks thin or wrong) and to
//! **reuse** (ask a second synth the same question over the same evidence, without paying
//! for the sweep twice).
//!
//! So the dossier lands in the media CAS like any other artifact, and the caller gets its
//! address. Amy, 2026-08-05: "I want to have the dossiers tossed into cas so we can peek
//! at them or reuse them sometimes."
//!
//! # What this is not
//!
//! It is not a new write path, not a new capability, and not a model-steered one:
//!
//! - The bytes reach disk through [`crate::cas::MediaStore::put`] — the blessed surface
//!   `generate` and `save_artifact` already use. `tests/no_write_path.rs` stays pinned.
//! - **kaibo writes this, not a model.** The dossier is kaibo's own byproduct of a call
//!   the operator made, so it rides the CAS's own switch (`[cas] enabled`) rather than
//!   `[artifacts] enabled` — that key exists for the one surface where a *model* decides
//!   bytes become durable ([`crate::artifact`]), and nothing here is model-decided. There
//!   is no `dossier` content a model chose, no label it wrote, and no per-call ask.
//! - **Keeping it never fails the call.** A refused or broken write is housekeeping lost,
//!   not evidence lost: the dossier is already in hand and the deliberation the caller is
//!   paying for proceeds. The refusal is loud on the operator's log and silent to the
//!   model team, exactly as `save_artifact`'s store errors are.
//!
//! The dossier is kept *before* stage 2 runs, so a deliberation that fails, times out, or
//! comes back thin still leaves the evidence behind to look at.

use std::path::PathBuf;
use std::sync::Arc;

use crate::cas::{Extension, MediaStore, Provenance};

/// The dossier is a cited prose report, so it is stored as markdown — that is what
/// `read_cas` will report its mime as, and what the sidecar records.
const DOSSIER_EXT: Extension = Extension::Md;

/// How much of the question rides in the sidecar `label`. The full question is already in
/// the sidecar's `prompt`; the label is the one line a hand-pruning pass reads, so it is a
/// short summary rather than a second copy.
const MAX_LABEL_BYTES: usize = 120;

/// How this call came by its dossier — the one thing the caller-facing lines say
/// differently, since "kept" and "reused" describe opposite spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// An explorer sweep built it on this call, and kaibo kept it.
    Built,
    /// The caller supplied its digest; no sweep ran.
    Reused,
}

/// A dossier kaibo kept: what the caller reads it back with.
///
/// `path` is `Some` only in disk mode — the same "read it with the tool, or find it on
/// disk" pair the artifact footer renders, because an operator holding a shell often
/// wants the file rather than a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeptDossier {
    pub digest: String,
    pub bytes: usize,
    pub path: Option<PathBuf>,
    pub origin: Origin,
}

/// Put a built dossier in the media CAS and describe where it landed.
///
/// `None` means the dossier was not kept — the CAS is off, or the store refused the
/// write. Both are non-events for the call in flight: see the module doc. Every refusal
/// is logged with its typed error, since the operator is the only audience who can act on
/// one.
pub(crate) fn keep_dossier(
    store: Option<&Arc<MediaStore>>,
    question: &str,
    dossier: &str,
    cast: &str,
    explorer_model: &str,
) -> Option<KeptDossier> {
    let store = store?;
    let bytes = dossier.as_bytes();
    let provenance = Provenance {
        prompt: question.to_string(),
        model: explorer_model.to_string(),
        cast: cast.to_string(),
        timestamp: crate::server::now_epoch_secs(),
        mime: DOSSIER_EXT.mime().to_string(),
        seed: None,
        // The producing tool and the slot that wrote the bytes — an explorer sweep, not a
        // synth's `save_artifact` and not a provider render. Whoever holds this digest can
        // tell the three apart without reading the content.
        tool: Some("deliberate".to_string()),
        slot: Some("explorer".to_string()),
        label: Some(label_for(question)),
        // `deliberate` carries no `session_id` — it is one question, not a conversation.
        session: None,
    };
    match store.put(bytes, DOSSIER_EXT, &provenance) {
        Ok(digest) => Some(KeptDossier {
            path: store.path_for(&digest),
            digest: digest.to_hex(),
            bytes: bytes.len(),
            origin: Origin::Built,
        }),
        // The bytes are durable; only the sidecar is missing. Still a kept dossier — the
        // digest names readable content, which is the whole point of handing it back.
        Err(crate::cas::CasError::ProvenanceNotRecorded { digest, cause }) => {
            tracing::warn!(
                digest = %digest,
                cause = %cause,
                "dossier stored without its provenance sidecar — the dossier is readable, \
                 the housekeeping record beside it is not"
            );
            let parsed = crate::cas::Digest::from_hex(&digest)
                .expect("the store renders its own digests in canonical hex");
            Some(KeptDossier {
                path: store.path_for(&parsed),
                digest,
                bytes: bytes.len(),
                origin: Origin::Built,
            })
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not keep this deliberate call's dossier — the deliberation \
                 proceeds, but the evidence it reasoned over will not be readable"
            );
            None
        }
    }
}

/// The refusal a `deliberate` call earns when it supplies a dossier AND explorer
/// arguments, or `None` when there is nothing wrong.
///
/// With a dossier supplied there is no sweep, so `attach`, the explorer overrides, and
/// `explorer_max_turns` have nothing to act on. Ignoring them would hand back an answer
/// that looks like it honored them, which is the quiet kind of wrong: the caller attached
/// a file, saw a deliberation, and has no way to learn the file never reached it.
///
/// A call that builds its own dossier can carry every one of them, so only a reuse call
/// has anything to answer for here. Checked before either front door resolves anything, so
/// a self-contradictory request costs nothing.
///
/// The argument names in the refusal are the MCP spellings; the CLI's own flags carry the
/// same words behind a `--`, so one sentence serves both readers.
pub(crate) fn inert_explorer_args(args: ExplorerArgs<'_>) -> Option<String> {
    args.dossier?;
    let inert: Vec<&str> = [
        (!args.attach.is_empty()).then_some("attach"),
        args.model.is_some().then_some("explorer_model"),
        args.backend.is_some().then_some("explorer_backend"),
        args.max_turns.is_some().then_some("explorer_max_turns"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if inert.is_empty() {
        return None;
    }
    Some(format!(
        "`dossier` reuses evidence that is already built, so no explorer runs on this call \
         and {} would do nothing. Drop {} to reuse the stored dossier, or drop `dossier` to \
         sweep for a fresh one.",
        inert
            .iter()
            .map(|a| format!("`{a}`"))
            .collect::<Vec<_>>()
            .join(", "),
        if inert.len() == 1 { "it" } else { "them" }
    ))
}

/// The explorer-phase arguments of one `deliberate` call, borrowed.
///
/// Both front doors take the same arguments under different names — MCP's
/// `DeliberateInput`, the CLI's flags — so [`inert_explorer_args`] reads them through this
/// view instead of existing twice and drifting apart. `dossier` rides along because it is
/// what makes the rest inert.
pub(crate) struct ExplorerArgs<'a> {
    pub dossier: Option<&'a str>,
    pub attach: &'a [String],
    pub model: Option<&'a str>,
    pub backend: Option<&'a str>,
    pub max_turns: Option<usize>,
}

/// Load a dossier the caller supplied by address, for a second synth to reason over.
///
/// This is the reuse half of keeping them: a sweep that cost hundreds of thousands of
/// tokens is worth asking twice over, and the second ask should cost only the synth. The
/// text comes back exactly as the store holds it — the same bytes the first synth read.
///
/// The caller is the operator's proxy, so any *textual* object in the store is fair
/// evidence here, not just something `deliberate` wrote: a report, a spec, a pasted
/// transcript a `save_artifact` call parked. What it will not do is hand an image or any
/// other binary to a text prompt, or invent a dossier when the address names nothing.
/// Every refusal says which of those happened.
///
/// `Err` is a sentence for the caller — the handler wraps it as invalid params.
pub(crate) fn load_dossier(
    store: Option<&Arc<MediaStore>>,
    reference: &str,
) -> Result<(String, KeptDossier), String> {
    let Some(store) = store else {
        return Err(
            "`dossier` names a stored dossier, but the media CAS is off ([cas] enabled = \
             false), so kaibo holds none. Re-enable the CAS and reconnect, or omit \
             `dossier` to build a fresh one."
                .to_string(),
        );
    };
    // The ack and the answer footer both render a `kaibo://cas/<digest>` URI, so the
    // caller most often has the URI in hand rather than the bare digest. Taking either is
    // not a fallback: the prefix is unambiguous, and refusing the exact string kaibo just
    // printed would be a puzzle, not a guardrail.
    let hex = reference
        .trim()
        .strip_prefix(crate::cas::CAS_URI_PREFIX)
        .unwrap_or(reference.trim());
    let digest = crate::cas::Digest::from_hex(hex).map_err(|e| {
        format!(
            "{e} — pass the digest kaibo handed back for a dossier, either bare or as its \
             full {}<digest> URI.",
            crate::cas::CAS_URI_PREFIX
        )
    })?;
    let (bytes, ext) = match store.get(&digest) {
        Ok(Some(found)) => found,
        Ok(None) => {
            return Err(format!(
                "no artifact with digest {hex} — it was never stored here, or (in memory \
                 mode) it did not survive a restart. Omit `dossier` to build a fresh one."
            ))
        }
        Err(e) => return Err(format!("{e}")),
    };
    if !ext.is_textual() {
        return Err(format!(
            "the artifact at {hex} is {}, and a dossier is the text an offline synth \
             reasons over. Pass the digest of a dossier or another text artifact.",
            ext.mime()
        ));
    }
    // Stored text is UTF-8 by construction on every path that writes it (a dossier is a
    // Rust `String`; `save_artifact` takes a JSON string). Say so plainly if some other
    // route ever produced text that is not, rather than deliberating over replacement
    // characters.
    let text = String::from_utf8(bytes).map_err(|_| {
        format!("the artifact at {hex} is not valid UTF-8, so it cannot be read as a dossier.")
    })?;
    let kept = KeptDossier {
        digest: hex.to_string(),
        bytes: text.len(),
        path: store.path_for(&digest),
        origin: Origin::Reused,
    };
    Ok((text, kept))
}

/// One single line of question text for the sidecar label, bounded and control-free.
fn label_for(question: &str) -> String {
    let mut flat = String::with_capacity(question.len());
    let mut spacing = false;
    for c in question.chars() {
        if c.is_control() || c == '\u{feff}' {
            spacing = true;
            continue;
        }
        if c.is_whitespace() {
            spacing = true;
            continue;
        }
        if spacing && !flat.is_empty() {
            flat.push(' ');
        }
        spacing = false;
        flat.push(c);
    }
    let flat = if flat.len() > MAX_LABEL_BYTES {
        let mut cut = MAX_LABEL_BYTES;
        while cut > 0 && !flat.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &flat[..cut])
    } else {
        flat
    };
    if flat.is_empty() {
        "deliberate dossier".to_string()
    } else {
        format!("deliberate dossier — {flat}")
    }
}

/// The clause both lane acks carry, naming where the dossier landed. Empty when nothing
/// was kept, so an ack on a CAS-off server reads exactly as it did before.
pub(crate) fn dossier_ack(kept: Option<&KeptDossier>) -> String {
    let Some(k) = kept else {
        return String::new();
    };
    let uri = format!("{}{}", crate::cas::CAS_URI_PREFIX, k.digest);
    match k.origin {
        Origin::Built => format!(
            " The dossier it built is kept at {uri} ({} bytes) — read it with `read_cas`, \
             or pass it back as `dossier` to ask another cast the same question without \
             re-exploring.",
            k.bytes
        ),
        Origin::Reused => format!(
            " It is reasoning over the dossier you passed, {uri} ({} bytes) — no sweep ran \
             on this call.",
            k.bytes
        ),
    }
}

/// Append the kept dossier's address to a finished deliberation, so the answer carries
/// its own evidence trail. Mirrors [`crate::artifact::with_artifacts`]: no kept dossier
/// leaves the answer byte-for-byte its old self.
pub(crate) fn with_dossier(answer: String, kept: Option<&KeptDossier>) -> String {
    let Some(k) = kept else {
        return answer;
    };
    let how = match k.origin {
        Origin::Built => "the explorer's report this deliberation reasoned over",
        Origin::Reused => "the dossier this deliberation reused; no sweep ran on this call",
    };
    let mut out = format!(
        "{answer}\n\n---\nDossier: {}{} ({} bytes) — {how}; read it with `read_cas`.",
        crate::cas::CAS_URI_PREFIX,
        k.digest,
        k.bytes
    );
    if let Some(path) = &k.path {
        out.push_str(&format!("\n   path: {}", path.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::{Digest, MemoryCas};

    fn store() -> Arc<MediaStore> {
        Arc::new(MediaStore::Memory(MemoryCas::new(None)))
    }

    /// The kept dossier is the dossier — byte-for-byte, at its own content address, with a
    /// sidecar that says which tool and which slot wrote it. The whole point is to audit
    /// what the synth received, so content that is not *exactly* what stage 2 gets would
    /// be worse than keeping nothing.
    #[test]
    fn the_dossier_is_kept_verbatim_at_its_content_address() {
        let store = store();
        let dossier = "src/consult/engine.rs:2064 the offline synth runs one turn\n";
        let kept = keep_dossier(
            Some(&store),
            "is the deliberate lane right?",
            dossier,
            "gemini-deliberate",
            "gemini/gemini-flash-lite-latest",
        )
        .expect("a live store keeps the dossier");

        assert_eq!(
            kept.digest,
            Digest::of_bytes(dossier.as_bytes()).to_hex(),
            "the address must be the dossier's own hash"
        );
        assert_eq!(kept.bytes, dossier.len());
        let digest = Digest::from_hex(&kept.digest).unwrap();
        let (bytes, ext) = store.get(&digest).expect("readable").expect("present");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            dossier,
            "the stored bytes must be the dossier verbatim"
        );
        assert_eq!(ext, Extension::Md, "a dossier is stored as markdown");

        let p = store.provenance(&digest).expect("a sidecar was written");
        assert_eq!(p.tool.as_deref(), Some("deliberate"));
        assert_eq!(p.slot.as_deref(), Some("explorer"));
        assert_eq!(p.cast, "gemini-deliberate");
        assert_eq!(p.model, "gemini/gemini-flash-lite-latest");
        assert_eq!(p.prompt, "is the deliberate lane right?");
        assert_eq!(p.session, None, "deliberate carries no session");
        assert!(
            p.label.as_deref().unwrap().contains("deliberate lane"),
            "the label names the question: {:?}",
            p.label
        );
    }

    /// A server with the CAS off deliberates exactly as it always did. Keeping is an
    /// addition to the call, never a precondition of it.
    #[test]
    fn a_cas_off_server_keeps_nothing_and_says_nothing() {
        assert_eq!(keep_dossier(None, "q", "dossier", "cast", "model"), None);
        assert_eq!(dossier_ack(None), "");
        assert_eq!(with_dossier("answer".into(), None), "answer");
    }

    /// A store that refuses the write loses the housekeeping, never the deliberation:
    /// `None` comes back and the caller carries on. This is the failure mode that must
    /// not turn a paid-for sweep into an error.
    #[test]
    fn a_refused_write_is_not_an_error_for_the_call() {
        // A capped store with no room for even a small dossier.
        let store = Arc::new(MediaStore::Memory(MemoryCas::new(Some(8))));
        let kept = keep_dossier(
            Some(&store),
            "q",
            "a dossier far larger than eight bytes",
            "cast",
            "model",
        );
        assert_eq!(
            kept, None,
            "a refused write keeps nothing, and does not panic"
        );
    }

    /// The sidecar label is one bounded line: a multi-line question cannot forge extra
    /// structure into it, and a long one is cut at a character boundary rather than
    /// panicking on a multi-byte split.
    #[test]
    fn the_label_is_one_bounded_line() {
        let label = label_for("first line\nsecond line\r\n\tthird");
        assert_eq!(label, "deliberate dossier — first line second line third");

        let long = label_for(&"あ".repeat(200));
        assert!(
            long.len() <= MAX_LABEL_BYTES + "deliberate dossier — ".len() + "…".len(),
            "the label is bounded: {} bytes",
            long.len()
        );
        assert!(long.ends_with('…'), "a cut label says it was cut: {long}");

        assert_eq!(
            label_for("   \n  "),
            "deliberate dossier",
            "a question with nothing quotable still labels the object"
        );
    }

    /// The round trip that makes reuse worth anything: what `keep_dossier` stored comes
    /// back byte-for-byte, so a second synth reasons over the same evidence the first one
    /// did rather than a re-rendering of it.
    #[test]
    fn a_kept_dossier_loads_back_verbatim_for_a_second_synth() {
        let store = store();
        let dossier = "src/x.rs:1 fn retry\nsrc/x.rs:9 the backoff is unbounded\n";
        let kept = keep_dossier(Some(&store), "is the retry safe?", dossier, "c", "m").unwrap();

        for reference in [
            kept.digest.clone(),
            format!("{}{}", crate::cas::CAS_URI_PREFIX, kept.digest),
            format!("  {}  ", kept.digest),
        ] {
            let (text, reused) = load_dossier(Some(&store), &reference)
                .unwrap_or_else(|e| panic!("loading {reference:?} must work: {e}"));
            assert_eq!(text, dossier, "the reused dossier is the stored one");
            assert_eq!(reused.digest, kept.digest);
            assert_eq!(
                reused.origin,
                Origin::Reused,
                "a loaded dossier is reused, not built — the two read differently"
            );
        }
    }

    /// Every way a supplied `dossier` can fail says which thing went wrong, and none of
    /// them invents a dossier or quietly falls back to sweeping.
    #[test]
    fn a_dossier_that_cannot_be_reused_says_which_thing_went_wrong() {
        let store = store();
        let cas_off = load_dossier(None, &"aa".repeat(32)).expect_err("no store, no reuse");
        assert!(cas_off.contains("[cas] enabled"), "{cas_off}");

        let bad = load_dossier(Some(&store), "not-a-digest").expect_err("garbage is refused");
        assert!(
            bad.contains("kaibo://cas/"),
            "the refusal shows the shape: {bad}"
        );

        let missing =
            load_dossier(Some(&store), &"aa".repeat(32)).expect_err("an absent object is refused");
        assert!(
            missing.contains("never stored here"),
            "an absent dossier is not a corrupt one: {missing}"
        );

        // A stored PNG is a real address, and emphatically not a dossier.
        let png = store
            .put(
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                Extension::Png,
                &Provenance {
                    prompt: "p".into(),
                    model: "m".into(),
                    cast: "c".into(),
                    timestamp: 0,
                    mime: "image/png".into(),
                    seed: None,
                    tool: Some("generate".into()),
                    slot: None,
                    label: None,
                    session: None,
                },
            )
            .unwrap();
        let binary = load_dossier(Some(&store), &png.to_hex()).expect_err("an image is refused");
        assert!(
            binary.contains("image/png"),
            "the refusal names what it actually is: {binary}"
        );
    }

    /// A reuse call that also carries explorer arguments is refused by name, never quietly
    /// stripped: `attach` is the dangerous one — a caller who attached a file and got an
    /// answer would have no way to learn the file never reached the synth.
    #[test]
    fn explorer_arguments_are_refused_on_a_reuse_call_by_name() {
        let digest = "aa".repeat(32);
        let none: Vec<String> = vec![];
        let one = vec!["src/x.rs".to_string()];
        let reuse = |attach: &'static [&'static str]| ExplorerArgs {
            dossier: Some(&digest),
            attach: if attach.is_empty() { &none } else { &one },
            model: None,
            backend: None,
            max_turns: None,
        };

        assert_eq!(
            inert_explorer_args(reuse(&[])),
            None,
            "a plain reuse call is fine"
        );

        let refusal = inert_explorer_args(reuse(&["src/x.rs"])).expect("attach is inert here");
        assert!(refusal.contains("`attach`"), "named: {refusal}");

        let refusal = inert_explorer_args(ExplorerArgs {
            model: Some("m"),
            max_turns: Some(10),
            ..reuse(&[])
        })
        .expect("both are inert");
        assert!(
            refusal.contains("`explorer_model`") && refusal.contains("`explorer_max_turns`"),
            "every inert argument is named, not just the first: {refusal}"
        );

        // A call that builds its own dossier carries every explorer argument happily — this
        // check must never touch the road it isn't about. (The synth overrides aren't here
        // at all: retargeting which model reads the evidence is what reuse is FOR, so they
        // are not part of this view.)
        assert_eq!(
            inert_explorer_args(ExplorerArgs {
                dossier: None,
                attach: &one,
                model: Some("m"),
                backend: Some("b"),
                max_turns: Some(10),
            }),
            None,
            "a sweeping call is what the explorer arguments are for"
        );
    }

    /// Both surfaces name the URI a caller acts on, and the answer footer names the disk
    /// path when there is one — the same pair the artifact footer renders.
    #[test]
    fn both_surfaces_name_the_uri_the_caller_reads() {
        let kept = KeptDossier {
            digest: "ab".repeat(32),
            bytes: 4096,
            path: Some(PathBuf::from("/data/kaibo/cas/ab/abab.md")),
            origin: Origin::Built,
        };
        let ack = dossier_ack(Some(&kept));
        assert!(
            ack.contains(&format!("kaibo://cas/{}", kept.digest)),
            "{ack}"
        );
        assert!(
            ack.contains("read_cas"),
            "the ack names the way to read it: {ack}"
        );

        let answer = with_dossier("DELIBERATION".into(), Some(&kept));
        assert!(
            answer.starts_with("DELIBERATION"),
            "the answer leads: {answer}"
        );
        assert!(
            answer.contains(&format!("kaibo://cas/{}", kept.digest)),
            "{answer}"
        );
        assert!(
            answer.contains("path: /data/kaibo/cas/ab/abab.md"),
            "disk mode names the file: {answer}"
        );

        // A reused dossier reads as reused on both surfaces — the caller must be able to
        // tell "kaibo swept for this" from "kaibo reasoned over what you handed it",
        // because only one of those cost an explorer.
        let reused = KeptDossier {
            origin: Origin::Reused,
            ..kept
        };
        assert!(
            dossier_ack(Some(&reused)).contains("no sweep ran"),
            "{}",
            dossier_ack(Some(&reused))
        );
        assert!(
            with_dossier("D".into(), Some(&reused)).contains("reused"),
            "{}",
            with_dossier("D".into(), Some(&reused))
        );
    }
}
