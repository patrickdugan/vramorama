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
    /// Restarts allowed after a GPU reset, a power-guard stop or a pause.
    pub restarts: u32,
    /// An exit with this code means the job stopped itself at a checkpoint because the card was blocked
    /// (`--pause-exit`): it is restarted like a power-guard stop, without being killed mid-kernel.
    pub pause_exit: Option<i32>,
}

impl Policy {
    pub fn supervised(&self) -> bool {
        self.guard_power_secs.is_some()
            || self.enforce_vram_slack_mib.is_some()
            || self.restarts > 0
            || self.pause_exit.is_some()
    }
}

/// How a job that exited on its own with `code` ended, under policy `p`.
pub fn classify_exit(p: &Policy, code: i32) -> Ended {
    if p.pause_exit == Some(code) { Ended::Paused } else { Ended::Exit(code) }
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
    /// Exited with the `--pause-exit` code: it saw the card blocked and stopped itself at a checkpoint.
    Paused,
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
        Ended::PowerGuard | Ended::Paused => true,
        Ended::VramGuard => false,
    }
}

/// A power-guard stop sooner than this after the job started was not a fair try (the cap was most likely
/// still on); a job that ran longer made progress, so its next stop starts the count over.
pub const FAIR_RUN_SECS: f64 = 900.0;
/// A `--pause-exit` sooner than this after the job started was not a fair try. A pause after longer made
/// progress (the job saves its checkpoint before it stops itself), so its next pause starts the count over.
/// 2026-11-04: the card reached its 15 W cap after about five minutes of full load every time, so a
/// checkpointing job paused every five to six minutes; counting those pauses as fruitless would have
/// doubled the wait to half an hour within four windows.
pub const PAUSE_FAIR_RUN_SECS: f64 = 180.0;
/// The longest the card is ever required to stay clear before a stopped job is started again.
pub const MAX_CLEAR_SECS: f64 = 1800.0;

/// How long the card has to stay free of every blocker before a stopped job is started again.
///
/// 2026-10-25: a job stopped by `--guard-power` was started again the moment one reading showed the cap
/// lifted, the cap came back, and the guard stopped it again two minutes later: five restarts gone in
/// fifteen minutes, each one loading the whole job onto a scarce card and scarce RAM for nothing. A
/// restart should only follow a clear stretch longer than the guard's own window, and each fruitless stop
/// in a row doubles that stretch.
#[derive(Default)]
pub struct Backoff {
    streak: u32,
}

impl Backoff {
    /// Record why the job ended and how long it had run.
    pub fn note_end(&mut self, ended: Ended, ran_secs: f64) {
        match ended {
            Ended::PowerGuard if ran_secs < FAIR_RUN_SECS => self.streak += 1,
            Ended::Paused if ran_secs < PAUSE_FAIR_RUN_SECS => self.streak += 1,
            _ => self.streak = 0,
        }
    }

    /// How long the card must stay clear before the next start. A stop by the power guard or a pause waits
    /// for one and a half guard windows (at least a minute), doubled for each earlier fruitless stop in a
    /// row; other endings (a GPU reset) need no clear stretch beyond the cool-down.
    pub fn clear_needed(&self, p: &Policy, ended: Ended) -> f64 {
        if !matches!(ended, Ended::PowerGuard | Ended::Paused) {
            return 0.0;
        }
        let base = (p.guard_power_secs.unwrap_or(60.0) * 1.5).max(60.0);
        let doublings = self.streak.saturating_sub(1).min(8);
        (base * f64::from(1u32 << doublings)).min(MAX_CLEAR_SECS)
    }
}

/// Starts a clock when nothing blocks and starts it over the moment something does.
#[derive(Default)]
pub struct ClearGate {
    clear_since: Option<f64>,
}

impl ClearGate {
    /// One look: `blocked` whether anything blocks right now. True once nothing has blocked for `need` seconds.
    pub fn open(&mut self, now: f64, blocked: bool, need: f64) -> bool {
        if blocked {
            self.clear_since = None;
            return false;
        }
        let since = *self.clear_since.get_or_insert(now);
        now - since >= need
    }

