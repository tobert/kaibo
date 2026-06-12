//! `view_image` — the perception tool that lets a vision-capable model *see* a
//! file in the workspace.
//!
//! kaibo is read-only and answers about a codebase; the natural image inputs are
//! files already in the tree — a debug screenshot, a design asset, a diagram in the
//! docs. So the whole input surface is a **path**: the model names a file, this tool
//! reads its bytes *through the kaish VFS* (the same read-only mount `run_kaish`
//! uses, so containment and read-only stay structural), and returns them as a rig
//! image part — the one channel that carries an image into model context (rig's
//! [`ToolResultContent::from_tool_output`] parses `{"response":…, "parts":[{"type":
//! "image","data":…,"mimeType":…}]}`). There is deliberately no base64/attach input:
//! if a genuinely-never-a-file image ever needs viewing, that's added then (see the
//! media-spine entry in `docs/issues.md`).
//!
//! The tool is only ever placed in a phase's toolset when that phase's [`Arm`]'s
//! resolved caps say the model is vision-capable (`consult.rs`), so a blind model
//! never sees it — there is no "image handed to a model that can't read it" path.
//!
//! [`ToolResultContent::from_tool_output`]: rig_core::completion::message
//! [`Arm`]: crate::consult::Arm

use std::path::{Path, PathBuf};

use base64::Engine;
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sandbox::KaishWorker;

/// Default cap on an image's raw (pre-base64) size. Generous enough for real
/// screenshots and diagrams, low enough to keep a single tool result from blowing
/// past a provider's per-image limit (Anthropic ~5 MB) or ballooning context once
/// base64 adds its ~33%. A loud error past it (no resize dependency — we don't
/// silently shrink the user's evidence); override per-tool with
/// [`ViewImage::with_max_bytes`]. A config knob can ride this later.
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The `view_image` tool: read one workspace image and hand it to the model as a
/// rig image part. Backed by a [`KaishWorker`] (shared with `run_kaish` — one
/// read-only kernel) so reads go through the same VFS mount.
pub struct ViewImage {
    worker: KaishWorker,
    /// The workspace root every path is resolved against and contained within. The
    /// caller passes the *canonicalized* root (the same one the kernel is mounted
    /// at), so `starts_with` boundary checks are exact.
    root: PathBuf,
    max_bytes: usize,
}

impl ViewImage {
    /// A `view_image` rooted at `root`, reading through `worker`'s kernel, with the
    /// default size cap.
    pub fn new(worker: KaishWorker, root: impl Into<PathBuf>) -> Self {
        Self {
            worker,
            root: root.into(),
            max_bytes: DEFAULT_MAX_IMAGE_BYTES,
        }
    }

    /// Override the size cap — the seam a test uses to drive the too-large path
    /// without allocating megabytes.
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Resolve `path_arg` to a canonical path *inside* the workspace, or an
    /// actionable error. Relative paths are taken against the workspace root; an
    /// absolute path is used as-is. Canonicalization resolves `..` and symlinks
    /// host-side, so a symlink pointing out of the tree is caught here, not followed.
    ///
    /// The out-of-workspace message is deliberately a *fix-it*: it names the file and
    /// the workspace and suggests copying the file in, because the caller's agent
    /// (not kaibo, which is read-only) can act on that and retry.
    fn resolve_in_workspace(&self, path_arg: &str) -> Result<PathBuf, ViewImageError> {
        let p = Path::new(path_arg);
        let raw = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let canon = std::fs::canonicalize(&raw).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ViewImageError(format!(
                "view_image: no file found at {path_arg:?} (looked at {}). Paths are \
                 relative to the workspace root {} unless absolute; list the directory \
                 with run_kaish (e.g. `ls -la`) to find the right path.",
                raw.display(),
                self.root.display(),
            )),
            _ => ViewImageError(format!(
                "view_image: cannot access {:?}: {e}",
                raw.display()
            )),
        })?;
        if !canon.starts_with(&self.root) {
            return Err(ViewImageError(format!(
                "view_image: {path_arg:?} resolves to {}, which is OUTSIDE this \
                 workspace ({}). I can only read files inside the workspace. To let me \
                 see this image, copy it into the workspace — e.g. `cp {} {}/` — then \
                 call view_image again with the path inside the workspace.",
                canon.display(),
                self.root.display(),
                canon.display(),
                self.root.display(),
            )));
        }
        Ok(canon)
    }

    /// The core: resolve → read through the VFS → size-check → sniff → encode →
    /// envelope. Split out of [`Tool::call`] so it's directly testable. Returns the
    /// rig hybrid envelope as a [`Value`] — *not* a `String`: rig serializes a tool's
    /// `Output` with `serde_json::to_string`, so a `String` would arrive double-encoded
    /// (a quoted JSON string) and `from_tool_output` would treat it as text, never
    /// extracting the image. A `Value::Object` serializes to the bare object the
    /// parser needs.
    async fn view(&self, path_arg: &str) -> Result<Value, ViewImageError> {
        let canon = self.resolve_in_workspace(path_arg)?;
        let bytes = self.worker.read_file(&canon).await.map_err(|e| {
            ViewImageError(format!(
                "view_image: failed to read {}: {e}",
                canon.display()
            ))
        })?;

        if bytes.len() > self.max_bytes {
            return Err(ViewImageError(format!(
                "view_image: {} is {:.1} MiB, larger than the {:.1} MiB view_image \
                 limit. Crop or downscale it (kaibo won't resize your evidence) and \
                 try again.",
                canon.display(),
                bytes.len() as f64 / (1024.0 * 1024.0),
                self.max_bytes as f64 / (1024.0 * 1024.0),
            )));
        }

        let mime = sniff_mime(&bytes).ok_or_else(|| {
            ViewImageError(format!(
                "view_image: {} doesn't look like an image I can show (recognized: \
                 PNG, JPEG, GIF, WebP). If it's source or text, read it with run_kaish \
                 instead.",
                canon.display()
            ))
        })?;

        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        // A workspace-relative label reads cleaner than the absolute path.
        let label = canon.strip_prefix(&self.root).unwrap_or(&canon).display();
        let note = format!(
            "Loaded image {label} ({mime}, {:.1} KiB).",
            bytes.len() as f64 / 1024.0
        );
        // The rig hybrid envelope: `response` is text the model also sees; `parts`
        // carries the image. The part key is camelCase `mimeType` (rig-core 0.34).
        Ok(json!({
            "response": note,
            "parts": [ { "type": "image", "data": data, "mimeType": mime } ],
        }))
    }
}

