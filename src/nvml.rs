//! Board power from NVML (`nvml.dll`, which ships with the NVIDIA driver in System32), resolved at
//! run time like the other Windows libraries here. Only power is read: under WDDM NVML cannot
//! report per-process memory (see `gpu.rs`), but board power and the power limits work.

use std::ffi::c_void;

use crate::power::PowerState;
use crate::sys::Lib;

type Init = unsafe extern "system" fn() -> i32;
type Shutdown = unsafe extern "system" fn() -> i32;
type Count = unsafe extern "system" fn(*mut u32) -> i32;
type HandleByIndex = unsafe extern "system" fn(u32, *mut *mut c_void) -> i32;
type Name = unsafe extern "system" fn(*mut c_void, *mut u8, u32) -> i32;
type Milliwatts = unsafe extern "system" fn(*mut c_void, *mut u32) -> i32;
type Utilization = unsafe extern "system" fn(*mut c_void, *mut [u32; 2]) -> i32;

const NVML_SUCCESS: i32 = 0;

/// Power state of every NVIDIA card NVML can see (none when there is no NVIDIA driver).
pub fn read() -> Vec<PowerState> {
    let Some(lib) = Lib::load("nvml.dll") else { return Vec::new() };
    // SAFETY: signatures are those of NVML's C API (nvml.h); all take and return plain integers/pointers.
    unsafe {
        let (Some(init), Some(shutdown), Some(count), Some(by_index)) = (
            lib.get::<Init>("nvmlInit_v2"),
            lib.get::<Shutdown>("nvmlShutdown"),
            lib.get::<Count>("nvmlDeviceGetCount_v2"),
            lib.get::<HandleByIndex>("nvmlDeviceGetHandleByIndex_v2"),
        ) else {
            return Vec::new();
        };
        let (name, usage, enforced, default, util) = (
            lib.get::<Name>("nvmlDeviceGetName"),
            lib.get::<Milliwatts>("nvmlDeviceGetPowerUsage"),
            lib.get::<Milliwatts>("nvmlDeviceGetEnforcedPowerLimit"),
            lib.get::<Milliwatts>("nvmlDeviceGetPowerManagementDefaultLimit"),
            lib.get::<Utilization>("nvmlDeviceGetUtilizationRates"),
        );
        if init() != NVML_SUCCESS {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut n = 0u32;
        if count(&mut n) == NVML_SUCCESS {
            for i in 0..n {
                let mut h: *mut c_void = std::ptr::null_mut();
                if by_index(i, &mut h) != NVML_SUCCESS || h.is_null() {
                    continue;
                }
                let mw = |f: Option<Milliwatts>| {
                    let mut v = 0u32;
                    f.filter(|f| f(h, &mut v) == NVML_SUCCESS).map_or(0.0, |_| f64::from(v) / 1000.0)
                };
                let mut buf = [0u8; 96];
                let label = match name {
                    Some(f) if f(h, buf.as_mut_ptr(), buf.len() as u32) == NVML_SUCCESS => {
                        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                        String::from_utf8_lossy(&buf[..end]).into_owned()
                    }
                    _ => format!("NVIDIA GPU {i}"),
                };
                let mut u = [0u32; 2];
                let util_pct = match util {
                    Some(f) if f(h, &mut u) == NVML_SUCCESS => u[0],
                    _ => 0,
                };
                out.push(PowerState {
                    name: label,
                    draw_w: mw(usage),
                    enforced_w: mw(enforced),
                    default_w: mw(default),
                    util_pct,
                });
            }
        }
        shutdown();
        out
    }
}

/// The power state of the card named like `want` (a substring of its name), else the first one.
pub fn pick(all: Vec<PowerState>, want: Option<&str>) -> Option<PowerState> {
    let want = want.map(str::to_ascii_lowercase);
    all.iter()
        .find(|p| want.as_ref().is_some_and(|w| p.name.to_ascii_lowercase().contains(w)))
        .cloned()
        .or_else(|| all.into_iter().next())
}
