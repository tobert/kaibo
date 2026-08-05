//! Behavioral tests for `save_artifact`'s ledger — the caps, the refusals, the
//! authorship record, and the one property that is a security boundary rather than a
//! budget: **dedup opacity**.
//!
//! The loop wiring (which toolsets carry the tool, which never do) is tested where the
//! toolsets are built — `src/consult/engine.rs` — and the two-key gate is tested at the
//! server seam that resolves it. This file is about what happens once bytes arrive.

use std::sync::Arc;

use kaibo::artifact::{
    ArtifactAuthor, ArtifactSink, Caps, MAX_ARTIFACTS_PER_CALL, MAX_ARTIFACT_BYTES,
    MAX_TOTAL_BYTES_PER_CALL,
};
use kaibo::cas::{Cas, Digest, Extension, MediaStore, MemoryCas};
use tempfile::TempDir;

fn author() -> ArtifactAuthor {
    ArtifactAuthor {
        prompt: "generate 100 kaish commands that try to break the parser".into(),
        model: "deepseek/deepseek-v4-pro".into(),
        cast: "deepseek".into(),
        slot: "synth",
        session: Some("sess-42".into()),
    }
}

/// A sink over an in-memory store — enough for every ledger property, and it touches no
/// filesystem.
fn sink() -> ArtifactSink {
    ArtifactSink::new(Arc::new(MediaStore::Memory(MemoryCas::new(None))), author())
}

/// A sink over a real disk CAS, plus the temp dir keeping it alive — for the tests that
/// read a sidecar back off disk.
fn disk_sink() -> (ArtifactSink, TempDir) {
    let dir = TempDir::new().unwrap();
    let cas = Cas::open(&dir.path().join("cas"), &[], None).expect("open cas");
    (
        ArtifactSink::new(Arc::new(MediaStore::Disk(cas)), author()),
        dir,
    )
}

// --- Caps: refuse, never truncate, and store nothing on refusal ---------------

/// Past the per-artifact cap the write is REFUSED, not truncated — and the refusal
/// carries the cap, the actual size, and the way out. A model that only learns "too
/// big" has to guess the next size and pay for the whole payload again to find out.
#[test]
fn an_oversize_artifact_is_refused_with_the_cap_the_size_and_the_recovery() {
    let s = sink();
    let big = "x".repeat(MAX_ARTIFACT_BYTES + 1);
    let err = s
        .save("a corpus", &big, Some("text"))
        .expect_err("past the per-artifact cap must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains(&MAX_ARTIFACT_BYTES.to_string()),
        "the refusal must name the cap, got: {msg}"
    );
    assert!(
        msg.contains(&(MAX_ARTIFACT_BYTES + 1).to_string()),
        "the refusal must name the actual size, got: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("split"),
        "the refusal must name the recovery, got: {msg}"
    );
    assert!(
        s.saved().is_empty(),
        "a refused save must store nothing and record nothing"
    );
}

/// Truncation is the failure this refusal exists to prevent: a digest handed back for
/// content that is not what the model wrote is silent corruption. Nothing at the cap
/// boundary gets shortened — one byte under is stored whole, one byte over stores
/// nothing at all.
#[test]
fn the_cap_boundary_stores_whole_content_or_nothing() {
    let s = sink();
    let ok = "y".repeat(MAX_ARTIFACT_BYTES);
    let digest = s
        .save("at the limit", &ok, Some("text"))
        .expect("at the cap");
    assert_eq!(
        digest,
        Digest::of_bytes(ok.as_bytes()),
        "the digest must address the WHOLE content, never a truncation of it"
    );
    assert_eq!(s.saved()[0].bytes, MAX_ARTIFACT_BYTES);
}

