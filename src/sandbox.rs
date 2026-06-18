//! The read-only kaish sandbox the explorer model runs inside.
//!
//! Safety is layered, with the heaviest lever at compile time. The read-only
//! invariant is carried entirely by structural levers — there is no hardcoded
//! builtin denylist; every mutation path is refused by construction:
//!
//! 0. **Minimal feature surface (primary).** kaibo depends on kaish-kernel with
//!    only the `localfs` axis; `subprocess`, `git`, `host`, and `os-integration`
//!    are OFF (see Cargo.toml). So `exec`/`spawn`/`kill`/`git`/`ps` are *never
//!    compiled in* — the dangerous surface doesn't exist, it isn't merely blocked.
//! 1. The project root is mounted with [`LocalFs::read_only`], so every write,
//!    delete, `mkdir`, `touch` (its mtime bump now routes through the backend's
//!    `set_mtime`, which the read-only mount rejects), etc. returns
//!    `PermissionDenied` at the VFS layer — regardless of which builtin issued it.
//!    This stops `rm`/`mv`/`cp`/`mkdir`/`tee`/`write`/`touch`.
//! 2. `/` is [`MemoryFs`], so paths *outside* the project resolve to ephemeral
//!    in-memory scratch that vanishes with the kernel and never touches disk —
//!    including where `mktemp` lands (it resolves its parent through the VFS, so a
//!    temp file is created in memory, never on the real `/tmp`).
//! 3. [`KernelConfig::with_allow_external_commands(false)`] — belt-and-suspenders
//!    now that `subprocess` is off; refuses any external-command path.
//!
//! There used to be a fourth lever: a hardcoded `DENYLIST` shadow-blocking `touch`
//! and `mktemp`, which reached real state directly via `std::fs` and bypassed the
//! mount. That leak was fixed upstream in kaish (`touch` routes mtime through a new
//! `set_mtime` backend op; `mktemp` resolves its parent through the VFS instead of
//! the host), so the shadow is gone — structural beats honor-system, and we no
//! longer carry a list that masks whether the real guard works.
//!
//! The [`Blocked`] wrapper survives for one job: the config-driven
//! `[sandbox].disable_builtins`, which lets an operator make the box *stricter* by
//! shadowing a builtin that would otherwise work (e.g. forbid `cat`). `ToolRegistry`
//! has no `unregister`, but `register` overwrites by name, so [`Blocked`] keeps the
//! real tool's schema (help/validation stay intact) while refusing to execute.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{Tool, ToolArgs, ToolCtx, ToolRegistry, ToolSchema};
use kaish_kernel::vfs::{ByteBudget, Filesystem, LocalFs, MemoryFs, VfsRouter};
use kaish_kernel::{
    IgnoreConfig, Kernel, KernelBackend, KernelConfig, LocalBackend, OutputLimitConfig,
};

/// Wraps a real builtin, preserving its identity and schema but refusing to run.
struct Blocked {
    inner: Arc<dyn Tool>,
}

#[async_trait]
impl Tool for Blocked {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn schema(&self) -> ToolSchema {
        self.inner.schema()
    }

    async fn execute(&self, _args: ToolArgs, _ctx: &mut dyn ToolCtx) -> ExecResult {
        // Exit **126** = "blocked by the read-only sandbox". Distinct from the
        // kernel's other non-zero codes a caller may see: 124 = killed for
        // exceeding [`KAISH_EXEC_TIMEOUT`], 130 = cancelled, 127 = command not
        // found, and any other non-zero = the script itself failed. 126 also
        // collides with POSIX "not executable", so an automated caller must read
        // the message, not just the code, to classify a sandbox block.
        ExecResult::failure(
            126,
            format!(
                "{}: disabled in kaibo's read-only sandbox",
                self.inner.name()
            ),
        )
    }
}

/// Shadow-block the caller-supplied `disable` builtins (the config's
/// `[sandbox].disable_builtins`).
///
/// This can only make the box *stricter* — it adds blocks, never removes the
/// structural read-only guards (the mount, MemoryFs, compiled-out axes). A name
/// that isn't a registered builtin is a no-op *here* (already validated loudly at
/// startup, see [`builtin_names`]). The read-only invariant needs no entries here:
/// every mutation is refused structurally (see the module doc).
fn apply_disabled_builtins(registry: &mut ToolRegistry, disable: &[String]) {
    for name in disable {
        if let Some(inner) = registry.get(name) {
            registry.register_arc(Arc::new(Blocked { inner }));
        }
    }
}

