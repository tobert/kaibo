//! Behavioral tests for the media lane's binary-input seam ([`kaibo::media`]).
//!
//! What these pin:
//!
//! - **Inputs are named and plural.** Stability's operations take `image`+`mask`,
//!   `init_image`+`style_image`, and in one case three parts — so the field name is
//!   data, not a constant, and one request carries several.
//! - **The store names each part's format, not the caller.** A part's filename
//!   extension comes from `MediaStore::extension_for`, which is what makes it
//!   impossible to label a part as something the object is not.
//! - **A provider with no route for binary input refuses instead of dropping it.**
//!   Dropping is the dangerous direction: the caller asked to edit *this* picture, and a
//!   dropped input returns an unrelated image that looks exactly like a success — with a
//!   digest and a provenance sidecar to make it convincing.
//! - **Every resolution failure happens before a request is built**, so a bad digest
//!   never costs a provider call.
//!
//! Teeth: make `refuse_binary_inputs` return `Ok(())` unconditionally and
//! `a_provider_without_an_input_route_refuses_rather_than_dropping` fails; have
//! `resolve_inputs` take the caller's word for the extension and
//! `the_store_names_the_parts_format_not_the_caller` fails.

use std::collections::BTreeMap;

use kaibo::cas::{Cas, Digest, Extension, MediaStore, Provenance};
use kaibo::media::{
    refuse_binary_inputs, resolve_inputs, MediaArm, MediaInput, MediaJobId, MediaModel,
    MediaOutcome, MediaPollOutcome, MediaRequest,
};
use tempfile::TempDir;

fn store() -> (MediaStore, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let cas = Cas::open(&dir.path().join("cas"), &[], None).expect("cas opens");
    (MediaStore::Disk(cas), dir)
}

fn prov(mime: &str) -> Provenance {
    Provenance {
        prompt: String::new(),
        model: String::new(),
        cast: String::new(),
        timestamp: 1_753_000_000,
        mime: mime.to_string(),
        seed: None,
        tool: Some("write_cas".into()),
        slot: None,
        label: None,
        session: None,
    }
}

/// Store `bytes` under `ext` and return its digest hex.
fn put(store: &MediaStore, bytes: &[u8], ext: Extension) -> String {
    store
        .put(bytes, ext, &prov(ext.mime()))
        .expect("put succeeds")
        .to_hex()
}

fn asked(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn request_with(inputs: Vec<MediaInput>) -> MediaRequest {
    MediaRequest {
        prompt: "erase the sign".into(),
        fields: Vec::new(),
        inputs,
        op: None,
    }
}

/// One operation, several named parts — the shape `edit/inpaint` (`image` + `mask`) and
/// `control/style-transfer` (`init_image` + `style_image`) actually need, and the whole
/// reason this is a list rather than one optional blob.
#[test]
fn several_named_parts_resolve_together() {
    let (store, _d) = store();
    let image = put(&store, b"\x89PNG\r\n\x1a\nphoto", Extension::Png);
    let mask = put(&store, b"\x89PNG\r\n\x1a\nmask", Extension::Png);

    let resolved = resolve_inputs(&store, &asked(&[("image", &image), ("mask", &mask)]))
        .expect("both parts resolve");

    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].field, "image");
    assert_eq!(resolved[0].filename, "image.png");
    assert_eq!(resolved[0].bytes, b"\x89PNG\r\n\x1a\nphoto");
    assert_eq!(resolved[1].field, "mask");
    assert_eq!(resolved[1].filename, "mask.png");
}

/// The filename's extension is the *store's* answer for what the object is, never the
/// caller's belief. The two differ whenever identical content was first stored under
/// another container, and a part labelled with the wrong one is a lie the provider acts
/// on.
#[test]
fn the_store_names_the_parts_format_not_the_caller() {
    let (store, _d) = store();
    let jpeg = put(&store, b"\xff\xd8\xffphoto", Extension::Jpeg);

    let resolved = resolve_inputs(&store, &asked(&[("image", &jpeg)])).expect("resolves");
    assert_eq!(
        resolved[0].filename, "image.jpeg",
        "the extension comes from the store's record, not from the field name"
    );
}

#[test]
fn a_digest_shaped_string_that_is_not_one_is_refused_by_name() {
    let (store, _d) = store();
    let err = resolve_inputs(&store, &asked(&[("image", "not-a-digest")]))
        .expect_err("not 64 hex characters");
    let msg = format!("{err:#}");
    assert!(msg.contains("inputs.image"), "names the field: {msg}");
    assert!(
        msg.contains("64 lowercase hex"),
        "says what a digest is: {msg}"
    );
}

/// A well-formed digest this store does not hold. The refusal has to say how to get one
/// in, because the caller's next move is `write_cas`, not a retry.
#[test]
fn a_digest_the_store_does_not_hold_is_refused_with_the_way_in() {
    let (store, _d) = store();
    let absent = Digest::of_bytes(b"never stored").to_hex();
    let err = resolve_inputs(&store, &asked(&[("image", &absent)])).expect_err("absent");
    let msg = format!("{err:#}");
    assert!(msg.contains("holds no object"), "{msg}");
    assert!(msg.contains("write_cas"), "names the way in: {msg}");
}

