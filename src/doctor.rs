//! `vramorama doctor`: read-only checks for the things that crash a GPU machine, each with advice you can
//! act on. Nothing here changes a setting. `sysinfo.rs` gathers the facts; this module judges them, so the
//! rules can be tested against recorded values.

use crate::crash::Stats;
use crate::json::{Value, obj};
use crate::parse::thousands;
use crate::tdr::parse_iso_utc;

/// A driver older than this many days is flagged.
pub const DRIVER_OLD_DAYS: f64 = 365.0;
/// Commit charge as a share of the commit limit: advice at the first, risk at the second.
pub const COMMIT_WARN_PCT: f64 = 80.0;
pub const COMMIT_RISK_PCT: f64 = 90.0;
/// The runner will not admit a job while the commit charge is at or above this (percent). 0 turns it off.
pub const DEFAULT_MAX_COMMIT_PCT: u32 = 92;
/// Free RAM below this is flagged.
pub const RAM_LOW_MIB: u64 = 1024;

#[derive(Clone, Debug, Default)]
pub struct Timeouts {
    /// Seconds until the display turns off, on AC and on battery. 0 means never; None means unreadable.
    pub display_ac: Option<u32>,
    pub display_dc: Option<u32>,
    /// Seconds until the system sleeps (Modern Standby counts), likewise.
    pub sleep_ac: Option<u32>,
    pub sleep_dc: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Driver {
    pub desc: String,
    /// Windows driver version, like `32.0.15.7705`.
    pub version: String,
    /// (year, month, day) from the driver's registry entry.
    pub date: Option<(i64, i64, i64)>,
    pub provider: String,
}

#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub name: String,
    pub total_mib: u64,
    pub used_mib: u64,
    /// Shared (system) GPU memory in use on this adapter, when the counter exists.
    pub shared_mib: Option<u64>,
    /// Processes that have spilled into system memory: (pid, MiB).
    pub spilled: Vec<(u32, u64)>,
}

#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub now: f64,
    pub on_ac: Option<bool>,
    pub timeouts: Timeouts,
    /// The machine uses Modern Standby (S0 low power idle) rather than classic S3 sleep.
    pub modern_standby: Option<bool>,
    pub drivers: Vec<Driver>,
    pub gpus: Vec<Gpu>,
    pub ram_total_mib: u64,
    pub ram_avail_mib: u64,
    pub commit_used_mib: u64,
    pub commit_limit_mib: u64,
    /// The power-cap warning text, when the card is capped.
    pub power_cap: Option<String>,
    /// GPU resets (each burst once) in the last 24 hours and the last 7 days.
    pub resets_24h: usize,
    pub resets_7d: usize,
    pub crashes: Option<Stats>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sev {
    Risk,
    Warn,
    Info,
    Ok,
}

