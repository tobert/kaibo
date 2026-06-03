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

use anyhow::{Context, Result};
use async_trait::async_trait;
use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{ExecContext, Tool, ToolArgs, ToolRegistry, ToolSchema};
use kaish_kernel::vfs::{LocalFs, MemoryFs, VfsRouter};
use kaish_kernel::{Kernel, KernelBackend, KernelConfig, LocalBackend};

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

    async fn execute(&self, _args: ToolArgs, _ctx: &mut ExecContext) -> ExecResult {
        ExecResult::failure(
            126,
            format!(
                "{}: disabled in kaibo's read-only sandbox",
                self.inner.name()
            ),
        )
    }
}

/// Shadow-block every [`DENYLIST`] builtin present in the registry.
fn apply_denylist(registry: &mut ToolRegistry) {
    for &name in DENYLIST {
        if let Some(inner) = registry.get(name) {
            registry.register_arc(Arc::new(Blocked { inner }));
        }
    }
}

/// Build a kernel that can *read* everything under `root` and *mutate* nothing.
///
/// The project is mounted at its real absolute path (mirroring kaish's own
/// sandbox layout) so familiar paths like `/home/me/proj/src/main.rs` resolve
/// transparently, while `cat`/`grep`/`find` see the live tree.
pub fn build_readonly_kernel(root: impl Into<PathBuf>) -> Result<Kernel> {
    let root = root.into();
    let mount_point = root.to_string_lossy().to_string();

    let mut vfs = VfsRouter::new();
    // Ephemeral scratch for anything outside the project; never hits disk.
    vfs.mount("/", MemoryFs::new());
    // The project itself: real files, read-only.
    vfs.mount(&mount_point, LocalFs::read_only(&root));

    let backend: Arc<dyn KernelBackend> = Arc::new(LocalBackend::new(Arc::new(vfs)));

    let config = KernelConfig::mcp()
        .with_cwd(root)
        .with_allow_external_commands(false);

    // `with_backend` ignores config.vfs_mode and routes all non-`/v/*` paths
    // through our read-only backend. `configure_tools` runs after the default
    // builtins are registered, so the denylist shadows win.
    Kernel::with_backend(backend, config, |_| {}, apply_denylist)
        .context("failed to build read-only kaish kernel")
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
    /// Spawn the worker thread and build its read-only kernel rooted at `root`.
    ///
    /// Blocks until the kernel is built so construction failures surface here
    /// rather than on the first `run`.
    pub fn spawn(root: impl Into<PathBuf>) -> Result<Self> {
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
                    let kernel = match build_readonly_kernel(&root) {
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
