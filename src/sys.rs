//! Thin Win32 layer, written by hand so the crate has no third-party dependencies.
//!
//! Only kernel32 is linked at build time. pdh.dll, dxgi.dll and ntdll.dll are resolved at run
//! time with `GetProcAddress`, so no import libraries are needed and the same source builds with
//! the MSVC and the GNU toolchains.

use std::ffi::c_void;

pub type Handle = isize;

pub const INVALID_HANDLE: Handle = -1;
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
pub const PROCESS_VM_READ: u32 = 0x0010;
pub const STILL_ACTIVE: u32 = 259;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const TH32CS_SNAPPROCESS: u32 = 0x2;
pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;

/// 100 ns intervals between 1601-01-01 and 1970-01-01.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FileTime {
    pub low: u32,
    pub high: u32,
}

impl FileTime {
    pub fn from_u64(v: u64) -> Self {
        FileTime { low: v as u32, high: (v >> 32) as u32 }
    }
    pub fn as_u64(self) -> u64 {
        ((self.high as u64) << 32) | self.low as u64
    }
}

#[repr(C)]
#[derive(Default)]
pub struct SystemTime {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

#[repr(C)]
pub struct ProcessEntry32W {
    pub size: u32,
    pub usage: u32,
    pub pid: u32,
    pub default_heap_id: usize,
    pub module_id: u32,
    pub threads: u32,
    pub ppid: u32,
    pub pri_class_base: i32,
    pub flags: u32,
    pub exe_file: [u16; 260],
}

#[repr(C)]
#[derive(Default)]
pub struct ConsoleScreenBufferInfo {
    pub size: [i16; 2],
    pub cursor: [i16; 2],
    pub attributes: u16,
    pub window: [i16; 4], // left, top, right, bottom
    pub max_window: [i16; 2],
}

/// MEMORYSTATUSEX.
#[repr(C)]
#[derive(Default)]
pub struct MemoryStatusEx {
    pub length: u32,
    pub memory_load: u32,
    pub total_phys: u64,
    pub avail_phys: u64,
    /// The commit limit (RAM plus page files) and how much of it is not yet committed.
    pub total_page_file: u64,
    pub avail_page_file: u64,
    pub total_virtual: u64,
    pub avail_virtual: u64,
    pub avail_extended_virtual: u64,
}

/// SYSTEM_POWER_STATUS.
#[repr(C)]
#[derive(Default)]
pub struct SystemPowerStatus {
    /// 0 on battery, 1 on AC, 255 unknown.
    pub ac_line_status: u8,
    pub battery_flag: u8,
    pub battery_life_percent: u8,
    pub system_status_flag: u8,
    pub battery_life_time: u32,
    pub battery_full_life_time: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    pub fn LoadLibraryW(name: *const u16) -> Handle;
    pub fn GetModuleHandleW(name: *const u16) -> Handle;
    pub fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
    pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    pub fn CloseHandle(h: Handle) -> i32;
    pub fn GetLastError() -> u32;
    pub fn ReadProcessMemory(h: Handle, base: *const c_void, buf: *mut c_void, size: usize, read: *mut usize) -> i32;
    pub fn GetProcessTimes(
        h: Handle,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    pub fn GetExitCodeProcess(h: Handle, code: *mut u32) -> i32;
    pub fn IsWow64Process(h: Handle, wow: *mut i32) -> i32;
    pub fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
    pub fn Process32FirstW(snap: Handle, entry: *mut ProcessEntry32W) -> i32;
    pub fn Process32NextW(snap: Handle, entry: *mut ProcessEntry32W) -> i32;
    pub fn FileTimeToLocalFileTime(ft: *const FileTime, local: *mut FileTime) -> i32;
    pub fn FileTimeToSystemTime(ft: *const FileTime, st: *mut SystemTime) -> i32;
    pub fn GetSystemTimeAsFileTime(ft: *mut FileTime);
    pub fn GetStdHandle(n: u32) -> Handle;
    pub fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
    pub fn SetConsoleMode(h: Handle, mode: u32) -> i32;
    pub fn GetConsoleScreenBufferInfo(h: Handle, info: *mut ConsoleScreenBufferInfo) -> i32;
    pub fn SetThreadExecutionState(flags: u32) -> u32;
    pub fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
    pub fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

/// NUL-terminated UTF-16 for passing to `W` functions.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// UTF-16 up to the first NUL (or the whole slice).
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Read a NUL-terminated UTF-16 string from a pointer owned by the OS.
///
/// # Safety
/// `p` must be null or point to a readable NUL-terminated UTF-16 string.
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    // SAFETY: caller guarantees a NUL terminator.
    unsafe {
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}

/// A DLL loaded (or already mapped) into this process. Never freed: these are system DLLs that
/// stay loaded for the life of the tool.
pub struct Lib(Handle);

impl Lib {
    pub fn load(name: &str) -> Option<Lib> {
        let h = unsafe { LoadLibraryW(wide(name).as_ptr()) };
        (h != 0).then_some(Lib(h))
    }

    pub fn mapped(name: &str) -> Option<Lib> {
        let h = unsafe { GetModuleHandleW(wide(name).as_ptr()) };
        (h != 0).then_some(Lib(h))
    }

    /// Look up an export and cast it to a function pointer type.
    ///
    /// # Safety
    /// `F` must be the exact `unsafe extern "system" fn` signature of the export.
    pub unsafe fn get<F: Copy>(&self, name: &str) -> Option<F> {
        assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*const c_void>());
        let mut n = name.as_bytes().to_vec();
        n.push(0);
        let p = unsafe { GetProcAddress(self.0, n.as_ptr()) };
        // SAFETY: same size, and the caller vouches for the signature.
        (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*const c_void, F>(&p) })
    }
}

/// A process handle closed on drop.
pub struct ProcHandle(pub Handle);

impl ProcHandle {
    pub fn open(pid: u32, access: u32) -> Option<ProcHandle> {
        let h = unsafe { OpenProcess(access, 0, pid) };
        // Not `then_some(ProcHandle(h))`: that builds and drops a ProcHandle(0) on failure, and its
        // CloseHandle overwrites the error code callers read with GetLastError.
        if h == 0 { None } else { Some(ProcHandle(h)) }
    }
}

impl Drop for ProcHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub fn now_filetime() -> u64 {
    let mut ft = FileTime::default();
    unsafe { GetSystemTimeAsFileTime(&mut ft) };
    ft.as_u64()
}

pub fn unix_to_filetime(secs: f64) -> u64 {
    (secs.max(0.0) * 1e7) as u64 + FILETIME_UNIX_EPOCH
}

pub fn filetime_to_unix(ft: u64) -> f64 {
    (ft.saturating_sub(FILETIME_UNIX_EPOCH)) as f64 / 1e7
}

fn system_time(ft: u64, local: bool) -> SystemTime {
    let mut src = FileTime::from_u64(ft);
    if local {
        let utc = src;
        unsafe { FileTimeToLocalFileTime(&utc, &mut src) };
    }
    let mut st = SystemTime::default();
    unsafe { FileTimeToSystemTime(&src, &mut st) };
    st
}

/// "10-08 12:26" in local time.
pub fn fmt_short(ft: u64) -> String {
    let t = system_time(ft, true);
    format!("{:02}-{:02} {:02}:{:02}", t.month, t.day, t.hour, t.minute)
}

/// "2026-10-08T04:26:59Z".
pub fn fmt_utc(ft: u64) -> String {
    let t = system_time(ft, false);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year, t.month, t.day, t.hour, t.minute, t.second)
}

