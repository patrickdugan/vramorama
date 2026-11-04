//! `run --after TARGET`: hold a lease in the queue until named GPU work has unloaded, then admit it the
//! usual way. Pure logic with tests; `run.rs` does the polling.
//!
//! The memory test alone cannot sequence two jobs when the first was not started through `run`: it sees
//! free memory in the moment between that job tearing down one model and loading the next, and admits
//! the new job into the gap (2026-11-04: an evaluation session ran two llama-servers back to back on a
//! 16 GB card while another session's 11 GB job waited on a guessed clock time). `--after` names the work
//! to wait for: a pid, an agent session, a `VRAMORAMA_OWNER` label, a working folder, or another lease.
//! The lease is admitted only once every target has held less than `UNLOADED_MIB` on the card for a
//! quiet window (`--after-quiet`, default two minutes), and then only by the ordinary memory, settle and
//! health tests. A target that loads again during the window starts it over.

use crate::owner::Owner;
use crate::parse::{basename, thousands};

/// A target holding less than this on the card counts as unloaded: a CUDA context or a desktop helper,
/// not a model. The same floor `run` uses for memory held without a lease.
pub const UNLOADED_MIB: u64 = 256;
/// Default `--after-quiet`: how long every target must stay unloaded before admission is considered.
pub const DEFAULT_QUIET_SECS: f64 = 120.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A process; unloaded once it has exited or holds less than `UNLOADED_MIB`.
    Pid(u32),
    /// An agent session: `claude:2b87abc0`, `2b87abc0`, or a prefix of the full id.
    Session(String),
    /// A `VRAMORAMA_OWNER` label, or the owner key `leases` shows.
    Owner(String),
    /// Processes whose working folder has this name (the `[name]` in `ps`), for untagged jobs.
    Cwd(String),
    /// Another `run` lease; unloaded once it has left the ledger and its processes hold nothing.
    Lease(String),
}

impl Target {
    /// `1234`, `pid:1234`, `session:claude:2b87abc0`, `owner:sweeps`, `cwd:bench`, `lease:234228b0`.
    pub fn parse(s: &str) -> Result<Target, String> {
        let s = s.trim();
        if let Ok(pid) = s.parse::<u32>() {
            return Ok(Target::Pid(pid));
        }
        let usage =
            || format!("--after takes a pid, pid:N, session:ID, owner:LABEL, cwd:FOLDER or lease:ID (got {s:?})");
        let (kind, rest) = s.split_once(':').ok_or_else(usage)?;
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(usage());
        }
        Ok(match kind.to_ascii_lowercase().as_str() {
            "pid" => Target::Pid(rest.parse().map_err(|_| usage())?),
            "session" => Target::Session(rest.to_ascii_lowercase()),
            "owner" => Target::Owner(rest.to_string()),
            "cwd" => Target::Cwd(rest.to_ascii_lowercase()),
            "lease" => Target::Lease(rest.to_ascii_lowercase()),
            _ => return Err(usage()),
        })
    }

    /// The target as `parse` reads it back, for the ledger and log lines.
    pub fn label(&self) -> String {
        match self {
            Target::Pid(p) => format!("pid:{p}"),
            Target::Session(s) => format!("session:{s}"),
            Target::Owner(o) => format!("owner:{o}"),
            Target::Cwd(c) => format!("cwd:{c}"),
            Target::Lease(l) => format!("lease:{l}"),
        }
    }

    /// Whether a process on the card is part of this target.
    pub fn matches(&self, h: &Holder) -> bool {
        match self {
            Target::Pid(p) => *p == h.pid,
            Target::Session(s) => {
                h.owner.agent_session_key().is_some_and(|k| {
                    let k = k.to_ascii_lowercase();
                    k == *s || k.ends_with(&format!(":{s}"))
                }) || h.owner.session().is_some_and(|full| full.to_ascii_lowercase().starts_with(s.as_str()))
            }
            Target::Owner(o) => {
                h.owner.label.as_deref() == Some(o.as_str()) || h.owner.key().as_deref() == Some(o.as_str())
            }
            Target::Cwd(c) => h.cwd.as_deref().is_some_and(|d| basename(d).eq_ignore_ascii_case(c)),
            Target::Lease(l) => h.owner.lease.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(l)),
        }
    }
}

/// A process holding memory on the card, as `run` observes it.
#[derive(Clone, Debug, Default)]
pub struct Holder {
    pub pid: u32,
    pub mib: u64,
    pub owner: Owner,
    pub cwd: Option<String>,
}

