//! Crash forensics: why did the machine just reboot, and was the GPU workload to blame?
//!
//! This reads what Windows already recorded in the System and Application event logs (through
//! `evt.rs`), groups it into incidents, one per unexpected reboot, and says what the evidence
//! supports. Nothing here touches a process, a driver or a setting.
//!
//! Where it helps, an incident is lined up against the `vramorama watch --log` file: what the card,
//! RAM and commit charge looked like in the minutes before the crash, and which sessions held the GPU.
//! That is what tells a crash from VRAM contention apart from a driver fault with one job running.
//!
//! The event data used (Windows 10/11):
//! - Kernel-Power 41: the reboot that was not a shutdown. Carries the bugcheck code and parameters (0 when no
//!   bugcheck was written, that is a hang or power loss), `ConnectedStandbyInProgress` (the machine was in
//!   Modern Standby) and power-button fields.
//! - EventLog 6008: "the previous shutdown was unexpected". Its binary payload holds the time the machine was
//!   last alive in UTC, which is when it crashed. Kernel-Power 41 is only logged at the next boot.
//! - WER-SystemErrorReporting 1001 (System log): the bugcheck and the minidump path.
//! - Windows Error Reporting 1001 (Application log): the fault bucket, which names the module and function
//!   (`AV_nvlddmkm!UvmChannelUpdateProgress`), and `LiveKernelEvent` reports: GPU hangs the OS recovered from.
//! - nvlddmkm 153 / Display 4101 / amdkmdag 4101: the GPU reset (TDR) itself.
//! - Kernel-Power 506 / 507 (and 42): Modern Standby (screen off) and sleep transitions.

use crate::json::{Value, obj};
use crate::parse::thousands;
use crate::tdr::parse_iso_utc;

// ---------------------------------------------------------------------------------------------
// Event XML

/// One event from the log, reduced to what the analysis reads.
#[derive(Clone, Debug, Default)]
pub struct Ev {
    /// Unix time.
    pub ts: f64,
    pub provider: String,
    pub id: u32,
    /// `<Data>` elements: the name (empty when the event does not name them) and the text.
    pub data: Vec<(String, String)>,
    /// `<Binary>` payload as hex digits.
    pub binary: String,
}

impl Ev {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.data.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    pub fn nth(&self, i: usize) -> Option<&str> {
        self.data.get(i).map(|(_, v)| v.as_str())
    }
    fn is(&self, provider: &str, id: u32) -> bool {
        self.id == id && self.provider == provider
    }
}

/// The value of attribute `name` in the start tag text `tag`, in single or double quotes.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = at == 0 || tag.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = tag[from..].trim_start();
        if !before_ok {
            continue;
        }
        if let Some(rest) = rest.strip_prefix('=') {
            let rest = rest.trim_start();
            let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
            let body = &rest[1..];
            return body.find(quote).map(|end| &body[..end]);
        }
    }
    None
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The start tag beginning at `<name` in `xml`: its attribute text, whether it is self-closing, and the
/// index just past the `>`.
fn start_tag<'a>(xml: &'a str, name: &str) -> Option<(&'a str, bool, usize)> {
    let pat = format!("<{name}");
    let mut from = 0;
    loop {
        let at = from + xml[from..].find(&pat)?;
        let after = at + pat.len();
        let next = xml[after..].chars().next()?;
        if next == ' ' || next == '>' || next == '/' {
            let end = after + xml[after..].find('>')?;
            let self_closing = xml[..end].ends_with('/');
            let attrs = xml[after..end].trim_end_matches('/');
            return Some((attrs, self_closing, end + 1));
        }
        from = after;
    }
}

/// Read one event rendered as XML by the event log API (or `wevtutil ... /f:xml`).
pub fn parse_event(xml: &str) -> Option<Ev> {
    let (provider, _, _) = start_tag(xml, "Provider")?;
    let (id_tag_attrs, _, id_from) = start_tag(xml, "EventID")?;
    let _ = id_tag_attrs;
    let id: u32 = xml[id_from..].split('<').next()?.trim().parse().ok()?;
    let (time, _, _) = start_tag(xml, "TimeCreated")?;
    let ts = parse_iso_utc(attr(time, "SystemTime")?)?;
    let mut ev = Ev { ts, provider: unescape(attr(provider, "Name").unwrap_or_default()), id, ..Default::default() };
    let Some(body_at) = xml.find("<EventData") else { return Some(ev) };
    let mut rest = &xml[body_at..];
    if let Some(end) = rest.find("</EventData>") {
        rest = &rest[..end];
    }
    let mut pos = 0;
    while let Some((attrs, self_closing, after)) = start_tag(&rest[pos..], "Data") {
        let name = unescape(attr(attrs, "Name").unwrap_or_default());
        let base = pos + after;
        if self_closing {
            ev.data.push((name, String::new()));
            pos = base;
            continue;
        }
        let Some(close) = rest[base..].find("</Data>") else { break };
        ev.data.push((name, unescape(rest[base..base + close].trim())));
        pos = base + close + "</Data>".len();
    }
    if let Some((_, false, after)) = start_tag(rest, "Binary") {
        if let Some(close) = rest[after..].find("</Binary>") {
            ev.binary = rest[after..after + close].trim().to_string();
        }
    }
    Some(ev)
}

/// Events from the rendered XML of each, oldest first.
pub fn parse_events(xmls: &[String]) -> Vec<Ev> {
    let mut v: Vec<Ev> = xmls.iter().filter_map(|x| parse_event(x)).collect();
    v.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    v
}

// ---------------------------------------------------------------------------------------------
// Queries

const KERNEL_POWER: &str = "Microsoft-Windows-Kernel-Power";
const KERNEL_GENERAL: &str = "Microsoft-Windows-Kernel-General";
const WER_SYSTEM: &str = "Microsoft-Windows-WER-SystemErrorReporting";
const WER_APP: &str = "Windows Error Reporting";
const EVENTLOG: &str = "EventLog";

/// XPath for the System log events the analysis reads, from the last `within_secs` seconds.
pub fn system_query(within_secs: u64) -> String {
    let ms = within_secs.saturating_mul(1000);
    format!(
        "*[System[(\
Provider[@Name='{KERNEL_POWER}'] and (EventID=41 or EventID=42 or EventID=506 or EventID=507) \
or Provider[@Name='{KERNEL_GENERAL}'] and EventID=12 \
or Provider[@Name='{WER_SYSTEM}'] and EventID=1001 \
or Provider[@Name='{EVENTLOG}'] and EventID=6008 \
or Provider[@Name='nvlddmkm'] and EventID=153 \
or Provider[@Name='amdkmdag'] and EventID=4101 \
or Provider[@Name='Display'] and EventID=4101\
) and TimeCreated[timediff(@SystemTime) <= {ms}]]]"
    )
}

/// XPath for the Application log's blue screen and live kernel (GPU hang) reports.
pub fn app_query(within_secs: u64) -> String {
    let ms = within_secs.saturating_mul(1000);
    format!(
        "*[System[Provider[@Name='{WER_APP}'] and EventID=1001 and TimeCreated[timediff(@SystemTime) <= {ms}]] \
and EventData[Data[@Name='EventName']='BlueScreen' or Data[@Name='EventName']='LiveKernelEvent']]"
    )
}

/// The events of both logs, oldest first.
#[derive(Default)]
pub struct Log {
    pub system: Vec<Ev>,
    pub app: Vec<Ev>,
}

// ---------------------------------------------------------------------------------------------
// Bugchecks

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The graphics stack: video memory manager, scheduler, TDR.
    Gpu,
    /// Memory manager and pool corruption.
    Memory,
    /// A kernel-mode driver crashed (any driver, including the GPU's).
    Driver,
    Power,
    /// CPU, bus or memory hardware errors.
    Hardware,
    Other,
}

