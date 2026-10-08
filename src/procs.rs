//! Process table, start times, and the command line, working directory and environment of a
//! process read from its PEB (the same technique psutil and Process Explorer use).

use std::collections::HashMap;
use std::ffi::c_void;

use crate::parse::parse_env_block;
use crate::sys::*;

pub struct Entry {
    pub ppid: u32,
    pub exe: String,
}

/// pid -> (parent pid, executable name) for every running process.
pub fn snapshot() -> HashMap<u32, Entry> {
    let mut out = HashMap::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE {
            return out;
        }
        let mut e: ProcessEntry32W = std::mem::zeroed();
        e.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            out.insert(e.pid, Entry { ppid: e.ppid, exe: from_wide(&e.exe_file) });
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    out
}

/// Creation time (FILETIME, UTC) of a running process.
pub fn created(pid: u32) -> Option<u64> {
    let h = ProcHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    created_h(&h)
}

fn created_h(h: &ProcHandle) -> Option<u64> {
    let (mut c, mut e, mut k, mut u) = Default::default();
    (unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u) } != 0).then(|| c.as_u64())
}

/// Running, and (when `not_after` is given) started no later than that time, which rules out
/// a recycled pid.
pub fn alive(pid: u32, not_after: Option<u64>) -> Option<bool> {
    let Some(h) = ProcHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        // ERROR_INVALID_PARAMETER means no such pid; anything else (access denied) is unknown.
        return (unsafe { GetLastError() } == ERROR_INVALID_PARAMETER).then_some(false);
    };
    let mut code = 0;
    if unsafe { GetExitCodeProcess(h.0, &mut code) } == 0 {
        return None;
    }
    if code != STILL_ACTIVE {
        return Some(false);
    }
    match (not_after, created_h(&h)) {
        (Some(limit), Some(c)) => Some(c <= limit),
        _ => Some(true),
    }
}

#[derive(Default, Clone)]
pub struct Details {
    pub created: Option<u64>,
    pub cmdline: Option<String>,
    pub cwd: Option<String>,
    pub env: Option<Vec<(String, String)>>,
    /// Why the PEB could not be read, when it could not.
    pub note: Option<&'static str>,
}

type NtQueryInformationProcess = unsafe extern "system" fn(Handle, u32, *mut c_void, u32, *mut u32) -> i32;

#[repr(C)]
#[derive(Default)]
struct ProcessBasicInformation {
    exit_status: usize,
    peb: usize,
    affinity: usize,
    base_priority: usize,
    pid: usize,
    ppid: usize,
}

// x64 offsets. PEB.ProcessParameters, then fields of RTL_USER_PROCESS_PARAMETERS.
const PEB_PARAMS: usize = 0x20;
const PARAMS_CWD: usize = 0x38;
const PARAMS_CMDLINE: usize = 0x70;
const PARAMS_ENV: usize = 0x80;
const PARAMS_ENV_SIZE: usize = 0x3F0;
const PARAMS_LEN: usize = 0x3F8;
const ENV_MAX: usize = 4 << 20;

pub fn details(pid: u32) -> Details {
    let mut d = Details::default();
    let Some(h) = ProcHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
        .or_else(|| ProcHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION))
    else {
        d.note = Some("access denied (run elevated to see this process)");
        return d;
    };
    d.created = created_h(&h);
    let mut wow = 0;
    if cfg!(target_pointer_width = "64") && unsafe { IsWow64Process(h.0, &mut wow) } != 0 && wow != 0 {
        d.note = Some("32-bit process; PEB not read");
        return d;
    }
    match read_peb(&h) {
        Some((cmd, cwd, env)) => {
            d.cmdline = cmd;
            d.cwd = cwd;
            d.env = env;
        }
        None => d.note = Some("could not read process memory (run elevated to see this process)"),
    }
    d
}

type PebFields = (Option<String>, Option<String>, Option<Vec<(String, String)>>);

