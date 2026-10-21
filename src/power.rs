//! Power cap: a laptop GPU can be held to a small fraction of its rated power (seen 2026-10-15 on an
//! RTX 5080 Laptop: an enforced limit of 15 W against a default of 80 W, usually after Modern
//! Standby). Everything on the card then runs roughly twenty times slower, and one long kernel can
//! pass the two seconds Windows allows before it resets the GPU (a TDR, `nvlddmkm` event 153). A
//! reset kills the jobs of every session on the card, not just the one that tipped it over.
//!
//! The signal is the enforced power limit against the driver's default limit. Clock readings are no
//! use: under WDDM `nvidia-smi` reports about 200 MHz even while the card draws 100 W.
//!
//! The cap comes and goes: on 2026-10-21 the same card went from 15 W to 137 W and back to 15 W within
//! ten minutes, on AC power. Every reading is therefore taken fresh from NVML and carries its time, and
//! `CapTracker` turns a run of readings into the transitions `run` logs.

/// A card counts as capped when its enforced limit is at most this fraction of its default limit.
pub const CAPPED_BELOW: f64 = 0.5;

#[derive(Clone, Debug, PartialEq)]
pub struct PowerState {
    pub name: String,
    pub draw_w: f64,
    pub enforced_w: f64,
    pub default_w: f64,
    pub util_pct: u32,
    /// NVML's clock event (throttle) reasons, a bit mask, when the driver reports them.
    pub clock_events: Option<u64>,
    /// When this was read, in Unix seconds.
    pub read_at: f64,
}

impl PowerState {
    pub fn capped(&self) -> bool {
        self.default_w > 0.0 && self.enforced_w > 0.0 && self.enforced_w <= self.default_w * CAPPED_BELOW
    }
}

/// NVML's clock event reason bits (`nvmlClocksEventReason*` in nvml.h), named as `nvidia-smi -q` names them.
const CLOCK_EVENTS: [(u64, &str); 9] = [
    (0x1, "gpu_idle"),
    (0x2, "applications_clocks_setting"),
    (0x4, "sw_power_cap"),
    (0x8, "hw_slowdown"),
    (0x10, "sync_boost"),
    (0x20, "sw_thermal_slowdown"),
    (0x40, "hw_thermal_slowdown"),
    (0x80, "hw_power_brake_slowdown"),
    (0x100, "display_clock_setting"),
];

/// The names of the clock event reasons set in `bits`. `sw_power_cap` alone is not a cap in our sense:
/// it is set whenever the card draws up to its enforced limit, whatever that limit is.
pub fn clock_event_names(bits: u64) -> Vec<&'static str> {
    CLOCK_EVENTS.iter().filter(|(b, _)| bits & b != 0).map(|&(_, n)| n).collect()
}

/// The clock event reasons of a reading in words.
pub fn clock_events(p: &PowerState) -> String {
    match p.clock_events {
        None => "not reported".to_string(),
        Some(0) => "none".to_string(),
        Some(b) => clock_event_names(b).join(", "),
    }
}

/// The reading in words: limits, draw, use and the clock event reasons.
pub fn reading(p: &PowerState) -> String {
    format!(
        "enforced limit {:.0} W against a {:.0} W default, drawing {:.0} W at {}% use, clock events: {}",
        p.enforced_w,
        p.default_w,
        p.draw_w,
        p.util_pct,
        clock_events(p)
    )
}

/// One line for the console and the GUI, or None when the card is not capped.
pub fn warning(p: &PowerState) -> Option<String> {
    p.capped().then(|| {
        format!(
            "{} is power-capped: enforced limit {:.0} W against a default of {:.0} W (drawing {:.0} W at {}% use). \
Jobs run many times slower and a long kernel can trip the 2 s Windows GPU timeout, which resets the card for \
every session. Do not start GPU jobs until it clears (waking the machine, or a power-mode change, usually does it).",
            p.name, p.enforced_w, p.default_w, p.draw_w, p.util_pct
        )
    })
}

