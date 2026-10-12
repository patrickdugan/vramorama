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

/// The memory of one process, from the kernel's process list.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Mem {
    /// Creation time (FILETIME), the same value `created` returns, which rules out recycled pids.
    pub created: u64,
    /// Private working set: the process's own pages in RAM (Task Manager's "Memory" column).
    pub private_ws: u64,
    /// Working set, including pages shared with other processes (mapped files, DLLs).
    pub ws: u64,
    pub peak_ws: u64,
    /// Private bytes committed, in RAM or the page file: what counts against the commit limit.
    pub commit: u64,
}

type NtQuerySystemInformation = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;
const SYSTEM_PROCESS_INFORMATION: u32 = 5;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

/// Memory of every process, in one call and without opening any process (so it covers
/// protected and other users' processes too): `NtQuerySystemInformation(SystemProcessInformation)`.
pub fn memory() -> HashMap<u32, Mem> {
    let Some(ntdll) = Lib::mapped("ntdll.dll") else { return HashMap::new() };
    let Some(query) = (unsafe { ntdll.get::<NtQuerySystemInformation>("NtQuerySystemInformation") }) else {
        return HashMap::new();
    };
    let mut buf: Vec<u64> = vec![0; 1 << 17]; // 1 MiB, 8-byte aligned
    for _ in 0..8 {
        let bytes = (buf.len() * 8) as u32;
        let mut needed = 0u32;
        let st = unsafe { query(SYSTEM_PROCESS_INFORMATION, buf.as_mut_ptr() as *mut c_void, bytes, &mut needed) };
        if st == STATUS_INFO_LENGTH_MISMATCH {
            buf = vec![0; (needed.max(bytes) as usize * 2).div_ceil(8)];
            continue;
        }
        if st < 0 {
            return HashMap::new();
        }
        // SAFETY: reinterpreting initialised u64s as bytes.
        let raw = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
        return parse_process_list(raw);
    }
    HashMap::new()
}

/// Walk a SYSTEM_PROCESS_INFORMATION list (x64 layout). Entries are chained by
/// NextEntryOffset; a truncated or malformed list ends the walk instead of reading past the end.
pub fn parse_process_list(buf: &[u8]) -> HashMap<u32, Mem> {
    const LEN: usize = 0xD0;
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let mut out = HashMap::new();
    let mut off = 0usize;
    while off + LEN <= buf.len() {
        let e = &buf[off..off + LEN];
        let pid = u64_at(e, 0x50) as u32;
        if pid != 0 {
            out.insert(
                pid,
                Mem {
                    created: u64_at(e, 0x20),
                    private_ws: u64_at(e, 0x08),
                    peak_ws: u64_at(e, 0x88),
                    ws: u64_at(e, 0x90),
                    commit: u64_at(e, 0xB8),
                },
            );
        }
        let next = u32_at(e, 0) as usize;
        if next == 0 || next < LEN {
            break;
        }
        off += next;
    }
    out
}

/// Physical memory and the commit charge, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct SysMem {
    pub total: u64,
    pub avail: u64,
    pub commit_limit: u64,
    pub commit_avail: u64,
}

pub fn system_memory() -> Option<SysMem> {
    let mut m = MemoryStatusEx { length: std::mem::size_of::<MemoryStatusEx>() as u32, ..Default::default() };
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(SysMem {
        total: m.total_phys,
        avail: m.avail_phys,
        commit_limit: m.total_page_file,
        commit_avail: m.avail_page_file,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(next: u32, pid: u64, created: u64, private_ws: u64, ws: u64, commit: u64) -> Vec<u8> {
        let mut e = vec![0u8; 0xD0];
        e[0..4].copy_from_slice(&next.to_le_bytes());
        e[0x08..0x10].copy_from_slice(&private_ws.to_le_bytes());
        e[0x20..0x28].copy_from_slice(&created.to_le_bytes());
        e[0x50..0x58].copy_from_slice(&pid.to_le_bytes());
        e[0x88..0x90].copy_from_slice(&(ws * 2).to_le_bytes());
        e[0x90..0x98].copy_from_slice(&ws.to_le_bytes());
        e[0xB8..0xC0].copy_from_slice(&commit.to_le_bytes());
        e
    }

    #[test]
    fn process_list_walk() {
        // The idle process (pid 0), then two processes; the second entry has room for thread records.
        let mut buf = entry(0xD0, 0, 0, 0, 0, 0);
        let mut a = entry(0x100, 4242, 133_000_000_000_000_001, 900 << 20, 1000 << 20, 1200 << 20);
        a.resize(0x100, 0xEE);
        buf.extend(a);
        buf.extend(entry(0, 77, 133_000_000_000_000_002, 1 << 20, 2 << 20, 3 << 20));
        let m = parse_process_list(&buf);
        assert_eq!(m.len(), 2);
        assert_eq!(
            m[&4242],
            Mem {
                created: 133_000_000_000_000_001,
                private_ws: 900 << 20,
                ws: 1000 << 20,
                peak_ws: 2000 << 20,
                commit: 1200 << 20
            }
        );
        assert_eq!(m[&77].commit, 3 << 20);
        // A chain pointing past the end stops instead of reading out of bounds.
        let mut bad = entry(0x4000, 9, 1, 1, 1, 1);
        bad.truncate(0xD0);
        assert_eq!(parse_process_list(&bad).len(), 1);
        assert!(parse_process_list(&bad[..0x40]).is_empty());
    }
}
