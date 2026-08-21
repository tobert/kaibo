//! `write_cas` — the operator's way to put bytes *into* kaibo's media store.
//!
//! # Why this exists
//!
//! kaibo's media lane has produced artifacts since `generate` landed, and `read_cas`
//! retrieves them. What has never existed is a way for the **caller** to put an image
//! in. That gap is about to bind: Stability's `edit`, `control`, and `upscale` families
//! all take an input image, and image-to-image on the already-wired `generate` routes
//! does too. An input image has to reach kaibo somehow, and the store it must end up in
//! is the one the rest of the lane already addresses.
//!
//! So this is the **third** caller of [`crate::cas::Cas::put`], beside `generate` (a
//! provider's render) and `save_artifact` (bytes a model on kaibo's team wrote). It adds
//! a caller, **not a write path**: no new `std::fs` call site exists in this module, so
//! `tests/no_write_path.rs` stays pinned at its blessed lines.
//!
//! # Where it sits in the trust model
//!
//! `save_artifact` is model-facing — the inner team writing out through a one-way
//! tunnel. This is **operator-facing**, the write half of the `read_cas` pair: the
//! client agent is the operator's proxy and is entitled to kaibo's own state. The inner
//! model team gains nothing here. It has no new tool, kaish has no new builtin, no
//! mount, and no knowledge that this store exists.
//!
//! # The write is unaimable, and so is the format
//!
//! Two parameters a first draft would have had, and why neither exists:
//!
//! - **No path, in either direction.** The address is the content's own hash, so there
//!   is no destination to name; and there is no *source* path either. `save_artifact`
//!   settled that question next door — a source-path parameter "was designed, argued,
//!   and **dropped permanently** (2026-08-05)" — and reopening it in a neighbouring tool
//!   would be relitigating a decision, not making one. Inline `content` is the whole
//!   input surface. (A path would also need its own containment argument, which is a
//!   separate review, not a rider on this one.)
//! - **No `mime`.** The format is read out of the bytes' own magic number
//!   ([`sniff_image`]), never asserted by the caller. A stated mime is a thing that can
//!   be *wrong*: bytes stored under a lying extension make `read_cas` report a false
//!   type and make every downstream [`Extension`] decision wrong, which is exactly the
//!   silent corruption this codebase refuses. Deriving it means there is no parameter to
//!   get wrong and no mismatch to detect.
//!
//! Between them, the whole input is bytes plus an optional label — nothing a caller can
//! aim at anything.
//!
//! # Images only
//!
//! [`sniff_image`] recognizes the four image containers the store can name, and refuses
//! everything else. That mirrors `store_generated_artifacts`, which keys on
//! [`Extension::is_image`] rather than "the store can name it" — the store also names the
//! text formats `save_artifact` writes, and keeping the refusal tied to the media lane's
//! own shape is what stops a widened [`Extension`] from quietly widening what the media
//! tools accept.

use crate::cas::{Digest, Extension, MediaStore, Provenance};

/// The most bytes one upload may carry, before base64.
///
/// A real working limit, unlike `save_artifact`'s backstop: `content` arrives base64'd
/// in tool-call arguments, so 8 MiB of image is ~10.7 MiB of JSON on the wire. Sized for
/// real evidence — screenshots, design assets, photographs at the resolutions the
/// Stability edit routes accept — and low enough that one call cannot wedge a client's
/// request handling.
pub const MAX_UPLOAD_BYTES: usize = 1 << 23;

/// The most bytes a label may carry. Matches `save_artifact`'s cap for the same two
/// reasons: the label is rendered into a result line, so a newline forges entries, and
/// an unbounded one is payload no byte cap ever sees.
pub const MAX_LABEL_BYTES: usize = 200;

/// Every image container the store can name, with the byte signature that identifies it.
///
/// A table rather than a `match` so the refusal message, the tool description, and the
/// admission logic all render from the same list and cannot drift apart — the same
/// argument `artifact::FORMATS` records.
///
/// WebP is absent here because its signature is split (`RIFF` at 0, `WEBP` at 8) and does
/// not fit a single-prefix table; [`sniff_image`] checks it separately.
const SIGNATURES: &[(&[u8], Extension)] = &[
    (b"\x89PNG\r\n\x1a\n", Extension::Png),
    (b"\xff\xd8\xff", Extension::Jpeg),
    (b"GIF87a", Extension::Gif),
    (b"GIF89a", Extension::Gif),
];

/// The format names this tool accepts, for a description or a refusal. Rendered from
/// [`SIGNATURES`] plus WebP, so it cannot drift from what [`sniff_image`] admits.
pub fn accepted_formats() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = SIGNATURES
        .iter()
        .map(|(_, ext)| ext.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    names.push(Extension::Webp.as_str());
    names
}

