//! Read-only facts about this machine for `doctor` and `crashes`: the event log, the active power plan's
//! timeouts, Modern Standby support, the display driver and memory use. Nothing is written anywhere.

use std::ffi::c_void;

use crate::crash::{self, Log};
use crate::doctor::{Driver, Facts, Gpu, Timeouts, parse_driver_date};
use crate::evt::EventLog;
use crate::gpu::{self, Sampler};
use crate::sys::{Lib, wide};

const MIB: u64 = 1 << 20;

/// Both logs, as far back as `days`.
pub fn read_log(days: f64) -> Result<Log, String> {
    let secs = (days.max(0.01) * 86_400.0) as u64;
    let evt = EventLog::open()?;
    // A day of margin on the window so a crash near its edge still finds the events around it.
    let margin = secs + 86_400;
    let system = evt.read("System", &crash::system_query(margin), 20_000)?;
    let app = evt.read("Application", &crash::app_query(margin), 5_000).unwrap_or_default();
    Ok(Log { system: crash::parse_events(&system), app: crash::parse_events(&app) })
}

// --- Power plan and capabilities (powrprof.dll)

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

type PowerGetActiveScheme = unsafe extern "system" fn(isize, *mut *mut Guid) -> u32;
type PowerReadValue = unsafe extern "system" fn(isize, *const Guid, *const Guid, *const Guid, *mut u32) -> u32;
type CallNtPowerInformation = unsafe extern "system" fn(i32, *const c_void, u32, *mut c_void, u32) -> i32;

const SUB_VIDEO: Guid = Guid(0x7516b95f, 0xf776, 0x4464, [0x8c, 0x53, 0x06, 0x16, 0x7f, 0x40, 0xcc, 0x99]);
const VIDEO_IDLE: Guid = Guid(0x3c0bc021, 0xc8a8, 0x4e07, [0xa9, 0x73, 0x6b, 0x14, 0xcb, 0xcb, 0x2b, 0x7e]);
const SUB_SLEEP: Guid = Guid(0x238c9fa8, 0x0aad, 0x41ed, [0x83, 0xf4, 0x97, 0xbe, 0x24, 0x2c, 0x8f, 0x20]);
const STANDBY_IDLE: Guid = Guid(0x29f6c1db, 0x86da, 0x48c5, [0x9f, 0xdb, 0xf2, 0xb6, 0x7b, 0x1f, 0x44, 0xda]);
const SYSTEM_POWER_CAPABILITIES: i32 = 4;
/// Offset of the `AoAc` (Modern Standby) flag in SYSTEM_POWER_CAPABILITIES.
const CAPS_AOAC: usize = 20;

/// The active plan's display-off and sleep timeouts, on AC and on battery.
pub fn timeouts() -> Timeouts {
    let mut t = Timeouts::default();
    let Some(lib) = Lib::load("powrprof.dll") else { return t };
    // SAFETY: signatures are those of powrprof.h; the scheme GUID is owned by the OS and freed with LocalFree.
    unsafe {
        let (Some(active), Some(ac), Some(dc)) = (
            lib.get::<PowerGetActiveScheme>("PowerGetActiveScheme"),
            lib.get::<PowerReadValue>("PowerReadACValueIndex"),
            lib.get::<PowerReadValue>("PowerReadDCValueIndex"),
        ) else {
            return t;
        };
        let mut scheme: *mut Guid = std::ptr::null_mut();
        if active(0, &mut scheme) != 0 || scheme.is_null() {
            return t;
        }
        let read = |f: PowerReadValue, sub: &Guid, setting: &Guid| {
            let mut v = 0u32;
            (f(0, scheme, sub, setting, &mut v) == 0).then_some(v)
        };
        t.display_ac = read(ac, &SUB_VIDEO, &VIDEO_IDLE);
        t.display_dc = read(dc, &SUB_VIDEO, &VIDEO_IDLE);
        t.sleep_ac = read(ac, &SUB_SLEEP, &STANDBY_IDLE);
        t.sleep_dc = read(dc, &SUB_SLEEP, &STANDBY_IDLE);
        crate::sys::LocalFree(scheme as *mut c_void);
    }
    t
}

