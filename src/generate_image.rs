//! `generate_image` — the MCP capability tool: prompt → image, returned inline.
//!
//! This is a *capability*, not consultation: no `run_phase` model loop, no kaish
//! shell. The handler resolves the cast's `image` slot into an [`ImageGen`]
//! ([`crate::image_gen`]), generates the bytes, sniffs their MIME, and hands them
//! back as an MCP [`Content::image`] alongside a short text caption. The provider
//! logic lives behind the [`ImageGen`] seam so this whole path is exercised offline
//! by a scripted backend; the live wire is proven by an `#[ignore]`d probe.
//!
//! **Scratch/inline only, by design (this slice).** The bytes ride back inline,
//! base64 on the MCP edge, capped at [`crate::view_image::DEFAULT_MAX_IMAGE_BYTES`].
//! An image past the cap is an honest, loud error — large-artifact delivery
//! (`--out-dir` + `ResourceLink`) and the kaish-builtin/VFS composition surface (for
//! image2image pipelines) are tracked follow-ons in `docs/issues.md`, not silent
//! truncation here.

use anyhow::{anyhow, Result};
use rmcp::model::Content;
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use crate::image_gen::ImageGen;
use crate::view_image::{sniff_mime, DEFAULT_MAX_IMAGE_BYTES};

/// Default square size when the caller doesn't ask for one. Generous enough to be
/// useful; a turbo SD model or a hosted gpt-image both accept it.
pub const DEFAULT_SIZE: (u32, u32) = (1024, 1024);

/// Arguments to `generate_image`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateImageInput {
    /// What to draw. A plain-language description; passed verbatim to the image model.
    pub prompt: String,

    /// Cast: a built-in name ("openai", …) or a cast from config.toml. The cast must
    /// carry an `image` slot on an OpenAI-compatible backend. Omit to use the default
    /// cast.
    #[serde(default)]
    pub cast: Option<String>,

    /// Image size as `WxH` (e.g. "1024x1024", "512x768"). Defaults to 1024x1024.
    /// Some models (turbo SD variants) ignore it and emit a fixed size.
    #[serde(default)]
    pub size: Option<String>,

    /// Override the `image` slot's model id (sent verbatim — an id with "/" is one
    /// id). Keeps the slot's configured backend.
    #[serde(default)]
    pub image_model: Option<String>,
}

/// A generated image and the MIME type sniffed from its bytes.
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

/// Parse a `WxH` size, defaulting when absent. Rejects zero/garbage loudly rather
/// than silently substituting — a malformed size is a caller mistake worth surfacing.
pub fn parse_size(spec: Option<&str>) -> Result<(u32, u32)> {
    let Some(spec) = spec else {
        return Ok(DEFAULT_SIZE);
    };
    let spec = spec.trim();
    let (w, h) = spec
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow!("size must be WxH (e.g. \"1024x1024\"), got {spec:?}"))?;
    let parse = |s: &str, which: &str| -> Result<u32> {
        let n: u32 = s
            .trim()
            .parse()
            .map_err(|_| anyhow!("size {which} must be a positive integer, got {s:?}"))?;
        if n == 0 {
            return Err(anyhow!("size {which} must be greater than zero"));
        }
        Ok(n)
    };
    Ok((parse(w, "width")?, parse(h, "height")?))
}

