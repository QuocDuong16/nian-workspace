//! Platform-isolated child-process lifecycle management.
//!
//! Every spawned child participates in one OS-level containment mechanism so
//! a timeout can take down the entire tree instead of only the direct child:
//!
//! * **Unix** — the child starts its own process group ([`configure`]), and
//!   termination signals the whole group (negative PID) rather than the
//!   leader alone.
//! * **Windows** — the child is attached to a fresh Job Object with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; terminating the job ends every
//!   descendant still inside it, and dropping the guard (closing the last
//!   job handle) does the same.
//!
//! Both mechanisms are best-effort containment, not a sandbox: a grandchild
//! spawned in the microseconds between `spawn()` and job attachment escapes,
//! and a process with `CREATE_BREAKAWAY_FROM_JOB` permission can opt out on
//! Windows. Security-wise this matches the project's documented boundary —
//! `--exec` runs real programs with full user rights.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::configure as configure_impl;
#[cfg(windows)]
pub(crate) use windows::configure as configure_impl;

/// A held resource that lets [`ProcessTreeGuard::terminate_tree`] reach the
/// whole process tree.
///
/// Construct immediately after a successful
/// [`spawn`](tokio::process::Command::spawn). Dropping the guard releases the
/// underlying job handle (which on Windows also kills any processes still
/// attached via kill-on-close — even after a normal exit of the direct
/// child, stray descendants do not survive their job being closed).
///
/// The Windows variant stores the raw handle as an integer so the guard
/// stays `Send + Sync`: tokio tool futures must remain sendable across the
/// async runtime, and kernel handles are thread-safe opaque values.
pub(crate) struct ProcessTreeGuard {
    inner: GuardInner,
}

enum GuardInner {
    #[cfg(unix)]
    Unix { pgid: u32 },
    #[cfg(windows)]
    Windows { job: isize },
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

impl ProcessTreeGuard {
    /// Attach containment to the freshly spawned child. Never fails: a
    /// failed attachment degrades to no-op termination with a warning.
    ///
    /// `child_pid` is the direct child's pid; on Windows it must be an
    /// already-running process id. `None` ids (possible on exotic platforms)
    /// skip tree attachment but [`terminate_tree`](Self::terminate_tree)
    /// still kills the direct child.
    pub(crate) fn attach(child_pid: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            match child_pid {
                Some(pgid) => Self {
                    inner: GuardInner::Unix { pgid },
                },
                // No id => nothing to signal group-wise; fall through to
                // plain child kill in terminate_tree.
                None => Self {
                    inner: GuardInner::Unix { pgid: 0 },
                },
            }
        }
        #[cfg(windows)]
        {
            let job = match child_pid {
                Some(pid) => windows::attach_to_job_object(pid),
                None => None,
            };
            match job {
                Some(job) => Self {
                    inner: GuardInner::Windows { job },
                },
                None => {
                    tracing::warn!(
                        "could not attach child to a Job Object; \
                         timeout termination will cover only the direct child"
                    );
                    Self {
                        inner: GuardInner::Windows { job: 0 },
                    }
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child_pid;
            Self {
                inner: GuardInner::Unsupported,
            }
        }
    }

    /// Terminate the entire process tree (best effort).
    ///
    /// The caller must await/reap the direct child afterwards so it does not
    /// linger as a zombie or unobserved exit status.
    pub(crate) async fn terminate_tree(&self, finish_child: &mut tokio::process::Child) {
        match &self.inner {
            #[cfg(unix)]
            GuardInner::Unix { pgid } if *pgid != 0 => unix::kill_process_group(*pgid),
            #[cfg(windows)]
            GuardInner::Windows { job } if *job != 0 => windows::terminate_job(*job),
            _ => {}
        }
        // Whether the signal/job reached the child or not, make sure the
        // direct child is dead on both platforms before it is reaped.
        let _ = finish_child.start_kill();
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        match &self.inner {
            GuardInner::Windows { job } if *job != 0 => windows::close_job_handle(*job),
            _ => {}
        }
    }
}

/// Apply platform process-group/job configuration to a command before spawn.
pub(crate) fn configure(cmd: &mut tokio::process::Command) {
    #[cfg(any(unix, windows))]
    configure_impl(cmd);
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn terminated_child_exit_reflects_signal() {
        // The guard signals the group; the direct child should end up
        // signal-killed rather than surviving.
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg("600");
        configure(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let guard = ProcessTreeGuard::attach(child.id());
        guard.terminate_tree(&mut child).await;
        let status = child.wait().await.unwrap();
        assert!(
            std::os::unix::process::ExitStatusExt::core_dumped(&status) || status.code().is_none(),
            "expected signal termination, got {status:?}"
        );
    }

    #[test]
    fn guard_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProcessTreeGuard>();
    }
}
