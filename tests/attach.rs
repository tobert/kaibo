//! Attachments: a tool-less call may name workspace files to inline as context, read
//! and containment-checked server-side so the bytes never transit the calling agent's
//! context. The boundary is the *same* allowed-set check every path obeys — these tests
//! are the teeth proving an attachment can't read outside the workspace, plus the vision
//! gate that refuses an image to a blind synth model.
//!
//! Written first (TDD): they exercise the real `resolve_attachments` containment path
//! and the real `batch_submit` vision gate, both offline (no provider key needed — the
//! gate fires before any network client is built).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use kaibo::attach::Attachment;
use kaibo::config::{Cast, Config, ModelRole, ModelSlot};
use kaibo::server::{BatchSubmitInput, KaiboHandler};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::tempdir;

/// A handler whose allowed set is just `root` (the default-root path, which also enters
/// the allowed set) — the single-workspace shape attachments are scoped to.
fn handler_rooted_at(root: &Path) -> KaiboHandler {
    let mut config = Config::builtin();
    config.root = Some(root.to_path_buf());
    config.allow_paths = vec![];
    KaiboHandler::new(config).expect("handler builds")
}

/// A minimal blob with a real PNG signature — enough to exercise sniff + base64 without
/// decoding a real image.
fn fake_png() -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend(std::iter::repeat_n(0xAB, 32));
    v
}

// --- happy paths: a workspace file inlines ----------------------------------

/// A UTF-8 file inside the workspace resolves to a Text attachment carrying its body.
#[tokio::test]
async fn workspace_text_file_inlines_as_text() {
    let root = tempdir().unwrap();
    let file = root.path().join("README.md");
    fs::write(&file, "# kaibo\nhello\n").unwrap();
    let handler = handler_rooted_at(root.path());

    let atts = handler
        .resolve_attachments(&[file.to_string_lossy().to_string()])
        .await
        .expect("an in-workspace text file must inline");
    assert_eq!(atts.len(), 1);
    match &atts[0] {
        Attachment::Text { body, .. } => assert!(body.contains("hello"), "body carried: {body}"),
        other => panic!("expected Text, got {other:?}"),
    }
}

/// A workspace image resolves to an Image attachment with the sniffed mime.
#[tokio::test]
async fn workspace_image_file_inlines_as_image() {
    let root = tempdir().unwrap();
    let file = root.path().join("logo.png");
    fs::write(&file, fake_png()).unwrap();
    let handler = handler_rooted_at(root.path());

    let atts = handler
        .resolve_attachments(&[file.to_string_lossy().to_string()])
        .await
        .expect("an in-workspace image must inline");
    match &atts[0] {
        Attachment::Image { mime, .. } => assert_eq!(*mime, "image/png"),
        other => panic!("expected Image, got {other:?}"),
    }
}

// --- teeth: the boundary holds ----------------------------------------------

/// An attachment whose path resolves *outside* the allowed set is refused, naming the
/// boundary and a widening knob — the same rejection a session root gets.
#[tokio::test]
async fn attachment_outside_allowed_set_is_rejected() {
    let allowed = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "sensitive\n").unwrap();
    let handler = handler_rooted_at(allowed.path());

    let err = handler
        .resolve_attachments(&[secret.to_string_lossy().to_string()])
        .await
        .expect_err("a file outside the workspace must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("allowed") || msg.contains("--allow-path"),
        "rejection must name the boundary, got: {msg}"
    );
}

/// A symlink *inside* the workspace whose target is outside is refused: `canonicalize`
/// resolves the link, so the containment check sees the real outside path. This proves
/// the boundary is canonicalize-based, not string-prefix-based — the read-escape teeth.
#[tokio::test]
async fn attachment_symlink_escaping_workspace_is_rejected() {
    let allowed = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "outside\n").unwrap();
    let link = allowed.path().join("link.txt");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let handler = handler_rooted_at(allowed.path());

    let err = handler
        .resolve_attachments(&[link.to_string_lossy().to_string()])
        .await
        .expect_err("a symlink whose target is outside must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("allowed") || msg.contains("--allow-path"),
        "rejection must name the boundary, got: {msg}"
    );
}

/// A directory is not a regular file — refused, since attachments inline a file's bytes
/// rather than mount a tree (the mirror image of `resolve_root`'s dir requirement).
#[tokio::test]
async fn attachment_directory_is_rejected() {
    let root = tempdir().unwrap();
    let sub = root.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let handler = handler_rooted_at(root.path());

    let err = handler
        .resolve_attachments(&[sub.to_string_lossy().to_string()])
        .await
        .expect_err("a directory must be refused");
    assert!(
        format!("{err:?}").contains("not a regular file"),
        "rejection must explain the file requirement, got: {err:?}"
    );
}

/// A binary file that is neither valid UTF-8 nor a recognized image is refused rather
/// than inlined as mojibake — crash over corruption, even from inside the workspace.
#[tokio::test]
async fn attachment_binary_inside_workspace_is_refused() {
    let root = tempdir().unwrap();
    let file = root.path().join("mystery.bin");
    fs::write(&file, [0x00, 0xFF, 0xFE, 0xFD]).unwrap();
    let handler = handler_rooted_at(root.path());

    let err = handler
        .resolve_attachments(&[file.to_string_lossy().to_string()])
        .await
        .expect_err("non-text non-image binary must be refused");
    assert!(
        format!("{err:?}").contains("neither valid UTF-8"),
        "rejection must explain the encoding gap, got: {err:?}"
    );
}

