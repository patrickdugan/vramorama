//! Read-only access to the Windows Event Log through `wevtapi.dll`.
//!
//! Used by `crashes`, `doctor` and the GPU-reset check. Like the PDH, DXGI and NVML bindings it is
//! resolved at run time with `LoadLibrary` and `GetProcAddress`, so nothing extra is linked. Events
//! come back as the XML Windows renders for them, newest first, and `crash.rs` reads that.

use crate::sys::{GetLastError, Lib, wide};

type Handle = isize;
type EvtQuery = unsafe extern "system" fn(Handle, *const u16, *const u16, u32) -> Handle;
type EvtNext = unsafe extern "system" fn(Handle, u32, *mut Handle, u32, u32, *mut u32) -> i32;
type EvtRender = unsafe extern "system" fn(Handle, Handle, u32, u32, *mut u16, *mut u32, *mut u32) -> i32;
type EvtClose = unsafe extern "system" fn(Handle) -> i32;

const EVT_QUERY_CHANNEL_PATH: u32 = 0x1;
const EVT_QUERY_REVERSE_DIRECTION: u32 = 0x200;
const EVT_RENDER_EVENT_XML: u32 = 1;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_EVT_CHANNEL_NOT_FOUND: u32 = 15007;
const BATCH: usize = 64;
/// How long one `EvtNext` waits for events, in milliseconds. A local log answers at once.
const NEXT_TIMEOUT_MS: u32 = 3000;

pub struct EventLog {
    query: EvtQuery,
    next: EvtNext,
    render: EvtRender,
    close: EvtClose,
}

impl EventLog {
    pub fn open() -> Result<EventLog, String> {
        let lib = Lib::load("wevtapi.dll").ok_or("wevtapi.dll not found")?;
        let missing = |f: &str| format!("wevtapi.dll has no {f}");
        unsafe {
            Ok(EventLog {
                query: lib.get("EvtQuery").ok_or_else(|| missing("EvtQuery"))?,
                next: lib.get("EvtNext").ok_or_else(|| missing("EvtNext"))?,
                render: lib.get("EvtRender").ok_or_else(|| missing("EvtRender"))?,
                close: lib.get("EvtClose").ok_or_else(|| missing("EvtClose"))?,
            })
        }
    }

    /// Up to `max` events of `channel` ("System", "Application") matching the XPath `query`, newest
    /// first, each rendered as XML. A channel this machine does not have yields no events.
    pub fn read(&self, channel: &str, query: &str, max: usize) -> Result<Vec<String>, String> {
        let flags = EVT_QUERY_CHANNEL_PATH | EVT_QUERY_REVERSE_DIRECTION;
        let h = unsafe { (self.query)(0, wide(channel).as_ptr(), wide(query).as_ptr(), flags) };
        if h == 0 {
            return match unsafe { GetLastError() } {
                ERROR_EVT_CHANNEL_NOT_FOUND => Ok(Vec::new()),
                code => Err(format!("event log query on {channel} failed (error {code}); query: {query}")),
            };
        }
        let mut out = Vec::new();
        'batches: loop {
            let mut events = [0 as Handle; BATCH];
            let mut got = 0u32;
            // ERROR_NO_MORE_ITEMS (and a timeout) end the loop; nothing else is worth distinguishing here.
            if unsafe { (self.next)(h, BATCH as u32, events.as_mut_ptr(), NEXT_TIMEOUT_MS, 0, &mut got) } == 0 {
                break;
            }
            for &e in &events[..got as usize] {
                if out.len() < max {
                    out.extend(self.xml(e));
                }
                unsafe { (self.close)(e) };
            }
            if out.len() >= max {
                break 'batches;
            }
        }
        unsafe { (self.close)(h) };
        Ok(out)
    }

    fn xml(&self, event: Handle) -> Option<String> {
        let mut buf: Vec<u16> = vec![0; 4096];
        for _ in 0..3 {
            let (mut used, mut props) = (0u32, 0u32);
            let ok = unsafe {
                (self.render)(
                    0,
                    event,
                    EVT_RENDER_EVENT_XML,
                    (buf.len() * 2) as u32,
                    buf.as_mut_ptr(),
                    &mut used,
                    &mut props,
                )
            };
            if ok != 0 {
                let units = (used as usize / 2).min(buf.len());
                return Some(crate::sys::from_wide(&buf[..units]));
            }
            if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
                return None;
            }
            buf.resize((used as usize).div_ceil(2) + 1, 0);
        }
        None
    }
}
