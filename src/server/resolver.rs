//! The shared **resolution glue** both front doors run: the MCP handler
//! ([`super::KaiboHandler`]) and the CLI (`crate::cli`). A [`Resolver`] holds the
//! resolved [`Config`] plus the canonicalized containment state (allowed trees +
//! the inferred default root), and turns a call's raw inputs into the concrete
//! pieces a consultation needs: a contained project root, a cast, per-call model
//! overrides, live [`Arm`]s, the operator's house rules / orientation / prompts,
//! and classified attachments (with the vision gate).
//!
//! The two front doors differ only in transport (MCP notifications vs. stderr
//! lines) and lifecycle (a long-lived server vs. a one-shot process); the
//! *resolution* is identical, so it lives here once. [`Resolver::from_config`] is
//! the single construction point — it computes the allowed set and the default
//! root exactly as the server always has, so a CLI invocation's cwd joins the
//! boundary the same way a launched stdio server's cwd does.
//!
//! The containment methods (`resolve_root`, `resolve_attachments`, and the private
//! `contained`/`containing_tree`) live in the sibling `containment.rs` as a split
//! `impl Resolver`, next to the boundary doc-comment they belong with.
//!
//! Not pure lookup glue: most methods here are cheap config/registry reads, but the
//! attachment resolvers (`resolve_consult_attachments`, `resolve_attachments`) are the
//! **heavyweight exception** — they spawn a read-only [`KaishWorker`](crate::sandbox::KaishWorker)
//! and read file bytes through its VFS (which is *why* a `Resolver` carries the
//! sandbox config, and why those two are `async`). Don't refactor on the assumption
//! that resolution is side-effect-free I/O-free lookup; attachment resolution mounts a
//! kernel and does real (read-only) filesystem work.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::ErrorData as McpError;

use crate::config::{Cast, Config, Lane, ModelRole, ModelSlot};
use crate::consult::{Arm, PromptOverrides};

/// The resolution state shared by the MCP handler and the CLI: the resolved
/// config plus the canonicalized containment boundary. Cheap to `Clone` (every
/// field is an `Arc` or `Copy`), so the per-request handler clones share one.
#[derive(Clone)]
pub struct Resolver {
    /// The resolved configuration: backend + cast registries, defaults, default
    /// root and cast. `Arc` because it's immutable after startup and shared with
    /// the handler (which keeps its own clone of the same `Arc`).
    pub(crate) config: Arc<Config>,
    /// The canonicalized allowed path trees. A per-call path must canonicalize to
    /// at-or-under one of these. Computed once from config.root/allow_paths;
    /// falls back to the canonicalized cwd when both are empty.
    allowed_set: Arc<Vec<PathBuf>>,
    /// The effective default root a call uses when it omits `path` — the explicit
    /// `--root`/config value, or the launch/invocation cwd inferred when it falls
    /// inside the allowed set. Canonicalized. `None` only when no root was
    /// configured *and* the cwd is outside the allowed set. `pub(super)` so the
    /// split containment `impl` in `containment.rs` reads it.
    pub(super) default_root: Arc<Option<PathBuf>>,
    /// True when [`default_root`](Self::default_root) was inferred from the cwd
    /// rather than configured explicitly. Surfaced in the scope section and
    /// `kaibo://config` so the boundary stays legible.
    default_root_inferred: bool,
}