/// The store also names the text formats `save_artifact` writes. An input part is an
/// image, and keeping the refusal keyed to that is what stops a widened `Extension` from
/// quietly widening what the media lane accepts.
#[test]
fn a_stored_object_that_is_not_an_image_is_refused_as_an_input() {
    let (store, _d) = store();
    let text = put(&store, b"a corpus of shell commands", Extension::Txt);
    let err = resolve_inputs(&store, &asked(&[("image", &text)])).expect_err("not an image");
    let msg = format!("{err:#}");
    assert!(msg.contains("not an image"), "{msg}");
    assert!(
        msg.contains("text/plain"),
        "names what it actually is: {msg}"
    );
}

/// **The dangerous direction, pinned.** A provider with no input-image route must refuse.
/// Dropping the input would send the prompt alone, return a plausible unrelated image,
/// and store it with a digest and a sidecar — a corrupt result wearing a success's
/// clothes.
#[test]
fn a_provider_without_an_input_route_refuses_rather_than_dropping() {
    let request = request_with(vec![
        MediaInput::new("image", Extension::Png, b"photo".to_vec()),
        MediaInput::new("mask", Extension::Png, b"mask".to_vec()),
    ]);
    let err = refuse_binary_inputs(&request, "dashscope/wan2.2").expect_err("no route for inputs");
    let msg = format!("{err:#}");
    assert!(msg.contains("dashscope/wan2.2"), "names the backend: {msg}");
    assert!(
        msg.contains("image") && msg.contains("mask"),
        "names every part it refused: {msg}"
    );
    assert!(
        msg.contains("nothing was generated"),
        "says no work was done, so the caller knows it was not billed: {msg}"
    );
}

/// The same guard must not fire on an ordinary text-to-image call — otherwise it would
/// break every provider that has no input route and never needed one.
#[test]
fn a_request_with_no_binary_parts_passes_the_guard() {
    assert!(refuse_binary_inputs(&request_with(Vec::new()), "dashscope/wan2.2").is_ok());
}

/// `MediaInput::new` builds the wire filename from the field name and the store's
/// extension. A part without a `filename=` is rejected or mis-typed by some multipart
/// servers, and every route that takes one of these is a file upload.
#[test]
fn every_part_carries_a_filename_with_the_stores_extension() {
    let part = MediaInput::new("subject_image", Extension::Webp, b"bytes".to_vec());
    assert_eq!(part.field, "subject_image");
    assert_eq!(part.filename, "subject_image.webp");
    assert_eq!(
        part.mime, "image/webp",
        "a part with no mime defaults to application/octet-stream on the wire, asking the \
         provider to infer what the store already knows"
    );
}

/// A provider that never thinks about `inputs` — the shape of a `MediaModel` added
/// later by someone who has not read this file.
struct ForgetfulProvider;

#[async_trait::async_trait]
impl MediaModel for ForgetfulProvider {
    async fn generate(&self, _request: &MediaRequest) -> anyhow::Result<MediaOutcome> {
        panic!("the arm must refuse before a forgetful provider is ever called")
    }
    async fn poll(&self, _job: &MediaJobId) -> anyhow::Result<MediaPollOutcome> {
        unreachable!("this test never defers")
    }
}

/// A provider that has an input route and says so.
struct AcceptingProvider;

#[async_trait::async_trait]
impl MediaModel for AcceptingProvider {
    fn accepts_inputs(&self) -> bool {
        true
    }
    async fn generate(&self, request: &MediaRequest) -> anyhow::Result<MediaOutcome> {
        assert_eq!(
            request.inputs.len(),
            1,
            "the parts reach the provider intact"
        );
        Ok(MediaOutcome::Complete(Vec::new()))
    }
    async fn poll(&self, _job: &MediaJobId) -> anyhow::Result<MediaPollOutcome> {
        unreachable!("this test never defers")
    }
}

/// **The guard is structural, not a convention each provider remembers.**
///
/// `accepts_inputs` defaults to `false` and `MediaArm::generate` — the single dispatch
/// point for every media call — refuses on that default. So a provider added later that
/// never considers `inputs` fails *closed*: a loud refusal, not a silently dropped image
/// and a convincing wrong answer. `ForgetfulProvider::generate` panics if it is ever
/// reached, so this test fails loudly if the arm stops checking.
#[tokio::test]
async fn the_arm_refuses_for_a_provider_that_never_opted_in() {
    let arm = MediaArm::new(std::sync::Arc::new(ForgetfulProvider), "fake/forgetful");
    let request = request_with(vec![MediaInput::new(
        "image",
        Extension::Png,
        b"photo".to_vec(),
    )]);
    let err = arm.generate(&request).await.expect_err("must refuse");
    let msg = format!("{err:#}");
    assert!(msg.contains("fake/forgetful"), "names the slot: {msg}");
    assert!(msg.contains("nothing was generated"), "{msg}");
}