/// A target that has not unloaded yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Loaded {
    pub target: String,
    pub mib: u64,
    pub pids: Vec<u32>,
    /// A lease target still in the ledger (queued or running) though its processes hold little.
    pub lease_open: bool,
}

/// The targets still loaded. `open_leases` are the ids of the leases in the ledger now.
pub fn loaded(targets: &[Target], holders: &[Holder], open_leases: &[String]) -> Vec<Loaded> {
    let mut out = Vec::new();
    for t in targets {
        let mut pids: Vec<u32> = Vec::new();
        let mut mib = 0;
        for h in holders.iter().filter(|h| t.matches(h)) {
            mib += h.mib;
            pids.push(h.pid);
        }
        let lease_open = matches!(t, Target::Lease(l) if open_leases.iter().any(|x| x.eq_ignore_ascii_case(l)));
        if mib >= UNLOADED_MIB || lease_open {
            pids.sort_unstable();
            out.push(Loaded { target: t.label(), mib, pids, lease_open });
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub enum Gate {
    /// Some targets still hold memory (or, for a lease, have not finished).
    Loaded(Vec<Loaded>),
    /// All unloaded; this many seconds of the quiet window remain.
    Quiet(f64),
    /// All unloaded for the whole quiet window: the lease may be admitted on the usual tests.
    Open,
}

/// Follows the targets from one check to the next.
#[derive(Default)]
pub struct AfterGate {
    clear_since: Option<f64>,
}

impl AfterGate {
    pub fn update(&mut self, loaded: Vec<Loaded>, now: f64, quiet_secs: f64) -> Gate {
        if !loaded.is_empty() {
            self.clear_since = None;
            return Gate::Loaded(loaded);
        }
        let since = *self.clear_since.get_or_insert(now);
        let left = quiet_secs - (now - since);
        if left <= 0.0 { Gate::Open } else { Gate::Quiet(left) }
    }
}

/// The waiting note for a closed gate, or None when it is open. The quiet-window note carries no
/// countdown, so it is printed once and then repeated each minute like any unchanged note.
pub fn note(gate: &Gate, quiet_secs: f64) -> Option<String> {
    match gate {
        Gate::Open => None,
        Gate::Quiet(_) => {
            Some(format!("every --after target has unloaded; confirming none loads again for {quiet_secs:.0}s"))
        }
        Gate::Loaded(list) => {
            let parts: Vec<String> = list
                .iter()
                .map(|l| {
                    let pids = if l.pids.is_empty() {
                        String::new()
                    } else {
                        format!(" in pid {}", l.pids.iter().map(u32::to_string).collect::<Vec<_>>().join(", "))
                    };
                    if l.lease_open && l.mib < UNLOADED_MIB {
                        format!("{} has not finished", l.target)
                    } else {
                        format!("{} holds {} MiB{pids}", l.target, thousands(l.mib))
                    }
                })
                .collect();
            Some(format!("waiting for --after work to unload: {}", parts.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(pid: u32, mib: u64, env: &[(&str, &str)], cwd: Option<&str>) -> Holder {
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Holder { pid, mib, owner: Owner::from_env(&env), cwd: cwd.map(str::to_string) }
    }

    const SESSION: &str = "2b87abc0-2672-4120-bc50-6adea00b5d17";

    #[test]
    fn parses_every_form() {
        assert_eq!(Target::parse("1234"), Ok(Target::Pid(1234)));
        assert_eq!(Target::parse("pid:77"), Ok(Target::Pid(77)));
        assert_eq!(Target::parse("session:Claude:2B87ABC0"), Ok(Target::Session("claude:2b87abc0".into())));
        assert_eq!(Target::parse("owner:sweeps"), Ok(Target::Owner("sweeps".into())));
        assert_eq!(Target::parse("cwd:bitcoin-defi-bench"), Ok(Target::Cwd("bitcoin-defi-bench".into())));
        assert_eq!(Target::parse("lease:234228B0"), Ok(Target::Lease("234228b0".into())));
        for bad in ["", "session:", "gpu:1", "pid:x", "nonsense"] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
        for s in ["pid:77", "session:claude:2b87abc0", "owner:sweeps", "cwd:bench", "lease:234228b0"] {
            assert_eq!(Target::parse(s).unwrap().label(), s);
        }
    }

    #[test]
    fn session_matches_key_short_id_and_full_prefix() {
        let h = holder(1, 7000, &[("CLAUDE_CODE_SESSION_ID", SESSION)], None);
        for t in ["session:claude:2b87abc0", "session:2b87abc0", "session:2b87abc0-2672"] {
            assert!(Target::parse(t).unwrap().matches(&h), "{t}");
        }
        assert!(!Target::parse("session:claude:aaaaaaaa").unwrap().matches(&h));
        assert!(!Target::parse("session:2b87abc0").unwrap().matches(&holder(2, 7000, &[], None)));
    }

    #[test]
    fn owner_cwd_lease_and_pid_match() {
        let h = holder(
            9,
            500,
            &[("VRAMORAMA_OWNER", "sweeps"), ("VRAMORAMA_LEASE", "234228b0")],
            Some(r"C:\Users\me\GitHub\bitcoin-defi-bench"),
        );
        assert!(Target::parse("owner:sweeps").unwrap().matches(&h));
        assert!(Target::parse("cwd:Bitcoin-DeFi-Bench").unwrap().matches(&h));
        assert!(Target::parse("lease:234228b0").unwrap().matches(&h));
        assert!(Target::parse("9").unwrap().matches(&h));
        assert!(!Target::parse("10").unwrap().matches(&h));
        assert!(!Target::parse("cwd:bench").unwrap().matches(&h));
    }

    #[test]
    fn small_holders_count_as_unloaded() {
        let t = vec![Target::parse("session:2b87abc0").unwrap()];
        let env = [("CLAUDE_CODE_SESSION_ID", SESSION)];
        let two_servers = [holder(5, 7149, &env, None), holder(6, 7149, &env, None)];
        let got = loaded(&t, &two_servers, &[]);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].mib, got[0].pids.clone()), (14_298, vec![5, 6]));
        // A leftover CUDA context is not a loaded model.
        assert!(loaded(&t, &[holder(5, 120, &env, None)], &[]).is_empty());
        assert!(loaded(&t, &[], &[]).is_empty());
    }

    #[test]
    fn a_lease_target_waits_until_it_leaves_the_ledger() {
        let t = vec![Target::parse("lease:234228b0").unwrap()];
        let open = vec!["234228b0".to_string()];
        let got = loaded(&t, &[], &open);
        assert!(got[0].lease_open);
        assert_eq!(
            note(&Gate::Loaded(got), 120.0).unwrap(),
            "waiting for --after work to unload: lease:234228b0 has not finished"
        );
        assert!(loaded(&t, &[], &[]).is_empty());
    }

    #[test]
    fn the_quiet_window_restarts_when_a_target_loads_again() {
        let env = [("CLAUDE_CODE_SESSION_ID", SESSION)];
        let t = vec![Target::parse("session:2b87abc0").unwrap()];
        let busy = || loaded(&t, &[holder(5, 7149, &env, None)], &[]);
        let mut g = AfterGate::default();
        assert!(matches!(g.update(busy(), 0.0, 120.0), Gate::Loaded(_)));
        assert_eq!(g.update(vec![], 10.0, 120.0), Gate::Quiet(120.0));
        assert_eq!(g.update(vec![], 70.0, 120.0), Gate::Quiet(60.0));
        // The next model loads inside the window: start over.
        assert!(matches!(g.update(busy(), 90.0, 120.0), Gate::Loaded(_)));
        assert_eq!(g.update(vec![], 100.0, 120.0), Gate::Quiet(120.0));
        assert_eq!(g.update(vec![], 220.0, 120.0), Gate::Open);
        // Open is not latched: a target that loads before admission closes the gate again.
        assert!(matches!(g.update(busy(), 221.0, 120.0), Gate::Loaded(_)));
        assert_eq!(g.update(vec![], 222.0, 0.0), Gate::Open);
    }

    #[test]
    fn notes_name_what_is_still_loaded() {
        let gate = Gate::Loaded(vec![Loaded {
            target: "session:claude:2b87abc0".into(),
            mib: 14_298,
            pids: vec![27276, 30196],
            lease_open: false,
        }]);
        assert_eq!(
            note(&gate, 120.0).unwrap(),
            "waiting for --after work to unload: session:claude:2b87abc0 holds 14,298 MiB in pid 27276, 30196"
        );
        assert_eq!(note(&Gate::Quiet(30.0), 120.0), note(&Gate::Quiet(90.0), 120.0), "stable while counting down");
        assert_eq!(note(&Gate::Open, 120.0), None);
    }
}
