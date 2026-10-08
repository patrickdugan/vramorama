//! Idle holders: processes that keep GPU memory while doing no GPU work, and whether anyone is
//! still around to want it.

use crate::owner::Owner;

/// Below this busiest-engine percentage a process counts as doing nothing.
pub const IDLE_UTIL: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Idle, and the agent session that started it has ended. Only positive evidence counts:
    /// an untagged process whose launcher exited is not stale, because that is also what every
    /// desktop app started by a short-lived launcher looks like.
    Stale,
    /// Idle, but an owner may still want it (a live session, a label, a lease, or unknown).
    Idle,
}

/// Judge an idle process from its owner tags, whether its Claude Code instance is still running,
/// and where its launch chain starts (`scan::origin`).
pub fn classify(owner: &Owner, harness_live: Option<bool>, root: Option<&str>) -> (Verdict, String) {
    if let Some(label) = &owner.label {
        return (Verdict::Idle, format!("labelled \"{label}\"; ask its owner"));
    }
    if owner.session.is_some() {
        return match harness_live {
            Some(false) => (Verdict::Stale, "the Claude Code session that started it has ended".into()),
            Some(true) => (Verdict::Idle, "its Claude Code session is still running".into()),
            None => (Verdict::Idle, "cannot tell whether its session is still running".into()),
        };
    }
    if let Some(agent) = &owner.agent {
        return (Verdict::Idle, format!("started by {agent}; session state unknown"));
    }
    if let Some(lease) = &owner.lease {
        return (Verdict::Idle, format!("holds lease {lease}"));
    }
    match root {
        Some(r) => (Verdict::Idle, format!("untagged, {r}; `vramorama trace` may find its owner")),
        None => (Verdict::Idle, "untagged".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Owner {
        Owner { session: Some("5f3c1e2a-7d41".into()), harness_pid: Some(41200), ..Default::default() }
    }

    #[test]
    fn only_ended_sessions_are_stale() {
        assert_eq!(classify(&session(), Some(false), None).0, Verdict::Stale);
        // A browser's GPU process whose launcher exited looks exactly like an orphaned job.
        let (v, why) = classify(&Owner::default(), None, Some("from Discord.exe 15568 (parent exited)"));
        assert_eq!(v, Verdict::Idle);
        assert!(why.contains("Discord.exe 15568") && why.contains("trace"));
    }

    #[test]
    fn anything_with_a_possible_owner_is_only_idle() {
        assert_eq!(classify(&session(), Some(true), None).0, Verdict::Idle);
        assert_eq!(classify(&session(), None, None).0, Verdict::Idle);
        let labelled = Owner { label: Some("eval-server".into()), session: Some("x".into()), ..Default::default() };
        assert_eq!(classify(&labelled, Some(false), None).0, Verdict::Idle, "a label is a person's claim");
        let leased = Owner { lease: Some("ab12cd34".into()), ..Default::default() };
        assert_eq!(classify(&leased, None, None).0, Verdict::Idle);
        assert_eq!(classify(&Owner::default(), None, Some("via WmiPrvSE.exe 35976")).0, Verdict::Idle);
        assert_eq!(classify(&Owner::default(), None, None).0, Verdict::Idle);
    }
}