impl Resolver {
    /// Build the resolver from a resolved [`Config`], computing the canonicalized
    /// allowed set and the default root — the containment setup both front doors
    /// share. A nonexistent or non-directory entry in root / allow_paths is a loud
    /// construction error (a path that can't be canonicalized can't bound anything).
    ///
    /// Without an explicit `--root`, the process cwd does double duty: it is both the
    /// zero-config allowed tree *and* the inferred default root — adopted whenever it
    /// falls inside the allowed set, so a call may omit `path` in the common
    /// single-workspace case. A cwd the containment check would reject is never
    /// adopted (an `--allow-path` that excludes it leaves the default root unset).
    pub fn from_config(config: Arc<Config>) -> Result<Self> {
        // Build the canonicalized allowed set. Each entry is canonicalized now so
        // the `starts_with` containment check is sound (symlinks resolved, `..`
        // collapsed). A nonexistent path can't bound anything — loud error.
        let mut allowed: Vec<PathBuf> = Vec::new();
        // The explicit default root, if `--root` was configured — canonicalized so it
        // doubles as both an allowed tree and the call's default root.
        let mut explicit_root: Option<PathBuf> = None;
        if let Some(root) = &config.root {
            let canon = std::fs::canonicalize(root)
                .with_context(|| format!("canonicalizing --root {}", root.display()))?;
            if !canon.is_dir() {
                anyhow::bail!("--root {} is not a directory", canon.display());
            }
            allowed.push(canon.clone());
            explicit_root = Some(canon);
        }
        for p in &config.allow_paths {
            let canon = std::fs::canonicalize(p)
                .with_context(|| format!("canonicalizing --allow-path {}", p.display()))?;
            if !canon.is_dir() {
                anyhow::bail!("--allow-path {} is not a directory", canon.display());
            }
            allowed.push(canon);
        }

        // Resolve the default root and the allowed-set cwd fallback together. With an
        // explicit `--root`, that is the default root and no cwd is consulted. Without
        // one, the launch/invocation cwd is both the zero-config allowed tree and the
        // natural default root — adopt it whenever it falls inside the allowed set. We
        // never adopt a cwd the containment check would then reject.
        let (default_root, default_root_inferred): (Option<PathBuf>, bool) = match explicit_root {
            Some(root) => (Some(root), false),
            None => {
                let cwd = std::env::current_dir()
                    .context("could not determine current directory for the default root")?;
                let cwd_canon = std::fs::canonicalize(&cwd)
                    .with_context(|| format!("canonicalizing cwd {}", cwd.display()))?;
                if allowed.is_empty() {
                    // Zero config: the workspace is the whole boundary. Push it here,
                    // before the guard below, so the `starts_with` check adopts cwd as
                    // the default root in the zero-config case.
                    allowed.push(cwd_canon.clone());
                }
                if allowed.iter().any(|tree| cwd_canon.starts_with(tree)) {
                    (Some(cwd_canon), true)
                } else {
                    (None, false)
                }
            }
        };

        Ok(Self {
            config,
            allowed_set: Arc::new(allowed),
            default_root: Arc::new(default_root),
            default_root_inferred,
        })
    }

    /// The canonicalized allowed path trees (owned clone) — for startup logging,
    /// the `kaibo://config` resource, and tests.
    pub(crate) fn allowed_set(&self) -> Vec<PathBuf> {
        (*self.allowed_set).clone()
    }

    /// The canonicalized allowed path trees as a borrowed slice — the cheap read
    /// path for renderers that only need to iterate.
    pub(crate) fn allowed_trees(&self) -> &[PathBuf] {
        &self.allowed_set
    }

    /// The effective default root a call resolves to when it omits `path`.
    pub(crate) fn default_root(&self) -> Option<PathBuf> {
        (*self.default_root).clone()
    }

    /// The effective default root, borrowed.
    pub(crate) fn default_root_ref(&self) -> Option<&Path> {
        (*self.default_root).as_deref()
    }

    /// Whether [`default_root`](Self::default_root) was inferred from the cwd.
    pub(crate) fn default_root_inferred(&self) -> bool {
        self.default_root_inferred
    }

    /// The allowed tree (or followed-worktree root) that contains `canon`, or `None`
    /// when it's outside the boundary — the *which-tree* sibling of `contained` (which
    /// is just `is_some()` on this). A static `allow_path` wins over a followed
    /// worktree. The returned root is what an attachment read mounts a read-only kaish
    /// worker at, so the VFS refuses a symlink escaping *that* tree.
    pub(super) fn containing_tree(&self, canon: &Path) -> Option<PathBuf> {
        if let Some(tree) = self.allowed_set.iter().find(|tree| canon.starts_with(tree)) {
            return Some(tree.clone());
        }
        if self.config.follow_worktrees {
            for tree in self.allowed_set.iter() {
                if let Some(common) = crate::worktree::common_git_dir(tree) {
                    if let Some(wt) = crate::worktree::vouched_worktrees(&common)
                        .into_iter()
                        .find(|wt| canon.starts_with(wt))
                    {
                        return Some(wt);
                    }
                }
            }
        }
        None
    }

