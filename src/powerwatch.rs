//! Windows power events, so `run` checks the card again the moment something changes rather than at
//! its next poll: the power source (AC or battery), the power plan, the power mode (the Windows 11
//! slider, as the effective power mode), battery saver and energy saver, the lid, the display turning
//! off or on (which is what starts and ends Modern Standby), and sleep and resume.
//!
//! Everything comes from powrprof.dll, resolved at run time like the other Windows libraries, in its
//! callback form: no window, no message loop and no thread of our own. Windows calls `on_notify` and
//! `on_mode` on one of its worker threads; they queue the event and wake `wait`. Subscribing changes no
//! setting. Windows sends each setting's current value when a subscription starts; that first value is
//! the baseline, not a change.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::sys::Lib;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid(u32, u16, u16, [u8; 8]);

const ACDC_POWER_SOURCE: Guid = Guid(0x5d3e9a59, 0xe9d5, 0x4b00, [0xa6, 0xbd, 0xff, 0x34, 0xff, 0x51, 0x65, 0x48]);
const ACTIVE_POWERSCHEME: Guid = Guid(0x31f9f286, 0x5084, 0x42fe, [0xb7, 0x20, 0x2b, 0x02, 0x64, 0x99, 0x37, 0x63]);
const CONSOLE_DISPLAY_STATE: Guid = Guid(0x6fe69556, 0x704a, 0x47a0, [0x8f, 0x24, 0xc2, 0x8d, 0x93, 0x6f, 0xda, 0x47]);
const POWER_SAVING_STATUS: Guid = Guid(0xe00958c0, 0xc213, 0x4ace, [0xac, 0x77, 0xfe, 0xcc, 0xed, 0x2e, 0xee, 0xa5]);
const ENERGY_SAVER_STATUS: Guid = Guid(0x550e8400, 0xe29b, 0x41d4, [0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00, 0x00]);
const LIDSWITCH_STATE_CHANGE: Guid = Guid(0xba3e0f4d, 0xb817, 0x4094, [0xa2, 0xd1, 0xd5, 0x63, 0x79, 0xe6, 0xa0, 0xf3]);
const SETTINGS: [Guid; 6] = [
    ACDC_POWER_SOURCE,
    ACTIVE_POWERSCHEME,
    CONSOLE_DISPLAY_STATE,
    POWER_SAVING_STATUS,
    ENERGY_SAVER_STATUS,
    LIDSWITCH_STATE_CHANGE,
];

/// The built-in power plans.
const SCHEMES: [(Guid, &str); 4] = [
    (Guid(0x381b4222, 0xf694, 0x41f0, [0x96, 0x85, 0xff, 0x5b, 0xb2, 0x60, 0xdf, 0x2e]), "Balanced"),
    (Guid(0x8c5e7fda, 0xe8bf, 0x4a96, [0x9a, 0x85, 0xa6, 0xe2, 0x3a, 0x8c, 0x63, 0x5c]), "High performance"),
    (Guid(0xa1841308, 0x3541, 0x4fab, [0xbc, 0x81, 0xf7, 0x15, 0x56, 0xf2, 0x0b, 0x4a]), "Power saver"),
    (Guid(0xe9a42b02, 0xd5df, 0x448d, [0xaa, 0x00, 0x03, 0xf1, 0x47, 0x49, 0xeb, 0x61]), "Ultimate Performance"),
];

const DEVICE_NOTIFY_CALLBACK: u32 = 2;
const PBT_APMSUSPEND: u32 = 0x4;
const PBT_APMRESUMEAUTOMATIC: u32 = 0x12;
const PBT_POWERSETTINGCHANGE: u32 = 0x8013;
const EFFECTIVE_POWER_MODE_V2: u32 = 2;
/// A setting's first value arriving this long after the subscriptions started is a change, not the
/// value Windows sends when a subscription starts.
const BASELINE_SECS: f64 = 5.0;
const MAX_QUEUED: usize = 64;

type NotifyCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> u32;
type ModeCallback = unsafe extern "system" fn(i32, *mut c_void);
type SettingRegister = unsafe extern "system" fn(*const Guid, u32, *const c_void, *mut *mut c_void) -> u32;
type SuspendRegister = unsafe extern "system" fn(u32, *const c_void, *mut *mut c_void) -> u32;
type ModeRegister = unsafe extern "system" fn(u32, ModeCallback, *mut c_void, *mut *mut c_void) -> i32;
type Unregister = unsafe extern "system" fn(*mut c_void) -> u32;

/// DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS. Static, so it outlives every subscription.
#[repr(C)]
struct Subscribe {
    callback: NotifyCallback,
    context: usize,
}

static SUBSCRIBE: Subscribe = Subscribe { callback: on_notify, context: 0 };

/// One thing Windows reported.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Unix seconds.
    pub at: f64,
    pub what: String,
}

struct Shared {
    events: VecDeque<Event>,
    /// The last value of each setting, to tell a change from a repeat.
    seen: Vec<(&'static str, String)>,
    started: f64,
}

impl Shared {
    const fn new() -> Shared {
        Shared { events: VecDeque::new(), seen: Vec::new(), started: 0.0 }
    }

    fn push(&mut self, e: Event) {
        if self.events.len() >= MAX_QUEUED {
            self.events.pop_front();
        }
        self.events.push_back(e);
    }

    /// A setting's value: queued as an event when it differs from the last one, or when it is the first
    /// and comes well after the subscriptions started.
    fn record(&mut self, at: f64, what: &'static str, value: String) {
        let started = self.started;
        match self.seen.iter_mut().find(|(w, _)| *w == what) {
            Some((_, old)) if *old == value => {}
            Some((_, old)) => {
                let text = format!("{what} {old} -> {value}");
                *old = value;
                self.push(Event { at, what: text });
            }
            None => {
                if at - started > BASELINE_SECS {
                    self.push(Event { at, what: format!("{what} {value}") });
                }
                self.seen.push((what, value));
            }
        }
    }
}

static SHARED: Mutex<Shared> = Mutex::new(Shared::new());
static WAKE: Condvar = Condvar::new();

fn lock() -> MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> f64 {
    crate::sys::filetime_to_unix(crate::sys::now_filetime())
}

fn guid_at(b: &[u8]) -> Option<Guid> {
    let b: &[u8; 16] = b.get(..16)?.try_into().ok()?;
    let mut d4 = [0u8; 8];
    d4.copy_from_slice(&b[8..]);
    Some(Guid(
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        d4,
    ))
}

fn guid_string(g: Guid) -> String {
    let d = g.3;
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.0, g.1, g.2, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

/// What a power-setting notification says: the setting in words and its new value. None for a setting
/// not subscribed to or data too short to read.
fn decode(setting: Guid, data: &[u8]) -> Option<(&'static str, String)> {
    let dword = data.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let word = |s: &str| s.to_string();
    Some(match setting {
        ACDC_POWER_SOURCE => (
            "power source",
            match dword? {
                0 => word("AC"),
                1 => word("battery"),
                2 => word("UPS"),
                n => format!("{n}"),
            },
        ),
        ACTIVE_POWERSCHEME => {
            let g = guid_at(data)?;
            ("power plan", SCHEMES.iter().find(|(s, _)| *s == g).map_or_else(|| guid_string(g), |(_, n)| word(n)))
        }
        // Dimmed counts as on: only off and on start and end Modern Standby.
        CONSOLE_DISPLAY_STATE => ("display", word(if dword? == 0 { "off" } else { "on" })),
        POWER_SAVING_STATUS => ("battery saver", word(if dword? == 0 { "off" } else { "on" })),
        ENERGY_SAVER_STATUS => (
            "energy saver",
            match dword? {
                0 => word("off"),
                1 => word("standard"),
                2 => word("high savings"),
                n => format!("{n}"),
            },
        ),
        LIDSWITCH_STATE_CHANGE => ("lid", word(if dword? == 0 { "closed" } else { "open" })),
        _ => return None,
    })
}

/// EFFECTIVE_POWER_MODE in words. The Windows 11 power-mode slider moves between better battery,
/// balanced and the performance modes.
fn mode_name(mode: i32) -> String {
    match mode {
        0 => "battery saver".into(),
        1 => "better battery".into(),
        2 => "balanced".into(),
        3 => "high performance".into(),
        4 => "max performance".into(),
        5 => "game mode".into(),
        6 => "mixed reality".into(),
        n => format!("{n}"),
    }
}

/// Called by Windows on a worker thread.
unsafe extern "system" fn on_notify(_context: *mut c_void, kind: u32, setting: *mut c_void) -> u32 {
    let at = now();
    let mut s = lock();
    match kind {
        PBT_POWERSETTINGCHANGE if !setting.is_null() => {
            // SAFETY: Windows passes a POWERBROADCAST_SETTING: a GUID, a DWORD byte count, then the bytes.
            let (guid, data) = unsafe {
                let p = setting as *const u8;
                let guid = std::ptr::read_unaligned(p as *const Guid);
                let len = std::ptr::read_unaligned(p.add(16) as *const u32) as usize;
                (guid, std::slice::from_raw_parts(p.add(20), len.min(64)).to_vec())
            };
            if let Some((what, value)) = decode(guid, &data) {
                s.record(at, what, value);
            }
        }
        PBT_APMSUSPEND => s.push(Event { at, what: "the machine is going to sleep".into() }),
        PBT_APMRESUMEAUTOMATIC => s.push(Event { at, what: "the machine resumed from sleep".into() }),
        _ => {}
    }
    drop(s);
    WAKE.notify_all();
    0
}

/// Called by Windows on a worker thread: with the current mode as the subscription starts, then on each change.
unsafe extern "system" fn on_mode(mode: i32, _context: *mut c_void) {
    let at = now();
    lock().record(at, "power mode", mode_name(mode));
    WAKE.notify_all();
}

/// The subscriptions, ended when this is dropped.
pub struct Watch {
    lib: Option<Lib>,
    settings: Vec<*mut c_void>,
    suspend: Option<*mut c_void>,
    mode: Option<*mut c_void>,
}

impl Watch {
    /// Subscribe to what Windows offers here; whatever it refuses is simply not watched (the poll still
    /// runs).
    pub fn start() -> Watch {
        let mut w = Watch { lib: None, settings: Vec::new(), suspend: None, mode: None };
        let Some(lib) = Lib::load("powrprof.dll") else { return w };
        lock().started = now();
        let recipient = &SUBSCRIBE as *const Subscribe as *const c_void;
        // SAFETY: signatures are those of powerbase.h / powersetting.h; the recipient and the callbacks are
        // statics, valid for as long as any subscription lasts.
        unsafe {
            if let Some(register) = lib.get::<SettingRegister>("PowerSettingRegisterNotification") {
                for g in SETTINGS {
                    let mut h: *mut c_void = std::ptr::null_mut();
                    if register(&g, DEVICE_NOTIFY_CALLBACK, recipient, &mut h) == 0 && !h.is_null() {
                        w.settings.push(h);
                    }
                }
            }
            if let Some(register) = lib.get::<SuspendRegister>("PowerRegisterSuspendResumeNotification") {
                let mut h: *mut c_void = std::ptr::null_mut();
                if register(DEVICE_NOTIFY_CALLBACK, recipient, &mut h) == 0 && !h.is_null() {
                    w.suspend = Some(h);
                }
            }
            if let Some(register) = lib.get::<ModeRegister>("PowerRegisterForEffectivePowerModeNotifications") {
                let mut h: *mut c_void = std::ptr::null_mut();
                if register(EFFECTIVE_POWER_MODE_V2, on_mode, std::ptr::null_mut(), &mut h) >= 0 && !h.is_null() {
                    w.mode = Some(h);
                }
            }
        }
        w.lib = Some(lib);
        w
    }

    /// How many subscriptions Windows accepted.
    pub fn subscriptions(&self) -> usize {
        self.settings.len() + usize::from(self.suspend.is_some()) + usize::from(self.mode.is_some())
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let Some(lib) = &self.lib else { return };
        // SAFETY: each handle came from the matching register call and is unregistered once.
        unsafe {
            if let Some(f) = lib.get::<Unregister>("PowerSettingUnregisterNotification") {
                for h in self.settings.drain(..) {
                    f(h);
                }
            }
            if let (Some(f), Some(h)) =
                (lib.get::<Unregister>("PowerUnregisterSuspendResumeNotification"), self.suspend)
            {
                f(h);
            }
            if let (Some(f), Some(h)) =
                (lib.get::<Unregister>("PowerUnregisterFromEffectivePowerModeNotifications"), self.mode)
            {
                f(h);
            }
        }
    }
}

/// Wait up to `timeout` for Windows power events and return them, at once when some are already queued
/// (an empty list when the time ran out).
pub fn wait(timeout: Duration) -> Vec<Event> {
    let (mut s, _) =
        WAKE.wait_timeout_while(lock(), timeout, |s| s.events.is_empty()).unwrap_or_else(|e| e.into_inner());
    s.events.drain(..).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(g: Guid) -> Vec<u8> {
        let mut v = g.0.to_le_bytes().to_vec();
        v.extend(g.1.to_le_bytes());
        v.extend(g.2.to_le_bytes());
        v.extend(g.3);
        v
    }

    #[test]
    fn notifications_decode_to_words() {
        assert_eq!(decode(ACDC_POWER_SOURCE, &[1, 0, 0, 0]), Some(("power source", "battery".into())));
        assert_eq!(decode(ACDC_POWER_SOURCE, &[0, 0, 0, 0]), Some(("power source", "AC".into())));
        assert_eq!(decode(CONSOLE_DISPLAY_STATE, &[0, 0, 0, 0]), Some(("display", "off".into())));
        // Dimmed (2) is not a change from on.
        assert_eq!(decode(CONSOLE_DISPLAY_STATE, &[2, 0, 0, 0]), Some(("display", "on".into())));
        assert_eq!(decode(LIDSWITCH_STATE_CHANGE, &[0, 0, 0, 0]), Some(("lid", "closed".into())));
        assert_eq!(decode(ENERGY_SAVER_STATUS, &[2, 0, 0, 0]), Some(("energy saver", "high savings".into())));
        assert_eq!(decode(POWER_SAVING_STATUS, &[1, 0, 0, 0]), Some(("battery saver", "on".into())));
        // The active plan arrives as a GUID; this machine's is Balanced.
        assert_eq!(decode(ACTIVE_POWERSCHEME, &bytes(SCHEMES[0].0)), Some(("power plan", "Balanced".into())));
        let custom = Guid(0x12345678, 0x9abc, 0xdef0, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            decode(ACTIVE_POWERSCHEME, &bytes(custom)),
            Some(("power plan", "12345678-9abc-def0-0102-030405060708".into()))
        );
        assert_eq!(guid_string(SCHEMES[0].0), "381b4222-f694-41f0-9685-ff5bb260df2e");
        // Too short, or a setting not subscribed to.
        assert_eq!(decode(ACDC_POWER_SOURCE, &[1, 0]), None);
        assert_eq!(decode(ACTIVE_POWERSCHEME, &[0; 8]), None);
        assert_eq!(decode(custom, &[1, 0, 0, 0]), None);
        assert_eq!(mode_name(4), "max performance");
        assert_eq!(mode_name(1), "better battery");
    }

    #[test]
    fn the_first_value_is_a_baseline_and_only_changes_are_queued() {
        let mut s = Shared::new();
        s.started = 1000.0;
        // What Windows sends as the subscriptions start.
        s.record(1000.1, "power source", "AC".into());
        s.record(1000.2, "power mode", "balanced".into());
        assert!(s.events.is_empty());
        // Repeats are not changes.
        s.record(1100.0, "power source", "AC".into());
        assert!(s.events.is_empty());
        s.record(1200.0, "power mode", "max performance".into());
        s.record(1300.0, "power source", "battery".into());
        let got: Vec<_> = s.events.iter().map(|e| (e.at, e.what.as_str())).collect();
        assert_eq!(
            got,
            vec![(1200.0, "power mode balanced -> max performance"), (1300.0, "power source AC -> battery")]
        );
        // A setting heard of first long after the start is a change in its own right.
        s.record(1400.0, "lid", "closed".into());
        assert_eq!(s.events.back().map(|e| e.what.as_str()), Some("lid closed"));
        // The queue is bounded: the oldest go first.
        for i in 0..(MAX_QUEUED + 10) {
            s.push(Event { at: i as f64, what: "x".into() });
        }
        assert_eq!(s.events.len(), MAX_QUEUED);
        assert_eq!(s.events.front().map(|e| e.at), Some(10.0));
    }

    #[test]
    fn wait_returns_as_soon_as_an_event_is_queued() {
        let t = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            lock().push(Event { at: 1.0, what: "test event".into() });
            WAKE.notify_all();
        });
        let start = std::time::Instant::now();
        let got = wait(Duration::from_secs(20));
        t.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(got.iter().any(|e| e.what == "test event"), "{got:?}");
    }

    /// Subscribes for real (it changes nothing) and checks that Windows calls back with the current
    /// power source, which it sends as a subscription starts.
    #[test]
    fn windows_accepts_the_subscriptions_and_calls_back() {
        let w = Watch::start();
        if w.subscriptions() == 0 {
            return; // powrprof.dll without these exports: only the poll runs.
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if lock().seen.iter().any(|(what, _)| *what == "power source") {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("no callback from Windows within 10 s ({} subscriptions)", w.subscriptions());
    }
}