/// The artifact count is per MCP CALL, so a driver cannot spread a flood across turns:
/// one sink lives for one call, and the ninth save is refused however it arrived.
#[test]
fn past_the_per_call_artifact_count_the_save_is_refused() {
    let s = sink();
    for i in 0..MAX_ARTIFACTS_PER_CALL {
        s.save(&format!("part {i}"), &format!("body {i}"), None)
            .unwrap_or_else(|e| panic!("save {i} within the count must succeed: {e}"));
    }
    let err = s
        .save("one too many", "body", None)
        .expect_err("past the count must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains(&MAX_ARTIFACTS_PER_CALL.to_string()),
        "the refusal must name the cap, got: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("follow-up call"),
        "the refusal must name the recovery, got: {msg}"
    );
    assert_eq!(
        s.saved().len(),
        MAX_ARTIFACTS_PER_CALL,
        "the refused save must not enter the ledger"
    );
}

/// The per-call byte total is its own ceiling, independent of the count and of the
/// per-artifact cap: several artifacts each under the single-artifact limit can still be
/// too much in aggregate. Driven on a small-caps sink, which is the only way to reach
/// this branch without allocating megabytes — and, with the shipped numbers, the only
/// way to reach it at all (see the relationship test below).
#[test]
fn past_the_per_call_byte_total_the_save_is_refused() {
    let s = ArtifactSink::with_caps(
        Arc::new(MediaStore::Memory(MemoryCas::new(None))),
        author(),
        Caps {
            per_artifact_bytes: 100,
            artifacts_per_call: 8,
            total_bytes_per_call: 150,
        },
    );
    s.save("part 1", &"a".repeat(100), None)
        .expect("first fits");
    let msg = match s.save("part 2", &"b".repeat(100), None) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("100 + 100 is past a 150-byte call total and must refuse"),
    };
    assert!(msg.contains("150"), "the refusal must name the cap: {msg}");
    assert!(
        msg.contains("100"),
        "the refusal must name what was spent and what was asked: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("smaller selection"),
        "the refusal must name the recovery, got: {msg}"
    );
    assert_eq!(
        s.saved().len(),
        1,
        "the refused save must add nothing to the ledger"
    );
}

/// **The shipped total cap can never fire, and that is worth stating out loud.**
///
/// `8 artifacts × 1 MiB each` is exactly the 8 MiB call total, and the check refuses only
/// when the total is *exceeded* — so with today's numbers the count and per-artifact caps
/// bind first, always. The total is a backstop that goes live the moment either of the
/// other two rises. Pinned here so a later change to any of the three has to look at the
/// relationship deliberately rather than discovering it as a surprise.
#[test]
fn the_shipped_caps_leave_the_call_total_as_a_backstop() {
    assert_eq!(
        MAX_ARTIFACTS_PER_CALL * MAX_ARTIFACT_BYTES,
        MAX_TOTAL_BYTES_PER_CALL,
        "if this stops holding, the call total starts refusing real saves — re-read \
         `past_the_per_call_byte_total_the_save_is_refused` and decide whether that is \
         what you meant"
    );
}

// --- The format allowlist -----------------------------------------------------

#[test]
fn text_and_jsonl_are_stored_under_their_own_container_and_mime() {
    let (s, _dir) = disk_sink();
    let text = s.save("a report", "hello\n", Some("text")).unwrap();
    let jsonl = s.save("rows", "{\"a\":1}\n", Some("jsonl")).unwrap();
    let saved = s.saved();
    assert_eq!(saved[0].mime, Extension::Txt.mime());
    assert_eq!(saved[1].mime, Extension::Jsonl.mime());
    assert_eq!(saved[0].digest, text.to_hex());
    assert_eq!(saved[1].digest, jsonl.to_hex());
}

/// Omitting `format` means text — the common case, and a default the model does not
/// have to think about.
#[test]
fn an_omitted_format_stores_text() {
    let s = sink();
    s.save("a report", "hello\n", None).unwrap();
    assert_eq!(s.saved()[0].mime, Extension::Txt.mime());
}