    /// The shared "outside the allowed set" rejection, naming the boundary and the
    /// three widening knobs.
    pub(super) fn containment_error(&self, raw: &Path, canon: &Path) -> McpError {
        let trees: Vec<String> = self
            .allowed_set
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        McpError::invalid_params(
            format!(
                "path {} resolves to {}, which is outside the allowed set [{}]. \
                 To widen the boundary: pass --allow-path DIR on the command line, \
                 set KAIBO_ALLOW_PATHS=DIR (colon-separated), or add \
                 `[server] allow_paths = [\"DIR\"]` in config.toml. The config and env \
                 forms expand `$VAR` / `${{VAR}}` and a leading `~`, so a scratch dir \
                 reads portably as `\"$TMPDIR\"` / `\"$XDG_RUNTIME_DIR/...\"`.",
                raw.display(),
                canon.display(),
                trees.join(", "),
            ),
            None,
        )
    }

    /// The worktrees the follow feature currently admits *beyond* the static allowed
    /// set — for the `kaibo://config` runtime section, so the live boundary stays
    /// legible. Recomputed on each read (it reflects worktrees that exist now).
    /// Empty when the feature is off or nothing extra is reachable. Deduplicated and
    /// sorted for a stable resource.
    pub(crate) fn followed_worktrees(&self) -> Vec<PathBuf> {
        if !self.config.follow_worktrees {
            return Vec::new();
        }
        let mut found: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        for tree in self.allowed_set.iter() {
            let Some(common) = crate::worktree::common_git_dir(tree) else {
                continue;
            };
            for wt in crate::worktree::vouched_worktrees(&common) {
                if !self.allowed_set.iter().any(|t| wt.starts_with(t)) {
                    found.insert(wt);
                }
            }
        }
        found.into_iter().collect()
    }

