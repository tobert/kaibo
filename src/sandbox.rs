//! The read-only kaish sandbox the explorer model runs inside.
//!
//! Safety is layered, with the heaviest lever at compile time:
//!
//! 0. **Minimal feature surface (primary).** kaibo depends on kaish-kernel with
//!    only the `localfs` axis; `subprocess`, `git`, `host`, and `os-integration`
//!    are OFF (see Cargo.toml). So `exec`/`spawn`/`kill`/`git`/`ps` are *never
//!    compiled in* — the dangerous surface doesn't exist, it isn't merely blocked.
//! 1. The project root is mounted with [`LocalFs::read_only`], so every write,
//!    delete, `mkdir`, etc. routed through the backend returns `PermissionDenied`
//!    at the VFS layer — regardless of which builtin issued it. This stops
//!    `rm`/`mv`/`cp`/`mkdir`/`tee`/`write`.
//! 2. `/` is [`MemoryFs`], so paths *outside* the project resolve to ephemeral
//!    in-memory scratch that vanishes with the kernel and never touches disk.
//! 3. [`KernelConfig::with_allow_external_commands(false)`] — belt-and-suspenders
//!    now that `subprocess` is off; refuses any external-command path.
//! 4. [`DENYLIST`] shadow-blocks the builtins that reach real state *directly*,
//!    bypassing the backend. Under `localfs`-only the live ones are `touch`
//!    (`std::fs` mtime) and `mktemp` (real temp files); the rest are
//!    defense-in-depth, firing only if a heavier axis is ever enabled.
//!
//! `ToolRegistry` has no `unregister`, but `register` overwrites by name, so we
//! shadow each denied builtin with [`Blocked`] — a wrapper that keeps the real
//! tool's schema (help/validation stay intact) but refuses to execute. A proper
//! `register_readonly_builtins`/`unregister` upstream would let us drop them from
//! the schema entirely; tracked as a nicety, not a gate.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{Tool, ToolArgs, ToolCtx, ToolRegistry, ToolSchema};
use kaish_kernel::vfs::{LocalFs, MemoryFs, VfsRouter};
use kaish_kernel::{Kernel, KernelBackend, KernelConfig, LocalBackend, OutputLimitConfig};

/// Builtins that bypass the read-only backend to touch real state directly, so
/// the [`LocalFs::read_only`] mount can't stop them (audited for
/// `std::fs`/`git2`/`Command`/signal use). Under the `localfs`-only build, only
/// `touch` and `mktemp` are actually compiled — the others (`git`/`spawn`/`exec`/
/// `kill`) live on heavier axes that are off, so `register.get` returns `None` and
/// they're skipped. They stay in the list as defense-in-depth: if someone enables
/// `subprocess`/`git` later, the block is already in place.
pub const DENYLIST: &[&str] = &["git", "touch", "spawn", "exec", "kill", "mktemp"];

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

/// Shadow-block the [`DENYLIST`] builtins plus any caller-supplied `extra` names.
///
/// `extra` is the config's `[sandbox].disable_builtins`: it can only *add* to the
/// block set, never remove from it — config makes the box stricter, never looser.
/// The hardcoded `DENYLIST` is the read-only invariant and is always applied. An
/// `extra` name that isn't a registered builtin is a no-op *here* (already
/// validated loudly at startup, see [`builtin_names`]).
fn apply_denylist(registry: &mut ToolRegistry, extra: &[String]) {
    let names = DENYLIST.iter().copied().chain(extra.iter().map(String::as_str));
    for name in names {
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

/// Default per-script output cap (matches `OutputLimitConfig::mcp()`'s 8 KB): a
/// single wide `cat`/`rg` can't flood the caller's context. Override via
/// `[sandbox].output_limit_bytes`.
pub const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 8 * 1024;

/// Tunable read-only-sandbox limits, set via `[sandbox]` in config.toml.
///
/// These can only make the box *stricter* — `disable_builtins` adds to the
/// hardcoded [`DENYLIST`], never removes from it, and the dangerous axes aren't
/// even compiled in. The read-only invariant is not a config knob.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Per-kaish-script wall-clock budget; exceeding it exits 124.
    pub exec_timeout: Duration,
    /// Max bytes of script output before truncation (exit 3 + head/tail sample).
    pub output_limit_bytes: usize,
    /// Builtins to shadow-block *in addition* to the read-only [`DENYLIST`].
    pub disable_builtins: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            exec_timeout: KAISH_EXEC_TIMEOUT,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
            disable_builtins: Vec::new(),
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
        &SandboxConfig { exec_timeout: timeout, ..SandboxConfig::default() },
    )
}