pub fn to_json(p: &PowerState) -> crate::json::Value {
    use crate::json::{Value, obj};
    obj([
        ("name", p.name.as_str().into()),
        ("draw_w", p.draw_w.into()),
        ("enforced_limit_w", p.enforced_w.into()),
        ("default_limit_w", p.default_w.into()),
        ("util_pct", u64::from(p.util_pct).into()),
        ("capped", p.capped().into()),
        (
            "clock_events",
            p.clock_events
                .map_or(Value::Null, |b| Value::Arr(clock_event_names(b).into_iter().map(Into::into).collect())),
        ),
        ("read_at", p.read_at.into()),
    ])
}

/// What one reading says about the cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    Capped,
    Clear,
    /// No reading (NVML failed, or no NVIDIA card). Never counts as capped.
    Unreadable,
}

impl Cap {
    pub fn of(p: Option<&PowerState>) -> Cap {
        match p {
            Some(p) if p.capped() => Cap::Capped,
            Some(_) => Cap::Clear,
            None => Cap::Unreadable,
        }
    }
}

/// What a new reading changed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    /// The first reading.
    First(Cap),
    /// The same state as before.
    Same,
    /// A transition from `from`, which had held for `held_secs`.
    To { from: Cap, to: Cap, held_secs: f64 },
}

/// Follows the cap from one reading to the next. Nothing is cached: it only remembers the last state
/// and since when, to say what a fresh reading changed.
#[derive(Default)]
pub struct CapTracker {
    last: Option<(Cap, f64)>,
}

impl CapTracker {
    pub fn update(&mut self, now: f64, cap: Cap) -> Change {
        match self.last {
            None => {
                self.last = Some((cap, now));
                Change::First(cap)
            }
            Some((from, _)) if from == cap => Change::Same,
            Some((from, since)) => {
                self.last = Some((cap, now));
                Change::To { from, to: cap, held_secs: (now - since).max(0.0) }
            }
        }
    }
}

