//! kaibo's read-scope boundary — the containment checks that keep every reachable
//! path inside the allowed set.
//!
//! Each call's path must canonicalize (symlinks and `..` resolved) into an allowed
//! tree (`--root` / `--allow-path`, or a followed worktree of one) before kaish ever
//! mounts it. Attachments obey the same boundary and are read *through the read-only
//! kaish VFS*, so a symlink swapped in after the check can't escape the mount at read
//! time. These are a split inherent `impl` on [`super::KaiboHandler`]; the shared
//! predicates they call (`containing_tree`, `containment_error`) live with the rest of
//! the handler in the parent module.

use std::path::PathBuf;

use rmcp::ErrorData as McpError;

use crate::sandbox::KaishWorker;

impl super::KaiboHandler {
    /// Resolve a call's project root with containment enforcement:
    ///
    /// 1. Select the raw path: the explicit `path` arg, else the effective default
    ///    root (an explicit `--root`, or the launch cwd inferred when it falls inside
    ///    the allowed set). An omitted `path` with no default root is a parameter
    ///    error — not a silent default.
    /// 2. Canonicalize the selected path (resolves symlinks and `..`). A path that
    ///    doesn't exist is `invalid_params` with the canonicalize error.
    /// 3. Require the canonicalized path to be at-or-under one of the allowed trees.
    ///    A violation is `invalid_params` naming the allowed trees and the three
    ///    widening knobs (`--allow-path`, `KAIBO_ALLOW_PATHS`, `[server] allow_paths`).
    ///
    /// Returns the CANONICALIZED path so the kaish mount target is always resolved.
    pub(super) fn resolve_root(&self, path: Option<String>) -> Result<PathBuf, McpError> {
        // Step 1: select the raw path. The default root is the explicit `--root` or
        // the inferred launch cwd (already canonicalized and dir-checked at startup,
        // and guaranteed inside the allowed set); the steps below re-validate it
        // uniformly with an explicit `path`, so there is no special-casing here.
        let raw = match path {
            Some(p) => PathBuf::from(p),
            None => (*self.default_root).clone().ok_or_else(|| {
                McpError::invalid_params(
                    "no `path` provided and the server has no default root \
                     (configure one with --root, or launch kaibo with its cwd \
                     inside the allowed set so the workspace is inferred)",
                    None,
                )
            })?,
        };

        // Step 2: canonicalize — resolves symlinks and `..` so starts_with is sound.
        let canon = std::fs::canonicalize(&raw).map_err(|e| {
            McpError::invalid_params(
                format!("path {} could not be resolved: {e}", raw.display()),
                None,
            )
        })?;

        // Step 2b: require a directory, symmetric with the construction-time check on
        // --root and --allow-path entries. A file path passes canonicalization and
        // containment but makes a degenerate session (cwd is a file); reject it here
        // at the parameter boundary with a clear error rather than failing deep in kaish.
        if !canon.is_dir() {
            return Err(McpError::invalid_params(
                format!("path {} is not a directory", canon.display()),
                None,
            ));
        }

        // Step 3: containment check — must be at-or-under an allowed tree (or a
        // followed worktree of one). Shared with `resolve_attachments` so a file
        // attachment obeys the exact same boundary as a session root, not a parallel
        // check that could drift.
        if self.contained(&canon) {
            return Ok(canon);
        }
        Err(self.containment_error(&raw, &canon))
    }

    /// Is `canon` (already canonicalized) inside the allowed boundary? At-or-under a
    /// static allowed tree, or — when `follow_worktrees` is on — inside a linked git
    /// worktree that an already-allowed repo vouches for (a sibling branch checkout
    /// reachable without an --allow-path). Worktree membership is resolved by reading
    /// git's link files, never by running git (subprocess/git are compiled out — see
    /// sandbox.rs), and trust flows only outward from the allowed repo (we enumerate
    /// the worktrees its own common git dir names and never consult the candidate's
    /// `.git`), so a foreign dir can't forge its way in. The single containment
    /// predicate — `resolve_root` and `resolve_attachments` both defer to it.
    fn contained(&self, canon: &std::path::Path) -> bool {
        self.containing_tree(canon).is_some()
    }