#[derive(Debug, Deserialize)]
pub struct ViewImageArgs {
    /// Path to the image, relative to the workspace root or absolute inside it.
    pub path: String,
}

/// A `view_image` failure (bad path, out-of-workspace, unreadable, too large, not an
/// image). The message is the actionable part — it tells the calling model (or its
/// agent) how to fix the call.
#[derive(Debug)]
pub struct ViewImageError(String);

impl std::fmt::Display for ViewImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ViewImageError {}

impl Tool for ViewImage {
    const NAME: &'static str = "view_image";
    type Error = ViewImageError;
    type Args = ViewImageArgs;
    type Output = Value;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Look at an image file in the project so you can see it \
                          directly — a screenshot, a diagram, a design asset, a \
                          rendered figure. Pass the path to a PNG, JPEG, GIF, or WebP \
                          (relative to the project root, or an absolute path inside \
                          it); the image is loaded into your view. Use this whenever \
                          the question involves what an image shows. For source or \
                          text files, use run_kaish instead."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the image file (PNG/JPEG/GIF/WebP), \
                                        relative to the project root or absolute inside it."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.view(&args.path).await
    }
}

/// Identify an image by its magic bytes (content, not extension — the model may name
/// a path with the wrong suffix, and we're handing a `mimeType` straight to the
/// provider). Returns the MIME string rig maps to its `ImageMediaType`, or `None`
/// for anything we don't carry.
fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(PNG) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF container with a WEBP form type at offset 8.
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// A minimal byte blob with a real PNG signature. We never decode the image, so a
    /// valid header + filler is enough to exercise sniffing and the round-trip.
    fn fake_png(filler: usize) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend(std::iter::repeat_n(0xAB, filler));
        v
    }

    fn worker_over(root: &Path) -> KaishWorker {
        KaishWorker::spawn(root).expect("spawn read-only worker")
    }

    #[test]
    fn sniff_mime_recognizes_the_four_formats_and_rejects_others() {
        assert_eq!(sniff_mime(&fake_png(0)), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0x00]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a....."), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff_mime(&webp), Some("image/webp"));
        // Text / unknown → None (the model should use run_kaish for these).
        assert_eq!(sniff_mime(b"#!/bin/sh\n"), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[tokio::test]
    async fn view_reads_a_workspace_png_and_emits_a_rig_image_part() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bytes = fake_png(32);
        std::fs::write(root.join("shot.png"), &bytes).unwrap();

        let tool = ViewImage::new(worker_over(&root), &root);
        let v = tool.view("shot.png").await.expect("view should succeed");

        // The hybrid envelope rig parses into an image part.
        assert!(v["response"].as_str().unwrap().contains("image/png"));
        let part = &v["parts"][0];
        assert_eq!(part["type"], "image");
        assert_eq!(part["mimeType"], "image/png");
        // The data round-trips to exactly the file's bytes.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(part["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, bytes, "the model sees the real file bytes");
    }

    #[tokio::test]
    async fn an_out_of_workspace_path_errors_with_a_copy_it_in_fix() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(inside.path()).unwrap();
        // A real file that exists, just not in the workspace — so canonicalize
        // succeeds and the boundary check (not a missing file) is what fires.
        let stray = std::fs::canonicalize(outside.path())
            .unwrap()
            .join("secret.png");
        std::fs::write(&stray, fake_png(4)).unwrap();

        let tool = ViewImage::new(worker_over(&root), &root);
        let err = tool
            .view(stray.to_str().unwrap())
            .await
            .expect_err("outside the workspace must be refused");
        let msg = err.to_string();
        assert!(msg.contains("OUTSIDE"), "names the boundary: {msg}");
        assert!(msg.contains("cp "), "suggests copying it in: {msg}");
        assert!(msg.contains("workspace"), "{msg}");
    }

    #[tokio::test]
    async fn a_non_image_file_says_use_run_kaish() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("main.rs"), b"fn main() {}\n").unwrap();

        let tool = ViewImage::new(worker_over(&root), &root);
        let err = tool
            .view("main.rs")
            .await
            .expect_err("a text file is not an image");
        assert!(err.to_string().contains("run_kaish"), "{err}");
    }

    #[tokio::test]
    async fn an_oversized_image_errors_loudly_instead_of_resizing() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("big.png"), fake_png(4096)).unwrap();

        let tool = ViewImage::new(worker_over(&root), &root).with_max_bytes(512);
        let err = tool
            .view("big.png")
            .await
            .expect_err("past the cap must be a loud error");
        assert!(err.to_string().contains("larger than"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_file_points_at_run_kaish_to_find_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let tool = ViewImage::new(worker_over(&root), &root);
        let err = tool
            .view("nope.png")
            .await
            .expect_err("a missing file errors");
        assert!(err.to_string().contains("no file found"), "{err}");
    }
}