/// Turn on ANSI escapes for the console (a no-op where they are already on or stdout is a pipe).
/// Returns the console width when stdout is a console.
pub fn console() -> Option<usize> {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(h, &mut mode) == 0 {
            return None;
        }
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        let mut info = ConsoleScreenBufferInfo::default();
        if GetConsoleScreenBufferInfo(h, &mut info) == 0 {
            return None;
        }
        Some((info.window[2] - info.window[0] + 1).max(40) as usize)
    }
}

/// "2026-10-19 06:03" in local time, from a Unix time.
pub fn fmt_local_unix(secs: f64) -> String {
    let t = system_time(unix_to_filetime(secs), true);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute)
}

/// Whether the machine runs on AC power, when Windows says.
pub fn on_ac_power() -> Option<bool> {
    let mut s = SystemPowerStatus::default();
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return None;
    }
    match s.ac_line_status {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

pub const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
pub const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
pub const ES_CONTINUOUS: u32 = 0x8000_0000;

/// What a running job asks of the power manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Awake {
    /// Ask for nothing.
    Off,
    /// The machine must not go to sleep or idle into Modern Standby; the screen may still switch off.
    System,
    /// As `System`, and the screen stays on. Screen-off is what starts Modern Standby on most laptops.
    SystemAndDisplay,
}

/// A request that the machine stay awake, held until dropped (`SetThreadExecutionState` with
/// `ES_CONTINUOUS`). It changes no power setting and no power plan: Windows forgets the request when
/// this thread ends, whatever the exit path, and an explicit `ES_CONTINUOUS` call on drop withdraws it
/// earlier. The request belongs to the thread that made it, so keep the guard on that thread.
pub struct KeepAwake(Awake);

impl KeepAwake {
    pub fn hold(how: Awake) -> KeepAwake {
        if let Some(flags) = Self::flags(how) {
            unsafe { SetThreadExecutionState(flags) };
        }
        KeepAwake(how)
    }

    fn flags(how: Awake) -> Option<u32> {
        match how {
            Awake::Off => None,
            Awake::System => Some(ES_CONTINUOUS | ES_SYSTEM_REQUIRED),
            Awake::SystemAndDisplay => Some(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED),
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if self.0 != Awake::Off {
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state this thread holds, read by setting the same continuous state again (the call returns the previous one).
    fn held_now() -> u32 {
        let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        // Put back what was there.
        unsafe { SetThreadExecutionState(prev) };
        prev & !ES_CONTINUOUS
    }

    #[test]
    fn keep_awake_holds_and_releases_on_this_thread() {
        assert_eq!(held_now(), 0);
        {
            let _g = KeepAwake::hold(Awake::SystemAndDisplay);
            assert_eq!(held_now(), ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
        }
        assert_eq!(held_now(), 0);
        {
            let _g = KeepAwake::hold(Awake::System);
            assert_eq!(held_now(), ES_SYSTEM_REQUIRED);
        }
        assert_eq!(held_now(), 0);
        let _g = KeepAwake::hold(Awake::Off);
        assert_eq!(held_now(), 0);
    }
}
