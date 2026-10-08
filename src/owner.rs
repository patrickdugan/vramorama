//! Who a process belongs to, read from tags in its inherited environment.

/// Tags vramorama understands. Environment variables are inherited by every child process, so a
/// job started from an agent's shell carries the agent's session id without any cooperation.
pub const LABEL_VAR: &str = "VRAMORAMA_OWNER";
const CLAUDE_SESSION: &str = "CLAUDE_CODE_SESSION_ID";
const CLAUDE_PID: &str = "CLAUDE_PID";
const AI_AGENT: &str = "AI_AGENT";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Owner {
    /// Free-form owner set by a person or script with `VRAMORAMA_OWNER`.
    pub label: Option<String>,
    /// Claude Code session id (also the transcript file name).
    pub session: Option<String>,
    /// Process id of the Claude Code instance that ran the shell.
    pub harness_pid: Option<u32>,
    /// `AI_AGENT`, a harness-neutral marker some agents set (Claude Code: `claude-code_<ver>_agent`).
    pub agent: Option<String>,
    /// The `vramorama run` lease the process was started under (inherited by its children).
    pub lease: Option<String>,
}

impl Owner {
    pub fn from_env(env: &[(String, String)]) -> Owner {
        let get = |k: &str| {
            env.iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Owner {
            label: get(LABEL_VAR),
            session: get(CLAUDE_SESSION),
            harness_pid: get(CLAUDE_PID).and_then(|v| v.parse().ok()),
            agent: get(AI_AGENT),
            lease: get(crate::ledger::LEASE_VAR),
        }
    }

    pub fn is_tagged(&self) -> bool {
        self.label.is_some() || self.session.is_some() || self.agent.is_some() || self.lease.is_some()
    }

    /// Short stable key for grouping: the label, else `claude:<first 8 of session>`, else the agent.
    pub fn key(&self) -> Option<String> {
        if let Some(l) = &self.label {
            return Some(l.clone());
        }
        if let Some(s) = &self.session {
            return Some(format!("claude:{}", short(s)));
        }
        self.agent.clone().or_else(|| self.lease.as_ref().map(|l| format!("lease:{l}")))
    }
}

pub fn short(session: &str) -> &str {
    session.get(..8).unwrap_or(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn claude_shell_env() {
        let o = Owner::from_env(&env(&[
            ("PATH", "C:\\bin"),
            ("CLAUDE_CODE_SESSION_ID", "5f3c1e2a-7d41-4b8e-9a6c-1e2f3a4b5c6d"),
            ("CLAUDE_PID", "41200"),
            ("AI_AGENT", "claude-code_2-1-293_agent"),
            ("VRAMORAMA_LEASE", "ab12cd34"),
        ]));
        assert_eq!(o.lease.as_deref(), Some("ab12cd34"));
        assert_eq!(o.session.as_deref(), Some("5f3c1e2a-7d41-4b8e-9a6c-1e2f3a4b5c6d"));
        assert_eq!(o.harness_pid, Some(41200));
        assert_eq!(o.key().as_deref(), Some("claude:5f3c1e2a"));
        assert!(o.is_tagged());
    }

    #[test]
    fn label_wins_and_blank_is_untagged() {
        let o = Owner::from_env(&env(&[("vramorama_owner", "nightly-sweep"), ("CLAUDE_CODE_SESSION_ID", "abc")]));
        assert_eq!(o.key().as_deref(), Some("nightly-sweep"));
        let o = Owner::from_env(&env(&[("VRAMORAMA_OWNER", "  "), ("PATH", "x")]));
        assert!(!o.is_tagged());
        assert_eq!(o.key(), None);
        let o = Owner::from_env(&env(&[("VRAMORAMA_LEASE", "ab12cd34")]));
        assert_eq!(o.key().as_deref(), Some("lease:ab12cd34"));
    }
}
