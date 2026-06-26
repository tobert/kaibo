//! `generate_image` — the MCP capability tool: prompt → image, written to the
//! artifact out-dir, path handed back.
//!
//! This is a *capability*, not consultation: no `run_phase` model loop, no kaish
//! shell. The handler resolves the cast's `image` slot into an [`ImageGen`]
//! ([`crate::image_gen`]), generates the bytes, sniffs their MIME, writes the artifact
//! to the kaibo-owned out-dir ([`crate::config::Config::out_dir`]), and hands back the
//! **absolute path** as a short text caption. The provider logic lives behind the
//! [`ImageGen`] seam so this whole path is exercised offline by a scripted backend; the
//! live wire is proven by an `#[ignore]`d probe.
//!
//! **Path delivery, not inline.** The image is written to disk and only its path
//! crosses the MCP edge — no base64 blob in the tool result, so a multi-MiB picture
//! costs the calling agent nothing until it chooses to open the file. The write is
//! handler-side (`std::fs`), never through kaish, so the read-only sandbox is
//! untouched; the out-dir is separately mounted *read-only* into kaish so a later
//! consult can read the artifact back. There is no inline size cap — the old
//! `GENERATE_IMAGE_MAX_BYTES` guard existed only to bound base64 in context and is gone
//! with inline delivery. The decision (path over `ResourceLink`) is recorded in
//! `docs/issues.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rmcp::model::Content;
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use crate::image_gen::ImageGen;
use crate::view_image::sniff_mime;

/// Default square size when the caller doesn't ask for one. Generous enough to be
/// useful; a turbo SD model or a hosted gpt-image both accept it.
pub const DEFAULT_SIZE: (u32, u32) = (1024, 1024);

/// Arguments to `generate_image`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateImageInput {
    /// What to draw. A plain-language description; passed verbatim to the image model.
    pub prompt: String,

    /// Cast: a built-in name ("openai-local", …) or a cast from config.toml. The cast must
    /// carry an `image` slot on an OpenAI-compatible backend. Omit to use the default
    /// cast.
    #[serde(default)]
    pub cast: Option<String>,

    /// Image size as `WxH` (e.g. "1024x1024", "512x768"). Defaults to 1024x1024.
    /// Some models (turbo SD variants) ignore it and emit a fixed size.
    #[serde(default)]
    pub size: Option<String>,

    /// Override the `image` slot's model id. See `kaibo://tools` for override semantics
    /// (pair with `image_backend` to also retarget — which works even on a cast with no
    /// `image` slot).
    #[serde(default)]
    pub image_model: Option<String>,

    /// Run the `image_model` override on this backend (name or alias). Requires
    /// `image_model`. See `kaibo://tools`.
    #[serde(default)]
    pub image_backend: Option<String>,
}

/// A generated image and the MIME type sniffed from its bytes.
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

/// Why a generation failed, split so the MCP handler can categorize honestly: the
/// caller can *act* on `Unusable` (lower the size, pick another model) but not on
/// `Backend` (the provider/network failed). Mapping both to one error category would
/// tell a calling agent "the server is broken, don't retry" when the fix is in its
/// own hands — the opposite of the loud-*and-honest* errors this project wants.
#[derive(Debug)]
pub enum GenerateError {
    /// The image backend / provider call failed (network, auth, rate-limit, an empty
    /// response). Not the caller's fault — maps to an MCP internal error.
    Backend(anyhow::Error),
    /// The bytes came back but can't be delivered as asked — an unrecognized format
    /// (not a real image). The caller can change the request (pick another model) —
    /// maps to an MCP invalid-params error.
    Unusable(String),
}