/// Per-exec wall-clock budget for a kaish script in the sandbox.
///
/// A read-only read/grep/find over a project almost never legitimately needs
/// this long; the budget exists so a hung provider script or a pathological loop
/// can't wedge the single serial worker thread (there's no `max_turns` braking a
/// caller-facing `run_kaish`). On elapse the kernel cancels and the script exits
/// **124** — distinct from 126 (a builtin refused by the read-only sandbox). 30s
/// matches a patient MCP caller while still bounding a runaway.
pub const KAISH_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// Default per-script output cap (matches `OutputLimitConfig::agent()`'s 8 KB): a
/// single wide `cat`/`rg` can't flood the caller's context. Override via
/// `[sandbox].output_limit_bytes`.
pub const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 8 * 1024;

/// Default cap on the `/` scratch `MemoryFs` (64 MB). Scratch is a feature — a
/// redirect or `mktemp` lands here, never on the read-only project — but unbounded
/// it's a host-memory liability: a steered or pathological explorer holds its
/// kernel for a whole phase loop, and `for … >> /grow` writes RAM until the exec
/// timeout. The budget makes that loud (ENOSPC-style) instead. Generous on purpose;
/// override via `[sandbox].scratch_limit_bytes` (stricter is the usual move).
pub const DEFAULT_SCRATCH_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// The resolved kaish-kernel runtime config kaibo hands the kernel builder.
///
/// Two distinct concerns travel together because both are consumed at one place
/// (the kernel build), but they map to *separate* TOML stanzas:
///
/// - The **safety/limit** fields come from `[sandbox]`. These can only make the box
///   *stricter* — `disable_builtins` shadow-blocks additional builtins, and the
///   dangerous axes aren't even compiled in. The read-only invariant is structural
///   (the mount, MemoryFs, compiled-out axes) and is *not* a config knob.
/// - [`SandboxConfig::ignore`] is **discovery/noise** policy from `[kaish.ignore]` —
///   which gitignore-format files the file-walking builtins (`glob`/`rg`/`ls`/`find`,
///   and the orientation repo-map that rides the same kernel) honor. It changes what
///   the explorer *sees*, never what it's *allowed* to read; containment and
///   read-only are unaffected.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Per-kaish-script wall-clock budget; exceeding it exits 124.
    pub exec_timeout: Duration,
    /// Max bytes of script output before truncation (exit 3 + head/tail sample).
    pub output_limit_bytes: usize,
    /// Cap on the `/` scratch `MemoryFs` in bytes; a write past it fails loudly
    /// (`StorageFull`), never silently eating RAM. See [`DEFAULT_SCRATCH_LIMIT_BYTES`].
    pub scratch_limit_bytes: u64,
    /// Builtins to shadow-block on top of the structural read-only guards.
    pub disable_builtins: Vec<String>,
    /// Ignore-file policy for the file-walking builtins, sourced from
    /// `[kaish.ignore]`. Defaults to [`IgnoreConfig::agent`] (`.gitignore` + built-in
    /// defaults, enforced scope) so omitting the stanza preserves today's behavior.
    pub ignore: IgnoreConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            exec_timeout: KAISH_EXEC_TIMEOUT,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
            scratch_limit_bytes: DEFAULT_SCRATCH_LIMIT_BYTES,
            disable_builtins: Vec::new(),
            ignore: IgnoreConfig::agent(),
        }
    }
}

/// Build a kernel that can *read* everything under `root` and *mutate* nothing,
/// with default sandbox limits.
///
/// The project is mounted at its real absolute path (mirroring kaish's own
/// sandbox layout) so familiar paths like `/home/me/proj/src/main.rs` resolve
/// transparently, while `cat`/`grep`/`find` see the live tree.
pub fn build_readonly_kernel(root: impl Into<PathBuf>) -> Result<Kernel> {
    build_readonly_kernel_with(root, &SandboxConfig::default())
}

/// Like [`build_readonly_kernel`] but with an explicit per-exec `timeout`.
///
/// Exposed so tests can drive the timeout path with a short budget without
/// waiting the full [`KAISH_EXEC_TIMEOUT`].
pub fn build_readonly_kernel_with_timeout(
    root: impl Into<PathBuf>,
    timeout: Duration,
) -> Result<Kernel> {
    build_readonly_kernel_with(
        root,
        &SandboxConfig {
            exec_timeout: timeout,
            ..SandboxConfig::default()
        },
    )
}

