//! Attachments — inline a workspace file into a tool-less prompt as context.
//!
//! The tool-less tools (batch today, `oneshot` a tracked follow-on in
//! `docs/issues.md`) answer from what the caller hands them. An attachment lets the
//! caller name a *file* in the workspace instead of pasting its bytes through the
//! calling agent's own context: kaibo reads it, containment-checks it against the
//! same allowed set every other path obeys
//! ([`resolve_attachments`](crate::server::KaiboHandler::resolve_attachments)), and
//! inlines it. The point is to keep the bytes off the calling agent's context window —
//! "review README.md" or `git diff > x.diff` instead of pasting the file.
//!
//! **Two encodings, picked by content, not extension.**
//! - **text** (valid UTF-8) splices into the prompt as `<file path="…">…</file>`.
//! - **image** (a sniffed image magic number, shared with [`crate::view_image`])
//!   rides as a base64 part the provider carries natively (an Anthropic `image`
//!   block / a Gemini `inlineData` part).
//!
//! Anything else — a binary that isn't a recognized image — is **refused loudly**
//! rather than inlined as mojibake: crash over corruption, per the project ethos.
//! Size caps are loud too (a file past its cap is refused, never silently truncated).
//!
//! A typed `FileRef` variant is reserved here in spirit for the later Gemini File API
//! path (oversized/reused media, Gemini-only) — see `docs/issues.md`. We design the
//! seam typed so that path slots in beside `Image` rather than re-architecting the
//! body builders.

use anyhow::{bail, Result};
use base64::Engine;

/// Cap on an attached *text* file's raw bytes. Generous — a large diff or source file
/// fits — but bounded so a runaway file is refused loudly, not folded silently into
/// every prompt (the `[context]` no-cap mistake in `docs/issues.md` is the lesson).
pub const DEFAULT_MAX_TEXT_BYTES: usize = 1 << 20; // 1 MiB

/// Cap on an attached *image*'s raw (pre-base64) bytes — reuses
/// [`crate::view_image::DEFAULT_MAX_IMAGE_BYTES`] so a read image and an attached one
/// share one ceiling.
pub use crate::view_image::DEFAULT_MAX_IMAGE_BYTES;

/// One resolved attachment, ready to fold into a provider request. The path is the
/// caller-facing label (what they passed), kept for the `<file>` wrapper and the image
/// part's provenance — never the canonical on-disk path, which is an implementation
/// detail of the containment check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// A UTF-8 text file, inlined as prompt text under a `<file>` wrapper.
    Text { path: String, body: String },
    /// An image, carried as a base64 part labelled with its sniffed mime.
    Image {
        path: String,
        mime: &'static str,
        data_b64: String,
    },
}

impl Attachment {
    /// The `<file>`-wrapped text for a *text* attachment — the exact form spliced into
    /// a prompt as context, so both provider body builders wrap identically (one source
    /// of truth for the wrapper). `None` for an image (which rides as a base64 part).
    pub fn wrapped_text(&self) -> Option<String> {
        match self {
            Attachment::Text { path, body } => {
                Some(format!("<file path=\"{path}\">\n{body}\n</file>"))
            }
            Attachment::Image { .. } => None,
        }
    }
}