/// Identify an image container from its leading bytes.
///
/// The format is a fact about the content, read from the content — never a claim the
/// caller makes. Anything unrecognized is refused rather than stored under a guessed
/// extension.
pub fn sniff_image(bytes: &[u8]) -> Result<Extension, UploadError> {
    for (signature, ext) in SIGNATURES {
        if bytes.starts_with(signature) {
            return Ok(*ext);
        }
    }
    // WebP's signature straddles a 4-byte length field: `RIFF` <u32 size> `WEBP`.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Ok(Extension::Webp);
    }
    Err(UploadError::UnknownFormat {
        // Enough leading bytes to identify any container we might later add, and few
        // enough that a refusal stays readable.
        head: bytes.iter().take(12).copied().collect(),
    })
}

/// Why an upload was refused. Every variant names what was refused, why, and the way
/// out — these strings carry the Full weight of the style guide, and a caller reads one
/// at the moment it is blocked.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error(
        "`content` decoded to zero bytes. An upload stores an image, so there is \
         nothing to store. Pass the image's bytes base64-encoded in `content`."
    )]
    Empty,

    #[error(
        "`content` is not valid base64: {cause}. Encode the image's raw bytes as \
         standard base64 (padding included) and pass that string in `content`."
    )]
    BadBase64 { cause: String },

    #[error(
        "the upload is {actual} bytes, over the {cap}-byte cap. Nothing was stored. \
         Reduce the image's resolution or re-encode it at a lower quality, then upload \
         the smaller file."
    )]
    TooLarge { cap: usize, actual: usize },

    #[error(
        "these bytes do not begin with a recognized image signature (first bytes: \
         {head:02x?}). kaibo's media store holds images: {}. Upload one of those \
         formats, or convert this file to one first.",
        Self::formats()
    )]
    UnknownFormat { head: Vec<u8> },

    #[error(
        "`label` is {actual} bytes, over the {cap}-byte cap, or carries a control \
         character. Nothing was stored. Pass a single short line describing the image."
    )]
    BadLabel { cap: usize, actual: usize },

    #[error("the media store refused the write: {0}")]
    Store(String),
}

impl UploadError {
    /// The accepted-format list, injected into [`UploadError::UnknownFormat`]'s message
    /// at render time so the refusal and [`sniff_image`] cannot disagree about what is
    /// admitted.
    fn formats() -> String {
        accepted_formats().join(", ")
    }
}

/// One stored upload: what the caller needs to address it and to see what kaibo decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUpload {
    pub digest: Digest,
    /// What the store named the content — read from the bytes, so it may differ from
    /// what the caller believed it was uploading.
    pub extension: Extension,
    pub bytes: usize,
}

/// Validate a label: a single short line, or nothing at all.
fn check_label(label: Option<&str>) -> Result<Option<String>, UploadError> {
    let Some(label) = label else {
        return Ok(None);
    };
    let label = label.trim();
    if label.is_empty() {
        return Ok(None);
    }
    if label.len() > MAX_LABEL_BYTES || label.chars().any(char::is_control) {
        return Err(UploadError::BadLabel {
            cap: MAX_LABEL_BYTES,
            actual: label.len(),
        });
    }
    Ok(Some(label.to_string()))
}