/// Build the read-only kernel with explicit [`SandboxConfig`] limits.
pub fn build_readonly_kernel_with(
    root: impl Into<PathBuf>,
    sandbox: &SandboxConfig,
) -> Result<Kernel> {
    Ok(build_readonly_kernel_and_vfs(root, sandbox)?.0)
}

/// Like [`build_readonly_kernel_with`], but also hand back the project's VFS router.
///
/// That router is the *only* handle that reads a file's bytes directly: under
/// `Kernel::with_backend` the kernel's own [`Kernel::vfs`] is just its internal
/// `/v/*` scratch — our project mounts live on the backend's router, reachable only
/// through execution (capped, text) or this handle. [`KaishWorker::read_file`]
/// (for `view_image`) needs the raw bytes uncapped, so the worker keeps this router
/// and reads through it. It's the *same* router execution uses, so a read stays
/// governed by the read-only project mount and the `/` scratch — containment and
/// read-only stay structural, identical to `run_kaish`.
fn build_readonly_kernel_and_vfs(
    root: impl Into<PathBuf>,
    sandbox: &SandboxConfig,
) -> Result<(Kernel, Arc<VfsRouter>)> {
    let root = root.into();
    let mount_point = root.to_string_lossy().to_string();

    let mut vfs = VfsRouter::new();
    // Ephemeral scratch for anything outside the project; never hits disk. Bounded
    // by an owned, labeled budget so a runaway redirect (`for … >> /grow`) fails
    // ENOSPC-style instead of eating host RAM for the kernel's whole lifetime. The
    // budget rides the mount, not `KernelConfig` — the `with_backend` path below
    // ignores `vfs_budget_bytes`, the embedder owns the VFS.
    let scratch_budget = Arc::new(ByteBudget::labeled(
        sandbox.scratch_limit_bytes,
        "kaibo-scratch",
    ));
    vfs.mount("/", MemoryFs::with_budget(scratch_budget));
    // The project itself: real files, read-only.
    vfs.mount(&mount_point, LocalFs::read_only(&root));

    // Keep a handle to the project router before it disappears into the backend, so
    // the worker can read bytes through these mounts (the kernel's own `vfs()` won't
    // carry them — see this fn's doc).
    let project_vfs = Arc::new(vfs);
    let backend: Arc<dyn KernelBackend> = Arc::new(LocalBackend::new(project_vfs.clone()));

    // Start from the agent output limit (head+tail truncation) and set the configured
    // cap so a runaway `cat` can't flood the caller's context; the timeout bounds
    // wall clock. Both guards matter for a caller-facing `run_kaish` with no turn cap.
    let mut output_limit = OutputLimitConfig::agent();
    output_limit.set_limit(Some(sandbox.output_limit_bytes));
    // `agent()` already seeds an `.gitignore`-aware filter; override with the resolved
    // `[kaish.ignore]` policy so configured extra ignore files (`.claudeignore`, …)
    // and scope/default toggles reach every file-walking builtin — and the
    // orientation repo-map, which enumerates through this same kernel.
    let config = KernelConfig::agent()
        .with_cwd(root)
        .with_allow_external_commands(false)
        .with_request_timeout(sandbox.exec_timeout)
        .with_output_limit(output_limit)
        .with_ignore_config(sandbox.ignore.clone());

    // `with_backend` ignores config.vfs_mode and routes all non-`/v/*` paths
    // through our read-only backend (the read-only invariant). `configure_tools`
    // runs after the default builtins are registered, so any config-driven
    // `disable_builtins` shadows win.
    let disable = sandbox.disable_builtins.clone();
    let kernel = Kernel::with_backend(
        backend,
        config,
        |_| {},
        move |reg| apply_disabled_builtins(reg, &disable),
    )
    .context("failed to build read-only kaish kernel")?;
    Ok((kernel, project_vfs))
}

/// The schemas of every builtin kaibo's kernel registers, sorted by name.
///
/// Used to drive `help builtins` / per-builtin help (the `kaibo://kaish/*`
/// resources) and the composed onboarding instructions. The set is static for a
/// given build — it depends only on the compiled feature axes, not on the project
/// root or VFS mode — so we read it from a throwaway `isolated()` kernel (pure
/// in-memory, no backend) rather than spinning a full read-only mount. Any
/// config-disabled builtins still appear here: shadow-blocking preserves their
/// schema (only execution is refused), so the help surface matches what's
/// registered.
pub fn builtin_schemas() -> Result<Vec<ToolSchema>> {
    let kernel = Kernel::new(KernelConfig::isolated().with_skip_validation(true))
        .context("failed to build schema kernel")?;
    Ok(kernel.tool_schemas())
}