/// Classify a file's bytes into an [`Attachment`], picking the encoding by *content*
/// (the magic number), not the path's extension — a `.md` full of PNG bytes is an
/// image, and we hand a `mimeType` straight to the provider, so the bytes are the only
/// honest source. `display_path` labels the result.
///
/// Loud failures, never silent: a file past its (encoding-specific) cap is refused, and
/// a binary that is neither valid UTF-8 nor a recognized image is refused rather than
/// inlined as garbage.
pub fn classify(
    display_path: &str,
    bytes: &[u8],
    max_text: usize,
    max_image: usize,
) -> Result<Attachment> {
    // Content decides the encoding. An image magic number wins — it can't be inlined
    // as text, and the sniffer is the same one `view_image`/`generate_image` trust.
    if let Some(mime) = crate::view_image::sniff_mime(bytes) {
        if bytes.len() > max_image {
            bail!(
                "attachment `{display_path}` is {} bytes, over the {max_image}-byte image cap — \
                 too large to inline (the Gemini File API path for oversized media is a tracked \
                 follow-on; see docs/issues.md)",
                bytes.len()
            );
        }
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        return Ok(Attachment::Image {
            path: display_path.to_string(),
            mime,
            data_b64,
        });
    }
    // Not a recognized image — it must be UTF-8 text to inline honestly.
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            if bytes.len() > max_text {
                bail!(
                    "attachment `{display_path}` is {} bytes, over the {max_text}-byte text cap — \
                     trim it or split the batch rather than inlining a runaway file into every prompt",
                    bytes.len()
                );
            }
            Ok(Attachment::Text {
                path: display_path.to_string(),
                body: text.to_string(),
            })
        }
        Err(_) => bail!(
            "attachment `{display_path}` is neither valid UTF-8 text nor a recognized image \
             (png/jpeg/gif/webp); kaibo won't inline binary as text — paste the relevant text, \
             or convert it first"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal blob with a real PNG signature — we never decode it, so a valid header
    /// plus filler exercises sniff + round-trip (mirrors `view_image`'s test helper).
    fn fake_png(filler: usize) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend(std::iter::repeat_n(0xAB, filler));
        v
    }

    /// A UTF-8 file becomes a Text attachment whose wrapper names the path and carries
    /// the body verbatim — the form a prompt sees.
    #[test]
    fn utf8_file_classifies_as_text_and_wraps() {
        let att = classify(
            "README.md",
            b"# Title\nbody\n",
            DEFAULT_MAX_TEXT_BYTES,
            DEFAULT_MAX_IMAGE_BYTES,
        )
        .expect("utf8 inlines as text");
        assert_eq!(
            att,
            Attachment::Text {
                path: "README.md".into(),
                body: "# Title\nbody\n".into()
            }
        );
        let wrapped = att.wrapped_text().expect("text attachments wrap");
        assert!(
            wrapped.contains("path=\"README.md\""),
            "wrapper names the path: {wrapped}"
        );
        assert!(
            wrapped.contains("# Title\nbody\n"),
            "wrapper carries the body verbatim: {wrapped}"
        );
    }

    /// An image magic number becomes an Image attachment with the sniffed mime and a
    /// base64 that decodes back to the original bytes.
    #[test]
    fn image_file_classifies_as_base64_image() {
        let bytes = fake_png(32);
        let att = classify(
            "logo.png",
            &bytes,
            DEFAULT_MAX_TEXT_BYTES,
            DEFAULT_MAX_IMAGE_BYTES,
        )
        .expect("a png inlines as an image part");
        let (mime, data_b64) = match &att {
            Attachment::Image { mime, data_b64, .. } => (*mime, data_b64.clone()),
            other => panic!("expected Image, got {other:?}"),
        };
        assert_eq!(mime, "image/png");
        assert!(att.wrapped_text().is_none(), "an image is not text-wrapped");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .expect("the base64 decodes");
        assert_eq!(decoded, bytes, "the round-trip preserves the bytes");
    }

    /// A text file past the text cap is refused loudly (no truncation), and the error
    /// names the cap so the caller can act.
    #[test]
    fn oversized_text_is_refused() {
        let big = vec![b'a'; 64];
        let err = classify("notes.txt", &big, 32, DEFAULT_MAX_IMAGE_BYTES)
            .expect_err("a text file over the cap must be refused, not truncated");
        assert!(err.to_string().contains("text cap"), "names the cap: {err}");
    }

    /// An image past the image cap is refused loudly.
    #[test]
    fn oversized_image_is_refused() {
        let bytes = fake_png(64);
        let err = classify("big.png", &bytes, DEFAULT_MAX_TEXT_BYTES, 16)
            .expect_err("an image over the cap must be refused");
        assert!(
            err.to_string().contains("image cap"),
            "names the cap: {err}"
        );
    }

    /// Binary that is neither valid UTF-8 nor a recognized image is refused rather than
    /// inlined as mojibake — crash over corruption.
    #[test]
    fn non_text_non_image_binary_is_refused() {
        // 0xFF is an invalid UTF-8 lead byte and matches no image magic.
        let err = classify(
            "mystery.bin",
            &[0x00, 0xFF, 0xFE, 0xFD],
            DEFAULT_MAX_TEXT_BYTES,
            DEFAULT_MAX_IMAGE_BYTES,
        )
        .expect_err("non-text non-image binary must be refused");
        assert!(
            err.to_string().contains("neither valid UTF-8"),
            "refusal explains the encoding gap: {err}"
        );
    }
}