impl Sev {
    pub fn tag(self) -> &'static str {
        match self {
            Sev::Risk => "RISK",
            Sev::Warn => "warn",
            Sev::Info => "info",
            Sev::Ok => "ok",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub sev: Sev,
    pub check: &'static str,
    pub title: String,
    pub advice: Option<String>,
}

fn f(sev: Sev, check: &'static str, title: String, advice: Option<&str>) -> Finding {
    Finding { sev, check, title, advice: advice.map(str::to_string) }
}

/// "5 min", "2 h 30 min", "never".
pub fn timeout_text(secs: u32) -> String {
    match secs {
        0 => "never".to_string(),
        1..60 => format!("{secs} s"),
        _ if secs % 3600 == 0 => format!("{} h", secs / 3600),
        _ if secs >= 3600 => format!("{} h {} min", secs / 3600, secs / 60 % 60),
        _ => format!("{} min", secs / 60),
    }
}

/// The NVIDIA release number inside a Windows driver version: `32.0.15.7705` is `577.05`.
pub fn nvidia_release(version: &str) -> Option<String> {
    let parts: Vec<&str> = version.split('.').collect();
    let [_, _, c, d] = parts.as_slice() else { return None };
    let digits = format!("{c}{d}");
    if !digits.chars().all(|ch| ch.is_ascii_digit()) || digits.len() < 5 {
        return None;
    }
    let last5 = &digits[digits.len() - 5..];
    Some(format!("{}.{}", last5[..3].trim_start_matches('0'), &last5[3..]))
}

/// A driver date from the registry, `M-D-YYYY`.
pub fn parse_driver_date(s: &str) -> Option<(i64, i64, i64)> {
    let mut it = s.trim().split(['-', '/']).map(|p| p.trim().parse::<i64>().ok());
    let (m, d, y) = (it.next()??, it.next()??, it.next()??);
    ((1..=12).contains(&m) && (1..=31).contains(&d) && y > 1990).then_some((y, m, d))
}

fn age_days(date: (i64, i64, i64), now: f64) -> Option<f64> {
    let then = parse_iso_utc(&format!("{:04}-{:02}-{:02}T00:00:00Z", date.0, date.1, date.2))?;
    Some((now - then) / 86_400.0)
}

fn is_gpu_vendor(provider: &str, desc: &str) -> bool {
    let all = format!("{provider} {desc}").to_ascii_lowercase();
    !all.contains("microsoft basic") && !all.contains("remote")
}

/// Judge the facts. Findings are most severe first.
pub fn advise(x: &Facts) -> Vec<Finding> {
    let mut out = Vec::new();
    let crashed_in_standby = x.crashes.map_or(0, |c| c.in_standby);
    let crashes = x.crashes.map_or(0, |c| c.reboots);

    // --- Modern Standby and the display timeout
    let screen = if x.on_ac == Some(false) { x.timeouts.display_dc } else { x.timeouts.display_ac };
    let which = if x.on_ac == Some(false) { "on battery" } else { "on AC" };
    match (x.modern_standby, screen) {
        (Some(true), Some(0)) => out.push(f(
            Sev::Ok,
            "standby",
            format!(
                "The screen never turns off {which}, so this PC (Modern Standby) does not drop into it while you work."
            ),
            None,
        )),
        (Some(true), Some(secs)) => {
            let sev = if crashed_in_standby > 0 { Sev::Risk } else { Sev::Warn };
            let seen = if crashed_in_standby > 0 {
                format!(" {crashed_in_standby} of {crashes} unexpected reboots in the log happened in it.")
            } else {
                String::new()
            };
            out.push(f(
                sev,
                "standby",
                format!(
                    "The screen turns off after {} {which}, and this PC uses Modern Standby: screen-off starts it even though sleep is set to {}.{seen}",
                    timeout_text(secs),
                    x.timeouts.sleep_ac.map_or("an unreadable value".to_string(), timeout_text)
                ),
                Some(
                    "In Modern Standby the GPU is power-gated while the CPU keeps running your job, and a driver fault in that transition \
takes the whole machine down. Before a long GPU job, set Settings > System > Power & battery > Screen, sleep & hibernate timeouts > \
\"Turn off my screen\" to Never (or run it with `vramorama run`, which asks Windows to keep the PC awake while the job runs and changes no \
setting). You can restore the timeout afterwards.",
                ),
            ));
        }
        (Some(false), _) => {
            out.push(f(Sev::Ok, "standby", "This PC uses classic S3 sleep, not Modern Standby.".to_string(), None))
        }
        _ => out.push(f(
            Sev::Info,
            "standby",
            "Could not read the power capabilities or the display timeout.".to_string(),
            None,
        )),
    }
    if let (Some(false), Some(secs)) = (x.modern_standby, x.timeouts.sleep_ac) {
        if secs != 0 {
            out.push(f(
                Sev::Warn,
                "sleep",
                format!(
                    "The PC sleeps after {} on AC; a GPU job that is not holding the machine awake will be suspended.",
                    timeout_text(secs)
                ),
                Some("Use `vramorama run` for the job, or set the sleep timeout to Never while it runs."),
            ));
        }
    }

    // --- Display drivers
    for d in x.drivers.iter().filter(|d| is_gpu_vendor(&d.provider, &d.desc)) {
        let nv = d.provider.to_ascii_uppercase().contains("NVIDIA").then(|| nvidia_release(&d.version)).flatten();
        let label = match &nv {
            Some(r) => format!("{} driver {} (release {r})", d.desc, d.version),
            None => format!("{} driver {}", d.desc, d.version),
        };
        match d.date.and_then(|dt| age_days(dt, x.now)) {
            Some(days) if days > DRIVER_OLD_DAYS => out.push(f(
                Sev::Warn,
                "driver",
                format!("{label} is {:.0} days old ({}-{:02}-{:02}).", days, d.date.map_or(0, |t| t.0), d.date.map_or(0, |t| t.1), d.date.map_or(0, |t| t.2)),
                Some(
                    "Display-driver faults are a common cause of GPU crashes and fixes ship every few weeks. Install the current driver \
from the GPU vendor (a clean install if the problems began after an upgrade), then watch whether `vramorama crashes` improves.",
                ),
            )),
            Some(days) => out.push(f(Sev::Ok, "driver", format!("{label} is {days:.0} days old."), None)),
            None => out.push(f(Sev::Info, "driver", format!("{label} (date unreadable)."), None)),
        }
    }
    if x.drivers.is_empty() {
        out.push(f(Sev::Info, "driver", "No display driver entry found in the registry.".to_string(), None));
    }

    // --- Memory: system RAM and commit charge
    if x.commit_limit_mib > 0 {
        let pct = x.commit_used_mib as f64 / x.commit_limit_mib as f64 * 100.0;
        let line = format!(
            "Commit charge {} of {} MiB ({pct:.0}%); RAM {} of {} MiB free.",
            thousands(x.commit_used_mib),
            thousands(x.commit_limit_mib),
            thousands(x.ram_avail_mib),
            thousands(x.ram_total_mib)
        );
        if pct >= COMMIT_RISK_PCT {
            out.push(f(
                Sev::Risk,
                "commit",
                line,
                Some(
                    "Windows is close to running out of memory to promise. Allocations then fail inside drivers, which is a bugcheck and \
not a clean error. Close or restart the biggest RAM holders (`vramorama ram`) or enlarge the page file; `vramorama run` will not start \
a job at this level.",
                ),
            ));
        } else if pct >= COMMIT_WARN_PCT || x.ram_avail_mib < RAM_LOW_MIB {
            out.push(f(
                Sev::Warn,
                "commit",
                line,
                Some("Little headroom: a job that grows can push the machine into swap. `vramorama ram` shows who holds the RAM."),
            ));
        } else {
            out.push(f(Sev::Ok, "commit", line, None));
        }
    }

    // --- GPU memory: dedicated, and spill into shared (system) memory
    for g in &x.gpus {
        let shared =
            g.shared_mib.map_or(String::new(), |s| format!(", {} MiB of shared system memory in use", thousands(s)));
        let line = format!("{}: {} of {} MiB used{shared}.", g.name, thousands(g.used_mib), thousands(g.total_mib));
        if g.spilled.is_empty() {
            out.push(f(Sev::Ok, "spill", line, None));
        } else {
            let mib: u64 = g.spilled.iter().map(|(_, m)| m).sum();
            out.push(f(
                Sev::Risk,
                "spill",
                format!("{line} {} MiB has spilled from the card into system memory.", thousands(mib)),
                Some(
                    "Spilled work runs many times slower, and it competes with the system for RAM and the page file. Stop or pause one \
job; start GPU jobs with `vramorama run` so they queue instead of spilling.",
                ),
            ));
        }
    }

    // --- Power cap
    if let Some(w) = &x.power_cap {
        out.push(f(
            Sev::Risk,
            "power-cap",
            w.clone(),
            Some("See `vramorama power`; waking the machine or changing the power mode usually clears it."),
        ));
    }

    // --- GPU resets (TDR)
    if x.resets_7d == 0 {
        out.push(f(Sev::Ok, "tdr", "No GPU resets (TDR) logged in the last 7 days.".to_string(), None));
    } else {
        let sev = if x.resets_24h >= 3 { Sev::Risk } else { Sev::Warn };
        out.push(f(
            sev,
            "tdr",
            format!("{} GPU reset(s) in the last 24 h, {} in the last 7 days.", x.resets_24h, x.resets_7d),
            Some(
                "A reset kills the jobs of every session on the card. Common causes: a kernel running past the 2 s Windows limit (often \
under a power cap), an old or unstable driver, or power-state transitions such as Modern Standby. `vramorama crashes` lines them up \
with the reboots.",
            ),
        ));
    }

    // --- The record of crashes
    if let Some(c) = x.crashes {
        if c.reboots == 0 {
            out.push(f(Sev::Ok, "crashes", "No unexpected reboots in the event log window.".to_string(), None));
        } else {
            out.push(f(
                if c.in_standby * 2 >= c.reboots || c.reboots >= 5 { Sev::Risk } else { Sev::Warn },
                "crashes",
                format!(
                    "{} unexpected reboot(s) in the event log window: {} in Modern Standby or sleep, {} in the graphics stack.",
                    c.reboots, c.in_standby, c.gpu_related
                ),
                Some("`vramorama crashes` gives each one with its cause; record the minutes before the next with `vramorama watch --quiet --log FILE`."),
            ));
        }
    }
    out.sort_by_key(|x| x.sev);
    out
}

/// The commit-charge admission gate used by `vramorama run`: Some(reason) while the system is at or above
/// `max_pct` of its commit limit.
pub fn commit_gate(used_mib: u64, limit_mib: u64, max_pct: u32) -> Option<String> {
    if max_pct == 0 || limit_mib == 0 {
        return None;
    }
    let pct = used_mib as f64 / limit_mib as f64 * 100.0;
    (pct >= f64::from(max_pct)).then(|| {
        format!(
            "system commit charge is {pct:.0}% of its limit ({} of {} MiB; the limit is {max_pct}%)",
            thousands(used_mib),
            thousands(limit_mib)
        )
    })
}

pub fn render(findings: &[Finding], width: usize) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut seen: Vec<&str> = Vec::new();
    for x in findings {
        for (i, line) in wrap(&x.title, width.saturating_sub(6).clamp(30, 110)).iter().enumerate() {
            let _ = writeln!(out, "{:<5} {line}", if i == 0 { x.sev.tag() } else { "" });
        }
        // The same advice under several findings (an old driver on each adapter) is printed once.
        if let Some(a) = x.advice.as_deref().filter(|a| !seen.contains(a)) {
            seen.push(a);
            for line in wrap(a, width.saturating_sub(10).clamp(30, 100)) {
                let _ = writeln!(out, "        {line}");
            }
        }
    }
    out
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(l) if l.chars().count() + 1 + word.chars().count() <= width => {
                l.push(' ');
                l.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
}

pub fn to_json(findings: &[Finding], x: &Facts) -> Value {
    let list = findings
        .iter()
        .map(|f| {
            obj([
                ("severity", f.sev.tag().to_ascii_lowercase().into()),
                ("check", f.check.into()),
                ("title", f.title.as_str().into()),
                ("advice", f.advice.clone().into()),
            ])
        })
        .collect();
    obj([
        ("modern_standby", x.modern_standby.into()),
        ("on_ac", x.on_ac.into()),
        ("display_off_ac_secs", x.timeouts.display_ac.map(u64::from).into()),
        ("display_off_dc_secs", x.timeouts.display_dc.map(u64::from).into()),
        ("sleep_ac_secs", x.timeouts.sleep_ac.map(u64::from).into()),
        ("sleep_dc_secs", x.timeouts.sleep_dc.map(u64::from).into()),
        ("commit_used_mib", x.commit_used_mib.into()),
        ("commit_limit_mib", x.commit_limit_mib.into()),
        ("ram_avail_mib", x.ram_avail_mib.into()),
        ("gpu_resets_24h", (x.resets_24h as u64).into()),
        ("gpu_resets_7d", (x.resets_7d as u64).into()),
        ("findings", Value::Arr(list)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-19 00:00 UTC.
    const NOW: f64 = 1_792_368_000.0;

    fn facts() -> Facts {
        Facts {
            now: NOW,
            on_ac: Some(true),
            timeouts: Timeouts { display_ac: Some(300), display_dc: Some(300), sleep_ac: Some(0), sleep_dc: Some(0) },
            modern_standby: Some(true),
            drivers: vec![Driver {
                desc: "NVIDIA GeForce RTX 5080 Laptop GPU".into(),
                version: "32.0.15.7705".into(),
                date: Some((2025, 8, 3)),
                provider: "NVIDIA".into(),
            }],
            gpus: vec![Gpu {
                name: "NVIDIA GeForce RTX 5080 Laptop GPU".into(),
                total_mib: 15_915,
                used_mib: 9_000,
                ..Default::default()
            }],
            ram_total_mib: 31_864,
            ram_avail_mib: 8_000,
            commit_used_mib: 40_000,
            commit_limit_mib: 112_080,
            ..Default::default()
        }
    }

    fn by<'a>(v: &'a [Finding], check: &str) -> Vec<&'a Finding> {
        v.iter().filter(|f| f.check == check).collect()
    }

    #[test]
    fn nvidia_release_numbers() {
        assert_eq!(nvidia_release("32.0.15.7705").as_deref(), Some("577.05"));
        assert_eq!(nvidia_release("31.0.15.5222").as_deref(), Some("552.22"));
        assert_eq!(nvidia_release("32.0.15.6094").as_deref(), Some("560.94"));
        assert_eq!(nvidia_release("nonsense"), None);
    }

    #[test]
    fn driver_dates_and_timeouts() {
        assert_eq!(parse_driver_date("8-3-2025"), Some((2025, 8, 3)));
        assert_eq!(parse_driver_date("10/19/2026"), Some((2026, 10, 19)));
        assert_eq!(parse_driver_date("junk"), None);
        assert_eq!(timeout_text(0), "never");
        assert_eq!(timeout_text(300), "5 min");
        assert_eq!(timeout_text(7200), "2 h");
        assert_eq!(timeout_text(5400), "1 h 30 min");
    }

    #[test]
    fn screen_off_on_a_modern_standby_pc_is_flagged_and_escalates_with_crashes() {
        let v = advise(&facts());
        let s = by(&v, "standby");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].sev, Sev::Warn);
        assert!(s[0].title.contains("5 min on AC"));
        assert!(s[0].advice.as_deref().unwrap().contains("vramorama run"));

        let mut x = facts();
        x.crashes = Some(Stats { reboots: 13, in_standby: 4, gpu_related: 9, gpu_resets: 20 });
        let v = advise(&x);
        assert_eq!(by(&v, "standby")[0].sev, Sev::Risk);
        assert!(by(&v, "standby")[0].title.contains("4 of 13"));

        let mut x = facts();
        x.timeouts.display_ac = Some(0);
        assert_eq!(by(&advise(&x), "standby")[0].sev, Sev::Ok);

        let mut x = facts();
        x.modern_standby = Some(false);
        assert_eq!(by(&advise(&x), "standby")[0].sev, Sev::Ok);
    }

