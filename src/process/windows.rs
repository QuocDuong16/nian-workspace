//! Windows implementation: each spawned child is attached to its own Job
//! Object with kill-on-close semantics, so terminating the job ends the whole
//! descendant tree and dropping the guard does too.

use tokio::process::Command;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    },
    System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
};

/// Nothing must be set before spawn on Windows; job attachment happens right
/// afterwards. Kept as a hook so the call-site stays platform-uniform.
pub(crate) fn configure(_cmd: &mut Command) {}

/// Create a kill-on-close Job Object containing the given process.
///
/// Returns the job handle (ownership moves to the caller-guard); `None` means
/// containment could not be established and the caller degrades gracefully.
pub(crate) fn attach_to_job_object(pid: u32) -> Option<HANDLE> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            tracing::warn!(
                "CreateJobObjectW failed: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            tracing::warn!(
                "SetInformationJobObject failed: {}",
                std::io::Error::last_os_error()
            );
            close_quietly(job);
            return None;
        }

        let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc.is_null() {
            tracing::warn!(
                "OpenProcess({pid}) failed: {}",
                std::io::Error::last_os_error()
            );
            close_quietly(job);
            return None;
        }

        if AssignProcessToJobObject(job, proc) == 0 {
            tracing::warn!(
                "AssignProcessToJobObject failed: {}",
                std::io::Error::last_os_error()
            );
            close_quietly(proc);
            close_quietly(job);
            return None;
        }
        close_quietly(proc);
        Some(job)
    }
}

/// Terminate every process currently inside the job.
pub(crate) fn terminate_job(job: HANDLE) {
    unsafe {
        if TerminateJobObject(job, 1) == 0 {
            tracing::debug!(
                "TerminateJobObject failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Release a job or process handle, ignoring secondary failures.
pub(crate) fn close_quietly(handle: HANDLE) {
    unsafe { CloseHandle(handle) };
}