/// And the guard does not stand in the way of a provider that opted in — otherwise it
/// would refuse the very calls the whole change exists to enable. Without this, the test
/// above would still pass if the arm refused unconditionally.
#[tokio::test]
async fn the_arm_passes_inputs_through_for_a_provider_that_opted_in() {
    let arm = MediaArm::new(std::sync::Arc::new(AcceptingProvider), "fake/accepting");
    let request = request_with(vec![MediaInput::new(
        "image",
        Extension::Png,
        b"photo".to_vec(),
    )]);
    arm.generate(&request).await.expect("accepted");
}

// --- Named operations ----------------------------------------------------------

use kaibo::media::refuse_operation;
use kaibo::stability::{op_by_name, op_names, STABLE_IMAGE_OPS};

fn request_with_op(op: &str) -> MediaRequest {
    MediaRequest {
        prompt: "erase the sign".into(),
        fields: Vec::new(),
        inputs: Vec::new(),
        op: Some(op.to_string()),
    }
}

/// **The twelve routes are reachable by the names the tool publishes.**
///
/// The table is the single source for the schema's list, the parser and the refusal, so
/// this asserts the round trip a caller actually makes: the published name resolves, and
/// resolves to the route whose path it claims.
#[test]
fn every_published_operation_name_resolves_to_its_own_route() {
    assert_eq!(op_names().len(), 12, "the wired sync stable-image surface");
    for spec in STABLE_IMAGE_OPS {
        let found = op_by_name(spec.name).unwrap_or_else(|| panic!("{} must resolve", spec.name));
        assert_eq!(found.path, spec.path);
        assert!(
            found.path.ends_with(spec.name),
            "the caller-facing name is the tail of the endpoint path, so one cannot drift \
             from the other: {} vs {}",
            spec.name,
            spec.path
        );
    }
}

/// Costs are published because they differ by twenty times across the table, and a model
/// that reads the price before picking a route spends differently from one that does not.
/// This pins the two ends, so a table edit that flattened them would fail.
#[test]
fn the_published_costs_span_the_range_that_makes_them_worth_publishing() {
    let fast = op_by_name("upscale/fast").expect("wired");
    let conservative = op_by_name("upscale/conservative").expect("wired");
    assert_eq!(fast.credits, 2);
    assert_eq!(conservative.credits, 40);
    assert!(
        conservative.credits >= fast.credits * 10,
        "if these ever converge, publishing the number stops earning its space"
    );
}

/// The deferred routes are deliberately absent — they return a job id, not an artifact,
/// so wiring them as if they were synchronous would mis-declare `Shape` and break the
/// one thing this module refuses to sniff from a response.
#[test]
fn the_deferred_routes_are_not_in_the_sync_table() {
    for absent in [
        "upscale/creative",
        "edit/replace-background-and-relight",
        "audio/stable-audio-2/text-to-audio",
        "3d/stable-fast-3d",
    ] {
        assert!(
            op_by_name(absent).is_none(),
            "{absent} is not synchronous or not storable yet; wiring it here would \
             mis-declare its shape"
        );
    }
}

/// An unrecognized `op` refuses and names every one it accepts, rendered from the same
/// table the parser reads — so the refusal cannot list something `op_by_name` rejects.
#[test]
fn an_unknown_operation_name_is_refused_with_the_wired_list() {
    assert!(op_by_name("edit/inpaintt").is_none());
    assert!(op_by_name("").is_none());
}

/// **The op guard is structural, like the inputs guard.** A provider with no operation
/// vocabulary refuses a named op rather than running its default route and returning a
/// text-to-image render for an inpaint request.
#[test]
fn a_provider_without_an_operation_vocabulary_refuses_a_named_op() {
    let err = refuse_operation(&request_with_op("edit/inpaint"), "dashscope/wan2.2")
        .expect_err("no vocabulary");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("edit/inpaint"),
        "names the op asked for: {msg}"
    );
    assert!(msg.contains("dashscope/wan2.2"), "names the backend: {msg}");
    assert!(msg.contains("nothing was generated"), "{msg}");
}

#[test]
fn a_request_with_no_op_passes_the_guard() {
    assert!(refuse_operation(&request_with(Vec::new()), "dashscope/wan2.2").is_ok());
}

/// And the arm enforces it at the single dispatch point, the same place the inputs guard
/// lives — with the same pair of tests, so a guard that refused unconditionally would
/// fail the second.
#[tokio::test]
async fn the_arm_refuses_a_named_op_for_a_provider_that_never_opted_in() {
    let arm = MediaArm::new(std::sync::Arc::new(ForgetfulProvider), "fake/forgetful");
    let err = arm
        .generate(&request_with_op("edit/inpaint"))
        .await
        .expect_err("must refuse");
    assert!(format!("{err:#}").contains("edit/inpaint"));
}