    #[test]
    fn on_battery_the_battery_timeout_applies() {
        let mut x = facts();
        x.on_ac = Some(false);
        x.timeouts.display_dc = Some(0);
        assert_eq!(by(&advise(&x), "standby")[0].sev, Sev::Ok);
    }

    #[test]
    fn old_drivers_are_flagged() {
        let v = advise(&facts());
        let d = by(&v, "driver");
        assert_eq!(d[0].sev, Sev::Warn);
        assert!(d[0].title.contains("577.05"));
        assert!(d[0].title.contains("days old"));
        let mut x = facts();
        x.drivers[0].date = Some((2026, 9, 1));
        assert_eq!(by(&advise(&x), "driver")[0].sev, Sev::Ok);
        // The software renderer is not a driver anyone updates.
        let mut x = facts();
        x.drivers[0] = Driver {
            desc: "Microsoft Basic Display Adapter".into(),
            version: "10.0.1.1".into(),
            date: Some((2006, 6, 21)),
            provider: "Microsoft".into(),
        };
        assert!(by(&advise(&x), "driver").iter().all(|f| f.sev == Sev::Info));
    }

    #[test]
    fn memory_pressure_levels() {
        let mut x = facts();
        assert_eq!(by(&advise(&x), "commit")[0].sev, Sev::Ok);
        x.commit_used_mib = 92_000;
        assert_eq!(by(&advise(&x), "commit")[0].sev, Sev::Warn);
        x.commit_used_mib = 105_000;
        assert_eq!(by(&advise(&x), "commit")[0].sev, Sev::Risk);
        let mut x = facts();
        x.ram_avail_mib = 600;
        assert_eq!(by(&advise(&x), "commit")[0].sev, Sev::Warn);
    }