/// Whether the machine uses Modern Standby (S0 low power idle) rather than classic S3 sleep.
pub fn modern_standby() -> Option<bool> {
    let lib = Lib::load("powrprof.dll")?;
    // SAFETY: CallNtPowerInformation(SystemPowerCapabilities) fills a 76-byte structure; the buffer is larger.
    unsafe {
        let call = lib.get::<CallNtPowerInformation>("CallNtPowerInformation")?;
        let mut buf = [0u8; 128];
        if call(SYSTEM_POWER_CAPABILITIES, std::ptr::null(), 0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) != 0
        {
            return None;
        }
        Some(buf[CAPS_AOAC] != 0)
    }
}

// --- Display drivers (registry, advapi32.dll)

type RegGetValueW =
    unsafe extern "system" fn(isize, *const u16, *const u16, u32, *mut u32, *mut c_void, *mut u32) -> i32;

/// winreg.h sign-extends the 32-bit predefined key values.
const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as i32 as isize;
const RRF_RT_REG_SZ: u32 = 0x2;
const DISPLAY_CLASS: &str = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

fn reg_string(get: RegGetValueW, key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 512];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: `buf` is `bytes` long; RegGetValueW writes a NUL-terminated string within it.
    let st = unsafe {
        get(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut bytes,
        )
    };
    (st == 0).then(|| crate::sys::from_wide(&buf))
}

/// The display drivers the registry lists, one per adapter.
pub fn display_drivers() -> Vec<Driver> {
    let Some(lib) = Lib::load("advapi32.dll") else { return Vec::new() };
    let Some(get) = (unsafe { lib.get::<RegGetValueW>("RegGetValueW") }) else { return Vec::new() };
    (0..16)
        .filter_map(|i| {
            let key = format!(r"{DISPLAY_CLASS}\{i:04}");
            let desc = reg_string(get, &key, "DriverDesc")?;
            Some(Driver {
                desc,
                version: reg_string(get, &key, "DriverVersion").unwrap_or_default(),
                date: reg_string(get, &key, "DriverDate").and_then(|d| parse_driver_date(&d)),
                provider: reg_string(get, &key, "ProviderName").unwrap_or_default(),
            })
        })
        .collect()
}

// --- Everything `doctor` reads

/// Collect the facts. `report` is the crash analysis, when it could be made.
pub fn facts(report: Option<&crash::Report>) -> Facts {
    let now = crate::sys::filetime_to_unix(crate::sys::now_filetime());
    let mut x = Facts {
        now,
        on_ac: crate::sys::on_ac_power(),
        timeouts: timeouts(),
        modern_standby: modern_standby(),
        drivers: display_drivers(),
        ..Default::default()
    };
    if let Some(m) = crate::procs::system_memory() {
        x.ram_total_mib = m.total / MIB;
        x.ram_avail_mib = m.avail / MIB;
        x.commit_limit_mib = m.commit_limit / MIB;
        x.commit_used_mib = m.commit_limit.saturating_sub(m.commit_avail) / MIB;
    }
    if let Ok(mut sampler) = Sampler::open() {
        let sample = sampler.sample();
        for a in gpu::adapters().into_iter().filter(|a| !a.software && a.dedicated >= 1 << 30) {
            let total = a.dedicated / MIB;
            let used = sample.adapter_used.get(&a.luid).copied().unwrap_or(0) / MIB;
            let spilled = sample
                .procs
                .iter()
                .filter(|p| p.luid == a.luid && crate::spill::spilled(p.shared / MIB, total, used))
                .map(|p| (p.pid, p.shared / MIB))
                .collect();
            x.gpus.push(Gpu {
                name: a.name,
                total_mib: total,
                used_mib: used,
                shared_mib: sample.adapter_shared.get(&a.luid).map(|b| b / MIB),
                spilled,
            });
        }
    }
    x.power_cap = crate::nvml::read().iter().find_map(crate::power::warning);
    if let Some(r) = report {
        x.crashes = Some(crash::stats(r));
        let times: Vec<f64> = r.gpu_events.iter().map(|e| e.ts).collect();
        let since = |secs: f64| {
            crash::bursts(&times.iter().copied().filter(|t| *t >= now - secs).collect::<Vec<_>>(), 30.0).len()
        };
        x.resets_24h = since(86_400.0);
        x.resets_7d = since(7.0 * 86_400.0);
    }
    x
}