    /// Resolve a call's cast: the explicit name (looked up in the registry, by name
    /// or alias), else the server's default cast. An unknown name is a parameter error
    /// naming the available casts. Returns an owned clone so the caller can layer
    /// per-call model overrides onto it.
    pub(crate) fn resolve_cast(&self, cast: Option<String>) -> Result<Cast, McpError> {
        let name = cast.unwrap_or_else(|| self.config.default_cast.clone());
        self.config
            .resolve_cast(&name)
            .cloned()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))
    }

    /// Refuse an interactive tool (`consult`/`consult_submit`/`oneshot`) on a cast
    /// whose synth runs on an offline lane. A `Batch` synth is a big, slow, expensive
    /// model tuned for free offline batch latency; a `Direct` synth is a big local
    /// model kaibo runs itself — either way, driving it through an interactive tool
    /// loop is the wrong-and-costly mistake this gate stops. Points the caller at the
    /// lane that fits.
    pub(crate) fn reject_offline_cast(&self, cast: &Cast, tool: &str) -> Result<(), McpError> {
        match cast.synth_lane() {
            Some(Lane::Batch) => Err(McpError::invalid_params(
                format!(
                    "cast `{}`'s synth runs on the `batch` lane — submit it with \
                     `batch_submit`, not `{tool}`. It's a big, slow model tuned for free \
                     offline batch latency; running it interactively would be slow and \
                     expensive. Pick an interactive cast for `{tool}`.",
                    cast.name
                ),
                None,
            )),
            Some(Lane::Direct) => Err(McpError::invalid_params(
                format!(
                    "cast `{}`'s synth runs offline (`lane = \"direct\"`) — interactive \
                     tools need an interactive synth. Pick an interactive cast for `{tool}`.",
                    cast.name
                ),
                None,
            )),
            None => Ok(()),
        }
    }

    /// Refuse `batch_submit` (`kaibo batch submit`) on a cast whose synth isn't on the
    /// `batch` lane specifically — the other half of the lane split. A batch cast must
    /// positively declare `lane = "batch"` on its synth slot, so an ordinary interactive
    /// cast is never silently batched (an accidental Opus/Pro batch is just as costly the
    /// other way), and a `direct` cast — offline, but not batch — gets its own honest
    /// message rather than the generic "not a batch cast" one. Points the caller at the
    /// built-in batch casts.
    pub(crate) fn require_batch_cast(&self, cast: &Cast) -> Result<(), McpError> {
        match cast.synth_lane() {
            Some(Lane::Batch) => Ok(()),
            Some(Lane::Direct) => Err(McpError::invalid_params(
                format!(
                    "cast `{}`'s synth runs on the `direct` lane, not `batch` — \
                     `batch_submit` needs a synth slot with `lane = \"batch\"`.",
                    cast.name
                ),
                None,
            )),
            None => Err(McpError::invalid_params(
                format!(
                    "cast `{}` is not a batch cast — `batch_submit` needs a cast whose synth \
                     slot declares `lane = \"batch\"` (the built-ins `gemini-batch`, \
                     `anthropic-batch`, or your own in config.toml). For an interactive \
                     answer, use `consult`/`oneshot`.",
                    cast.name
                ),
                None,
            )),
        }
    }

    /// Apply a per-call model override to one of `cast`'s slots.
    ///
    /// The model id rides *verbatim* — an id containing `/` (HuggingFace-style
    /// `org/model`) is still one id, never parsed for a backend. Retargeting is the
    /// explicit `backend` arg's job: when set, it resolves (aliases included) and the
    /// slot is replaced wholesale, which also works on a role the cast doesn't carry.
    /// Either way the configured slot's pins/tunables are dropped — they described the
    /// *configured* model; the new id classifies fresh.
    pub(crate) fn override_model(
        &self,
        cast: &mut Cast,
        role: ModelRole,
        model: &str,
        backend: Option<&str>,
    ) -> Result<(), McpError> {
        let model = model.trim();
        if model.is_empty() {
            return Err(McpError::invalid_params(
                format!("the {} model id is empty", role.key()),
                None,
            ));
        }
        let backend = match backend {
            Some(name) => self
                .config
                .resolve_backend(name)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?
                .name
                .clone(),
            None => cast.slot(role).map(|s| s.backend.clone()).ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "cast {:?} has no {} slot to override — pass the matching \
                         backend override arg to target one",
                        cast.name,
                        role.key()
                    ),
                    None,
                )
            })?,
        };
        cast.slots.insert(role, ModelSlot::bare(backend, model));
        Ok(())
    }

    /// The tool-input face of [`override_model`](Self::override_model): folds one
    /// tool's `(model, backend)` override args onto `cast`'s `role` slot. A backend
    /// arg without its model arg is a loud parameter error naming both spellings.
    pub(crate) fn apply_model_override(
        &self,
        cast: &mut Cast,
        role: ModelRole,
        model: Option<&str>,
        backend: Option<&str>,
        model_arg: &str,
        backend_arg: &str,
    ) -> Result<(), McpError> {
        match (model, backend) {
            (Some(model), backend) => self.override_model(cast, role, model, backend),
            (None, Some(_)) => Err(McpError::invalid_params(
                format!(
                    "{backend_arg} was sent without {model_arg} — a backend override \
                     needs the model id to run there"
                ),
                None,
            )),
            (None, None) => Ok(()),
        }
    }

    /// Resolve one of `cast`'s slots into a live [`Arm`] for `role`. A missing slot is
    /// the loud call-time gap ("cast `x` has no synth slot"); a backend that fails to
    /// build (key resolution, client init) is an internal error.
    pub(crate) fn arm(&self, cast: &Cast, role: ModelRole) -> Result<Arm, McpError> {
        let slot = cast
            .require_slot(role)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let backend = self
            .config
            .resolve_backend(&slot.backend)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Arm::from_slot(backend, slot, role, &self.config.defaults)
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))
    }

    /// Assemble the operator's house rules for this call against the resolved `root`:
    /// the `[context]` files read here, in trusted server-side Rust, and folded into
    /// the phase preamble. A missing *declared* user file is a loud `internal_error`,
    /// never a silent skip. `None` when nothing's configured/present.
    pub(crate) fn house_rules(&self, root: &Path) -> Result<Option<Arc<str>>, McpError> {
        self.config
            .context
            .assemble(root)
            .map(|opt| opt.map(Arc::from))
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))
    }

    /// Assemble the static repo-orientation map for this call against the resolved
    /// `root` — the `[orientation]` block injected into the exploring preamble. Runs
    /// the kernel's own `glob` server-side (no model turn). Errors here are real
    /// failures (kernel spawn, unparseable enumeration), not size.
    pub(crate) async fn orientation(&self, root: &Path) -> Result<Option<Arc<str>>, McpError> {
        self.config
            .orientation
            .assemble(root, self.config.sandbox.clone())
            .await
            .map(|opt| opt.map(Arc::from))
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))
    }

    /// Resolve this call's per-phase system prompts for `cast` — the per-slot
    /// `preamble` over the global `[prompts]` table. `cast` is the post-override clone,
    /// so a per-call model override (a bare slot) correctly carries no preamble.
    pub(crate) fn resolved_prompts(&self, cast: &Cast) -> PromptOverrides {
        cast.resolved_prompts(&self.config.prompts)
    }

    /// Resolve caller-named `consult`/`explore` attachments: attach means *the model
    /// sees the bytes*. A text file within `budget` (cumulative, caller order) is read
    /// server-side and inlined into the driver prompt; a text file past it is demoted
    /// to a named path the prompt directs the model to read WHOLE through its shell.
    /// An image is routed to `view_image`. Each path must canonicalize to a regular
    /// file *under `root`* (returned root-relative). Static — the sandbox rides in as a
    /// param so the caller's config drives the read-only VFS the bytes are read through.
    pub(crate) async fn resolve_consult_attachments(
        root: &Path,
        attach: &[String],
        budget: usize,
        sandbox: &crate::sandbox::SandboxConfig,
    ) -> Result<Vec<crate::consult::ConsultAttachment>, McpError> {
        use crate::attach::{check_attachment_bounds, DEFAULT_MAX_ATTACHMENTS};
        check_attachment_bounds(
            attach.len(),
            0,
            DEFAULT_MAX_ATTACHMENTS,
            crate::attach::DEFAULT_MAX_TOTAL_BYTES,
        )
        .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?;
        let mut worker: Option<crate::sandbox::KaishWorker> = None;
        let mut remaining = budget as u64;
        let mut out = Vec::with_capacity(attach.len());
        for p in attach {
            let raw = PathBuf::from(p);
            // A relative path reads from the project root (where the model's shell starts).
            let joined = if raw.is_absolute() {
                raw.clone()
            } else {
                root.join(&raw)
            };
            let canon = std::fs::canonicalize(&joined).map_err(|e| {
                McpError::invalid_params(
                    format!("attached file {} could not be resolved: {e}", raw.display()),
                    None,
                )
            })?;
            let meta = std::fs::metadata(&canon).map_err(|e| {
                McpError::invalid_params(
                    format!("attached file {} could not be read: {e}", canon.display()),
                    None,
                )
            })?;
            if !meta.is_file() {
                return Err(McpError::invalid_params(
                    format!("attached file {} is not a regular file", canon.display()),
                    None,
                ));
            }
            let display_path = if let Ok(rel) = canon.strip_prefix(root) {
                rel.display().to_string()
            } else {
                return Err(McpError::invalid_params(
                    format!(
                        "attached file {} resolves outside the project root {} — consult reads \
                         attachments through its shell, which only mounts that. Paste it into \
                         `context`, or use `oneshot`/`batch` attach (which inline a file from \
                         anywhere in the allowed set).",
                        raw.display(),
                        root.display(),
                    ),
                    None,
                ));
            };
            // Sniff the file's type by content, not extension. This prefix is read with
            // `std::fs`, NOT through the kaish VFS — a bounded exception that only routes
            // (the 16 bytes feed `sniff_mime` to a bool and are dropped). Anything INLINED
            // below is read through the VFS and re-sniffed from full bytes.
            let prefix_is_image = {
                use std::io::Read;
                let mut buf = [0u8; 16];
                let n = std::fs::File::open(&canon)
                    .and_then(|mut f| f.read(&mut buf))
                    .unwrap_or(0);
                crate::view_image::sniff_mime(&buf[..n]).is_some()
            };
            if prefix_is_image {
                out.push(crate::consult::ConsultAttachment::Image { path: display_path });
                continue;
            }
            // Text past the remaining inline budget is demoted, not refused: this model
            // CAN read the file itself, and the prompt orders it to — whole, paged.
            if meta.len() > remaining {
                out.push(crate::consult::ConsultAttachment::TextOversize {
                    path: display_path,
                    size: meta.len(),
                });
                continue;
            }
            // Within budget: inline. The read is VFS-mounted — these bytes enter context.
            if worker.is_none() {
                worker = Some(
                    crate::sandbox::KaishWorker::spawn_with(root, sandbox.clone()).map_err(
                        |e| {
                            McpError::internal_error(
                                format!("attachment reader for {}: {e:#}", root.display()),
                                None,
                            )
                        },
                    )?,
                );
            }
            // Read at most one byte past `remaining` — bounds the stat-then-read TOCTOU
            // window: a raced-huge file comes back as `remaining + 1` bytes, which the
            // length check below demotes to `TextOversize`, never an OOM.
            let bytes = worker
                .as_ref()
                .expect("worker was just spawned")
                .read_file_capped(canon.clone(), remaining + 1)
                .await
                .map_err(|e| {
                    McpError::invalid_params(
                        format!("attached file {} could not be read: {e:#}", canon.display()),
                        None,
                    )
                })?;
            // Re-sniff from the (capped) bytes — authoritative for anything inlined.
            if crate::view_image::sniff_mime(&bytes).is_some() {
                out.push(crate::consult::ConsultAttachment::Image { path: display_path });
                continue;
            }
            if bytes.len() as u64 > remaining {
                out.push(crate::consult::ConsultAttachment::TextOversize {
                    path: display_path,
                    size: bytes.len() as u64,
                });
                continue;
            }
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    remaining -= bytes.len() as u64;
                    out.push(crate::consult::ConsultAttachment::Text {
                        path: display_path,
                        body: text.to_string(),
                    });
                }
                Err(_) => {
                    return Err(McpError::invalid_params(
                        format!(
                            "attached file {display_path} is neither valid UTF-8 text nor a \
                             recognized image (png/jpeg/gif/webp) — kaibo won't inline binary, \
                             and the model's shell can't read it either. Convert it first, or \
                             paste the relevant text into `context`."
                        ),
                        None,
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Resolve attachments for a sweep-only tool (`explore`, `deliberate`'s dossier
    /// stage): read-WHOLE directives, never inlined bytes — budget 0, so no file is read
    /// here at all. Images are refused up front: the sweep toolset carries no `view_image`,
    /// so naming one would send the investigator down a dead end (`cat` refuses binary).
    /// Uses the resolver's own sandbox config for the read-only VFS.
    pub(crate) async fn resolve_sweep_attachments(
        &self,
        root: &Path,
        attach: &[String],
        tool: &str,
    ) -> Result<Vec<crate::consult::ConsultAttachment>, McpError> {
        let attachments =
            Self::resolve_consult_attachments(root, attach, 0, &self.config.sandbox).await?;
        if let Some(img) = attachments.iter().find(|a| a.is_image()) {
            return Err(McpError::invalid_params(
                format!(
                    "attached file {} is an image, but {tool}'s investigator reads through \
                     the shell and can't view images — attach it to `consult` with a \
                     vision-capable cast instead",
                    img.path()
                ),
                None,
            ));
        }
        Ok(attachments)
    }

    /// Refuse an image attachment to a vision-blind consult synth. consult never
    /// inlines an image's bytes; the model opens an attached image with the
    /// `view_image` tool, only wired in when the synth is vision-capable. So an image
    /// attached to a blind synth could never be seen — refuse honestly up front,
    /// naming the cast. Text attachments (and the no-attachment case) always pass.
    pub(crate) fn gate_consult_image_attachments(
        attachments: &[crate::consult::ConsultAttachment],
        vision: bool,
        model: &str,
        cast: &str,
    ) -> Result<(), McpError> {
        if !vision && attachments.iter().any(|a| a.is_image()) {
            return Err(McpError::invalid_params(
                format!(
                    "an image was attached, but the consult synth `{model}` on cast `{cast}` \
                     can't see images — consult opens an attached image with its `view_image` \
                     tool, which only a vision-capable synth carries. Use a vision-capable \
                     cast, or attach only text files. `kaibo://config` lists each slot's \
                     `vision`."
                ),
                None,
            ));
        }
        Ok(())
    }

    /// Refuse image attachments to a vision-blind model, naming the cast so the caller
    /// can pick a vision-capable one. Shared by the tool-less attach surfaces (`batch`
    /// and `oneshot`). Text-only attachments (and the no-attachment case) always pass.
    pub(crate) fn gate_image_attachments(
        &self,
        vision: bool,
        attachments: &[crate::attach::Attachment],
        model: &str,
        cast: &str,
    ) -> Result<(), McpError> {
        if !vision
            && attachments
                .iter()
                .any(|a| matches!(a, crate::attach::Attachment::Image { .. }))
        {
            return Err(McpError::invalid_params(
                format!(
                    "an image attachment was given, but the model `{model}` on cast `{cast}` \
                     doesn't accept image input. Use a vision-capable cast/model, or attach \
                     only text files. `kaibo://config` lists each slot's `vision`."
                ),
                None,
            ));
        }
        Ok(())
    }
}
