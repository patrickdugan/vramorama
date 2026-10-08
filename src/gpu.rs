//! GPU adapters (DXGI) and per-process GPU memory and engine use (PDH performance counters).
//!
//! Under the WDDM driver model NVML reports per-process memory as "N/A", so `nvidia-smi` cannot
//! say who holds VRAM. Windows itself tracks it for Task Manager and publishes it through the
//! `GPU Process Memory` and `GPU Engine` counter sets, which work for every vendor.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;

use crate::parse::{luid_key, parse_instance};
use crate::sys::{Lib, from_wide, from_wide_ptr, wide};

#[derive(Clone)]
pub struct Adapter {
    pub luid: u64,
    pub name: String,
    pub vendor: u32,
    pub dedicated: u64,
    pub software: bool,
}

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const IID_IDXGI_FACTORY1: Guid = Guid(0x770aae78, 0xf26f, 0x4dba, [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87]);
const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;

#[repr(C)]
struct AdapterDesc1 {
    description: [u16; 128],
    vendor_id: u32,
    device_id: u32,
    sub_sys_id: u32,
    revision: u32,
    dedicated_video: usize,
    dedicated_system: usize,
    shared_system: usize,
    luid_low: u32,
    luid_high: i32,
    flags: u32,
}

type CreateDxgiFactory1 = unsafe extern "system" fn(*const Guid, *mut *mut c_void) -> i32;
type EnumAdapters1 = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32;
type GetDesc1 = unsafe extern "system" fn(*mut c_void, *mut AdapterDesc1) -> i32;
type Release = unsafe extern "system" fn(*mut c_void) -> u32;

// COM vtable slots: IUnknown 0-2, IDXGIObject 3-6, IDXGIFactory 7-11, IDXGIFactory1::EnumAdapters1 12.
// IDXGIAdapter 7-9, IDXGIAdapter1::GetDesc1 10.
const SLOT_RELEASE: usize = 2;
const SLOT_ENUM_ADAPTERS1: usize = 12;
const SLOT_GET_DESC1: usize = 10;

/// # Safety
/// `obj` must be a live COM object whose vtable has a slot `idx` of type `F`.
unsafe fn slot<F: Copy>(obj: *mut c_void, idx: usize) -> F {
    unsafe {
        let vtbl = *(obj as *const *const *const c_void);
        std::mem::transmute_copy::<*const c_void, F>(&*vtbl.add(idx))
    }
}

/// Adapters known to DXGI, in enumeration order. Empty if DXGI is unavailable.
pub fn adapters() -> Vec<Adapter> {
    let mut out = Vec::new();
    let Some(dxgi) = Lib::load("dxgi.dll") else { return out };
    let Some(create) = (unsafe { dxgi.get::<CreateDxgiFactory1>("CreateDXGIFactory1") }) else { return out };
    unsafe {
        let mut factory = null_mut();
        if create(&IID_IDXGI_FACTORY1, &mut factory) < 0 || factory.is_null() {
            return out;
        }
        let enum_adapters: EnumAdapters1 = slot(factory, SLOT_ENUM_ADAPTERS1);
        for i in 0.. {
            let mut adapter = null_mut();
            if enum_adapters(factory, i, &mut adapter) < 0 || adapter.is_null() {
                break; // DXGI_ERROR_NOT_FOUND ends the list
            }
            let mut d: AdapterDesc1 = std::mem::zeroed();
            let get_desc: GetDesc1 = slot(adapter, SLOT_GET_DESC1);
            if get_desc(adapter, &mut d) >= 0 {
                out.push(Adapter {
                    luid: luid_key(d.luid_high as u32, d.luid_low),
                    name: from_wide(&d.description),
                    vendor: d.vendor_id,
                    dedicated: d.dedicated_video as u64,
                    software: d.flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0,
                });
            }
            slot::<Release>(adapter, SLOT_RELEASE)(adapter);
        }
        slot::<Release>(factory, SLOT_RELEASE)(factory);
    }
    out
}

/// One process's use of one adapter.
#[derive(Clone, Debug)]
pub struct ProcUsage {
    pub pid: u32,
    pub luid: u64,
    pub dedicated: u64,
    /// Busiest engine, in percent, as Task Manager shows it. `None` before two collections.
    pub util: Option<f64>,
}

pub struct Sample {
    pub procs: Vec<ProcUsage>,
    /// Dedicated memory in use per adapter, all processes and the kernel together.
    pub adapter_used: HashMap<u64, u64>,
}

type PdhOpenQueryW = unsafe extern "system" fn(*const u16, usize, *mut isize) -> u32;
type PdhAddEnglishCounterW = unsafe extern "system" fn(isize, *const u16, usize, *mut isize) -> u32;
type PdhCollectQueryData = unsafe extern "system" fn(isize) -> u32;
type PdhGetFormattedCounterArrayW = unsafe extern "system" fn(isize, u32, *mut u32, *mut u32, *mut u8) -> u32;
type PdhCloseQuery = unsafe extern "system" fn(isize) -> u32;

const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
const PDH_FMT_LARGE: u32 = 0x0000_0400;
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;
const PDH_MORE_DATA: u32 = 0x8000_07D2;

