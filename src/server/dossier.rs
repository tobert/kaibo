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
    match kept {
        Some(k) => format!(
            " The dossier it built is kept at {}{} ({} bytes) — read it with `read_cas`.",
            crate::cas::CAS_URI_PREFIX,
            k.digest,
            k.bytes
        ),
        None => String::new(),
    }
}

/// Append the kept dossier's address to a finished deliberation, so the answer carries
/// its own evidence trail. Mirrors [`crate::artifact::with_artifacts`]: no kept dossier
/// leaves the answer byte-for-byte its old self.
pub(crate) fn with_dossier(answer: String, kept: Option<&KeptDossier>) -> String {
    let Some(k) = kept else {
        return answer;
    };
    let mut out = format!(
        "{answer}\n\n---\nDossier: {}{} ({}, {} bytes) — the explorer's report this \
         deliberation reasoned over; read it with `read_cas`.",
        crate::cas::CAS_URI_PREFIX,
        k.digest,
        DOSSIER_EXT.mime(),
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

    /// Both surfaces name the URI a caller acts on, and the answer footer names the disk
    /// path when there is one — the same pair the artifact footer renders.
    #[test]
    fn both_surfaces_name_the_uri_the_caller_reads() {
        let kept = KeptDossier {
            digest: "ab".repeat(32),
            bytes: 4096,
            path: Some(PathBuf::from("/data/kaibo/cas/ab/abab.md")),
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
    }
}