const BUGCHECKS: &[(u32, &str, Kind)] = &[
    (0x0A, "IRQL_NOT_LESS_OR_EQUAL", Kind::Driver),
    (0x19, "BAD_POOL_HEADER", Kind::Memory),
    (0x1A, "MEMORY_MANAGEMENT", Kind::Memory),
    (0x1E, "KMODE_EXCEPTION_NOT_HANDLED", Kind::Driver),
    (0x3B, "SYSTEM_SERVICE_EXCEPTION", Kind::Driver),
    (0x4E, "PFN_LIST_CORRUPT", Kind::Memory),
    (0x50, "PAGE_FAULT_IN_NONPAGED_AREA", Kind::Memory),
    (0x77, "KERNEL_STACK_INPAGE_ERROR", Kind::Memory),
    (0x7A, "KERNEL_DATA_INPAGE_ERROR", Kind::Memory),
    (0x7E, "SYSTEM_THREAD_EXCEPTION_NOT_HANDLED", Kind::Driver),
    (0x7F, "UNEXPECTED_KERNEL_MODE_TRAP", Kind::Hardware),
    (0x9C, "MACHINE_CHECK_EXCEPTION", Kind::Hardware),
    (0x9F, "DRIVER_POWER_STATE_FAILURE", Kind::Power),
    (0xA0, "INTERNAL_POWER_ERROR", Kind::Power),
    (0xC2, "BAD_POOL_CALLER", Kind::Memory),
    (0xC4, "DRIVER_VERIFIER_DETECTED_VIOLATION", Kind::Driver),
    (0xC5, "DRIVER_CORRUPTED_EXPOOL", Kind::Memory),
    (0xD1, "DRIVER_IRQL_NOT_LESS_OR_EQUAL", Kind::Driver),
    (0xEF, "CRITICAL_PROCESS_DIED", Kind::Other),
    (0xF4, "CRITICAL_OBJECT_TERMINATION", Kind::Other),
    (0x101, "CLOCK_WATCHDOG_TIMEOUT", Kind::Hardware),
    (0x109, "CRITICAL_STRUCTURE_CORRUPTION", Kind::Memory),
    (0x10E, "VIDEO_MEMORY_MANAGEMENT_INTERNAL", Kind::Gpu),
    (0x113, "VIDEO_DXGKRNL_FATAL_ERROR", Kind::Gpu),
    (0x116, "VIDEO_TDR_FAILURE", Kind::Gpu),
    (0x117, "VIDEO_TDR_TIMEOUT_DETECTED", Kind::Gpu),
    (0x119, "VIDEO_SCHEDULER_INTERNAL_ERROR", Kind::Gpu),
    (0x124, "WHEA_UNCORRECTABLE_ERROR", Kind::Hardware),
    (0x133, "DPC_WATCHDOG_VIOLATION", Kind::Driver),
    (0x139, "KERNEL_SECURITY_CHECK_FAILURE", Kind::Memory),
    (0x13A, "KERNEL_MODE_HEAP_CORRUPTION", Kind::Memory),
    (0x141, "VIDEO_ENGINE_TIMEOUT_DETECTED", Kind::Gpu),
    (0x193, "VIDEO_DXGKRNL_LIVEDUMP", Kind::Gpu),
];

pub fn bugcheck_name(code: u32) -> Option<&'static str> {
    BUGCHECKS.iter().find(|(c, _, _)| *c == code).map(|(_, n, _)| *n)
}

pub fn bugcheck_kind(code: u32) -> Kind {
    BUGCHECKS.iter().find(|(c, _, _)| *c == code).map_or(Kind::Other, |(_, _, k)| *k)
}

/// NTSTATUS / exception codes that show up as the first parameter of 0x3B, 0x1E and 0x7E.
pub fn exception_name(code: u64) -> Option<&'static str> {
    Some(match code {
        0xC000_0005 => "access violation",
        0xC000_0006 => "in-page I/O error",
        0xC000_001D => "illegal instruction",
        0xC000_0094 => "integer divide by zero",
        0xC000_00FD => "stack overflow",
        0xC000_0374 => "heap corruption",
        0xC000_0409 => "stack buffer overrun",
        0x8000_0003 => "breakpoint",
        _ => return None,
    })
}

/// Whether a module name is a display driver or the Windows graphics kernel.
pub fn is_gpu_module(module: &str) -> bool {
    let m = module.to_ascii_lowercase();
    let m = m.trim_end_matches(".sys");
    matches!(m, "nvlddmkm" | "amdkmdag" | "atikmdag" | "igdkmd64" | "igdkmd32" | "dxgkrnl" | "dxgmms1" | "dxgmms2")
}

fn vendor_of(module: &str) -> &'static str {
    match module.to_ascii_lowercase().trim_end_matches(".sys") {
        "nvlddmkm" => "NVIDIA display driver",
        "amdkmdag" | "atikmdag" => "AMD display driver",
        "igdkmd64" | "igdkmd32" => "Intel display driver",
        "dxgkrnl" | "dxgmms1" | "dxgmms2" => "Windows graphics kernel",
        _ => "driver",
    }
}

/// The module and function a fault bucket names: `AV_nvlddmkm!UvmChannelUpdateProgress` gives
/// `("nvlddmkm", Some("UvmChannelUpdateProgress"))`; `LKD_0x141_Tdr:6_IMAGE_nvlddmkm.sys_Blackwell` gives
/// `("nvlddmkm", None)`. Numeric buckets name nothing.
pub fn blame(bucket: &str) -> Option<(String, Option<String>)> {
    if let Some((left, right)) = bucket.split_once('!') {
        let module = left.rsplit('_').next().filter(|m| !m.is_empty())?;
        return Some((module.to_ascii_lowercase(), Some(right.to_string()).filter(|s| !s.is_empty())));
    }
    let lower = bucket.to_ascii_lowercase();
    let end = lower.find(".sys")?;
    let start = lower[..end].rfind('_').map_or(0, |p| p + 1);
    Some((lower[start..end].to_string(), None))
}

// ---------------------------------------------------------------------------------------------
// Watch log

/// One line of a `vramorama watch --log` file, reduced to what a crash needs.
#[derive(Clone, Debug)]
pub struct WatchSample {
    pub ts: f64,
    pub total_mib: u64,
    pub used_mib: u64,
    pub shared_mib: Option<u64>,
    /// Processes that had spilled into system memory: (pid, shared MiB).
    pub spilled: Vec<(u32, u64)>,
    /// GPU memory by holder: its session or job label, or the executable for untagged processes.
    pub holders: Vec<(String, u64)>,
    pub ram_avail_mib: Option<u64>,
    pub commit_pct: Option<f64>,
}

/// The `"ts"` of a log line without parsing the line.
fn quick_ts(line: &str) -> Option<f64> {
    let at = line.find("\"ts\"")? + 4;
    let rest = line[at..].trim_start().strip_prefix(':')?.trim_start();
    let end =
        rest.find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+'))).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn sample_of(v: &Value) -> Option<WatchSample> {
    let ts = v.f64("ts")?;
    let mut s = WatchSample {
        ts,
        total_mib: 0,
        used_mib: 0,
        shared_mib: None,
        spilled: Vec::new(),
        holders: Vec::new(),
        ram_avail_mib: None,
        commit_pct: None,
    };
    let mut by: Vec<(String, u64)> = Vec::new();
    for a in v.arr("adapters") {
        s.total_mib += a.f64("total_mib").unwrap_or(0.0) as u64;
        s.used_mib += a.f64("used_mib").unwrap_or(0.0) as u64;
        if let Some(sh) = a.f64("shared_mib") {
            *s.shared_mib.get_or_insert(0) += sh as u64;
        }
        for p in a.arr("spilled") {
            s.spilled.push((p.f64("pid").unwrap_or(0.0) as u32, p.f64("shared_mib").unwrap_or(0.0) as u64));
        }
        for p in a.arr("procs") {
            let key = p
                .str("session_key")
                .or_else(|| p.str("owner"))
                .map_or_else(|| format!("untagged {}", p.str("name").unwrap_or("?")), str::to_string);
            let mib = p.f64("mib").unwrap_or(0.0) as u64;
            match by.iter_mut().find(|(k, _)| *k == key) {
                Some((_, m)) => *m += mib,
                None => by.push((key, mib)),
            }
        }
    }
    by.sort_by_key(|a| std::cmp::Reverse(a.1));
    s.holders = by;
    if let Some(sys) = v.get("system") {
        s.ram_avail_mib = sys.f64("available_mib").map(|m| m as u64);
        if let (Some(used), Some(limit)) = (sys.f64("commit_mib"), sys.f64("commit_limit_mib")) {
            s.commit_pct = (limit > 0.0).then(|| used / limit * 100.0);
        }
    }
    Some(s)
}

/// Samples of watch-log lines whose time falls in any of the `windows`, oldest first, and the time span the
/// whole log covers. Lines that are not JSON, or are outside every window, are skipped without a full parse.
pub fn load_watch_lines<S: AsRef<str>>(
    lines: impl Iterator<Item = S>,
    windows: &[(f64, f64)],
) -> (Vec<WatchSample>, Option<(f64, f64)>) {
    let mut out = Vec::new();
    let mut span: Option<(f64, f64)> = None;
    for line in lines {
        let line = line.as_ref();
        let Some(ts) = quick_ts(line) else { continue };
        span = Some(span.map_or((ts, ts), |(a, b)| (a.min(ts), b.max(ts))));
        if !windows.iter().any(|(a, b)| ts >= *a && ts <= *b) {
            continue;
        }
        if let Some(s) = crate::json::parse(line).ok().as_ref().and_then(sample_of) {
            out.push(s);
        }
    }
    out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    (out, span)
}