/// The names of every builtin compiled into kaibo's kernel. Used to validate
/// `[sandbox].disable_builtins` at startup so a typo (`"rgg"`) is a loud error
/// rather than a silent no-op that leaves the builtin enabled.
pub fn builtin_names() -> Result<Vec<String>> {
    Ok(builtin_schemas()?.into_iter().map(|s| s.name).collect())
}

/// Run one kaish script in the sandbox and hand back the raw kernel result.
///
/// A non-zero `code` (or non-empty `err`) is a normal in-band outcome the
/// explorer should see — we only `Err` on a kernel-level failure.
pub async fn run(kernel: &Kernel, script: &str) -> Result<ExecResult> {
    kernel
        .execute(script)
        .await
        .context("kaish kernel execution failed")
}

/// A plain, `Send` snapshot of one execution — what crosses the worker boundary.
///
/// [`Kernel::execute`] returns a `!Send` future, so the kernel can never live on
/// rig's (Send-requiring) agent task. We keep it on a dedicated thread and ship
/// back only these owned scalars, which are trivially `Send`.
#[derive(Debug, Clone)]
pub struct KaishOutput {
    pub code: i64,
    pub stdout: String,
    pub stderr: String,
}

impl KaishOutput {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// Flatten one [`ExecResult`] into the `Send` snapshot the worker ships back.
    ///
    /// A text result passes through verbatim. A **binary** result — kaish 0.8.3's
    /// binary-aware builtins (`cat` of a non-UTF-8 file, `dd` without `of=`) now
    /// carry a `Bytes` payload — is the trap: [`ExecResult::text_out`] would
    /// lossy-decode those bytes to mojibake (U+FFFD replacement chars) and hand the
    /// model silent garbage with exit 0. We refuse that corruption: stdout stays
    /// empty and stderr carries an actionable note (byte count + the two real outs —
    /// `view_image` for an image, base64/xxd/redirect for anything else). A pipeline
    /// that *ends* in text (`cat blob | base64`) is a text result and never lands here.
    pub fn from_result(r: &ExecResult) -> Self {
        if let Some(bytes) = r.out_bytes() {
            let mut stderr = format!(
                "output is binary ({} bytes), not text — kaibo won't lossy-decode it. \
                 For an image, call view_image with the path; otherwise pipe through \
                 base64 (or xxd), or redirect to a file.",
                bytes.len()
            );
            // Preserve any real stderr the script also emitted (rare alongside a
            // binary success, but never drop it).
            if !r.err.is_empty() {
                stderr.push('\n');
                stderr.push_str(&r.err);
            }
            return Self {
                code: r.code,
                stdout: String::new(),
                stderr,
            };
        }
        Self {
            code: r.code,
            stdout: r.text_out().into_owned(),
            stderr: r.err.clone(),
        }
    }
}

/// A long-lived, read-only kaish kernel pinned to its own thread.
///
/// The kernel (and its `!Send` execution future) stay on the worker thread; the
/// handle holds only a `Send` channel, so [`KaishWorker::run`]'s future is `Send`
/// and can be awaited from inside a rig tool's `call`. The kernel persists across
/// calls, so the explorer gets shell continuity (cwd, vars) within a session.
///
/// `Clone` shares one kernel thread: the handle is just a `Send` channel sender, so
/// two tools backed by the same worker (e.g. `run_kaish` + `view_image`) drive the
/// *same* read-only kernel rather than spinning a second one.
#[derive(Clone)]
pub struct KaishWorker {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
}

/// A unit of work for the worker thread. Two shapes: run a script (the explorer's
/// whole interface), or read a file's raw bytes straight through the VFS.
enum Job {
    /// Run a kaish script; reply with its captured output.
    Run {
        script: String,
        reply: tokio::sync::oneshot::Sender<KaishOutput>,
    },
    /// Read a file's bytes through the kernel's VFS, *bypassing* the script-output
    /// cap (an image is megabytes; `cat | base64` through `run`'s capped stdout
    /// would truncate it). Same VFS as every read, so containment and read-only stay
    /// structural — an out-of-mount path routes to the empty `/` scratch and 404s.
    /// Reply carries the bytes or a rendered error string (`io::Error` is not `Send`
    /// across the boundary cleanly, and the caller only needs the message).
    Read {
        path: PathBuf,
        reply: tokio::sync::oneshot::Sender<std::result::Result<Vec<u8>, String>>,
    },
}