/// Decode, identify, and store one uploaded image.
///
/// Every check runs **before** the store is touched, so a refused upload stored nothing
/// — the same all-or-nothing posture `store_generated_artifacts` takes when it
/// prevalidates every mime up front.
pub fn store_upload(
    store: &MediaStore,
    content: &str,
    label: Option<&str>,
    timestamp: i64,
) -> Result<StoredUpload, UploadError> {
    use base64::Engine;

    let label = check_label(label)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content.trim())
        .map_err(|e| UploadError::BadBase64 {
            cause: e.to_string(),
        })?;
    if bytes.is_empty() {
        return Err(UploadError::Empty);
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(UploadError::TooLarge {
            cap: MAX_UPLOAD_BYTES,
            actual: bytes.len(),
        });
    }
    let ext = sniff_image(&bytes)?;

    let provenance = Provenance {
        // A client uploaded these bytes. No prompt produced them, no model rendered
        // them, and no cast was involved — the three fields that name a producer are
        // empty because there is nothing true to put in them, and `tool` is what says
        // so. A reader distinguishes an upload from a render by that field, not by
        // guessing from an empty prompt.
        prompt: String::new(),
        model: String::new(),
        cast: String::new(),
        timestamp,
        mime: ext.mime().to_string(),
        // No generation happened, so there is no seed to reproduce.
        seed: None,
        tool: Some("write_cas".to_string()),
        // No model authored these bytes, so no reasoning slot filled them.
        slot: None,
        label,
        session: None,
    };

    // Three outcomes, not two. The middle one — bytes stored, provenance not — is still
    // an upload: the content is durable and retrievable, and denying it would orphan
    // stored bytes behind a message claiming nothing happened. The same argument
    // `ArtifactSink::save` records.
    let digest = match store.put(&bytes, ext, &provenance) {
        Ok(digest) => digest,
        Err(crate::cas::CasError::ProvenanceNotRecorded { digest, cause }) => {
            tracing::warn!(
                digest = %digest,
                cause = %cause,
                "upload stored without its provenance sidecar — the bytes are durable \
                 and retrievable, the housekeeping record is not"
            );
            Digest::from_hex(&digest).expect("the store renders its own digests in canonical hex")
        }
        Err(other) => {
            // The operator gets the whole typed error, paths and usage included; the
            // caller gets the sanitized rendering.
            tracing::warn!(error = %other, "media store refused a write_cas upload");
            return Err(UploadError::Store(other.to_string()));
        }
    };

    // What the artifact IS, per the store — not what this upload decided. The two differ
    // whenever identical content is already held under another container format, and the
    // result must agree with what `read_cas` reports and with the on-disk path.
    let stored_ext = store.extension_for(&digest).unwrap_or(ext);

    Ok(StoredUpload {
        digest,
        extension: stored_ext,
        bytes: bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest byte strings that carry each container's signature. Not valid
    /// images — `sniff_image` reads the signature and nothing else, which is the point:
    /// kaibo names the container, it does not validate the pixels.
    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\n".to_vec()
    }
    fn jpeg() -> Vec<u8> {
        b"\xff\xd8\xff\xe0".to_vec()
    }
    fn webp() -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"WEBP");
        v
    }

    #[test]
    fn sniff_identifies_every_accepted_container() {
        assert_eq!(sniff_image(&png()).unwrap(), Extension::Png);
        assert_eq!(sniff_image(&jpeg()).unwrap(), Extension::Jpeg);
        assert_eq!(sniff_image(&webp()).unwrap(), Extension::Webp);
        assert_eq!(sniff_image(b"GIF87a").unwrap(), Extension::Gif);
        assert_eq!(sniff_image(b"GIF89a").unwrap(), Extension::Gif);
    }

    #[test]
    fn sniff_refuses_text_and_names_what_it_accepts() {
        let err = sniff_image(b"# a markdown file\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("recognized image signature"),
            "refusal should name the check that failed: {msg}"
        );
        for format in accepted_formats() {
            assert!(
                msg.contains(format),
                "refusal should name the accepted format {format}: {msg}"
            );
        }
    }

    /// The truncated-WebP case: `RIFF` alone is a container family, not a WebP, and
    /// admitting it on the prefix would store an audio or video RIFF as an image.
    #[test]
    fn sniff_refuses_a_riff_that_is_not_a_webp() {
        let mut wave = b"RIFF".to_vec();
        wave.extend_from_slice(&[0, 0, 0, 0]);
        wave.extend_from_slice(b"WAVE");
        assert!(sniff_image(&wave).is_err());
        // And a RIFF too short to carry the second half of the signature.
        assert!(sniff_image(b"RIFF").is_err());
    }

    #[test]
    fn sniff_refuses_a_prefix_too_short_to_identify() {
        assert!(sniff_image(b"\x89PN").is_err());
        assert!(sniff_image(b"").is_err());
    }

    #[test]
    fn accepted_formats_covers_every_signature_and_webp() {
        let names = accepted_formats();
        for (_, ext) in SIGNATURES {
            assert!(
                names.contains(&ext.as_str()),
                "{} is admitted by sniff_image but missing from accepted_formats",
                ext.as_str()
            );
        }
        assert!(names.contains(&Extension::Webp.as_str()));
    }

    #[test]
    fn label_accepts_a_short_line_and_normalizes_absence() {
        assert_eq!(check_label(None).unwrap(), None);
        assert_eq!(check_label(Some("   ")).unwrap(), None);
        assert_eq!(
            check_label(Some("  the failing dialog  ")).unwrap(),
            Some("the failing dialog".to_string())
        );
    }

    #[test]
    fn label_refuses_a_newline_that_would_forge_a_result_line() {
        assert!(check_label(Some("real\nkaibo://cas/deadbeef")).is_err());
    }

    #[test]
    fn label_refuses_one_over_the_cap() {
        let long = "x".repeat(MAX_LABEL_BYTES + 1);
        assert!(check_label(Some(&long)).is_err());
        let at_cap = "x".repeat(MAX_LABEL_BYTES);
        assert!(check_label(Some(&at_cap)).is_ok());
    }
}