/// What the watch log shows for the minutes before a crash.
#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    pub samples: usize,
    /// Seconds between the last sample and the crash.
    pub last_before_secs: f64,
    pub used_mib: u64,
    pub total_mib: u64,
    pub peak_used_mib: u64,
    pub shared_mib: Option<u64>,
    pub spilled: Vec<(u32, u64)>,
    pub holders: Vec<(String, u64)>,
    pub peak_commit_pct: Option<f64>,
    pub min_ram_avail_mib: Option<u64>,
}

/// Holders of at least this much count as a job, as opposed to the desktop.
pub const HOLDER_MIN_MIB: u64 = 256;
/// Commit charge at or above this share of the commit limit is memory pressure.
pub const COMMIT_PRESSURE_PCT: f64 = 92.0;
/// Free RAM below this is memory pressure.
pub const RAM_PRESSURE_MIB: u64 = 512;
/// The card counts as full with this little dedicated memory left.
pub const VRAM_FULL_MARGIN_MIB: u64 = 512;

impl Context {
    pub fn jobs(&self) -> usize {
        self.holders.iter().filter(|(_, m)| *m >= HOLDER_MIN_MIB).count()
    }

    /// The memory-pressure evidence in this window, if any: spill into shared GPU memory, commit charge near
    /// its limit, almost no free RAM, or a full card shared by several jobs.
    pub fn pressure(&self) -> Option<String> {
        let mut why = Vec::new();
        if !self.spilled.is_empty() {
            let mib: u64 = self.spilled.iter().map(|(_, m)| m).sum();
            why.push(format!("{} MiB spilled from the card into system memory", thousands(mib)));
        }
        if let Some(p) = self.peak_commit_pct.filter(|p| *p >= COMMIT_PRESSURE_PCT) {
            why.push(format!("commit charge reached {p:.0}% of the commit limit"));
        }
        if let Some(m) = self.min_ram_avail_mib.filter(|m| *m < RAM_PRESSURE_MIB) {
            why.push(format!("free RAM fell to {} MiB", thousands(m)));
        }
        if self.total_mib > 0 && self.peak_used_mib + VRAM_FULL_MARGIN_MIB >= self.total_mib && self.jobs() >= 2 {
            why.push(format!(
                "the card was full ({} of {} MiB) with {} jobs on it",
                thousands(self.peak_used_mib),
                thousands(self.total_mib),
                self.jobs()
            ));
        }
        (!why.is_empty()).then(|| why.join("; "))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LogState {
    /// No watch log was given or found.
    None,
    /// The log exists but starts after, or ends before, the crash.
    Outside {
        first: f64,
        last: f64,
    },
    /// The log spans the crash but has no sample in the window before it (the watcher was not running).
    Gap,
    Found(Context),
}

/// The state of the machine in the `lookback` seconds before `t`, from watch-log samples.
pub fn context_at(samples: &[WatchSample], span: Option<(f64, f64)>, t: f64, lookback: f64) -> LogState {
    let Some((first, last)) = span else { return LogState::None };
    // A sample can be written a moment after the crash timestamp; allow a few seconds.
    let inside: Vec<&WatchSample> = samples.iter().filter(|s| s.ts >= t - lookback && s.ts <= t + 5.0).collect();
    if inside.is_empty() {
        return if t < first || t > last { LogState::Outside { first, last } } else { LogState::Gap };
    }
    let last_s = inside[inside.len() - 1];
    let mut spilled: Vec<(u32, u64)> = Vec::new();
    for s in &inside {
        for &(pid, mib) in &s.spilled {
            match spilled.iter_mut().find(|(p, _)| *p == pid) {
                Some((_, m)) => *m = (*m).max(mib),
                None => spilled.push((pid, mib)),
            }
        }
    }
    LogState::Found(Context {
        samples: inside.len(),
        last_before_secs: (t - last_s.ts).max(0.0),
        used_mib: last_s.used_mib,
        total_mib: last_s.total_mib,
        peak_used_mib: inside.iter().map(|s| s.used_mib).max().unwrap_or(0),
        shared_mib: last_s.shared_mib,
        spilled,
        holders: last_s.holders.clone(),
        peak_commit_pct: inside.iter().filter_map(|s| s.commit_pct).max_by(f64::total_cmp),
        min_ram_avail_mib: inside.iter().filter_map(|s| s.ram_avail_mib).min(),
    })
}

// ---------------------------------------------------------------------------------------------
// Incidents

/// How far back from a crash GPU resets and standby entry are looked for.
pub const LOOKBACK_SECS: f64 = 600.0;
/// Resets this close together are one reset (a TDR logs three events within a second or two).
const BURST_GAP_SECS: f64 = 30.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Standby {
    /// When the machine entered Modern Standby (or sleep), if the log has it.
    pub since: Option<f64>,
    /// Windows recorded Modern Standby (or sleep) as in progress at the crash. When false the log only shows
    /// an entry with no exit: the crash came while the machine was resuming, before the exit was logged.
    pub in_progress: bool,
}

#[derive(Clone, Debug)]
pub struct Incident {
    /// When the reboot was logged (Kernel-Power 41, written at the next boot).
    pub logged: f64,
    /// When the machine stopped: exact when the event log recorded it, else `logged`.
    pub crashed: f64,
    pub crashed_exact: bool,
    /// The bugcheck code, or None when no bugcheck was written (a hang or power loss).
    pub bugcheck: Option<u32>,
    pub params: [u64; 4],
    pub standby: Option<Standby>,
    pub power_button: bool,
    pub dump: Option<String>,
    pub bucket: Option<String>,
    /// The module a fault bucket names, and the function.
    pub blamed: Option<(String, Option<String>)>,
    /// GPU resets in the `LOOKBACK_SECS` before the crash (events within 30 s count once).
    pub resets_before: usize,
    pub log: LogState,
    pub verdict: Verdict,
}

impl Incident {
    pub fn gpu_blamed(&self) -> bool {
        self.blamed.as_ref().is_some_and(|(m, _)| is_gpu_module(m))
    }
}

/// A GPU hang or reset, from any source.
#[derive(Clone, Debug)]
pub struct GpuEvent {
    pub ts: f64,
    pub what: &'static str,
}

#[derive(Debug, Default)]
pub struct Report {
    pub window_days: f64,
    /// Unix time the window starts at.
    pub since: f64,
    pub incidents: Vec<Incident>,
    pub gpu_events: Vec<GpuEvent>,
    /// GPU resets, counting events within 30 s of each other once.
    pub gpu_resets: usize,
    /// Times Windows entered Modern Standby or sleep in the window.
    pub standby_entries: usize,
}

/// "46 x GPU reset event, 15 x GPU engine timeout (0x141)": the logged GPU events in the window by source, a
/// reset and the duplicate reports of one hang (a minute and a half apart at most) counted once.
pub fn gpu_breakdown(r: &Report) -> String {
    let mut kinds: Vec<&str> = Vec::new();
    for e in &r.gpu_events {
        if !kinds.contains(&e.what) {
            kinds.push(e.what);
        }
    }
    let parts: Vec<String> = kinds
        .iter()
        .filter_map(|k| {
            let times: Vec<f64> =
                r.gpu_events.iter().filter(|e| e.what == *k && e.ts >= r.since).map(|e| e.ts).collect();
            let n = bursts(&times, 90.0).len();
            (n > 0).then(|| format!("{n} x {k}"))
        })
        .collect();
    if parts.is_empty() { "none".to_string() } else { parts.join(", ") }
}

/// First time of each group of times that lie within `gap` seconds of the previous one.
pub fn bursts(times: &[f64], gap: f64) -> Vec<f64> {
    let mut sorted = times.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mut out: Vec<f64> = Vec::new();
    let mut last = f64::NEG_INFINITY;
    for t in sorted {
        if t - last > gap {
            out.push(t);
        }
        last = t;
    }
    out
}

/// The UTC time in an EventLog 6008 payload: two SYSTEMTIME structures, local then UTC.
pub fn unexpected_shutdown_time(binary: &str) -> Option<f64> {
    let bytes: Vec<u8> =
        (0..binary.len() / 2).map_while(|i| u8::from_str_radix(binary.get(2 * i..2 * i + 2)?, 16).ok()).collect();
    let st = bytes.get(16..32)?;
    let w = |i: usize| u16::from_le_bytes([st[2 * i], st[2 * i + 1]]);
    let (y, mo, d, h, mi, s) = (w(0), w(1), w(3), w(4), w(5), w(6));
    if !(2000..=2100).contains(&y) || mo == 0 || mo > 12 || d == 0 || d > 31 || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    parse_iso_utc(&format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"))
}

fn hex(s: &str) -> Option<u64> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(t, 16).ok()
}

/// `0x0000003b (0x00000000c0000005, 0x..., 0x..., 0x...)` from System 1001's first parameter.
fn parse_bugcheck_text(s: &str) -> Option<(u32, [u64; 4])> {
    let (code, rest) = s.split_once('(')?;
    let mut params = [0u64; 4];
    for (i, p) in
        rest.trim_end_matches(|c: char| c == ')' || c.is_whitespace() || c == '.').split(',').take(4).enumerate()
    {
        params[i] = hex(p)?;
    }
    Some((u32::try_from(hex(code)?).ok()?, params))
}

fn dump_in(files: &str) -> Option<String> {
    files
        .lines()
        .map(|l| l.trim().trim_start_matches("\\\\?\\"))
        .find(|l| l.to_ascii_lowercase().ends_with(".dmp"))
        .map(str::to_string)
}

fn gpu_reset_times(system: &[Ev]) -> Vec<f64> {
    system
        .iter()
        .filter(|e| e.is("nvlddmkm", 153) || e.is("amdkmdag", 4101) || e.is("Display", 4101))
        .map(|e| e.ts)
        .collect()
}

fn live_kernel_gpu(e: &Ev) -> Option<&'static str> {
    if !e.is(WER_APP, 1001) || e.get("EventName") != Some("LiveKernelEvent") {
        return None;
    }
    match hex(e.get("P1")?)? {
        0x117 => Some("GPU timeout (0x117)"),
        0x141 => Some("GPU engine timeout (0x141)"),
        0x193 => Some("video driver live dump (0x193)"),
        _ => None,
    }
}

/// Build the report from the logs, for the incidents and resets at or after `since` (Unix time). The log may
/// reach a little further back than that, so the events around the oldest incident are there too.
pub fn analyse(log: &Log, window_days: f64, since: f64) -> Report {
    let mut r = Report { window_days, since, ..Default::default() };
    let kp = |id: u32| log.system.iter().filter(move |e| e.is(KERNEL_POWER, id));

    let reset_times = gpu_reset_times(&log.system);
    for t in &reset_times {
        r.gpu_events.push(GpuEvent { ts: *t, what: "GPU reset event" });
    }
    for e in &log.app {
        if let Some(what) = live_kernel_gpu(e) {
            r.gpu_events.push(GpuEvent { ts: e.ts, what });
        }
    }
    r.gpu_events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let recent: Vec<f64> = r.gpu_events.iter().map(|e| e.ts).filter(|t| *t >= since).collect();
    r.gpu_resets = bursts(&recent, BURST_GAP_SECS).len();

    // Standby transitions: (time, entered).
    let mut transitions: Vec<(f64, bool)> = log
        .system
        .iter()
        .filter(|e| e.provider == KERNEL_POWER)
        .filter_map(|e| match e.id {
            506 | 42 => Some((e.ts, true)),
            507 => Some((e.ts, false)),
            _ => None,
        })
        .collect();
    transitions.sort_by(|a, b| a.0.total_cmp(&b.0));
    r.standby_entries = transitions.iter().filter(|t| t.1 && t.0 >= since).count();
    let boots: Vec<f64> = log.system.iter().filter(|e| e.is(KERNEL_GENERAL, 12)).map(|e| e.ts).collect();

    let mut incidents: Vec<Incident> = Vec::new();
    for e in kp(41) {
        let code = e.get("BugcheckCode").and_then(|c| c.trim().parse::<u32>().ok()).unwrap_or(0);
        let mut params = [0u64; 4];
        for (i, p) in params.iter_mut().enumerate() {
            *p = e.get(&format!("BugcheckParameter{}", i + 1)).and_then(hex).unwrap_or(0);
        }
        let mut bugcheck = (code != 0).then_some(code);
        let cs_flag = e.get("ConnectedStandbyInProgress").is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let sleeping = e.get("SleepInProgress").is_some_and(|v| v.trim() != "0" && !v.trim().is_empty());
        let power_button =
            e.get("PowerButtonTimestamp").and_then(|v| v.trim().parse::<u64>().ok()).is_some_and(|v| v != 0)
                || e.get("LongPowerButtonPressDetected").is_some_and(|v| v.eq_ignore_ascii_case("true"));

        // When it stopped: EventLog 6008 carries the last time the machine was alive.
        let crash_utc = log
            .system
            .iter()
            .filter(|s| s.is(EVENTLOG, 6008) && s.ts >= e.ts - 60.0 && s.ts <= e.ts + 300.0)
            .find_map(|s| unexpected_shutdown_time(&s.binary))
            .filter(|t| *t <= e.ts + 60.0 && *t >= e.ts - 14.0 * 86_400.0);
        let crashed = crash_utc.unwrap_or(e.ts);

        // Dump path and parameters from System 1001; fault bucket from the Application log.
        let sys_1001 =
            log.system.iter().find(|s| s.is(WER_SYSTEM, 1001) && s.ts >= e.ts - 60.0 && s.ts <= e.ts + 600.0);
        let mut dump =
            sys_1001.and_then(|s| s.nth(1)).filter(|p| p.to_ascii_lowercase().ends_with(".dmp")).map(str::to_string);
        if let Some((c, p)) = sys_1001.and_then(|s| s.nth(0)).and_then(parse_bugcheck_text) {
            if bugcheck.is_none() {
                bugcheck = Some(c);
                params = p;
            }
        }
        let mut blue: Vec<&Ev> = log
            .app
            .iter()
            .filter(|a| {
                a.is(WER_APP, 1001)
                    && a.get("EventName") == Some("BlueScreen")
                    && a.ts >= e.ts - 60.0
                    && a.ts <= e.ts + 3600.0
                    && bugcheck.is_none_or(|c| a.get("P1").and_then(hex) == Some(u64::from(c)))
            })
            .collect();
        blue.sort_by_key(|a| a.get("Bucket").is_none_or(str::is_empty)); // reports with a bucket first
        let bucket = blue.iter().find_map(|a| a.get("Bucket").filter(|b| !b.is_empty())).map(str::to_string);
        if dump.is_none() {
            dump = blue.iter().find_map(|a| a.get("AttachedFiles").and_then(dump_in));
        }
        let blamed = bucket.as_deref().and_then(blame);

        // Modern Standby: the reboot event says so, and the transitions say since when.
        let boot_before = boots.iter().copied().filter(|b| *b <= crashed).fold(f64::NEG_INFINITY, f64::max);
        let last = transitions.iter().rev().find(|t| t.0 <= crashed + 1.0 && t.0 >= boot_before - 1.0);
        let entered = last.filter(|t| t.1).map(|t| t.0);
        let standby = (cs_flag || sleeping || entered.is_some())
            .then_some(Standby { since: entered, in_progress: cs_flag || sleeping });

        let before: Vec<f64> = r
            .gpu_events
            .iter()
            .filter(|g| g.ts >= crashed - LOOKBACK_SECS && g.ts <= crashed + 60.0)
            .map(|g| g.ts)
            .collect();
        let resets_before = bursts(&before, BURST_GAP_SECS).len();

        incidents.push(Incident {
            logged: e.ts,
            crashed,
            crashed_exact: crash_utc.is_some(),
            bugcheck,
            params,
            standby,
            power_button,
            dump,
            bucket,
            blamed,
            resets_before,
            log: LogState::None,
            verdict: Verdict::unknown(),
        });
    }
    incidents.retain(|i| i.crashed >= since);
    incidents.sort_by(|a, b| b.crashed.total_cmp(&a.crashed));
    for i in &mut incidents {
        i.verdict = judge(i);
    }
    r.incidents = incidents;
    r
}

/// The time windows (the minutes before each crash) a watch log is read for.
pub fn watch_windows(r: &Report) -> Vec<(f64, f64)> {
    r.incidents.iter().map(|i| (i.crashed - LOOKBACK_SECS, i.crashed + 5.0)).collect()
}

/// Line the incidents up with watch-log samples (see `load_watch_lines`) and judge them again with that evidence.
pub fn attach_watch(r: &mut Report, samples: &[WatchSample], span: Option<(f64, f64)>) {
    for i in &mut r.incidents {
        i.log = context_at(samples, span, i.crashed, LOOKBACK_SECS);
        i.verdict = judge(i);
    }
}

// ---------------------------------------------------------------------------------------------
// Verdicts

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Memory pressure measured by the watch log (spill, commit near its limit, a full shared card).
    MemoryPressure,
    /// The machine was in Modern Standby (or asleep) when it failed.
    StandbyFault,
    /// Repeated GPU resets in the minutes before.
    TdrCascade,
    /// A GPU or display-driver fault with one job on the card and no pressure: not contention.
    SingleWorkloadFault,
    /// A GPU or display-driver fault, with no log to tell whether the card was shared.
    GpuFault,
    /// No bugcheck written: the machine froze or lost power.
    HardHang,
    /// The power button was held down after a hang.
    ForcedPowerOff,
    /// A bugcheck that is not about the GPU.
    OtherBugcheck,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub cause: Cause,
    pub summary: String,
}