// --- the VFS read path: per-tree worker rooting --------------------------------

/// An attachment in a *second* allowed tree (not the default root) reads correctly. This
/// exercises the new read path's tree selection: `resolve_attachments` reads through a
/// kaish worker rooted at the attachment's *containing* allowed tree, so picking the right
/// one is load-bearing — a worker rooted at the wrong tree would fail the VFS read
/// (the file sits outside its mount). Proves the read goes through the VFS, mounted right.
#[tokio::test]
async fn attachment_in_a_second_allowed_tree_reads_through_its_own_worker() {
    let root = tempdir().unwrap();
    let other = tempdir().unwrap();
    let file = other.path().join("notes.md");
    fs::write(&file, "# notes\nsecond-tree-body\n").unwrap();

    // Two allowed trees: the default root and a second --allow-path.
    let mut config = Config::builtin();
    config.root = Some(root.path().to_path_buf());
    config.allow_paths = vec![other.path().to_path_buf()];
    let handler = KaiboHandler::new(config).expect("handler builds");

    let atts = handler
        .resolve_attachments(&[file.to_string_lossy().to_string()])
        .await
        .expect("a file in a second allowed tree must inline");
    match &atts[0] {
        Attachment::Text { body, .. } => assert!(
            body.contains("second-tree-body"),
            "body read through the second tree's worker: {body}"
        ),
        other => panic!("expected Text, got {other:?}"),
    }
}

// --- vision gate: an image to a blind synth is refused offline ----------------

/// `batch_submit` with an image attachment on a synth model pinned vision-blind is
/// refused before any provider client is built — so the misconfig is reported with no
/// key and no network. Proves images are gated on the model's actual vision capability.
#[tokio::test]
async fn image_attachment_to_blind_synth_is_refused() {
    let root = tempdir().unwrap();
    let png = root.path().join("shot.png");
    fs::write(&png, fake_png()).unwrap();

    // A cast whose synth slot is pinned vision-blind on a (batch-capable) anthropic
    // backend. The vision pin forces the gate regardless of the model's real caps.
    let mut config = Config::builtin();
    config.root = Some(root.path().to_path_buf());
    let mut slot = ModelSlot::bare("anthropic", "claude-sonnet-4-6");
    slot.vision = Some(false);
    let mut slots = BTreeMap::new();
    slots.insert(ModelRole::Synth, slot);
    config.casts.insert(
        "blindcast".to_string(),
        Cast {
            name: "blindcast".to_string(),
            slots,
        },
    );
    let handler = KaiboHandler::new(config).expect("handler builds");

    let err = handler
        .batch_submit(Parameters(BatchSubmitInput {
            prompts: vec!["describe this".to_string()],
            attach: vec![png.to_string_lossy().to_string()],
            cast: Some("blindcast".to_string()),
            model: None,
            backend: None,
        }))
        .await
        .expect_err("an image to a vision-blind synth must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("image") && msg.to_lowercase().contains("vision"),
        "refusal must name the image/vision mismatch, got: {msg}"
    );
}

// --- the shared vision gate (batch + oneshot use it identically) -------------

/// `gate_image_attachments` — the one gate both `batch` and `oneshot` call — refuses an
/// image to a vision-blind model, passes text-only and the no-attachment case, and lets
/// an image through to a vision-capable model. This pins the shared rule directly (the
/// `oneshot` handler needs a `Peer` to drive end-to-end, so the gate is tested here).
#[test]
fn shared_vision_gate_refuses_image_to_blind_model_only() {
    let handler = handler_rooted_at(tempdir().unwrap().path());
    let text = Attachment::Text {
        path: "a.txt".into(),
        body: "x".into(),
    };
    let image = Attachment::Image {
        path: "a.png".into(),
        mime: "image/png",
        data_b64: "QUJD".into(),
    };

    // Blind model + image → refused, naming the model, the cast, and the mismatch.
    let err = handler
        .gate_image_attachments(
            false,
            std::slice::from_ref(&image),
            "some-model",
            "blindcast",
        )
        .expect_err("an image to a blind model must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("some-model") && msg.contains("blindcast"),
        "names model + cast: {msg}"
    );
    assert!(
        msg.contains("image") && msg.to_lowercase().contains("vision"),
        "names the mismatch: {msg}"
    );

    // Blind model + text-only → allowed (text needs no vision).
    handler
        .gate_image_attachments(false, std::slice::from_ref(&text), "some-model", "c")
        .expect("text-only attachments need no vision");
    // Blind model + nothing → allowed.
    handler
        .gate_image_attachments(false, &[], "some-model", "c")
        .expect("no attachments, no gate");
    // Vision model + image → allowed.
    handler
        .gate_image_attachments(true, &[image], "vlm", "c")
        .expect("a vision model accepts an image");
}
