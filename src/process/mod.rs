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
//!   descendant still inside it, and closing the guard does the same.
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

/// A held resource that lets [`terminate`] reach the whole process tree.
///
/// Construct immediately after a successful [`spawn`](tokio::process::Command::spawn);
/// dropping the guard releases the underlying job handle (which on Windows
/// also kills any processes still attached via kill-on-close).
pub(crate) struct ProcessTreeGuard {
    inner: GuardInner,
}

enum GuardInner {
    #[cfg(unix)]
    Unix { pgid: u32 },
    #[cfg(windows)]
    Windows {
        job: windows_sys::Win32::Foundation::HANDLE,
    },
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

impl ProcessTreeGuard {
    /// Attach containment to the freshly spawned child. Never fails: a
    /// failed attachment degrades to no-op termination with a warning.
    ///
    /// `child_pid` is the direct child's pid; on Windows it must be an
    /// already-running process id, hence taking the value rather than the
    /// still-spawning handle. `None` ids (possible on exotic platforms) skip
    /// tree attachment but `terminate_tree` still kills the child directly.
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
                        inner: GuardInner::Windows {
                            job: std::ptr::null_mut(),
                        },
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
    /// `finish_child` must eventually reap the direct child afterwards so it
    /// does not linger as a zombie.
    pub(crate) async fn terminate_tree(&self, finish_child: &mut tokio::process::Child) {
        match &self.inner {
            #[cfg(unix)]
            GuardInner::Unix { pgid } if *pgid != 0 => unix::kill_process_group(*pgid),
            #[cfg(windows)]
            GuardInner::Windows { job } => {
                if !job.is_null() {
                    windows::terminate_job(*job);
                }
            }
            _ => {}
        }
        // Whether the signal/job reached the child or not, make sure the
        // direct child is dead and reaped on both platforms.
        let _ = finish_child.start_kill();
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
}