    #[test]
    fn spill_and_power_cap_are_risks() {
        let mut x = facts();
        x.gpus[0].spilled = vec![(100, 3_850)];
        x.power_cap = Some("capped".into());
        let v = advise(&x);
        assert_eq!(by(&v, "spill")[0].sev, Sev::Risk);
        assert!(by(&v, "spill")[0].title.contains("3,850 MiB has spilled"));
        assert_eq!(by(&v, "power-cap")[0].sev, Sev::Risk);
        // Risks sort first.
        assert_eq!(v[0].sev, Sev::Risk);
    }

    #[test]
    fn resets_and_crash_summary() {
        let mut x = facts();
        assert_eq!(by(&advise(&x), "tdr")[0].sev, Sev::Ok);
        x.resets_7d = 6;
        x.resets_24h = 1;
        assert_eq!(by(&advise(&x), "tdr")[0].sev, Sev::Warn);
        x.resets_24h = 3;
        assert_eq!(by(&advise(&x), "tdr")[0].sev, Sev::Risk);
        x.crashes = Some(Stats { reboots: 2, in_standby: 0, gpu_related: 1, gpu_resets: 3 });
        assert_eq!(by(&advise(&x), "crashes")[0].sev, Sev::Warn);
        x.crashes = Some(Stats { reboots: 0, ..Default::default() });
        assert_eq!(by(&advise(&x), "crashes")[0].sev, Sev::Ok);
    }

    #[test]
    fn commit_gate_blocks_at_the_limit() {
        assert_eq!(commit_gate(80_000, 112_080, 92), None);
        let g = commit_gate(104_000, 112_080, 92).unwrap();
        assert!(g.contains("93%"));
        assert!(g.contains("104,000 of 112,080"));
        assert_eq!(commit_gate(111_000, 112_080, 0), None);
        assert_eq!(commit_gate(1, 0, 92), None);
    }

    #[test]
    fn render_wraps_advice_under_the_title() {
        let v = advise(&facts());
        let text = render(&v, 100);
        assert!(text.lines().next().unwrap().starts_with("warn"));
        assert!(text.lines().all(|l| l.chars().count() <= 130));
        let j = to_json(&v, &facts()).to_string();
        assert!(j.contains("\"findings\":[{\"severity\":\"warn\""));
    }
}
