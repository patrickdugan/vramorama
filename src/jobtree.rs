//! A Windows Job Object around the process `run` starts. vramorama holds the only handle and the job has
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so when vramorama dies however it dies, the job's whole process
//! tree dies with it. When the job's main process exits, whatever it left behind is listed and terminated,
//! which returns its RAM and commit charge before the next start.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::time::{Duration, Instant};

use crate::parse::thousands;
use crate::procs;
use crate::sys::{CloseHandle, Handle};

const KILL_ON_JOB_CLOSE: u32 = 0x2000;
const EXTENDED_LIMIT_INFORMATION: i32 = 9;
const BASIC_PROCESS_ID_LIST: i32 = 3;
/// Process ids listed in one report; a job with more is still terminated whole.
const MAX_LISTED: usize = 1024;
/// Names shown in the release line before it ends in "...".
const MAX_SHOWN: usize = 6;

#[repr(C)]
#[derive(Default)]
struct BasicLimits {
    per_process_user_time: i64,
    per_job_user_time: i64,
    limit_flags: u32,
    min_working_set: usize,
    max_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io: [u64; 6],
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}

#[repr(C)]
struct IdList {
    assigned: u32,
    in_list: u32,
    ids: [usize; MAX_LISTED],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attrs: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
    fn QueryInformationJobObject(job: Handle, class: i32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, code: u32) -> i32;
}

/// One process left in the job: pid, executable name, working set in bytes.
pub struct Left {
    pub pid: u32,
    pub name: String,
    pub ws: u64,
}

/// The release line, or None when nothing was left.
pub fn release_line(left: &[Left]) -> Option<String> {
    if left.is_empty() {
        return None;
    }
    let mib = left.iter().map(|l| l.ws).sum::<u64>() / (1 << 20);
    let mut names: Vec<String> = left.iter().take(MAX_SHOWN).map(|l| format!("{} {}", l.name, l.pid)).collect();
    if left.len() > MAX_SHOWN {
        names.push("...".into());
    }
    Some(format!(
        "vramorama: job exited; released {} leftover process{} ({} MiB): {}",
        left.len(),
        if left.len() == 1 { "" } else { "es" },
        thousands(mib),
        names.join(", ")
    ))
}

pub struct JobTree {
    job: Handle,
}

impl JobTree {
    /// Put `child` in a new kill-on-close job. On failure (for example a surrounding job that forbids
    /// nesting) print one warning and return None: the job then runs as it did before.
    pub fn attach(child: &Child) -> Option<JobTree> {
        match Self::try_attach(child) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "vramorama: could not put the job in a job object ({e}); its leftover processes will not be cleaned up"
                );
                None
            }
        }
    }

    fn try_attach(child: &Child) -> Result<JobTree, String> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return Err(format!("CreateJobObject failed: {}", crate::sys::GetLastError()));
            }
            let tree = JobTree { job };
            let mut limits = ExtendedLimits::default();
            limits.basic.limit_flags = KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                EXTENDED_LIMIT_INFORMATION,
                &limits as *const _ as *const c_void,
                std::mem::size_of::<ExtendedLimits>() as u32,
            );
            if ok == 0 {
                return Err(format!("SetInformationJobObject failed: {}", crate::sys::GetLastError()));
            }
            if AssignProcessToJobObject(job, child.as_raw_handle() as Handle) == 0 {
                return Err(format!("AssignProcessToJobObject failed: {}", crate::sys::GetLastError()));
            }
            Ok(tree)
        }
    }

    fn members(&self) -> Vec<u32> {
        let mut list = IdList { assigned: 0, in_list: 0, ids: [0; MAX_LISTED] };
        let ok = unsafe {
            QueryInformationJobObject(
                self.job,
                BASIC_PROCESS_ID_LIST,
                &mut list as *mut _ as *mut c_void,
                std::mem::size_of::<IdList>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Vec::new();
        }
        list.ids[..(list.in_list as usize).min(MAX_LISTED)].iter().map(|p| *p as u32).collect()
    }

    /// The job's main process has exited: report what is still running in the job, then terminate it.
    pub fn release(self, quiet: bool) {
        let pids = self.members();
        if !pids.is_empty() {
            let names = procs::snapshot();
            let mem = procs::memory();
            let left: Vec<Left> = pids
                .iter()
                .map(|p| Left {
                    pid: *p,
                    name: names.get(p).map(|e| e.exe.clone()).unwrap_or_else(|| "?".into()),
                    ws: mem.get(p).map(|m| m.ws).unwrap_or(0),
                })
                .collect();
            if !quiet && let Some(line) = release_line(&left) {
                eprintln!("{line}");
            }
            unsafe { TerminateJobObject(self.job, 1) };
            // Termination is asynchronous; wait briefly so the memory is back before the caller goes on.
            let t = Instant::now();
            while !self.members().is_empty() && t.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

impl Drop for JobTree {
    /// Closing the last handle kills whatever is still in the job (kill-on-close).
    fn drop(&mut self) {
        unsafe { CloseHandle(self.job) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(pid: u32, name: &str, mib: u64) -> Left {
        Left { pid, name: name.into(), ws: mib << 20 }
    }

    #[test]
    fn nothing_left_prints_nothing() {
        assert_eq!(release_line(&[]), None);
    }

    #[test]
    fn line_names_processes_and_totals_memory() {
        let s = release_line(&[l(4412, "lgym.exe", 1140), l(5120, "python.exe", 1000)]).unwrap();
        assert_eq!(
            s,
            "vramorama: job exited; released 2 leftover processes (2,140 MiB): lgym.exe 4412, python.exe 5120"
        );
    }

    #[test]
    fn singular_and_long_lists() {
        assert!(release_line(&[l(1, "a.exe", 1)]).unwrap().contains("1 leftover process (1 MiB)"));
        let many: Vec<Left> = (0..9).map(|i| l(i, "p.exe", 1)).collect();
        let s = release_line(&many).unwrap();
        assert!(s.contains("9 leftover processes") && s.ends_with(", ..."));
    }
}
