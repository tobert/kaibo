//! kaibo's read-scope boundary — the containment checks that keep every reachable
//! path inside the allowed set.
//!
//! Each call's path must canonicalize (symlinks and `..` resolved) into an allowed
//! tree (`--root` / `--allow-path`, or a followed worktree of one) before kaish ever
//! mounts it. Attachments obey the same boundary and are read *through the read-only
//! kaish VFS*, so a symlink swapped in after the check can't escape the mount at read
//! time. These are a split inherent `impl` on [`super::Resolver`]; the shared
//! predicates they call (`containing_tree`, `containment_error`) live with the rest of
//! the resolver in `resolver.rs`.

use std::path::PathBuf;

use rmcp::ErrorData as McpError;

use crate::sandbox::KaishWorker;

impl super::Resolver {
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
    pub(crate) fn resolve_root(&self, path: Option<String>) -> Result<PathBuf, McpError> {
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

    /// Read one caller-named file's bytes, contained.
    ///
    /// The single-file sibling of [`resolve_attachments`](Self::resolve_attachments),
    /// and it obeys exactly the same boundary for the same reasons: canonicalize
    /// (symlinks and `..` resolved), require a regular file, require a containing
    /// allowed tree, refuse an over-cap file from its metadata *before* reading it, then
    /// read **through the read-only kaish VFS** rooted at that tree rather than
    /// `std::fs::read`.
    ///
    /// That last step is the one worth not skipping. The canonicalize-and-check above is
    /// a friendly early error; the VFS re-resolves at read time and refuses to follow a
    /// symlink out of the mount, which closes the check-then-open window structurally
    /// instead of by racing a re-check. `tests/containment.rs`'s `mount_layer_symlink_*`
    /// battery is what proves it.
    ///
    /// The read is capped at `max_bytes`, and a file that grows past the cap between the
    /// stat and the read is refused by length rather than slurped — the same
    /// stat-then-read swap `resolve_attachments` guards against.
    pub(crate) async fn read_contained_file(
        &self,
        path: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, McpError> {
        let raw = PathBuf::from(path);
        let canon = std::fs::canonicalize(&raw).map_err(|e| {
            McpError::invalid_params(
                format!(
                    "{} could not be resolved: {e}. Paths are relative to kaibo's launch \
                     directory unless absolute.",
                    raw.display()
                ),
                None,
            )
        })?;
        let meta = std::fs::metadata(&canon).map_err(|e| {
            McpError::invalid_params(format!("{} could not be read: {e}", canon.display()), None)
        })?;
        if !meta.is_file() {
            return Err(McpError::invalid_params(
                format!(
                    "{} is not a regular file. Name the image file itself.",
                    canon.display()
                ),
                None,
            ));
        }
        let tree = self
            .containing_tree(&canon)
            .ok_or_else(|| self.containment_error(&raw, &canon))?;
        // Refused from the file's metadata, before a single byte is read.
        if meta.len() > max_bytes {
            return Err(McpError::invalid_params(
                format!(
                    "{} is {} bytes, over the {max_bytes}-byte cap. Nothing was read.",
                    canon.display(),
                    meta.len()
                ),
                None,
            ));
        }
        let worker = KaishWorker::spawn_with(&tree, self.config.sandbox.clone()).map_err(|e| {
            McpError::internal_error(format!("file reader for {}: {e:#}", tree.display()), None)
        })?;
        // One byte past the cap, so a file raced larger between the stat and this read
        // comes back over-length and is refused below rather than read whole.
        let bytes = worker
            .read_file_capped(canon.clone(), max_bytes + 1)
            .await
            .map_err(|e| {
                McpError::invalid_params(format!("reading {}: {e:#}", canon.display()), None)
            })?;
        if bytes.len() as u64 > max_bytes {
            return Err(McpError::invalid_params(
                format!(
                    "{} is over the {max_bytes}-byte cap. Nothing was stored.",
                    canon.display()
                ),
                None,
            ));
        }
        Ok(bytes)
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

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::server::Resolver;
    use crate::test_support::{capture_tracing, CapturedEvent};
    use std::sync::Arc;

    /// A resolver whose whole allowed set is one temp directory.
    fn resolver_rooted_at(root: &std::path::Path) -> Resolver {
        let mut config = Config::builtin();
        config.root = Some(root.to_path_buf());
        config.infer_cwd = false;
        Resolver::from_config(Arc::new(config)).expect("resolver builds")
    }

    /// The one event a containment refusal must emit, or a description of what was
    /// emitted instead — the assertion message is what a reader debugs from.
    fn refusal_event(events: &[CapturedEvent]) -> CapturedEvent {
        events
            .iter()
            .find(|e| e.has_field("outcome", "refused"))
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "no `outcome = refused` event; captured {} event(s): {:?}",
                    events.len(),
                    events
                        .iter()
                        .map(|e| (e.level, &e.message, &e.fields))
                        .collect::<Vec<_>>()
                )
            })
    }