/// The log line for a change worth reporting, without the time stamp: a transition, or a first reading
/// that is capped (a first reading that is clear or unreadable, as on a card that is not NVIDIA's, is
/// not news). `why` says what prompted the reading.
pub fn change_line(change: Change, p: Option<&PowerState>, why: &str) -> Option<String> {
    let what = match p {
        Some(p) => format!("{} (NVML)", reading(p)),
        None => "no power reading from NVML".to_string(),
    };
    let head = match change {
        Change::Same | Change::First(Cap::Clear | Cap::Unreadable) => return None,
        Change::First(Cap::Capped) => return Some(format!("power-capped at the first check: {what}")),
        Change::To { from, to, held_secs } => {
            let word = |c: Cap| match c {
                Cap::Capped => "capped",
                Cap::Clear => "not capped",
                Cap::Unreadable => "unreadable",
            };
            let verb = match (from, to) {
                (_, Cap::Capped) => "power cap set",
                (Cap::Capped, _) => "power cap cleared",
                _ => "power state changed",
            };
            format!("{verb} ({} for {}, now {})", word(from), crate::parse::duration(held_secs), word(to))
        }
    };
    Some(format!("{head}: {what}; prompted by {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(enforced: f64, default: f64) -> PowerState {
        PowerState {
            name: "NVIDIA GeForce RTX 5080 Laptop GPU".into(),
            draw_w: 19.0,
            enforced_w: enforced,
            default_w: default,
            util_pct: 100,
            clock_events: Some(0x4),
            read_at: 1_792_036_800.0,
        }
    }

    #[test]
    fn capped_means_enforced_limit_far_below_default() {
        // 2026-10-15: 15 W enforced against an 80 W default.
        assert!(state(15.0, 80.0).capped());
        // The healthy state of the same card: 100 W enforced, above the default.
        assert!(!state(100.6, 80.0).capped());
        assert!(state(40.0, 80.0).capped());
        assert!(!state(41.0, 80.0).capped());
        // Unreadable limits never count as capped.
        assert!(!state(0.0, 80.0).capped());
        assert!(!state(15.0, 0.0).capped());
    }

    #[test]
    fn warning_only_when_capped() {
        assert_eq!(warning(&state(100.0, 80.0)), None);
        let w = warning(&state(15.0, 80.0)).unwrap();
        assert!(w.contains("15 W against a default of 80 W"));
        assert!(w.contains("2 s Windows GPU timeout"));
    }

    #[test]
    fn clock_events_are_named_like_nvidia_smi() {
        assert_eq!(clock_event_names(0), Vec::<&str>::new());
        assert_eq!(clock_event_names(0x4), vec!["sw_power_cap"]);
        assert_eq!(clock_event_names(0x1 | 0x4 | 0x40), vec!["gpu_idle", "sw_power_cap", "hw_thermal_slowdown"]);
        // Bits nvml.h does not define are left out rather than misnamed.
        assert_eq!(clock_event_names(0x8000), Vec::<&str>::new());
        let mut p = state(15.0, 80.0);
        assert!(reading(&p).ends_with("clock events: sw_power_cap"));
        p.clock_events = None;
        assert!(reading(&p).ends_with("clock events: not reported"));
        p.clock_events = Some(0);
        assert_eq!(
            reading(&p),
            "enforced limit 15 W against a 80 W default, drawing 19 W at 100% use, clock events: none"
        );
        let j = to_json(&state(15.0, 80.0)).to_string();
        assert!(j.contains(r#""clock_events":["sw_power_cap"]"#), "{j}");
        assert!(j.contains(r#""read_at":"#), "{j}");
    }

    #[test]
    fn tracker_reports_each_transition_once_with_how_long_the_old_state_held() {
        let mut t = CapTracker::default();
        assert_eq!(t.update(100.0, Cap::Capped), Change::First(Cap::Capped));
        assert_eq!(t.update(102.0, Cap::Capped), Change::Same);
        // 2026-10-21: 15 W, then 137 W a few minutes later, then 15 W again.
        assert_eq!(t.update(340.0, Cap::Clear), Change::To { from: Cap::Capped, to: Cap::Clear, held_secs: 240.0 });
        assert_eq!(t.update(342.0, Cap::Clear), Change::Same);
        assert_eq!(t.update(640.0, Cap::Capped), Change::To { from: Cap::Clear, to: Cap::Capped, held_secs: 300.0 });
        // Losing the reading is a change too, and it does not count as capped.
        assert_eq!(
            t.update(650.0, Cap::Unreadable),
            Change::To { from: Cap::Capped, to: Cap::Unreadable, held_secs: 10.0 }
        );
        assert_eq!(Cap::of(None), Cap::Unreadable);
        assert_eq!(Cap::of(Some(&state(15.0, 80.0))), Cap::Capped);
        assert_eq!(Cap::of(Some(&state(137.0, 80.0))), Cap::Clear);
    }

    #[test]
    fn change_lines_carry_the_reading_its_source_and_what_prompted_it() {
        let capped = state(15.0, 80.0);
        let clear = PowerState { enforced_w: 137.0, draw_w: 41.0, clock_events: Some(0), ..state(15.0, 80.0) };
        assert_eq!(change_line(Change::Same, Some(&capped), "poll"), None);
        assert_eq!(change_line(Change::First(Cap::Clear), Some(&clear), "poll"), None);
        assert_eq!(change_line(Change::First(Cap::Unreadable), None, "the first check"), None);
        let first = change_line(Change::First(Cap::Capped), Some(&capped), "the first check").unwrap();
        assert_eq!(
            first,
            "power-capped at the first check: enforced limit 15 W against a 80 W default, drawing 19 W at 100% use, \
clock events: sw_power_cap (NVML)"
        );
        let cleared = Change::To { from: Cap::Capped, to: Cap::Clear, held_secs: 250.0 };
        let line = change_line(cleared, Some(&clear), "Windows: power mode balanced -> max performance").unwrap();
        assert!(line.starts_with("power cap cleared (capped for 4m, now not capped): enforced limit 137 W"), "{line}");
        assert!(line.ends_with("(NVML); prompted by Windows: power mode balanced -> max performance"), "{line}");
        let set = Change::To { from: Cap::Clear, to: Cap::Capped, held_secs: 60.0 };
        assert!(
            change_line(set, Some(&capped), "poll")
                .unwrap()
                .starts_with("power cap set (not capped for 1m, now capped)")
        );
        let lost = Change::To { from: Cap::Capped, to: Cap::Unreadable, held_secs: 5.0 };
        let lost = change_line(lost, None, "the regular check").unwrap();
        assert_eq!(
            lost,
            "power cap cleared (capped for 5s, now unreadable): no power reading from NVML; prompted by the regular check"
        );
    }
}
