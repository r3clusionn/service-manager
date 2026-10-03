//! Starting a service's process, asking it to stop, and killing it with everything it started.
//!
//! | | Windows | Unix |
//! |---|---|---|
//! | grouping | a Job Object per service, set to kill its processes when closed | a new process group per service |
//! | polite stop | `CTRL_BREAK_EVENT` to the service's process group | `SIGTERM` to the process group |
//! | kill | `TerminateJobObject`: the service and every process it started | `SIGKILL` to the process group |
//!
//! Grouping is what lets a stop reach grandchildren: a service started through a shell script, or
//! one that forks workers, is stopped as a whole.

use std::io;
use std::process::{Child, Command, Stdio};

use crate::config::Service;

/// A running service process: its id, and what is needed to stop or kill it and its children.
pub struct Proc {
    pub pid: u32,
    #[cfg(windows)]
    job: isize,
}

// The job handle is only used through thread-safe Win32 calls.
unsafe impl Send for Proc {}

impl Proc {
    /// Asks the service to stop. Returns false if the request could not be delivered (then only
    /// [`Proc::kill`] is left).
    #[cfg(windows)]
    pub fn request_stop(&self) -> bool {
        use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
        // The service was started in its own process group, whose id is its process id; the event
        // reaches it only if it shares this program's console.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.pid) != 0 }
    }

    #[cfg(unix)]
    pub fn request_stop(&self) -> bool {
        unsafe { libc::kill(-(self.pid as libc::pid_t), libc::SIGTERM) == 0 }
    }

    /// Kills the service and every process it started.
    #[cfg(windows)]
    pub fn kill(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        unsafe {
            TerminateJobObject(self.job as _, 1);
        }
    }

    #[cfg(unix)]
    pub fn kill(&self) {
        unsafe {
            libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
impl Drop for Proc {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // The job kills whatever is left in it when its last handle closes: processes a service
        // left behind do not outlive it.
        unsafe {
            CloseHandle(self.job as _);
        }
    }
}

/// Starts the service with its standard output and error piped. The extra variables `TEND_SERVICE`
/// and `TEND_RESTARTS` tell it its name and how often it has been restarted.
pub fn spawn(name: &str, s: &Service, restarts: u32) -> io::Result<(Proc, Child)> {
    let mut cmd = Command::new(&s.command[0]);
    cmd.args(&s.command[1..]).envs(&s.env).env("TEND_SERVICE", name).env("TEND_RESTARTS", restarts.to_string());
    if let Some(dir) = &s.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    platform_spawn(cmd)
}

#[cfg(windows)]
fn platform_spawn(mut cmd: Command) -> io::Result<(Proc, Child)> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                windows_sys::Win32::Foundation::CloseHandle(job);
                return Err(e);
            }
        };
        // A process the service starts before this call is not in the job; in practice there is
        // no time for one. (Starting suspended would close the gap; std does not expose it.)
        AssignProcessToJobObject(job, child.as_raw_handle() as _);
        Ok((Proc { pid: child.id(), job: job as isize }, child))
    }
}

#[cfg(unix)]
fn platform_spawn(mut cmd: Command) -> io::Result<(Proc, Child)> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let child = cmd.spawn()?;
    Ok((Proc { pid: child.id() }, child))
}