/// Build the read-only kernel with explicit [`SandboxConfig`] limits.
pub fn build_readonly_kernel_with(
    root: impl Into<PathBuf>,
    sandbox: &SandboxConfig,
) -> Result<Kernel> {
    let root = root.into();
    let mount_point = root.to_string_lossy().to_string();

    let mut vfs = VfsRouter::new();
    // Ephemeral scratch for anything outside the project; never hits disk.
    vfs.mount("/", MemoryFs::new());
    // The project itself: real files, read-only.
    vfs.mount(&mount_point, LocalFs::read_only(&root));

    let backend: Arc<dyn KernelBackend> = Arc::new(LocalBackend::new(Arc::new(vfs)));

    // Start from the MCP output limit (head+tail truncation) and set the configured
    // cap so a runaway `cat` can't flood the caller's context; the timeout bounds
    // wall clock. Both guards matter for a caller-facing `run_kaish` with no turn cap.
    let mut output_limit = OutputLimitConfig::mcp();
    output_limit.set_limit(Some(sandbox.output_limit_bytes));
    let config = KernelConfig::mcp()
        .with_cwd(root)
        .with_allow_external_commands(false)
        .with_request_timeout(sandbox.exec_timeout)
        .with_output_limit(output_limit);

    // `with_backend` ignores config.vfs_mode and routes all non-`/v/*` paths
    // through our read-only backend. `configure_tools` runs after the default
    // builtins are registered, so the denylist shadows win. The extra disabled
    // builtins are union'd with the hardcoded DENYLIST.
    let extra = sandbox.disable_builtins.clone();
    Kernel::with_backend(backend, config, |_| {}, move |reg| apply_denylist(reg, &extra))
        .context("failed to build read-only kaish kernel")
}

/// The schemas of every builtin kaibo's kernel registers, sorted by name.
///
/// Used to drive `help builtins` / per-builtin help (the `kaibo://kaish/*`
/// resources) and the composed onboarding instructions. The set is static for a
/// given build — it depends only on the compiled feature axes, not on the project
/// root or VFS mode — so we read it from a throwaway `isolated()` kernel (pure
/// in-memory, no backend) rather than spinning a full read-only mount. The
/// `DENYLIST` builtins still appear here: shadow-blocking preserves their schema
/// (only execution is refused), so the help surface matches what's registered.
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
}

/// A long-lived, read-only kaish kernel pinned to its own thread.
///
/// The kernel (and its `!Send` execution future) stay on the worker thread; the
/// handle holds only a `Send` channel, so [`KaishWorker::run`]'s future is `Send`
/// and can be awaited from inside a rig tool's `call`. The kernel persists across
/// calls, so the explorer gets shell continuity (cwd, vars) within a session.
pub struct KaishWorker {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
}

struct Job {
    script: String,
    reply: tokio::sync::oneshot::Sender<KaishOutput>,
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
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = init_tx.send(Err(format!("runtime build failed: {e}")));
                        return;
                    }
                };
                // `block_on` runs the `!Send` kernel future on this thread only.
                rt.block_on(async move {
                    let kernel = match build_readonly_kernel_with(&root, &sandbox) {
                        Ok(k) => {
                            let _ = init_tx.send(Ok(()));
                            k
                        }
                        Err(e) => {
                            let _ = init_tx.send(Err(format!("{e:#}")));
                            return;
                        }
                    };
                    while let Some(job) = jobs_rx.recv().await {
                        let out = match run(&kernel, &job.script).await {
                            Ok(r) => KaishOutput {
                                code: r.code,
                                stdout: r.text_out().into_owned(),
                                stderr: r.err.clone(),
                            },
                            Err(e) => KaishOutput {
                                code: -1,
                                stdout: String::new(),
                                stderr: format!("{e:#}"),
                            },
                        };
                        // Receiver gone (caller cancelled) is fine — drop the result.
                        let _ = job.reply.send(out);
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
            .send(Job { script: script.into(), reply })
            .map_err(|_| anyhow::anyhow!("kaish worker thread is gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("kaish worker dropped the reply"))
    }
}