    /// The boundary firing is the most interesting thing kaibo does, and until now it
    /// left no trace at all: `resolve_root` returns the refusal to the caller and the
    /// server records nothing, so an operator watching a fleet cannot tell a boundary
    /// that never fired from one that fired a hundred times.
    ///
    /// The event carries `outcome = "refused"` as a field — that name is already on
    /// [`SAFE_ATTRIBUTES`](crate::otel_filter::SAFE_ATTRIBUTES), so the *record that a
    /// refusal happened* exports with no content policy change — and names the path in
    /// the message body, which follows kaibo's existing rule for a path: content, and
    /// so redacted from traces unless the operator opts in.
    #[test]
    fn a_refused_root_says_so() {
        let allowed = tempfile::tempdir().expect("temp allowed tree");
        let outside = tempfile::tempdir().expect("temp outside tree");
        let resolver = resolver_rooted_at(allowed.path());
        let asked = outside.path().display().to_string();

        let captured = capture_tracing(async {
            let refusal = resolver.resolve_root(Some(asked.clone()));
            assert!(
                refusal.is_err(),
                "guard: the path is outside the allowed set"
            );
        });

        let event = refusal_event(&captured.events());
        assert_eq!(
            event.level,
            tracing::Level::WARN,
            "a boundary refusal is a warning, not an info line: {event:?}"
        );
        assert!(
            event.message.contains(&asked),
            "the message names the path that was refused: {}",
            event.message
        );
        assert!(
            !event.fields.values().any(|v| v.contains(&asked)),
            "the path stays in the message body, out of the always-exported fields: {:?}",
            event.fields
        );
    }

    /// The same funnel, reached from the attachment surface. `resolve_attachments` and
    /// `resolve_root` share `containment_error` precisely so the two boundaries cannot
    /// drift; this pins that the telemetry cannot drift either.
    #[test]
    fn a_refused_attachment_says_so() {
        let allowed = tempfile::tempdir().expect("temp allowed tree");
        let outside = tempfile::tempdir().expect("temp outside tree");
        let file = outside.path().join("secret.txt");
        std::fs::write(&file, b"outside the boundary").expect("fixture file");
        let resolver = resolver_rooted_at(allowed.path());
        let asked = file.display().to_string();

        let captured = capture_tracing(async {
            let refusal = resolver
                .resolve_attachments(std::slice::from_ref(&asked))
                .await;
            assert!(
                refusal.is_err(),
                "guard: the file is outside the allowed set"
            );
        });

        let event = refusal_event(&captured.events());
        assert!(
            event.message.contains(&asked),
            "the message names the attachment that was refused: {}",
            event.message
        );
    }

    /// And from the single-file surface `view_image` reads through.
    #[test]
    fn a_refused_file_read_says_so() {
        let allowed = tempfile::tempdir().expect("temp allowed tree");
        let outside = tempfile::tempdir().expect("temp outside tree");
        let file = outside.path().join("secret.png");
        std::fs::write(&file, b"outside the boundary").expect("fixture file");
        let resolver = resolver_rooted_at(allowed.path());
        let asked = file.display().to_string();

        let captured = capture_tracing(async {
            let refusal = resolver.read_contained_file(&asked, 1 << 20).await;
            assert!(
                refusal.is_err(),
                "guard: the file is outside the allowed set"
            );
        });

        let event = refusal_event(&captured.events());
        assert!(
            event.message.contains(&asked),
            "the message names the file that was refused: {}",
            event.message
        );
    }

    /// A path *inside* the boundary must stay quiet. The warn is a signal an operator
    /// can alert on, which it stops being the moment an ordinary call emits it.
    #[test]
    fn an_allowed_root_emits_no_refusal() {
        let allowed = tempfile::tempdir().expect("temp allowed tree");
        let resolver = resolver_rooted_at(allowed.path());
        let asked = allowed.path().display().to_string();

        let captured = capture_tracing(async {
            resolver
                .resolve_root(Some(asked))
                .expect("guard: the path is inside the allowed set");
        });

        assert!(
            !captured
                .events()
                .iter()
                .any(|e| e.has_field("outcome", "refused")),
            "an allowed path emits no refusal: {:?}",
            captured.events()
        );
    }
}
