//! Supervised runs: the decisions behind `vramorama run --restart / --guard-power / --enforce-vram`.
//!
//! vramorama never stops a process it did not start, and stops one it did start only when asked. These
//! flags ask: a training job that checkpoints can survive a power-capped card (stop it, wait, start it
//! again) and a Windows GPU reset (start it again), and a job that outgrows its lease can be stopped
//! before it pushes everyone else into spill. All of it is pure logic here; `run.rs` does the polling.

/// What a supervised run asked for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Policy {
    /// Stop the job after the card has been power-capped this long.
    pub guard_power_secs: Option<f64>,
    /// Stop the job when it holds more than its lease plus this many MiB.
    pub enforce_vram_slack_mib: Option<u64>,
    /// Restarts allowed after a GPU reset or a power-guard stop.
    pub restarts: u32,
}

impl Policy {
    pub fn supervised(&self) -> bool {
        self.guard_power_secs.is_some() || self.enforce_vram_slack_mib.is_some() || self.restarts > 0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Fine,
    StopPower { capped_for: f64 },
    StopVram { used_mib: u64, limit_mib: u64 },
}

/// Why a supervised job ended, for the restart decision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ended {
    /// Exited on its own with this code.
    Exit(i32),
    /// Stopped by the power guard.
    PowerGuard,
    /// Stopped for exceeding its lease.
    VramGuard,
}

#[derive(Default)]
pub struct Watch {
    capped_since: Option<f64>,
}

impl Watch {
    /// One look at the running job. `now` in seconds, `capped` whether the card is power-capped,
    /// `used_mib` what the lease's processes hold, `lease_mib` what was asked for.
    pub fn judge(&mut self, p: &Policy, now: f64, capped: bool, used_mib: u64, lease_mib: u64) -> Verdict {
        if !capped {
            self.capped_since = None;
        } else if self.capped_since.is_none() {
            self.capped_since = Some(now);
        }
        if let (Some(limit), Some(since)) = (p.guard_power_secs, self.capped_since) {
            if now - since >= limit {
                return Verdict::StopPower { capped_for: now - since };
            }
        }
        if let Some(slack) = p.enforce_vram_slack_mib {
            let limit_mib = lease_mib + slack;
            if used_mib > limit_mib {
                return Verdict::StopVram { used_mib, limit_mib };
            }
        }
        Verdict::Fine
    }
}

/// Whether to start the job again. A reset of the GPU (`gpu_reset`: a Windows TDR event logged while the
/// job ran) or a power-guard stop is worth another try, because the job will resume from its checkpoint;
/// an ordinary failure is not, or a bug would loop; neither is a job that outgrew its lease, which would
/// only do it again.
pub fn should_restart(ended: Ended, gpu_reset: bool, restarts_left: u32) -> bool {
    if restarts_left == 0 {
        return false;
    }
    match ended {
        Ended::Exit(0) => false,
        Ended::Exit(_) => gpu_reset,
        Ended::PowerGuard => true,
        Ended::VramGuard => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol() -> Policy {
        Policy { guard_power_secs: Some(10.0), enforce_vram_slack_mib: Some(512), restarts: 3 }
    }

    #[test]
    fn power_guard_waits_for_a_sustained_cap() {
        let mut w = Watch::default();
        let p = pol();
        assert_eq!(w.judge(&p, 0.0, true, 1000, 4000), Verdict::Fine);
        assert_eq!(w.judge(&p, 9.0, true, 1000, 4000), Verdict::Fine);
        // The cap lifted for a moment: the clock starts over.
        assert_eq!(w.judge(&p, 9.5, false, 1000, 4000), Verdict::Fine);
        assert_eq!(w.judge(&p, 10.0, true, 1000, 4000), Verdict::Fine);
        assert_eq!(w.judge(&p, 19.0, true, 1000, 4000), Verdict::Fine);
        assert_eq!(w.judge(&p, 20.0, true, 1000, 4000), Verdict::StopPower { capped_for: 10.0 });
    }

    #[test]
    fn vram_guard_allows_slack_over_the_lease() {
        let mut w = Watch::default();
        let p = pol();
        assert_eq!(w.judge(&p, 0.0, false, 4512, 4000), Verdict::Fine);
        assert_eq!(w.judge(&p, 1.0, false, 4513, 4000), Verdict::StopVram { used_mib: 4513, limit_mib: 4512 });
    }

    #[test]
    fn nothing_is_stopped_unless_asked() {
        let mut w = Watch::default();
        let none = Policy::default();
        assert!(!none.supervised());
        assert_eq!(w.judge(&none, 1000.0, true, 99_999, 1), Verdict::Fine);
        assert_eq!(w.judge(&none, 2000.0, true, 99_999, 1), Verdict::Fine);
    }

    #[test]
    fn restart_only_for_gpu_resets_and_power_stops() {
        // 2026-10-15: the 10B ctx-512 run died with 0xC0000409 after a TDR event; resuming from the checkpoint worked.
        assert!(should_restart(Ended::Exit(-1073740791), true, 2));
        // The same exit code with no reset logged is a bug, not a reset.
        assert!(!should_restart(Ended::Exit(-1073740791), false, 2));
        assert!(!should_restart(Ended::Exit(0), true, 2));
        assert!(should_restart(Ended::PowerGuard, false, 1));
        assert!(!should_restart(Ended::VramGuard, true, 5));
        assert!(!should_restart(Ended::PowerGuard, true, 0), "no restarts left");
    }
}