    /// How long nothing has blocked, for the waiting line.
    pub fn clear_for(&self, now: f64) -> f64 {
        self.clear_since.map_or(0.0, |s| now - s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol() -> Policy {
        Policy { guard_power_secs: Some(10.0), enforce_vram_slack_mib: Some(512), restarts: 3, pause_exit: None }
    }

    #[test]
    fn a_pause_exit_restarts_and_backs_off_like_a_power_stop() {
        // 2026-11-04: the LOTUS capture was killed mid-kernel by the power guard and Windows logged a GPU
        // reset a second later; a job that checks the cap itself between cases exits cleanly instead.
        let p = Policy { guard_power_secs: Some(90.0), restarts: 2, pause_exit: Some(75), ..Policy::default() };
        assert!(p.supervised());
        assert!(Policy { pause_exit: Some(75), ..Policy::default() }.supervised());
        assert_eq!(classify_exit(&p, 75), Ended::Paused);
        assert_eq!(classify_exit(&p, 2), Ended::Exit(2));
        assert_eq!(classify_exit(&pol(), 75), Ended::Exit(75), "without --pause-exit 75 is an ordinary failure");
        assert!(should_restart(Ended::Paused, false, 1));
        assert!(!should_restart(Ended::Paused, false, 0));
        assert!(!should_restart(classify_exit(&pol(), 75), false, 3));
        let mut b = Backoff::default();
        assert_eq!(b.clear_needed(&p, Ended::Paused), 135.0);
        b.note_end(Ended::Paused, 60.0);
        b.note_end(Ended::PowerGuard, 300.0);
        assert_eq!(b.clear_needed(&p, Ended::Paused), 270.0, "short pauses and guard stops count as one streak");
    }

    #[test]
    fn a_pause_after_a_productive_window_starts_the_count_over() {
        // 2026-11-04: the cap came back after five to six minutes of full load, every time.
        let p = Policy { guard_power_secs: Some(90.0), restarts: 20, pause_exit: Some(75), ..Policy::default() };
        let mut b = Backoff::default();
        for _ in 0..10 {
            b.note_end(Ended::Paused, 330.0);
            assert_eq!(b.clear_needed(&p, Ended::Paused), 135.0);
        }
        // A pause right after the start means the cap never really cleared: back off as before.
        b.note_end(Ended::Paused, 40.0);
        b.note_end(Ended::Paused, 40.0);
        assert_eq!(b.clear_needed(&p, Ended::Paused), 270.0);
        // The power guard keeps its own, longer fairness window.
        let mut g = Backoff::default();
        g.note_end(Ended::PowerGuard, 330.0);
        g.note_end(Ended::PowerGuard, 330.0);
        assert_eq!(g.clear_needed(&p, Ended::PowerGuard), 270.0);
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

    #[test]
    fn each_fruitless_power_stop_in_a_row_doubles_the_clear_stretch() {
        // 2026-10-25: --guard-power 120, the job stopped every ~2 minutes because the cap kept coming back.
        let p = Policy { guard_power_secs: Some(120.0), enforce_vram_slack_mib: None, restarts: 5, pause_exit: None };
        let mut b = Backoff::default();
        assert_eq!(
            b.clear_needed(&p, Ended::PowerGuard),
            180.0,
            "even the first stop waits longer than the guard window"
        );
        b.note_end(Ended::PowerGuard, 125.0);
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), 180.0);
        b.note_end(Ended::PowerGuard, 130.0);
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), 360.0);
        b.note_end(Ended::PowerGuard, 128.0);
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), 720.0);
        b.note_end(Ended::PowerGuard, 127.0);
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), 1440.0);
        for _ in 0..10 {
            b.note_end(Ended::PowerGuard, 120.0);
        }
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), MAX_CLEAR_SECS, "never more than half an hour");
    }

    #[test]
    fn a_long_run_or_another_ending_starts_the_count_over() {
        let p = Policy { guard_power_secs: Some(120.0), enforce_vram_slack_mib: None, restarts: 5, pause_exit: None };
        let mut b = Backoff::default();
        b.note_end(Ended::PowerGuard, 100.0);
        b.note_end(Ended::PowerGuard, 100.0);
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), 360.0);
        // It ran for an hour before the cap came back: that was progress.
        b.note_end(Ended::PowerGuard, 3600.0);
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), 180.0);
        b.note_end(Ended::PowerGuard, 100.0);
        b.note_end(Ended::Exit(-1), 10.0);
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), 180.0);
    }

    #[test]
    fn a_gpu_reset_needs_no_clear_stretch_and_a_tiny_guard_window_still_waits_a_minute() {
        let p = Policy { guard_power_secs: Some(10.0), enforce_vram_slack_mib: None, restarts: 3, pause_exit: None };
        let mut b = Backoff::default();
        assert_eq!(b.clear_needed(&p, Ended::Exit(-1073740791)), 0.0);
        assert_eq!(b.clear_needed(&p, Ended::PowerGuard), 60.0);
        // No guard window at all (--restart alone, then only a reset can end it): the floor still applies.
        let none = Policy { guard_power_secs: None, enforce_vram_slack_mib: None, restarts: 3, pause_exit: None };
        assert_eq!(b.clear_needed(&none, Ended::PowerGuard), 90.0);
        b.note_end(Ended::PowerGuard, 5.0);
        assert_eq!(b.clear_needed(&none, Ended::PowerGuard), 90.0);
    }

    #[test]
    fn the_gate_opens_only_after_an_unbroken_clear_stretch() {
        let mut g = ClearGate::default();
        assert!(!g.open(0.0, true, 180.0));
        assert!(!g.open(10.0, false, 180.0), "the clock starts at the first clear reading");
        assert!(!g.open(100.0, false, 180.0));
        // The cap came back for one reading: the stretch starts over.
        assert!(!g.open(130.0, true, 180.0));
        assert_eq!(g.clear_for(130.0), 0.0);
        assert!(!g.open(160.0, false, 180.0));
        assert!(!g.open(339.0, false, 180.0));
        assert!(g.open(340.0, false, 180.0));
        assert_eq!(g.clear_for(340.0), 180.0);
        // Needing nothing opens it on the first clear reading.
        let mut now = ClearGate::default();
        assert!(now.open(5.0, false, 0.0));
    }
}
