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
use kaibo::media::{refuse_binary_inputs, resolve_inputs, MediaInput, MediaRequest};
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
        MediaInput::new("image", "png", b"photo".to_vec()),
        MediaInput::new("mask", "png", b"mask".to_vec()),
    ]);
    let err = refuse_binary_inputs(&request, "DashScope").expect_err("no route for inputs");
    let msg = format!("{err:#}");
    assert!(msg.contains("DashScope"), "names the backend: {msg}");
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
    assert!(refuse_binary_inputs(&request_with(Vec::new()), "DashScope").is_ok());
}

/// `MediaInput::new` builds the wire filename from the field name and the store's
/// extension. A part without a `filename=` is rejected or mis-typed by some multipart
/// servers, and every route that takes one of these is a file upload.
#[test]
fn every_part_carries_a_filename_with_the_stores_extension() {
    let part = MediaInput::new("subject_image", "webp", b"bytes".to_vec());
    assert_eq!(part.field, "subject_image");
    assert_eq!(part.filename, "subject_image.webp");
}