impl KaishWorker {
    /// Spawn the worker thread and build its read-only kernel rooted at `root`,
    /// with default sandbox limits.
    pub fn spawn(root: impl Into<PathBuf>) -> Result<Self> {
        Self::spawn_with(root, SandboxConfig::default())
    }

    /// Spawn the worker thread and build its read-only kernel rooted at `root`,
    /// with explicit [`SandboxConfig`] limits.
    ///
    /// Blocks until the kernel is built so construction failures surface here
    /// rather than on the first `run`.
    pub fn spawn_with(root: impl Into<PathBuf>, sandbox: SandboxConfig) -> Result<Self> {
        let root = root.into();
        let (jobs_tx, mut jobs_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        std::thread::Builder::new()
            .name("kaibo-kaish".to_string())
            // kaish parses deeply-recursive ASTs on large inputs; mirror kaish-mcp's
            // generous stack rather than the default 2 MiB worker stack.
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = init_tx.send(Err(format!("runtime build failed: {e}")));
                        return;
                    }
                };
                // `block_on` runs the `!Send` kernel future on this thread only.
                rt.block_on(async move {
                    let (kernel, project_vfs) = match build_readonly_kernel_and_vfs(&root, &sandbox)
                    {
                        Ok(kv) => {
                            let _ = init_tx.send(Ok(()));
                            kv
                        }
                        Err(e) => {
                            let _ = init_tx.send(Err(format!("{e:#}")));
                            return;
                        }
                    };
                    while let Some(job) = jobs_rx.recv().await {
                        match job {
                            Job::Run { script, reply } => {
                                let out = match run(&kernel, &script).await {
                                    Ok(r) => KaishOutput::from_result(&r),
                                    Err(e) => KaishOutput {
                                        code: -1,
                                        stdout: String::new(),
                                        stderr: format!("{e:#}"),
                                    },
                                };
                                // Receiver gone (caller cancelled) is fine — drop it.
                                let _ = reply.send(out);
                            }
                            Job::Read { path, reply } => {
                                // Read straight through the *project* VFS router on
                                // this thread. The read-only mount governs what
                                // resolves: under `root` → real bytes; anywhere else →
                                // the empty `/` scratch → NotFound. No output cap
                                // applies — that's a script-execution guard, not a VFS
                                // one. (The kernel's own `vfs()` carries only its
                                // internal `/v/*` scratch under `with_backend`, not
                                // these mounts — hence the retained handle.)
                                let out = project_vfs.read(&path).await.map_err(|e| format!("{e}"));
                                let _ = reply.send(out);
                            }
                        }
                    }
                });
            })
            .context("failed to spawn kaish worker thread")?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self { jobs: jobs_tx }),
            Ok(Err(e)) => Err(anyhow::anyhow!("kaish worker init failed: {e}")),
            Err(_) => Err(anyhow::anyhow!("kaish worker thread exited before init")),
        }
    }

    /// Run one script on the worker's kernel. The returned future is `Send`.
    pub async fn run(&self, script: impl Into<String>) -> Result<KaishOutput> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.jobs
            .send(Job::Run {
                script: script.into(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("kaish worker thread is gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("kaish worker dropped the reply"))
    }

    /// Read a file's raw bytes through the kernel's VFS. The returned future is
    /// `Send`. This is the read path for binary content (`view_image`): it bypasses
    /// the script-output cap (which exists to keep a wide `cat` from flooding the
    /// model's context, not to bound a deliberate single-file read) while keeping the
    /// *same* read-only mount as every other read — so containment and read-only stay
    /// structural. A path that resolves outside the project mount lands in the empty
    /// `/` scratch and comes back `NotFound`.
    ///
    /// `path` should be absolute and already inside the workspace; the caller
    /// (`view_image`) canonicalizes and boundary-checks host-side first so it can
    /// give an actionable out-of-workspace error before ever reaching here.
    pub async fn read_file(&self, path: impl Into<PathBuf>) -> Result<Vec<u8>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.jobs
            .send(Job::Read {
                path: path.into(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("kaish worker thread is gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("kaish worker dropped the reply"))?
            .map_err(|e| anyhow::anyhow!(e))
    }
}