/// A format outside the allowlist is refused loudly, naming what IS available, and
/// stores nothing. The allowlist is a serving concern rather than a safety boundary
/// (a model can put anything inside a `.txt`), but a silently-coerced format would
/// mislabel the bytes the caller retrieves.
#[test]
fn a_format_outside_the_allowlist_is_refused_and_stores_nothing() {
    let s = sink();
    for asked in ["png", "sh", "application/json", "exe"] {
        let msg = match s.save("payload", "body", Some(asked)) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("format {asked:?} must be refused"),
        };
        assert!(msg.contains(asked), "the refusal must echo the ask: {msg}");
        assert!(
            msg.contains("text") && msg.contains("jsonl"),
            "the refusal must name what IS available, got: {msg}"
        );
    }
    assert!(s.saved().is_empty(), "nothing was stored");
}

/// A blank label is refused: the caller reads the label beside the URI, so an artifact
/// with no description is one the caller cannot act on.
#[test]
fn a_blank_label_is_refused() {
    let s = sink();
    assert!(s.save("   ", "body", None).is_err());
    assert!(s.saved().is_empty());
}

// --- Dedup opacity: the one property here that is a boundary ------------------

/// **Storing the same content twice must be indistinguishable from storing it once.**
///
/// The CAS spans every project this kaibo has ever served. A tool result that said
/// "already present" would answer "is this content in the store?" for arbitrary bytes —
/// an existence oracle over other projects' artifacts, handed to a model whose prompt an
/// untrusted repository can influence. So the observable output is a pure function of
/// the content: same bytes in, byte-identical result out.
#[test]
fn saving_identical_content_twice_yields_byte_identical_results() {
    let (s, _dir) = disk_sink();
    let body = "the same corpus, twice\n";
    let first = s.save("corpus", body, Some("text")).expect("first save");
    let second = s.save("corpus", body, Some("text")).expect("second save");
    assert_eq!(
        first.to_hex(),
        second.to_hex(),
        "the address is the content, so both saves address the same object"
    );

    // And the ledger entries — what the caller's footer is rendered from — agree
    // field for field. A "new" vs "deduplicated" marker anywhere in this record would
    // leak straight into the footer.
    let saved = s.saved();
    assert_eq!(saved[0], saved[1]);
}

/// The same, one level up: the *tool result text* a model reads is identical for
/// identical content. This is where a leak would actually reach the model, so it gets
/// its own assertion rather than riding on the ledger's.
#[tokio::test]
async fn the_tool_result_text_is_identical_for_identical_content() {
    use kaibo::artifact::{SaveArtifact, SaveArtifactArgs};
    use rig_agent::tool::{Tool, ToolContext};

    let (s, _dir) = disk_sink();
    let tool = SaveArtifact::new(Arc::new(s));
    let args = || SaveArtifactArgs {
        label: "corpus".into(),
        content: "the same corpus, twice\n".into(),
        format: Some("text".into()),
    };
    let mut ctx = ToolContext::default();
    let first = tool.call(&mut ctx, args()).await.expect("first save");
    let second = tool.call(&mut ctx, args()).await.expect("second save");
    assert_eq!(
        first, second,
        "a model must not be able to tell a new object from one already in the store"
    );
    assert!(
        !first.to_lowercase().contains("already")
            && !first.to_lowercase().contains("dedup")
            && !first.to_lowercase().contains("new"),
        "the result says nothing about the object's prior existence, got: {first}"
    );
}

// --- Authorship: the sidecar is the trust record ------------------------------

