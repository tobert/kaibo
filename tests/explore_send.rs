//! Compile-time pin for the riskiest mechanical piece of the recomposition:
//! `RunExplore` is a rig `Tool` whose `call` runs a *nested* rig agent driving a
//! `!Send` kaish kernel. rig requires every `Tool::call` future be `Send` (it runs
//! on the agent task) and every boxed tool be `Send + Sync`. The kernel never
//! crosses an `.await` — it lives on the `KaishWorker` thread, and only the `Send`
//! channel handle does. The client itself now hides behind the `Arm`'s type-erased
//! runner (`Arc<dyn PhaseRunner>`), so the `Send + Sync` obligation lands on that
//! vtable instead of a generic parameter — same invariant, one place. If a future
//! change ever holds the kernel across an await inside `call`, or makes `Arm` /
//! `RunExplore` non-`Send`, THIS FILE STOPS COMPILING. That failure is the test.
//! No network.

use kaibo::consult::RunExplore;

fn assert_send_sync<T: Send + Sync>() {}
fn assert_is_tool<T: rig::tool::Tool>() {}

#[test]
fn run_explore_is_a_send_sync_rig_tool() {
    // Send + Sync so it can be a `Box<dyn ToolDyn>` in a consult toolset.
    assert_send_sync::<RunExplore>();
    // `Tool` — and the trait's `call -> impl Future + Send` bound is checked at the
    // impl site, so this also proves the nested-agent future is Send.
    assert_is_tool::<RunExplore>();
}