#[repr(C)]
struct FmtItem {
    name: *const u16,
    status: u32,
    _pad: u32,
    value: u64, // union of i64 (PDH_FMT_LARGE) and f64 (PDH_FMT_DOUBLE)
}

pub struct Sampler {
    query: isize,
    proc_mem: isize,
    engine: isize,
    adapter_mem: isize,
    collect: PdhCollectQueryData,
    get_array: PdhGetFormattedCounterArrayW,
    close: PdhCloseQuery,
    collections: u32,
}

impl Sampler {
    pub fn open() -> Result<Sampler, String> {
        let pdh = Lib::load("pdh.dll").ok_or("pdh.dll not found")?;
        let missing = |f: &str| format!("pdh.dll has no {f}");
        unsafe {
            let open: PdhOpenQueryW = pdh.get("PdhOpenQueryW").ok_or_else(|| missing("PdhOpenQueryW"))?;
            let add: PdhAddEnglishCounterW =
                pdh.get("PdhAddEnglishCounterW").ok_or_else(|| missing("PdhAddEnglishCounterW"))?;
            let collect = pdh.get("PdhCollectQueryData").ok_or_else(|| missing("PdhCollectQueryData"))?;
            let get_array =
                pdh.get("PdhGetFormattedCounterArrayW").ok_or_else(|| missing("PdhGetFormattedCounterArrayW"))?;
            let close = pdh.get("PdhCloseQuery").ok_or_else(|| missing("PdhCloseQuery"))?;
            let mut query = 0;
            let st = open(std::ptr::null(), 0, &mut query);
            if st != 0 {
                return Err(format!("PdhOpenQuery failed: 0x{st:08X}"));
            }
            let counter = |path: &str| -> Result<isize, String> {
                let mut c = 0;
                match add(query, wide(path).as_ptr(), 0, &mut c) {
                    0 => Ok(c),
                    st => Err(format!(
                        "counter {path} unavailable (0x{st:08X}); this Windows build or GPU driver may not publish GPU counters"
                    )),
                }
            };
            let proc_mem = counter(r"\GPU Process Memory(*)\Dedicated Usage")?;
            let engine = counter(r"\GPU Engine(*)\Utilization Percentage")?;
            let adapter_mem = counter(r"\GPU Adapter Memory(*)\Dedicated Usage")?;
            Ok(Sampler { query, proc_mem, engine, adapter_mem, collect, get_array, close, collections: 0 })
        }
    }

    /// Collect once without reading. Utilization is a rate, so it needs a previous collection.
    pub fn prime(&mut self) {
        unsafe { (self.collect)(self.query) };
        self.collections += 1;
    }

    pub fn sample(&mut self) -> Sample {
        self.prime();
        let mut by_key: HashMap<(u32, u64), ProcUsage> = HashMap::new();
        for (name, raw) in self.read(self.proc_mem, PDH_FMT_LARGE) {
            if let Some((Some(pid), luid)) = parse_instance(&name) {
                let e = by_key.entry((pid, luid)).or_insert(ProcUsage { pid, luid, dedicated: 0, util: None });
                e.dedicated += raw.max(0) as u64;
            }
        }
        if self.collections >= 2 {
            for (name, raw) in self.read(self.engine, PDH_FMT_DOUBLE | PDH_FMT_NOCAP100) {
                if let Some((Some(pid), luid)) = parse_instance(&name) {
                    if let Some(e) = by_key.get_mut(&(pid, luid)) {
                        let v = f64::from_bits(raw as u64);
                        e.util = Some(e.util.map_or(v, |u| u.max(v)));
                    }
                }
            }
            for e in by_key.values_mut() {
                e.util.get_or_insert(0.0);
            }
        }
        let mut adapter_used = HashMap::new();
        for (name, raw) in self.read(self.adapter_mem, PDH_FMT_LARGE) {
            if let Some((None, luid)) = parse_instance(&name) {
                *adapter_used.entry(luid).or_insert(0) += raw.max(0) as u64;
            }
        }
        Sample { procs: by_key.into_values().collect(), adapter_used }
    }

    /// (instance name, raw 64-bit value) for every instance with valid data.
    fn read(&self, counter: isize, fmt: u32) -> Vec<(String, i64)> {
        for _ in 0..4 {
            let (mut size, mut count) = (0u32, 0u32);
            let st = unsafe { (self.get_array)(counter, fmt, &mut size, &mut count, null_mut()) };
            if st != PDH_MORE_DATA || size == 0 {
                return Vec::new();
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)]; // 8-byte aligned
            let st = unsafe { (self.get_array)(counter, fmt, &mut size, &mut count, buf.as_mut_ptr() as *mut u8) };
            if st == PDH_MORE_DATA {
                continue; // instances appeared between the two calls
            }
            if st != 0 {
                return Vec::new();
            }
            let items = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const FmtItem, count as usize) };
            return items
                .iter()
                .filter(|it| it.status <= 1) // PDH_CSTATUS_VALID_DATA or NEW_DATA
                .map(|it| (unsafe { from_wide_ptr(it.name) }, it.value as i64))
                .collect();
        }
        Vec::new()
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe { (self.close)(self.query) };
    }
}