/// Generate one image: call the backend, sniff the MIME, enforce the inline cap.
///
/// Errors loudly on (a) an empty/failed generation, (b) bytes whose format we can't
/// recognize — a generated blob that isn't a known image type is suspicious, not
/// something to mislabel — and (c) bytes over the inline cap (no out-dir yet to spill
/// to). None of these is a silent fallback.
pub async fn generate(
    image_gen: &dyn ImageGen,
    prompt: &str,
    size: (u32, u32),
) -> Result<GeneratedImage> {
    let bytes = image_gen.generate(prompt, size).await?;
    let mime = sniff_mime(&bytes).ok_or_else(|| {
        anyhow!(
            "image model returned {} bytes in an unrecognized format (not png/jpeg/gif/webp); \
             refusing to mislabel them",
            bytes.len()
        )
    })?;
    if bytes.len() > DEFAULT_MAX_IMAGE_BYTES {
        return Err(anyhow!(
            "generated image is {} bytes, over the {} byte inline cap; large-artifact \
             delivery (--out-dir / ResourceLink) is not yet wired — lower the size or use a \
             model that emits smaller images",
            bytes.len(),
            DEFAULT_MAX_IMAGE_BYTES
        ));
    }
    Ok(GeneratedImage { bytes, mime })
}

/// Assemble the MCP content for a generated image: the image part (base64 on the
/// wire) plus a short text caption so a client rendering only text still sees what
/// happened.
pub fn to_content(image: &GeneratedImage, prompt: &str, size: (u32, u32)) -> Vec<Content> {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
    let caption = format!(
        "Generated a {}×{} {} ({:.1} KiB) for: {}",
        size.0,
        size.1,
        image.mime,
        image.bytes.len() as f64 / 1024.0,
        prompt
    );
    vec![Content::image(data, image.mime), Content::text(caption)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_gen::ScriptedImageGen;

    fn fake_png(filler: usize) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend(std::iter::repeat_n(0xAB, filler));
        v
    }

    #[test]
    fn parse_size_defaults_and_parses_and_rejects() {
        assert_eq!(parse_size(None).unwrap(), DEFAULT_SIZE);
        assert_eq!(parse_size(Some("512x768")).unwrap(), (512, 768));
        assert_eq!(parse_size(Some(" 640 X 480 ")).unwrap(), (640, 480));
        assert!(parse_size(Some("1024")).is_err());
        assert!(parse_size(Some("0x10")).is_err());
        assert!(parse_size(Some("axb")).is_err());
    }

    #[tokio::test]
    async fn generate_sniffs_mime_and_passes_size_through() {
        let backend = ScriptedImageGen::returning(fake_png(16));
        let img = generate(&backend, "a red circle", (640, 480))
            .await
            .expect("scripted backend returns a valid png");
        assert_eq!(img.mime, "image/png");
        assert_eq!(
            backend.calls(),
            vec![("a red circle".to_string(), (640, 480))],
            "the prompt and size must reach the backend verbatim"
        );

        let content = to_content(&img, "a red circle", (640, 480));
        // First part is the image; the caption rides as text.
        let is_image = matches!(content.first().map(|c| c.raw.as_image()), Some(Some(_)));
        assert!(
            is_image,
            "the first content part must be the image: {content:?}"
        );
    }

    #[tokio::test]
    async fn generate_rejects_unrecognized_bytes_loudly() {
        let backend = ScriptedImageGen::returning(b"not an image".to_vec());
        let err = generate(&backend, "x", DEFAULT_SIZE)
            .await
            .expect_err("unrecognized bytes must error, not be mislabeled");
        assert!(
            err.to_string().contains("unrecognized format"),
            "error should name the format problem: {err}"
        );
    }

    #[tokio::test]
    async fn generate_rejects_oversized_loudly() {
        // A valid png header followed by enough filler to exceed the inline cap.
        let backend = ScriptedImageGen::returning(fake_png(DEFAULT_MAX_IMAGE_BYTES + 1));
        let err = generate(&backend, "x", DEFAULT_SIZE)
            .await
            .expect_err("an over-cap image must error, never be silently dropped");
        assert!(
            err.to_string().contains("inline cap"),
            "error should name the cap: {err}"
        );
    }

    #[tokio::test]
    async fn generate_surfaces_a_backend_failure() {
        let backend = ScriptedImageGen::failing("model is down");
        let err = generate(&backend, "x", DEFAULT_SIZE)
            .await
            .expect_err("a backend failure must propagate");
        assert!(err.to_string().contains("model is down"), "{err}");
    }
}