    /// Resolve caller-named attachment paths into [`Attachment`](crate::attach::Attachment)s,
    /// read and encoded server-side so the bytes never transit the calling agent's
    /// context. Each path obeys the *same* boundary as a session root — canonicalize
    /// (symlinks + `..` resolved), require a regular file, then the shared
    /// [`containing_tree`](Self::containing_tree) check (allowed set + followed worktrees)
    /// — so attachments can't read outside the workspace any more than `run_kaish` can.
    ///
    /// Failures are loud and per-path: a missing file, a directory, an over-cap or
    /// non-text/non-image file is a clear `invalid_params`, never a silent skip — an
    /// attachment the caller named but we dropped would be a corrupt answer. An absolute
    /// per-file size ceiling is enforced *before* reading (via the file's metadata) so a
    /// giant file is refused without first slurping it into memory; a batch-level count cap
    /// and cumulative-byte budget ([`check_attachment_bounds`](crate::attach::check_attachment_bounds))
    /// bound the *whole call* the same way — a stray thousand-file glob, or many
    /// individually-legal files summing to an OOM, is refused before the offending read.
    ///
    /// **The read goes through the read-only kaish VFS, not `std::fs::read`** — the same
    /// mechanism `view_image` uses. The canonicalize + containment check above is the
    /// *friendly early error*; the read itself is mounted on a [`KaishWorker`] rooted at
    /// the attachment's containing tree, whose VFS re-resolves at read time and refuses to
    /// follow a symlink out of that tree (proved by `tests/containment.rs`'s
    /// `mount_layer_symlink_*` battery). That closes the check-then-open TOCTOU window the
    /// old `std::fs::read` left open — a path swapped for an out-of-tree symlink after the
    /// check is rejected at the mount layer regardless of timing, structurally rather than
    /// by racing a re-check. One worker is spawned per *distinct* containing tree and
    /// reused across attachments under it, so the common case (files under one project
    /// root) builds a single worker.
    pub async fn resolve_attachments(
        &self,
        paths: &[String],
    ) -> Result<Vec<crate::attach::Attachment>, McpError> {
        use crate::attach::{
            check_attachment_bounds, classify, DEFAULT_MAX_ATTACHMENTS, DEFAULT_MAX_IMAGE_BYTES,
            DEFAULT_MAX_TEXT_BYTES, DEFAULT_MAX_TOTAL_BYTES,
        };
        // The pre-read ceiling: whichever encoding cap is larger. `classify` applies the
        // precise per-encoding cap after sniffing; this just bounds the read itself.
        let read_ceiling = DEFAULT_MAX_TEXT_BYTES.max(DEFAULT_MAX_IMAGE_BYTES);
        // Fail fast on count *before* canonicalizing the whole list (a stray glob could
        // name thousands). The cumulative-byte budget is enforced per file below as the
        // running total grows — before each read, so an oversized batch never slurps in.
        check_attachment_bounds(
            paths.len(),
            0,
            DEFAULT_MAX_ATTACHMENTS,
            DEFAULT_MAX_TOTAL_BYTES,
        )
        .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?;
        // One read-only worker per distinct containing tree, reused across attachments
        // under it (a worker owns a thread + kernel build, so we don't want one per file).
        let mut workers: std::collections::HashMap<PathBuf, KaishWorker> =
            std::collections::HashMap::new();
        let mut out = Vec::with_capacity(paths.len());
        let mut total_bytes: u64 = 0;
        for p in paths {
            let raw = std::path::PathBuf::from(p);
            let canon = std::fs::canonicalize(&raw).map_err(|e| {
                McpError::invalid_params(
                    format!("attachment {} could not be resolved: {e}", raw.display()),
                    None,
                )
            })?;
            // A regular file, not a directory — symmetric with resolve_root's dir
            // check, the mirror image (we inline a file's bytes, not mount a tree).
            let meta = std::fs::metadata(&canon).map_err(|e| {
                McpError::invalid_params(
                    format!("attachment {} could not be read: {e}", canon.display()),
                    None,
                )
            })?;
            if !meta.is_file() {
                return Err(McpError::invalid_params(
                    format!("attachment {} is not a regular file", canon.display()),
                    None,
                ));
            }
            // Same boundary as a session root — and the tree to root the VFS read at.
            let tree = self
                .containing_tree(&canon)
                .ok_or_else(|| self.containment_error(&raw, &canon))?;
            // Bound the read by the absolute ceiling before slurping.
            if meta.len() > read_ceiling as u64 {
                return Err(McpError::invalid_params(
                    format!(
                        "attachment {} is {} bytes, over the {read_ceiling}-byte limit",
                        canon.display(),
                        meta.len()
                    ),
                    None,
                ));
            }
            // Cumulative budget across the batch, checked *before* this file's read so a
            // batch of individually-legal files can't sum to an out-of-memory read. The
            // running total saturates so a crafted size can't wrap past the budget.
            total_bytes = total_bytes.saturating_add(meta.len());
            check_attachment_bounds(
                out.len() + 1,
                total_bytes,
                DEFAULT_MAX_ATTACHMENTS,
                DEFAULT_MAX_TOTAL_BYTES,
            )
            .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?;
            // Read *through the VFS* rooted at the containing tree — see the doc-comment.
            // A swapped escaping symlink is refused at the mount, not read through.
            if !workers.contains_key(&tree) {
                let worker =
                    KaishWorker::spawn_with(&tree, self.config.sandbox.clone()).map_err(|e| {
                        McpError::internal_error(
                            format!("attachment reader for {}: {e:#}", tree.display()),
                            None,
                        )
                    })?;
                workers.insert(tree.clone(), worker);
            }
            // Cap the read one byte past the largest a single file may legally be — the
            // greater of the two per-encoding caps `classify` enforces. The stat above
            // fed the *batch* budget; this bounds the *per-file* read so a stat-then-read
            // swap can't slurp a raced-huge file into memory. An over-cap file comes back
            // truncated at cap+1 and `classify` refuses it loudly by length, exactly as it
            // would refuse an honest over-cap file — same outcome, no OOM window.
            let read_cap = DEFAULT_MAX_TEXT_BYTES.max(DEFAULT_MAX_IMAGE_BYTES) as u64 + 1;
            let bytes = workers[&tree]
                .read_file_capped(canon.clone(), read_cap)
                .await
                .map_err(|e| {
                    McpError::invalid_params(
                        format!("attachment {} could not be read: {e:#}", canon.display()),
                        None,
                    )
                })?;
            // Label the attachment with the caller's path (what they typed), not the
            // canonical one — it's their reference and it's what the model should see.
            out.push(
                classify(p, &bytes, DEFAULT_MAX_TEXT_BYTES, DEFAULT_MAX_IMAGE_BYTES)
                    .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?,
            );
        }
        Ok(out)
    }
}
