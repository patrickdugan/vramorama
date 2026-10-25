//! Why a waiting `vramorama run` lease is not admitted yet, and when to say so. Pure logic with tests;
//! `run.rs` does the polling.
//!
//! Every condition that blocks admission is reported, not only the first one checked. Each waiting
//! line carries the time of the readings behind it, and a line that has not changed is printed again
//! every minute with the time of its new reading, so the last line of a log is never an old reading
//! that only looks current.

use crate::gpu::Adapter;
use crate::parse::thousands;
use crate::power::PowerState;
use crate::thrash::Thrash;

/// An unchanged waiting line is printed again this often.
pub const REPEAT_NOTE_SECS: f64 = 60.0;
/// Checks this much later than planned mean the machine slept, or this process was suspended (Modern
/// Standby suspends desktop processes), in between.
pub const GAP_SECS: f64 = 30.0;

/// Every health condition that keeps anything from being admitted right now, in words, in a fixed
/// order: spill, thrash, power cap, commit charge.
pub fn blockers(
    spills: &[(u32, u64)],
    thrash: Option<&Thrash>,
    power: Option<&PowerState>,
    commit: Option<(u64, u64)>,
    max_commit_pct: u32,
) -> Vec<String> {
    let mut out = Vec::new();
    if !spills.is_empty() {
        // The card is already over-committed: one more job only makes everything slower.
        let mib: u64 = spills.iter().map(|(_, m)| m).sum();
        out.push(format!(
            "{} MiB has spilled into system memory (pid {})",
            thousands(mib),
            spills.iter().map(|(p, _)| p.to_string()).collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(t) = thrash {
        // Processes are already taking turns on the card: another job would join the queue for it.
        out.push(format!(
            "pids {} are taking turns on the full card (thrashing)",
            t.procs.iter().map(|p| p.pid.to_string()).collect::<Vec<_>>().join(" and ")
        ));
    }
    if let Some(p) = power.filter(|p| p.capped()) {
        // A capped card runs everything far slower and a long kernel can trip the 2 s Windows GPU
        // timeout, which resets the card for every session: do not add a job now.
        out.push(format!(
            "power-capped ({:.0} W enforced against a {:.0} W default, read from NVML)",
            p.enforced_w, p.default_w
        ));
    }
    if let Some(why) = commit.and_then(|(used, limit)| crate::doctor::commit_gate(used, limit, max_commit_pct)) {
        // Near the commit limit a job that grows pushes the machine into swap, and a failed allocation
        // inside a driver is a blue screen rather than an error message.
        out.push(why);
    }
    out
}

/// This request's own wait, for when it could not start even once the blockers clear: the memory is
/// not free, or earlier requests come first. Without figures, so the note stays the same while they
/// move; the line printed once the blockers clear has them.
pub fn then_waiting(want_mib: u64, avail_mib: i64, ahead: usize) -> String {
    if avail_mib < want_mib as i64 {
        format!("even then {} MiB does not fit yet", thousands(want_mib))
    } else if ahead > 0 {
        format!("even then {ahead} earlier request(s) come first")
    } else {
        "even then this request waits its turn".to_string()
    }
}

/// The waiting note for these blockers, or None when nothing blocks admission. `then` is this
/// request's own wait behind them (see `then_waiting`).
pub fn blocked_note(blockers: &[String], then: Option<&str>) -> Option<String> {
    let until = match blockers.len() {
        0 => return None,
        1 => "it clears".to_string(),
        2 => "both clear".to_string(),
        n => format!("all {n} clear"),
    };
    let mut note = format!("{}; not admitting anything until {until}", blockers.join("; also "));
    if let Some(t) = then {
        note.push_str("; ");
        note.push_str(t);
    }
    Some(note)
}

/// Other places a job could go while this card is blocked, each with the reason it may not help. Printed
/// once per wait, never acted on: moving a job to another device is the job's own business.
///
/// 2026-10-25: a 19 GB-commit job and a 12 GB-commit job were sharing a 16 GB card that Windows kept
/// resetting, with 0.4 GB of RAM free. The other adapters on that machine were a 0.5 GB integrated GPU
/// (shares system RAM) and an NPU (only runs compiled models), so neither was a way out.
pub fn routing_hints(adapters: &[Adapter], card_luid: u64) -> Vec<String> {
    const GIB: u64 = 1 << 30;
    let mut out = Vec::new();
    for a in adapters.iter().filter(|a| a.luid != card_luid && !a.software) {
        if a.dedicated < GIB {
            out.push(format!(
                "{} is an integrated GPU that borrows system memory: it cannot run CUDA jobs, and a job moved there adds to the RAM and commit pressure instead of relieving it",
                a.name
            ));
        } else {
            out.push(format!(
                "{} has {} GiB of its own: start the job with --adapter \"{}\" if its code can run there",
                a.name,
                a.dedicated / GIB,
                a.name
            ));
        }
    }
    out.push(
        "an NPU, if the machine has one, only runs models compiled for it (ONNX or the vendor's toolchain), not \
PyTorch or CUDA code; the CPU runs small jobs but adds to RAM pressure"
            .to_string(),
    );
    out
}

/// When to print a waiting note: when it changes, and again every `REPEAT_NOTE_SECS` while it does not.
#[derive(Default)]
pub struct NoteLog {
    last: Option<(String, f64)>,
}

impl NoteLog {
    pub fn due(&mut self, note: &str, now: f64) -> bool {
        match &self.last {
            Some((n, at)) if n == note && now - at < REPEAT_NOTE_SECS => false,
            _ => {
                self.last = Some((note.to_string(), now));
                true
            }
        }
    }
}

/// How long it has been since the previous check, when that is at least `GAP_SECS` more than the
/// `interval` the checks run at: the machine slept, or this process was suspended, in between.
pub fn gap(prev: Option<f64>, now: f64, interval: f64) -> Option<f64> {
    prev.map(|p| now - p).filter(|g| *g >= interval + GAP_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thrash::Proc;

    fn power(enforced: f64) -> PowerState {
        PowerState {
            name: "NVIDIA GeForce RTX 5080 Laptop GPU".into(),
            draw_w: 17.55,
            enforced_w: enforced,
            default_w: 80.0,
            util_pct: 100,
            clock_events: Some(0x4),
            read_at: 1_792_036_860.0,
        }
    }

    fn thrash() -> Thrash {
        let p = |pid| Proc { pid, low_mib: 1000, high_mib: 7000, reversals: 6, ram_antiphase: true };
        Thrash { procs: vec![p(101), p(202)], period_secs: Some(8.0), window_secs: 60.0, samples: 30, full_share: 0.9 }
    }

    #[test]
    fn nothing_blocks_a_healthy_machine() {
        let ok = power(136.69);
        assert!(blockers(&[], None, Some(&ok), Some((80_000, 112_000)), 92).is_empty());
        assert!(blockers(&[], None, None, None, 92).is_empty());
        assert_eq!(blocked_note(&[], None), None);
        assert_eq!(blocked_note(&[], Some("even then this request waits its turn")), None);
    }

    #[test]
    fn a_power_cap_and_a_high_commit_charge_are_both_reported() {
        // 2026-10-21 morning: the card was capped while the commit charge was over the limit, and the
        // waiting line named only the first check that failed.
        let capped = power(15.0);
        let b = blockers(&[], None, Some(&capped), Some((104_000, 112_080)), 92);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0], "power-capped (15 W enforced against a 80 W default, read from NVML)");
        assert!(b[1].starts_with("system commit charge is 93% of its limit"), "{}", b[1]);
        let note = blocked_note(&b, None).unwrap();
        assert!(note.starts_with("power-capped (15 W"), "{note}");
        assert!(note.contains("; also system commit charge is 93%"), "{note}");
        assert!(note.ends_with("; not admitting anything until both clear"), "{note}");
    }

    #[test]
    fn the_requests_own_wait_is_named_too_without_figures() {
        assert_eq!(then_waiting(6144, 3347, 0), "even then 6,144 MiB does not fit yet");
        assert_eq!(then_waiting(6144, -200, 2), "even then 6,144 MiB does not fit yet");
        assert_eq!(then_waiting(6144, 10_761, 2), "even then 2 earlier request(s) come first");
        assert_eq!(then_waiting(6144, 10_761, 0), "even then this request waits its turn");
        let b = blockers(&[], None, Some(&power(15.0)), None, 92);
        assert_eq!(
            blocked_note(&b, Some(&then_waiting(6144, 3347, 0))).unwrap(),
            "power-capped (15 W enforced against a 80 W default, read from NVML); not admitting anything until it \
clears; even then 6,144 MiB does not fit yet"
        );
    }

    #[test]
    fn every_blocker_is_listed_in_a_fixed_order() {
        let capped = power(15.0);
        let b = blockers(&[(4242, 900), (77, 600)], Some(&thrash()), Some(&capped), Some((111_000, 112_000)), 92);
        assert_eq!(b.len(), 4);
        assert_eq!(b[0], "1,500 MiB has spilled into system memory (pid 4242, 77)");
        assert_eq!(b[1], "pids 101 and 202 are taking turns on the full card (thrashing)");
        assert!(b[2].starts_with("power-capped"));
        assert!(b[3].starts_with("system commit charge is 99%"));
        assert!(blocked_note(&b, None).unwrap().ends_with("until all 4 clear"));
        // One alone reads as before.
        let one = blocked_note(&b[1..2], None).unwrap();
        assert_eq!(
            one,
            "pids 101 and 202 are taking turns on the full card (thrashing); not admitting anything until it clears"
        );
    }

    #[test]
    fn the_commit_gate_can_be_turned_off_and_an_uncapped_card_never_blocks() {
        assert!(blockers(&[], None, None, Some((111_000, 112_000)), 0).is_empty());
        // 40 W against 80 W is capped; 41 W is not (see power.rs).
        assert_eq!(blockers(&[], None, Some(&power(41.0)), None, 92), Vec::<String>::new());
        assert_eq!(blockers(&[], None, Some(&power(40.0)), None, 92).len(), 1);
    }

    fn adapter(luid: u64, name: &str, dedicated_mib: u64, software: bool) -> Adapter {
        Adapter { luid, name: name.into(), vendor: 0, dedicated: dedicated_mib << 20, software }
    }

    #[test]
    fn routing_hints_say_why_the_other_devices_do_not_help() {
        let all = [
            adapter(1, "NVIDIA GeForce RTX 5080 Laptop GPU", 16_303, false),
            adapter(2, "AMD Radeon(TM) 880M Graphics", 512, false),
            adapter(3, "Microsoft Basic Render Driver", 0, true),
        ];
        let h = routing_hints(&all, 1);
        assert_eq!(h.len(), 2, "the card itself and the software adapter are not offered: {h:?}");
        assert!(h[0].starts_with("AMD Radeon(TM) 880M Graphics is an integrated GPU that borrows system memory"), "{}", h[0]);
        assert!(h[0].contains("adds to the RAM and commit pressure"), "{}", h[0]);
        assert!(h[1].starts_with("an NPU, if the machine has one, only runs models compiled for it"), "{}", h[1]);
    }

    #[test]
    fn a_second_discrete_card_is_offered_with_the_adapter_flag() {
        let all = [adapter(1, "NVIDIA A", 16_000, false), adapter(2, "NVIDIA B", 24_576, false)];
        let h = routing_hints(&all, 1);
        assert_eq!(h[0], "NVIDIA B has 24 GiB of its own: start the job with --adapter \"NVIDIA B\" if its code can run there");
        // Alone on the machine: only the general caveat is left.
        assert_eq!(routing_hints(&all[..1], 1).len(), 1);
    }

    #[test]
    fn an_unchanged_note_is_repeated_every_minute_so_the_latest_line_is_fresh() {
        let mut log = NoteLog::default();
        assert!(log.due("power-capped", 1000.0));
        assert!(!log.due("power-capped", 1002.0));
        assert!(!log.due("power-capped", 1059.0));
        assert!(log.due("power-capped", 1060.0));
        assert!(!log.due("power-capped", 1062.0));
        // A change is printed at once, and so is a change back.
        assert!(log.due("10,761 MiB free", 1064.0));
        assert!(log.due("power-capped", 1066.0));
    }

    #[test]
    fn a_long_gap_between_checks_means_sleep_or_suspension() {
        assert_eq!(gap(None, 1000.0, 2.0), None);
        assert_eq!(gap(Some(998.0), 1000.0, 2.0), None);
        assert_eq!(gap(Some(970.0), 1000.0, 2.0), None);
        assert_eq!(gap(Some(968.0), 1000.0, 2.0), Some(32.0));
        // The restart wait checks every 30 s; that alone is no gap.
        assert_eq!(gap(Some(970.0), 1000.0, 30.0), None);
        assert_eq!(gap(Some(400.0), 1000.0, 30.0), Some(600.0));
        // A clock set backwards is not a gap.
        assert_eq!(gap(Some(2000.0), 1000.0, 2.0), None);
    }
}