impl std::fmt::Display for GenerateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "{e:#}"),
            Self::Unusable(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for GenerateError {}

/// Parse a `WxH` size, defaulting when absent. Rejects zero/garbage loudly rather
/// than silently substituting — a malformed size is a caller mistake worth surfacing.
/// Accepts the ASCII `x`/`X` and the Unicode `×` (U+00D7) so a size copied from a spec
/// sheet — or echoed back from our own caption, which prints `×` — round-trips.
pub fn parse_size(spec: Option<&str>) -> Result<(u32, u32)> {
    let Some(spec) = spec else {
        return Ok(DEFAULT_SIZE);
    };
    let spec = spec.trim();
    let (w, h) = spec
        .split_once(['x', 'X', '×'])
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

/// Generate one image: call the backend, sniff the MIME.
///
/// Errors loudly, and *categorized* (see [`GenerateError`]): a failed/empty
/// generation is [`GenerateError::Backend`]; bytes in an unrecognized format are
/// [`GenerateError::Unusable`] (the caller can pick another model). Neither is a silent
/// fallback — a blob we can't recognize is suspicious, not something to mislabel. There
/// is no size cap: the artifact is written to disk, not inlined, so a large image is
/// fine ([`write_artifact`] is the next step).
pub async fn generate(
    image_gen: &dyn ImageGen,
    prompt: &str,
    size: (u32, u32),
) -> std::result::Result<GeneratedImage, GenerateError> {
    let bytes = image_gen
        .generate(prompt, size)
        .await
        .map_err(GenerateError::Backend)?;
    let mime = sniff_mime(&bytes).ok_or_else(|| {
        GenerateError::Unusable(format!(
            "image model returned {} bytes in an unrecognized format (not png/jpeg/gif/webp); \
             refusing to mislabel them",
            bytes.len()
        ))
    })?;
    Ok(GeneratedImage { bytes, mime })
}

/// The file extension for a sniffed image MIME. Total over the four formats
/// [`sniff_mime`] recognizes — an artifact only reaches here after a successful sniff,
/// so the fallback (`bin`) is unreachable in practice but keeps the function total.
fn mime_ext(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// A unique artifact filename: `kaibo-image-<unix_nanos>-<pid>-<counter>.<ext>`. The
/// nanosecond clock plus the process id plus a monotonic in-process counter make a
/// collision astronomically unlikely without pulling in an RNG dependency — two writes
/// in the same process get distinct counters, and two processes differ on pid.
fn unique_artifact_name(ext: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("kaibo-image-{nanos}-{}-{n}.{ext}", std::process::id())
}

/// Write a generated image to `out_dir`, returning its absolute path.
///
/// Handler-side `std::fs` — this is the *only* write path for an artifact, and it never
/// touches kaish, so the read-only sandbox is unaffected. Creates `out_dir` on demand
/// (the lazy creation the read-back mount waits on) and errors loudly on any failure
/// (a full disk or unwritable cache dir is the operator's environment, not the caller's
/// request — the handler maps it to an internal error). Never delivers a half-written
/// artifact as success: a failed write is an `Err`, not a quietly dropped image.
pub fn write_artifact(out_dir: &Path, image: &GeneratedImage) -> Result<PathBuf> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating artifact out-dir {}", out_dir.display()))?;
    let path = out_dir.join(unique_artifact_name(mime_ext(image.mime)));
    std::fs::write(&path, &image.bytes)
        .with_context(|| format!("writing artifact to {}", path.display()))?;
    Ok(path)
}

/// The MCP content for a written artifact: a single text caption naming the size, MIME,
/// byte count, and the **absolute path** the calling agent opens to view it. No image
/// part — delivery is by path, not inline base64 (see the module doc).
pub fn to_content(
    path: &Path,
    image: &GeneratedImage,
    prompt: &str,
    size: (u32, u32),
) -> Vec<Content> {
    let caption = format!(
        "Generated a {}×{} {} ({:.1} KiB) for: {}\nSaved to {} — open it to view.",
        size.0,
        size.1,
        image.mime,
        image.bytes.len() as f64 / 1024.0,
        prompt,
        path.display(),
    );
    vec![Content::text(caption)]
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
        // The Unicode × is accepted — our own caption emits it, so it must round-trip.
        assert_eq!(parse_size(Some("1024×768")).unwrap(), (1024, 768));
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
    }

    #[test]
    fn write_artifact_writes_bytes_with_the_right_ext_and_unique_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let img = GeneratedImage {
            bytes: fake_png(32),
            mime: "image/png",
        };
        let p1 = write_artifact(dir.path(), &img).expect("write succeeds");
        let p2 = write_artifact(dir.path(), &img).expect("second write succeeds");
        // Bytes land verbatim on disk — no truncation, no re-encode.
        assert_eq!(
            std::fs::read(&p1).unwrap(),
            img.bytes,
            "file holds the exact bytes"
        );
        // Extension comes from the sniffed MIME.
        assert_eq!(p1.extension().and_then(|e| e.to_str()), Some("png"));
        // Two writes never collide — the artifact channel must not clobber.
        assert_ne!(p1, p2, "each artifact gets a distinct path");
        assert!(p1.starts_with(dir.path()), "artifact lands inside out_dir");
    }

    #[test]
    fn write_artifact_creates_a_missing_out_dir() {
        // The read-back mount waits on lazy creation — write_artifact must make the dir.
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("does/not/exist/yet");
        let img = GeneratedImage {
            bytes: fake_png(8),
            mime: "image/png",
        };
        let p = write_artifact(&nested, &img).expect("write creates the dir tree");
        assert!(
            p.exists(),
            "artifact written under a freshly-created out_dir"
        );
    }

    #[test]
    fn to_content_delivers_the_path_not_an_inline_image() {
        let img = GeneratedImage {
            bytes: fake_png(16),
            mime: "image/png",
        };
        let path = Path::new("/cache/kaibo/kaibo-image-1-2-3.png");
        let content = to_content(path, &img, "a red circle", (640, 480));
        // Exactly one part, and it is text (no base64 image part rides along).
        assert_eq!(
            content.len(),
            1,
            "path delivery is a single text part: {content:?}"
        );
        let is_image = matches!(content.first().map(|c| c.raw.as_image()), Some(Some(_)));
        assert!(!is_image, "no inline image part — delivery is by path");
        let text = content[0]
            .raw
            .as_text()
            .expect("the part is text")
            .text
            .clone();
        assert!(
            text.contains("/cache/kaibo/kaibo-image-1-2-3.png"),
            "the caption names the artifact path: {text}"
        );
    }

    #[tokio::test]
    async fn generate_rejects_unrecognized_bytes_as_unusable() {
        let backend = ScriptedImageGen::returning(b"not an image".to_vec());
        let err = generate(&backend, "x", DEFAULT_SIZE)
            .await
            .expect_err("unrecognized bytes must error, not be mislabeled");
        // Unusable, not Backend — the caller can pick a different model.
        assert!(
            matches!(err, GenerateError::Unusable(ref m) if m.contains("unrecognized format")),
            "expected a caller-fixable Unusable naming the format problem: {err:?}"
        );
    }

    #[tokio::test]
    async fn generate_surfaces_a_backend_failure_as_backend() {
        let backend = ScriptedImageGen::failing("model is down");
        let err = generate(&backend, "x", DEFAULT_SIZE)
            .await
            .expect_err("a backend failure must propagate");
        // Backend, not Unusable — the server/provider failed, not the request.
        assert!(
            matches!(err, GenerateError::Backend(ref e) if e.to_string().contains("model is down")),
            "expected a Backend error carrying the cause: {err:?}"
        );
    }
}