fn read_peb(h: &ProcHandle) -> Option<PebFields> {
    let ntdll = Lib::mapped("ntdll.dll")?;
    let query: NtQueryInformationProcess = unsafe { ntdll.get("NtQueryInformationProcess")? };
    let mut pbi = ProcessBasicInformation::default();
    let size = std::mem::size_of::<ProcessBasicInformation>() as u32;
    let st = unsafe { query(h.0, 0, &mut pbi as *mut _ as *mut c_void, size, &mut 0) };
    if st < 0 || pbi.peb == 0 {
        return None;
    }
    let params_addr = read_usize(h, pbi.peb + PEB_PARAMS)?;
    let params = read(h, params_addr, PARAMS_LEN)?;
    let cmd = unicode_string(h, &params, PARAMS_CMDLINE);
    let cwd = unicode_string(h, &params, PARAMS_CWD).map(|s| s.trim_end_matches('\\').to_string());
    let env_addr = usize_at(&params, PARAMS_ENV);
    let env_size = usize_at(&params, PARAMS_ENV_SIZE);
    let env = if env_addr == 0 {
        None
    } else if env_size > 0 && env_size <= ENV_MAX {
        read(h, env_addr, env_size).map(|b| parse_env_block(&to_u16(&b)))
    } else {
        read_env_until_end(h, env_addr)
    };
    Some((cmd, cwd, env))
}

/// Older layouts lack EnvironmentSize: read page by page until the double NUL.
fn read_env_until_end(h: &ProcHandle, addr: usize) -> Option<Vec<(String, String)>> {
    let mut bytes = Vec::new();
    while bytes.len() < ENV_MAX {
        let Some(chunk) = read(h, addr + bytes.len(), 4096) else { break };
        bytes.extend_from_slice(&chunk);
        if to_u16(&bytes).windows(2).any(|w| w == [0, 0]) {
            break;
        }
    }
    (!bytes.is_empty()).then(|| parse_env_block(&to_u16(&bytes)))
}

fn read(h: &ProcHandle, addr: usize, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut got = 0;
    let ok = unsafe { ReadProcessMemory(h.0, addr as *const c_void, buf.as_mut_ptr() as *mut c_void, len, &mut got) };
    (ok != 0 && got == len).then_some(buf)
}

fn read_usize(h: &ProcHandle, addr: usize) -> Option<usize> {
    read(h, addr, 8).map(|b| usize_at(&b, 0))
}

fn usize_at(b: &[u8], off: usize) -> usize {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap()) as usize
}

/// UNICODE_STRING { u16 Length; u16 MaximumLength; PWSTR Buffer @ +8 } embedded at `off`.
fn unicode_string(h: &ProcHandle, params: &[u8], off: usize) -> Option<String> {
    let len = u16::from_le_bytes([params[off], params[off + 1]]) as usize;
    let ptr = usize_at(params, off + 8);
    if len == 0 || ptr == 0 {
        return None;
    }
    read(h, ptr, len).map(|b| String::from_utf16_lossy(&to_u16(&b)))
}

fn to_u16(b: &[u8]) -> Vec<u16> {
    b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

/// The chain of live ancestors, nearest first, and the pid of the first parent that is gone
/// (exited, or its pid was reused by a younger process).
pub fn ancestry(pid: u32, table: &HashMap<u32, Entry>) -> (Vec<u32>, Option<u32>) {
    let mut chain = Vec::new();
    let mut cur = pid;
    let mut cur_created = created(pid);
    while let Some(e) = table.get(&cur) {
        let parent = e.ppid;
        if parent == 0 || parent == cur || chain.len() > 64 {
            return (chain, None);
        }
        let parent_created = created(parent);
        let genuine = table.contains_key(&parent)
            && match (parent_created, cur_created) {
                (Some(p), Some(c)) => p <= c,
                _ => true, // cannot tell (protected process); assume the link holds
            };
        if !genuine {
            return (chain, Some(parent));
        }
        chain.push(parent);
        cur = parent;
        cur_created = parent_created;
    }
    (chain, None)
}
