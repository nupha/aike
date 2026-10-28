// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! A running sandboxed child, and the handle used to control it from another
//! thread.
//!
//! This is the shape every backend returns. `js.rs` never learns which
//! mechanism is underneath — it takes the pipes, hands out the control
//! handle, and blocks on [`SpawnedChild::wait`].

use std::sync::Arc;

/// Tree-kill and liveness handle, cloned out before the child is handed to a
/// blocking `wait()`. Depends only on the pid (or the Windows job handle), so
/// it stays valid from any thread with no access to the pipes.
#[derive(Clone)]
pub(crate) struct ChildControl {
    kill_tree: Arc<dyn Fn() -> bool + Send + Sync>,
    is_running: Arc<dyn Fn() -> bool + Send + Sync>,
    // Only Unix delivers signals; the Windows job object has no equivalent, so
    // its `kill` is the whole API there.
    #[cfg_attr(not(unix), allow(dead_code))]
    send_signal: Arc<dyn Fn(i32) -> bool + Send + Sync>,
}

impl ChildControl {
    pub(crate) fn new(
        kill_tree: impl Fn() -> bool + Send + Sync + 'static,
        is_running: impl Fn() -> bool + Send + Sync + 'static,
        send_signal: impl Fn(i32) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            kill_tree: Arc::new(kill_tree),
            is_running: Arc::new(is_running),
            send_signal: Arc::new(send_signal),
        }
    }

    /// Kill the whole sandboxed process tree. `false` when it was already gone.
    pub(crate) fn kill(&self) -> bool {
        (self.kill_tree)()
    }

    pub(crate) fn is_running(&self) -> bool {
        (self.is_running)()
    }

    /// Signal the tree. `false` when the signal could not be delivered.
    ///
    /// On macOS only SIGKILL reliably terminates the run: `sandbox-exec` is a
    /// persistent daemon that does not forward signals to the command it
    /// exec'd, so any other signal may appear to be delivered and do nothing.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn send_signal(&self, sig: i32) -> bool {
        (self.send_signal)(sig)
    }
}

/// A spawned child whose stdio has not been handed to the host yet.
///
/// The backend's own handle to the process stays behind in `reap`, so
/// [`SpawnedChild`] itself is the same type on every platform and `js.rs` never
/// learns which mechanism is underneath.
pub(crate) struct SpawnedChild {
    pub(crate) pid: i32,
    pub(crate) stdin: Option<std::fs::File>,
    pub(crate) stdout: Option<std::fs::File>,
    pub(crate) stderr: Option<std::fs::File>,
    pub(crate) control: ChildControl,
    /// Block until the command exits and report how. Called exactly once.
    reap: Box<dyn FnOnce() -> Result<WaitResult, String> + Send>,
}

impl SpawnedChild {
    pub(crate) fn new(
        pid: i32,
        stdin: Option<std::fs::File>,
        stdout: Option<std::fs::File>,
        stderr: Option<std::fs::File>,
        control: ChildControl,
        reap: impl FnOnce() -> Result<WaitResult, String> + Send + 'static,
    ) -> Self {
        Self {
            pid,
            stdin,
            stdout,
            stderr,
            control,
            reap: Box::new(reap),
        }
    }

    /// Block until the command exits. Consumes the child, since a process can
    /// only be waited on once.
    ///
    /// Whatever the command left behind dies with the call: the tree is swept
    /// once `reap` reports the exit, so `nohup cmd &` cannot outlive it. Linux
    /// reaches the same end state through `--die-with-parent` (the namespace
    /// init leaving kills its namespace) and Windows through
    /// `kill_on_job_close`; on macOS nothing does it for us, and the leftover
    /// would be a process no later sandbox can reach — the policy only allows
    /// signals inside its own sandbox.
    pub(crate) fn wait(self) -> Result<WaitResult, String> {
        let control = self.control.clone();
        let r = (self.reap)();
        control.kill();
        r
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WaitResult {
    pub(crate) exit_code: i32,
    /// The signal that killed it, if one did.
    pub(crate) signal: Option<i32>,
    /// Set by the caller when it killed the tree on a timeout; backends do not
    /// decide this, they only report what the process did.
    pub(crate) killed_by_timeout: bool,
}