impl Verdict {
    fn unknown() -> Verdict {
        Verdict { cause: Cause::Unknown, summary: String::new() }
    }
}

impl Cause {
    pub fn tag(self) -> &'static str {
        match self {
            Cause::MemoryPressure => "memory-pressure",
            Cause::StandbyFault => "standby",
            Cause::TdrCascade => "tdr-cascade",
            Cause::SingleWorkloadFault => "single-workload",
            Cause::GpuFault => "gpu-fault",
            Cause::HardHang => "hard-hang",
            Cause::ForcedPowerOff => "forced-power-off",
            Cause::OtherBugcheck => "other-bugcheck",
            Cause::Unknown => "unknown",
        }
    }
}

fn who(i: &Incident) -> String {
    match &i.blamed {
        Some((m, Some(f))) => format!("{m}!{f}"),
        Some((m, None)) => m.clone(),
        None => "the graphics stack".to_string(),
    }
}

fn standby_phrase(i: &Incident, fmt_dur: &dyn Fn(f64) -> String) -> String {
    let Some(s) = &i.standby else { return String::new() };
    match (s.since, s.in_progress) {
        (Some(since), true) => {
            format!("{} after it entered Modern Standby (screen off)", fmt_dur((i.crashed - since).max(0.0)))
        }
        (Some(since), false) => format!(
            "{} after it entered Modern Standby, as it was resuming (Windows had left standby but not yet logged the exit)",
            fmt_dur((i.crashed - since).max(0.0))
        ),
        (None, _) => "while it was in Modern Standby".to_string(),
    }
}

