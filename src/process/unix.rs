//! Unix implementation: children run in their own process group so timeout
//! termination can signal every descendant, not just the direct child.

use tokio::process::Command;

/// Make the child the leader of a fresh process group (`pgid == child pid`).
pub(crate) fn configure(cmd: &mut Command) {
    cmd.process_group(0);
}

/// Kill every process in the group led by `pgid`. Best effort: a group that
/// has already exited simply reports ESRCH here.
pub(crate) fn kill_process_group(pgid: u32) {
    let target = -(pgid as libc::pid_t);
    let rc = unsafe { libc::kill(target, libc::SIGKILL) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        tracing::debug!(%pgid, %err, "process-group kill did not succeed (group already gone?)");
    }
}