/// Somebody downstream will *execute* an artifact's contents (a corpus of shell
/// commands is the motivating case). The sidecar is what they decide trust from, so a
/// model-authored artifact must be distinguishable from a provider-rendered one, and
/// must name who wrote it.
#[test]
fn a_saved_artifact_records_its_authorship_in_the_sidecar() {
    let (s, dir) = disk_sink();
    let digest = s
        .save("100 kaish commands", "cat /etc/passwd\n", Some("text"))
        .expect("save");

    let cas = Cas::open(&dir.path().join("cas"), &[], None).expect("reopen the same store");
    let p = cas
        .provenance_for(&digest)
        .expect("every saved artifact carries a sidecar");

    assert_eq!(
        p.tool.as_deref(),
        Some("save_artifact"),
        "the tool that recorded it is what separates authored bytes from a render"
    );
    assert_eq!(p.slot.as_deref(), Some("synth"));
    assert_eq!(p.label.as_deref(), Some("100 kaish commands"));
    assert_eq!(p.session.as_deref(), Some("sess-42"));
    assert_eq!(p.cast, "deepseek");
    assert_eq!(p.model, "deepseek/deepseek-v4-pro");
    assert_eq!(
        p.prompt,
        "generate 100 kaish commands that try to break the parser"
    );
    assert_eq!(p.mime, Extension::Txt.mime());
    assert_eq!(p.seed, None, "authored text has no provider seed");
}

/// A sidecar written before the authorship fields existed still deserializes — the CAS
/// never rewrites, so every past sidecar must stay readable forever.
#[test]
fn a_sidecar_written_before_the_authorship_fields_still_deserializes() {
    let old = r#"{"prompt":"a cat","model":"m","cast":"c","timestamp":1,"mime":"image/png"}"#;
    let p: kaibo::cas::Provenance = serde_json::from_str(old).expect("an older sidecar must load");
    assert_eq!(
        p.tool, None,
        "absent means `generate`, the only producer then"
    );
    assert_eq!(p.slot, None);
    assert_eq!(p.label, None);
    assert_eq!(p.session, None);
}

/// The authorship fields are written only when they apply, so a sidecar carries no
/// null-filled lines about things it has nothing to say about. The stored JSON is the
/// artifact's permanent record; keeping it minimal is what keeps a `jq`-based pruning
/// pass legible a decade from now.
#[test]
fn absent_authorship_fields_are_omitted_from_the_stored_json() {
    let rendered = serde_json::to_string(&kaibo::cas::Provenance {
        prompt: "a cat".into(),
        model: "m".into(),
        cast: "c".into(),
        timestamp: 1,
        mime: "image/png".into(),
        seed: None,
        tool: Some("generate".into()),
        slot: None,
        label: None,
        session: None,
    })
    .expect("serialize");
    assert!(rendered.contains("\"tool\":\"generate\""));
    for absent in ["slot", "label", "session"] {
        assert!(
            !rendered.contains(absent),
            "an inapplicable field must not appear at all, found {absent} in {rendered}"
        );
    }
}

// --- The caller's footer -------------------------------------------------------

/// A consult that saved nothing appends nothing — the pre-artifact answer, byte for
/// byte.
#[test]
fn an_unused_sink_appends_no_footer() {
    let s = sink();
    assert_eq!(
        kaibo::artifact::with_artifacts("ANSWER".into(), Some(&s)),
        "ANSWER"
    );
    assert_eq!(
        kaibo::artifact::with_artifacts("ANSWER".into(), None),
        "ANSWER"
    );
}

/// The footer names each artifact by the URI the caller reads it at, with the mime, the
/// size, and the model's own label — everything needed to decide whether to retrieve it.
#[test]
fn the_footer_names_every_artifact_by_its_resource_uri() {
    let (s, _dir) = disk_sink();
    let a = s.save("the corpus", "one\ntwo\n", Some("text")).unwrap();
    let b = s.save("the rows", "{\"a\":1}\n", Some("jsonl")).unwrap();

    let out = kaibo::artifact::with_artifacts("ANSWER".into(), Some(&s));
    assert!(out.starts_with("ANSWER"), "the answer stays first");
    for digest in [a, b] {
        assert!(
            out.contains(&format!("kaibo://cas/{}", digest.to_hex())),
            "the footer must name {digest:?} by its resource URI, got: {out}"
        );
    }
    assert!(out.contains("the corpus") && out.contains("the rows"));
    assert!(out.contains(Extension::Txt.mime()) && out.contains(Extension::Jsonl.mime()));
    assert!(
        out.contains("path: "),
        "disk mode names the real path, as `generate` does, got: {out}"
    );
}