/// What the evidence supports for one incident. The order matters: measured memory pressure first, since
/// that is the contention hypothesis with proof; then standby, the commonest trigger on laptops; then a run
/// of GPU resets; then a lone GPU fault; then the rest.
pub fn judge(i: &Incident) -> Verdict {
    let kind = i.bugcheck.map(bugcheck_kind);
    let gpu_related = kind == Some(Kind::Gpu) || i.gpu_blamed() || i.resets_before > 0;
    let pressure = match &i.log {
        LogState::Found(c) => c.pressure(),
        _ => None,
    };
    let plain = |s: f64| crate::parse::duration(s);
    let mk = |cause, summary: String| Verdict { cause, summary };

    let explainable = gpu_related || matches!(kind, None | Some(Kind::Memory | Kind::Driver | Kind::Power));
    if let Some(p) = pressure.filter(|_| explainable) {
        return mk(
            Cause::MemoryPressure,
            format!(
                "memory pressure before the crash ({p}). Contention is a credible cause: keep the card and RAM below \
these limits, and start GPU jobs with `vramorama run` so they queue."
            ),
        );
    }
    if i.standby.is_some() && (gpu_related || matches!(kind, None | Some(Kind::Driver | Kind::Power))) {
        let kindword = if gpu_related { "GPU driver fault" } else { "failure" };
        return mk(
            Cause::StandbyFault,
            format!(
                "{kindword} {}. Display-off power management is the trigger, not VRAM contention. Keep the machine out of \
Modern Standby while a GPU job runs (`vramorama run` does; see `vramorama doctor` for the timeouts).",
                standby_phrase(i, &plain)
            ),
        );
    }
    if i.resets_before >= 2 {
        return mk(
            Cause::TdrCascade,
            format!(
                "{} GPU resets in the {} before the crash: the card was already failing and Windows gave up recovering it. \
Look at what ran long kernels, a power cap (`vramorama power`) and driver age (`vramorama doctor`).",
                i.resets_before,
                plain(LOOKBACK_SECS)
            ),
        );
    }
    if gpu_related {
        return match &i.log {
            LogState::Found(c) if c.jobs() <= 1 => mk(
                Cause::SingleWorkloadFault,
                format!(
                    "GPU driver fault in {} with {} on the card ({} of {} MiB) and no spill or commit pressure: not contention.",
                    who(i),
                    if c.jobs() == 0 { "no job".to_string() } else { "one job".to_string() },
                    thousands(c.used_mib),
                    thousands(c.total_mib)
                ),
            ),
            LogState::Found(c) => mk(
                Cause::GpuFault,
                format!(
                    "GPU driver fault in {} with {} jobs on the card ({} of {} MiB) but no spill or commit pressure measured.",
                    who(i),
                    c.jobs(),
                    thousands(c.used_mib),
                    thousands(c.total_mib)
                ),
            ),
            _ => mk(
                Cause::GpuFault,
                format!(
                    "GPU driver fault in {}. No watch log covers the minutes before it, so contention can be neither shown nor \
excluded: run `vramorama watch --quiet --log FILE` to record the next one.",
                    who(i)
                ),
            ),
        };
    }
    match i.bugcheck {
        None if i.power_button => mk(
            Cause::ForcedPowerOff,
            "the machine hung and the power button was held to switch it off; Windows wrote no bugcheck or dump. A freeze with no \
bugcheck points at the GPU, firmware or power delivery rather than a driver that Windows could catch."
                .to_string(),
        ),
        None => mk(
            Cause::HardHang,
            "no bugcheck was written: the machine froze or lost power. A freeze with no dump points at the GPU, firmware or \
power delivery (battery, charger, thermal shutdown) rather than a driver that Windows could catch."
                .to_string(),
        ),
        Some(c) => mk(
            Cause::OtherBugcheck,
            format!(
                "bugcheck 0x{c:X} {} ({}): not a graphics-stack failure; the dump names the driver or hardware at fault.",
                bugcheck_name(c).unwrap_or("(unnamed)"),
                match bugcheck_kind(c) {
                    Kind::Memory => "memory corruption or exhaustion",
                    Kind::Hardware => "CPU, bus or RAM error",
                    Kind::Power => "a device failed a power transition",
                    Kind::Driver => "a kernel driver failed",
                    _ => "system component",
                }
            ),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Output

pub fn bugcheck_title(i: &Incident) -> String {
    match i.bugcheck {
        Some(c) => {
            let extra = if matches!(c, 0x3B | 0x1E | 0x7E) {
                exception_name(i.params[0]).map(|n| format!(", {n}"))
            } else {
                None
            };
            format!("blue screen 0x{c:X} {}{}", bugcheck_name(c).unwrap_or("(unnamed)"), extra.unwrap_or_default())
        }
        None if i.power_button => "hang, forced power-off (no bugcheck)".to_string(),
        None => "hang or power loss (no bugcheck)".to_string(),
    }
}

/// The report as text. `fmt_time` renders a Unix time in the user's local time, `fmt_clock` as HH:MM:SS.
pub fn render(r: &Report, limit: usize, fmt_time: &dyn Fn(f64) -> String) -> String {
    use std::fmt::Write;
    let dur = |s: f64| crate::parse::duration(s);
    let mut out = String::new();
    let n = r.incidents.len();
    let span = if r.window_days < 2.0 {
        format!("{:.0} hours", (r.window_days * 24.0).round())
    } else {
        format!("{:.0} days", r.window_days)
    };
    let _ = writeln!(out, "Crash forensics, last {span} (System and Application event logs)");
    if n == 0 {
        out.push_str("No unexpected reboots in that time.\n");
    } else {
        let bsod = r.incidents.iter().filter(|i| i.bugcheck.is_some()).count();
        let _ = writeln!(
            out,
            "{n} unexpected reboot(s): {bsod} blue screen(s), {} with no bugcheck (hang or power loss)",
            n - bsod
        );
        let mut causes: Vec<(Cause, usize)> = Vec::new();
        for i in &r.incidents {
            match causes.iter_mut().find(|(c, _)| *c == i.verdict.cause) {
                Some((_, k)) => *k += 1,
                None => causes.push((i.verdict.cause, 1)),
            }
        }
        causes.sort_by_key(|a| std::cmp::Reverse(a.1));
        let parts: Vec<String> = causes.iter().map(|(c, k)| format!("{k} {}", c.tag())).collect();
        let _ = writeln!(out, "Verdicts: {}", parts.join(", "));
        let in_standby = r.incidents.iter().filter(|i| i.standby.is_some()).count();
        let _ = writeln!(out, "{in_standby} of {n} happened while the machine was in Modern Standby or asleep");
    }
    let _ = writeln!(
        out,
        "GPU resets and hangs logged: {}; Modern Standby / sleep entered {} time(s)",
        gpu_breakdown(r),
        r.standby_entries
    );
    for i in r.incidents.iter().take(limit) {
        let _ = writeln!(
            out,
            "\n{}{}  {}",
            fmt_time(i.crashed),
            if i.crashed_exact { "" } else { " (reboot logged; crash time not recorded)" },
            bugcheck_title(i)
        );
        if let Some((m, f)) = &i.blamed {
            let _ = writeln!(
                out,
                "  blamed:   {m}{} ({})",
                f.as_ref().map(|f| format!("!{f}")).unwrap_or_default(),
                vendor_of(m)
            );
        }
        match &i.standby {
            Some(s) => {
                let _ = writeln!(
                    out,
                    "  power:    {}",
                    match (s.since, s.in_progress) {
                        (Some(since), true) => format!(
                            "in Modern Standby since {} ({} before the crash)",
                            fmt_time(since),
                            dur(i.crashed - since)
                        ),
                        (Some(since), false) => format!(
                            "entered Modern Standby at {} ({} before) and no exit was logged: the crash came as it resumed",
                            fmt_time(since),
                            dur(i.crashed - since)
                        ),
                        (None, _) => "in Modern Standby (Windows recorded it; entry time not in the log)".to_string(),
                    }
                );
            }
            None => {
                let _ = writeln!(out, "  power:    awake");
            }
        }
        let _ = writeln!(
            out,
            "  GPU:      {}",
            match i.resets_before {
                0 => format!("no GPU resets in the {} before", dur(LOOKBACK_SECS)),
                k => format!("{k} GPU reset(s) in the {} before", dur(LOOKBACK_SECS)),
            }
        );
        match &i.log {
            LogState::None => {
                let _ =
                    writeln!(out, "  watch:    no watch log (record one with `vramorama watch --quiet --log FILE`)");
            }
            LogState::Outside { first, last } => {
                let _ =
                    writeln!(out, "  watch:    log spans {} to {}, not this time", fmt_time(*first), fmt_time(*last));
            }
            LogState::Gap => {
                let _ = writeln!(
                    out,
                    "  watch:    log covers this time but has no sample in the {} before it",
                    dur(LOOKBACK_SECS)
                );
            }
            LogState::Found(c) => {
                let _ = writeln!(
                    out,
                    "  watch:    {} sample(s), last {} before: card {} of {} MiB (peak {}){}{}",
                    c.samples,
                    dur(c.last_before_secs),
                    thousands(c.used_mib),
                    thousands(c.total_mib),
                    thousands(c.peak_used_mib),
                    c.peak_commit_pct.map(|p| format!(", commit {p:.0}%")).unwrap_or_default(),
                    c.min_ram_avail_mib.map(|m| format!(", RAM free min {} MiB", thousands(m))).unwrap_or_default()
                );
                let held: Vec<String> = c
                    .holders
                    .iter()
                    .filter(|(_, m)| *m >= HOLDER_MIN_MIB)
                    .take(5)
                    .map(|(k, m)| format!("{k} {} MiB", thousands(*m)))
                    .collect();
                let _ = writeln!(
                    out,
                    "            held by: {}",
                    if held.is_empty() { "nothing over 256 MiB".to_string() } else { held.join("; ") }
                );
                if !c.spilled.is_empty() {
                    let _ = writeln!(
                        out,
                        "            spilled to system memory: {}",
                        c.spilled
                            .iter()
                            .map(|(p, m)| format!("pid {p} {} MiB", thousands(*m)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
        if let Some(d) = &i.dump {
            let _ = writeln!(out, "  dump:     {d}");
        }
        let _ = writeln!(out, "  verdict:  [{}] {}", i.verdict.cause.tag(), i.verdict.summary);
    }
    if n > limit {
        let _ = writeln!(out, "\n... and {} older incident(s); --last N shows more", n - limit);
    }
    out
}

pub fn to_json(r: &Report) -> Value {
    let incidents = r
        .incidents
        .iter()
        .map(|i| {
            let ctx = match &i.log {
                LogState::Found(c) => obj([
                    ("samples", (c.samples as u64).into()),
                    ("last_before_secs", c.last_before_secs.into()),
                    ("used_mib", c.used_mib.into()),
                    ("total_mib", c.total_mib.into()),
                    ("peak_used_mib", c.peak_used_mib.into()),
                    ("shared_mib", c.shared_mib.into()),
                    ("peak_commit_pct", c.peak_commit_pct.into()),
                    ("min_ram_avail_mib", c.min_ram_avail_mib.into()),
                    (
                        "spilled",
                        Value::Arr(
                            c.spilled
                                .iter()
                                .map(|(pid, mib)| obj([("pid", (*pid).into()), ("shared_mib", (*mib).into())]))
                                .collect(),
                        ),
                    ),
                    (
                        "holders",
                        Value::Arr(
                            c.holders
                                .iter()
                                .map(|(k, m)| obj([("holder", k.as_str().into()), ("mib", (*m).into())]))
                                .collect(),
                        ),
                    ),
                    ("pressure", c.pressure().into()),
                ]),
                _ => Value::Null,
            };
            let log_state = match &i.log {
                LogState::None => "none",
                LogState::Outside { .. } => "outside",
                LogState::Gap => "gap",
                LogState::Found(_) => "found",
            };
            obj([
                ("crashed", iso_utc(i.crashed).into()),
                ("crashed_unix", i.crashed.into()),
                ("crash_time_recorded", i.crashed_exact.into()),
                ("logged_unix", i.logged.into()),
                ("bugcheck", i.bugcheck.map(|c| format!("0x{c:X}")).into()),
                ("bugcheck_name", i.bugcheck.and_then(bugcheck_name).into()),
                ("title", bugcheck_title(i).into()),
                ("blamed_module", i.blamed.as_ref().map(|b| b.0.clone()).into()),
                ("blamed_function", i.blamed.as_ref().and_then(|b| b.1.clone()).into()),
                ("bucket", i.bucket.clone().into()),
                ("standby", i.standby.is_some().into()),
                ("standby_since_unix", i.standby.as_ref().and_then(|s| s.since).into()),
                ("standby_in_progress", i.standby.as_ref().map(|s| s.in_progress).into()),
                ("power_button", i.power_button.into()),
                ("gpu_resets_before", (i.resets_before as u64).into()),
                ("dump", i.dump.clone().into()),
                ("watch_log", log_state.into()),
                ("watch", ctx),
                ("verdict", i.verdict.cause.tag().into()),
                ("explanation", i.verdict.summary.as_str().into()),
            ])
        })
        .collect();
    obj([
        ("window_days", r.window_days.into()),
        ("gpu_resets", (r.gpu_resets as u64).into()),
        ("gpu_events", gpu_breakdown(r).into()),
        ("standby_entries", (r.standby_entries as u64).into()),
        ("unexpected_reboots", (r.incidents.len() as u64).into()),
        ("incidents", Value::Arr(incidents)),
    ])
}

/// "2026-10-19T06:03:22Z" for a Unix time.
pub fn iso_utc(secs: f64) -> String {
    let t = secs.floor() as i64;
    let (days, rem) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Counts for `doctor`: how many crashes, how many in standby, how many blamed on the GPU stack.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub reboots: usize,
    pub in_standby: usize,
    pub gpu_related: usize,
    pub gpu_resets: usize,
}

pub fn stats(r: &Report) -> Stats {
    Stats {
        reboots: r.incidents.len(),
        in_standby: r.incidents.iter().filter(|i| i.standby.is_some()).count(),
        gpu_related: r
            .incidents
            .iter()
            .filter(|i| i.bugcheck.map(bugcheck_kind) == Some(Kind::Gpu) || i.gpu_blamed() || i.resets_before > 0)
            .count(),
        gpu_resets: r.gpu_resets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KP_NS: &str = "xmlns='http://schemas.microsoft.com/win/2004/08/events/event'";

    fn event(provider: &str, id: u32, ts: &str, data: &[(&str, &str)], binary: &str) -> String {
        let data: String = data
            .iter()
            .map(
                |(k, v)| {
                    if k.is_empty() { format!("<Data>{v}</Data>") } else { format!("<Data Name='{k}'>{v}</Data>") }
                },
            )
            .collect();
        let bin = if binary.is_empty() { String::new() } else { format!("<Binary>{binary}</Binary>") };
        format!(
            "<Event {KP_NS}><System><Provider Name='{provider}'/><EventID Qualifiers='0'>{id}</EventID><Level>2</Level>\
<TimeCreated SystemTime='{ts}'/><Channel>System</Channel></System><EventData>{data}{bin}</EventData></Event>"
        )
    }

    fn t(iso: &str) -> f64 {
        parse_iso_utc(iso).unwrap()
    }

    // The 6008 payload of a crash at 2026-10-18 22:03:22 UTC (local time first, then UTC).
    const BIN_6008: &str = "EA070A00010013000600030016002700EA070A00000012001600030016002700DC0500003C00000001000000DC050000010000001E00000001000000FFFFFFFF";

    /// A blue-screen night: standby entered 5 minutes before an access violation in the display driver.
    fn blue_screen_log(cs: bool) -> Log {
        let xml = vec![
            event("Microsoft-Windows-Kernel-General", 12, "2026-10-18T20:00:00.0Z", &[("StartTime", "x")], ""),
            event("Microsoft-Windows-Kernel-Power", 506, "2026-10-18T21:58:00.0Z", &[("Reason", "12")], ""),
            event(
                "Microsoft-Windows-Kernel-Power",
                41,
                "2026-10-18T22:06:36.5Z",
                &[
                    ("BugcheckCode", "59"),
                    ("BugcheckParameter1", "0xc0000005"),
                    ("BugcheckParameter2", "0xfffff807ba4d841b"),
                    ("BugcheckParameter3", "0xffffa88c6086d880"),
                    ("BugcheckParameter4", "0x0"),
                    ("SleepInProgress", "0"),
                    ("PowerButtonTimestamp", "0"),
                    ("ConnectedStandbyInProgress", if cs { "true" } else { "false" }),
                ],
                "",
            ),
            event("EventLog", 6008, "2026-10-18T22:06:54.1Z", &[("", "6:03:22 AM"), ("", "?10/?19/?2026")], BIN_6008),
            event(
                "Microsoft-Windows-WER-SystemErrorReporting",
                1001,
                "2026-10-18T22:06:53.4Z",
                &[
                    (
                        "param1",
                        "0x0000003b (0x00000000c0000005, 0xfffff807ba4d841b, 0xffffa88c6086d880, 0x0000000000000000)",
                    ),
                    ("param2", "C:\\Windows\\Minidump\\101926-21421-01.dmp"),
                    ("param3", "00000000-0000-0000-0000-000000000000"),
                ],
                "",
            ),
        ];
        let app = vec![
            event(
                "Windows Error Reporting",
                1001,
                "2026-10-18T22:07:01.7Z",
                &[("Bucket", ""), ("EventName", "BlueScreen"), ("P1", "3b"), ("P2", "c0000005")],
                "",
            ),
            event(
                "Windows Error Reporting",
                1001,
                "2026-10-18T22:07:36.7Z",
                &[
                    ("Bucket", "AV_nvlddmkm!UvmChannelUpdateProgress"),
                    ("EventName", "BlueScreen"),
                    ("P1", "3b"),
                    ("P2", "c0000005"),
                ],
                "",
            ),
        ];
        Log { system: parse_events(&xml), app: parse_events(&app) }
    }

    const SINCE: f64 = 1_700_000_000.0;

    #[test]
    fn parses_events_as_the_event_log_renders_them() {
        let real = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='nvlddmkm'/>\
<EventID Qualifiers='0'>153</EventID><Version>0</Version><Level>2</Level><TimeCreated SystemTime='2026-10-18T16:31:22.2740051Z'/>\
<EventRecordID>36831</EventRecordID><Channel>System</Channel></System><EventData><Data>\\Device\\Video9</Data>\
<Data>Restarting TDR occurred on GPUID:c500</Data><Binary>0000</Binary></EventData></Event>";
        let e = parse_event(real).unwrap();
        assert_eq!((e.provider.as_str(), e.id), ("nvlddmkm", 153));
        assert_eq!(e.ts.floor(), t("2026-10-18T16:31:22Z"));
        assert_eq!(e.nth(1), Some("Restarting TDR occurred on GPUID:c500"));
        assert_eq!(e.binary, "0000");
        // Double quotes, entities, empty and self-closing data.
        let odd = "<Event><System><Provider Name=\"A &amp; B\"/><EventID>7</EventID><TimeCreated SystemTime=\"2026-01-01T00:00:00Z\"/></System>\
<EventData><Data Name=\"x\">a &lt; b</Data><Data Name=\"empty\"></Data><Data Name=\"closed\"/></EventData></Event>";
        let e = parse_event(odd).unwrap();
        assert_eq!(e.provider, "A & B");
        assert_eq!(e.get("x"), Some("a < b"));
        assert_eq!(e.get("empty"), Some(""));
        assert_eq!(e.get("closed"), Some(""));
        assert!(parse_event("<Event></Event>").is_none());
        assert!(parse_event("garbage").is_none());
    }

    #[test]
    fn bugchecks_decode() {
        assert_eq!(bugcheck_name(0x10E), Some("VIDEO_MEMORY_MANAGEMENT_INTERNAL"));
        assert_eq!(bugcheck_name(0x116), Some("VIDEO_TDR_FAILURE"));
        assert_eq!(bugcheck_name(0x3B), Some("SYSTEM_SERVICE_EXCEPTION"));
        assert_eq!(bugcheck_name(0x124), Some("WHEA_UNCORRECTABLE_ERROR"));
        assert_eq!(bugcheck_name(0x9999), None);
        assert_eq!(bugcheck_kind(0x119), Kind::Gpu);
        assert_eq!(bugcheck_kind(0x1A), Kind::Memory);
        assert_eq!(bugcheck_kind(0x124), Kind::Hardware);
        assert_eq!(exception_name(0xC000_0005), Some("access violation"));
        assert_eq!(
            parse_bugcheck_text(
                "0x0000003b (0x00000000c0000005, 0xfffff807ba4d841b, 0xffffa88c6086d880, 0x0000000000000000)"
            ),
            Some((0x3B, [0xc000_0005, 0xfffff807ba4d841b, 0xffffa88c6086d880, 0]))
        );
        assert_eq!(parse_bugcheck_text("nonsense"), None);
    }

    #[test]
    fn fault_buckets_name_the_module() {
        assert_eq!(
            blame("AV_nvlddmkm!UvmChannelUpdateProgress"),
            Some(("nvlddmkm".into(), Some("UvmChannelUpdateProgress".into())))
        );
        assert_eq!(
            blame("0x10e_2d_nvlddmkm!LDDM_MapCpuHostAperture"),
            Some(("nvlddmkm".into(), Some("LDDM_MapCpuHostAperture".into())))
        );
        assert_eq!(blame("LKD_0x141_Tdr:6_IMAGE_nvlddmkm.sys_Blackwell"), Some(("nvlddmkm".into(), None)));
        assert_eq!(blame("1228625694153023101"), None);
        assert_eq!(blame(""), None);
        assert!(is_gpu_module("nvlddmkm") && is_gpu_module("NVLDDMKM.SYS") && !is_gpu_module("ntfs"));
    }

    #[test]
    fn the_unexpected_shutdown_event_holds_the_crash_time_in_utc() {
        assert_eq!(unexpected_shutdown_time(BIN_6008), Some(t("2026-10-18T22:03:22Z")));
        assert_eq!(unexpected_shutdown_time("00"), None);
        assert_eq!(unexpected_shutdown_time(""), None);
        // Not a plausible date.
        assert_eq!(unexpected_shutdown_time(&"FF".repeat(40)), None);
    }

    #[test]
    fn iso_round_trip() {
        let ts = t("2026-10-18T22:03:22Z");
        assert_eq!(iso_utc(ts), "2026-10-18T22:03:22Z");
        assert_eq!(iso_utc(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(t("2000-02-29T23:59:59Z")), "2000-02-29T23:59:59Z");
    }

    #[test]
    fn queries_are_balanced() {
        for q in [system_query(86_400), app_query(86_400)] {
            assert_eq!(q.matches('[').count(), q.matches(']').count(), "{q}");
            assert_eq!(q.matches('(').count(), q.matches(')').count(), "{q}");
            assert!(q.contains("timediff(@SystemTime) <= 86400000"));
        }
        assert!(system_query(1).contains("EventID=6008"));
    }

    #[test]
    fn a_standby_blue_screen_is_reconstructed_end_to_end() {
        let r = analyse(&blue_screen_log(true), 30.0, SINCE);
        assert_eq!(r.incidents.len(), 1);
        let i = &r.incidents[0];
        assert_eq!(i.crashed, t("2026-10-18T22:03:22Z"));
        assert!(i.crashed_exact);
        assert_eq!(i.bugcheck, Some(0x3B));
        assert_eq!(i.params[0], 0xc000_0005);
        assert_eq!(i.blamed, Some(("nvlddmkm".into(), Some("UvmChannelUpdateProgress".into()))));
        assert_eq!(i.dump.as_deref(), Some("C:\\Windows\\Minidump\\101926-21421-01.dmp"));
        assert_eq!(i.standby, Some(Standby { since: Some(t("2026-10-18T21:58:00Z")), in_progress: true }));
        assert_eq!(i.verdict.cause, Cause::StandbyFault);
        assert!(i.verdict.summary.contains("5m after it entered Modern Standby"), "{}", i.verdict.summary);
        assert_eq!(bugcheck_title(i), "blue screen 0x3B SYSTEM_SERVICE_EXCEPTION, access violation");
        assert_eq!(stats(&r), Stats { reboots: 1, in_standby: 1, gpu_related: 1, gpu_resets: 0 });
        let text = render(&r, 10, &|t| iso_utc(t));
        assert!(text.contains("blamed:   nvlddmkm!UvmChannelUpdateProgress (NVIDIA display driver)"));
        assert!(text.contains("verdict:  [standby]"));
        assert!(to_json(&r).to_string().contains("\"verdict\":\"standby\""));
    }

    #[test]
    fn an_entry_without_an_exit_is_a_crash_on_resume_not_in_standby() {
        let r = analyse(&blue_screen_log(false), 30.0, SINCE);
        let s = r.incidents[0].standby.clone().unwrap();
        assert!(!s.in_progress);
        assert!(r.incidents[0].verdict.summary.contains("as it was resuming"));
    }

    #[test]
    fn a_standby_entry_from_an_earlier_boot_does_not_count() {
        let mut log = blue_screen_log(false);
        // The only standby entry predates the boot that crashed.
        log.system.retain(|e| e.id != 506);
        log.system.push(Ev {
            ts: t("2026-10-18T19:00:00Z"),
            provider: "Microsoft-Windows-Kernel-Power".into(),
            id: 506,
            ..Default::default()
        });
        log.system.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        let r = analyse(&log, 30.0, SINCE);
        assert_eq!(r.incidents[0].standby, None);
        assert_eq!(r.incidents[0].verdict.cause, Cause::GpuFault);
    }

    #[test]
    fn exits_end_a_standby_period() {
        let mut log = blue_screen_log(false);
        log.system.push(Ev {
            ts: t("2026-10-18T22:00:00Z"),
            provider: "Microsoft-Windows-Kernel-Power".into(),
            id: 507,
            ..Default::default()
        });
        log.system.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        let r = analyse(&log, 30.0, SINCE);
        assert_eq!(r.incidents[0].standby, None);
    }

    #[test]
    fn gpu_resets_before_the_crash_are_counted_once_per_reset() {
        let mut log = blue_screen_log(false);
        log.system.retain(|e| e.id != 506);
        // Two resets, three events each, a minute and a half apart; a third long before.
        for base in ["2026-10-18T22:00:10Z", "2026-10-18T22:01:30Z", "2026-10-18T21:00:00Z"] {
            for k in 0..3 {
                log.system.push(Ev {
                    ts: t(base) + f64::from(k),
                    provider: "nvlddmkm".into(),
                    id: 153,
                    ..Default::default()
                });
            }
        }
        log.system.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        let r = analyse(&log, 30.0, SINCE);
        assert_eq!(r.incidents[0].resets_before, 2);
        assert_eq!(r.incidents[0].verdict.cause, Cause::TdrCascade);
        assert_eq!(r.gpu_resets, 3);
    }

    #[test]
    fn a_reboot_with_no_bugcheck_is_a_hang_unless_the_power_button_was_held() {
        let kp = |button: &str| {
            vec![event(
                "Microsoft-Windows-Kernel-Power",
                41,
                "2026-10-01T02:53:16Z",
                &[("BugcheckCode", "0"), ("PowerButtonTimestamp", button), ("ConnectedStandbyInProgress", "false")],
                "",
            )]
        };
        let hang = analyse(&Log { system: parse_events(&kp("0")), app: vec![] }, 30.0, SINCE);
        assert_eq!(hang.incidents[0].verdict.cause, Cause::HardHang);
        assert!(!hang.incidents[0].crashed_exact);
        let held = analyse(&Log { system: parse_events(&kp("134352967485162723")), app: vec![] }, 30.0, SINCE);
        assert_eq!(held.incidents[0].verdict.cause, Cause::ForcedPowerOff);
    }

    #[test]
    fn incidents_older_than_the_window_are_dropped() {
        let r = analyse(&blue_screen_log(true), 1.0, t("2026-10-19T00:00:00Z"));
        assert!(r.incidents.is_empty());
    }

    fn line(
        ts: f64,
        used: u64,
        procs: &[(&str, u64)],
        spilled: &[(u32, u64)],
        commit: (u64, u64),
        avail: u64,
    ) -> String {
        let procs: Vec<String> = procs
            .iter()
            .map(|(k, m)| format!("{{\"pid\":1,\"mib\":{m},\"session_key\":\"{k}\",\"name\":\"python.exe\"}}"))
            .collect();
        let spilled: Vec<String> = spilled.iter().map(|(p, m)| format!("{{\"pid\":{p},\"shared_mib\":{m}}}")).collect();
        format!(
            "{{\"ts\":{ts},\"adapters\":[{{\"total_mib\":15915,\"used_mib\":{used},\"shared_mib\":300,\"spilled\":[{}],\"procs\":[{}]}}],\
\"system\":{{\"available_mib\":{avail},\"commit_mib\":{},\"commit_limit_mib\":{}}}}}",
            spilled.join(","),
            procs.join(","),
            commit.0,
            commit.1
        )
    }

    fn with_log(text: &str, mut r: Report) -> Report {
        let (samples, span) = load_watch_lines(text.lines(), &watch_windows(&r));
        attach_watch(&mut r, &samples, span);
        r
    }

    #[test]
    fn the_watch_log_turns_a_driver_fault_into_a_single_workload_finding() {
        let crashed = t("2026-10-18T22:03:22Z");
        let mut log = blue_screen_log(false);
        log.system.retain(|e| e.id != 506); // awake
        let text: String = (1..=5)
            .map(|k| {
                line(
                    crashed - 5.0 * f64::from(k),
                    9_000,
                    &[("claude 11111111", 8_900), ("desktop", 100)],
                    &[],
                    (40_000, 112_000),
                    8_000,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let r = with_log(&text, analyse(&log, 30.0, SINCE));
        let LogState::Found(c) = &r.incidents[0].log else { panic!("no context: {:?}", r.incidents[0].log) };
        assert_eq!(c.samples, 5);
        assert_eq!(c.jobs(), 1);
        assert_eq!(c.pressure(), None);
        assert_eq!(r.incidents[0].verdict.cause, Cause::SingleWorkloadFault);
        assert!(r.incidents[0].verdict.summary.contains("not contention"));
        let text = render(&r, 10, &|t| iso_utc(t));
        assert!(text.contains("held by: claude 11111111 8,900 MiB"), "{text}");
    }

    #[test]
    fn measured_pressure_beats_standby_as_the_primary_verdict() {
        let crashed = t("2026-10-18T22:03:22Z");
        let text: String = (1..=4)
            .map(|k| {
                line(
                    crashed - 10.0 * f64::from(k),
                    15_800,
                    &[("a", 9_000), ("b", 6_500)],
                    &[(77, 2_100)],
                    (105_000, 112_000),
                    400,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let r = with_log(&text, analyse(&blue_screen_log(true), 30.0, SINCE));
        let i = &r.incidents[0];
        assert_eq!(i.verdict.cause, Cause::MemoryPressure);
        let why = i.verdict.summary.as_str();
        assert!(why.contains("2,100 MiB spilled"), "{why}");
        assert!(why.contains("commit charge reached 94%"), "{why}");
        assert!(why.contains("free RAM fell to 400 MiB"), "{why}");
        assert!(why.contains("full (15,800 of 15,915 MiB) with 2 jobs"), "{why}");
        // The standby fact is still reported.
        assert!(i.standby.is_some());
    }

    #[test]
    fn watch_log_states() {
        let r0 = analyse(&blue_screen_log(true), 30.0, SINCE);
        assert_eq!(r0.incidents[0].log, LogState::None);
        let crashed = t("2026-10-18T22:03:22Z");
        // A log from another day.
        let other = line(crashed - 86_400.0, 1000, &[], &[], (1, 2), 5000);
        let r = with_log(&other, analyse(&blue_screen_log(true), 30.0, SINCE));
        assert!(matches!(r.incidents[0].log, LogState::Outside { .. }));
        // A log that spans the crash but has a hole around it.
        let early = line(crashed - 7_200.0, 1000, &[], &[], (1, 2), 5000);
        let late = line(crashed + 7_200.0, 1000, &[], &[], (1, 2), 5000);
        let r = with_log(&format!("{early}\n{late}\nnot json at all"), analyse(&blue_screen_log(true), 30.0, SINCE));
        assert_eq!(r.incidents[0].log, LogState::Gap);
    }

    #[test]
    fn quick_timestamps() {
        assert_eq!(quick_ts("{\"ts\":1792076217.5,\"x\":1}"), Some(1_792_076_217.5));
        assert_eq!(quick_ts("{\"ts\":12}"), Some(12.0));
        assert_eq!(quick_ts("{}"), None);
        assert_eq!(quick_ts("{\"ts\": 1792076217.5, \"x\": 1}"), Some(1_792_076_217.5));
        assert_eq!(quick_ts("{\"ts\":7"), Some(7.0));
    }

    #[test]
    fn other_bugchecks_are_not_pinned_on_the_gpu() {
        let kp = event(
            "Microsoft-Windows-Kernel-Power",
            41,
            "2026-10-05T10:00:00Z",
            &[("BugcheckCode", "292"), ("ConnectedStandbyInProgress", "false")],
            "",
        );
        let r = analyse(&Log { system: parse_events(&[kp]), app: vec![] }, 30.0, SINCE);
        let i = &r.incidents[0];
        assert_eq!(i.bugcheck, Some(0x124));
        assert_eq!(i.verdict.cause, Cause::OtherBugcheck);
        assert!(i.verdict.summary.contains("WHEA_UNCORRECTABLE_ERROR"));
        assert_eq!(stats(&r).gpu_related, 0);
    }
}
