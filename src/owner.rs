//! Who a process belongs to, read from tags in its inherited environment.

/// Tags vramorama understands. Environment variables are inherited by every child process, so a
/// job started from an agent's shell carries the agent's session id without any cooperation.
pub const LABEL_VAR: &str = "VRAMORAMA_OWNER";
const CLAUDE_SESSION: &str = "CLAUDE_CODE_SESSION_ID";
const CLAUDE_HOST_SESSION: &str = "CLAUDE_CODE_HOST_SESSION_ID";
const CLAUDE_ENTRYPOINT: &str = "CLAUDE_CODE_ENTRYPOINT";
const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
const CLAUDE_PID: &str = "CLAUDE_PID";
const CODEX_THREAD: &str = "CODEX_THREAD_ID";
const CODEX_ORIGINATOR: &str = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";
const CODEX_HOME: &str = "CODEX_HOME";
const AI_AGENT: &str = "AI_AGENT";

/// `CLAUDE_CODE_ENTRYPOINT` of the Claude Code instances the Claude desktop app runs for Cowork.
const COWORK_ENTRYPOINT: &str = "local-agent";

/// The agent app whose session launched a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Harness {
    /// Claude Code: the CLI, the desktop app's Code tab, IDE extensions, the Agent SDK.
    Claude,
    /// Claude Cowork, the desktop app's agent mode. Its sessions run Claude Code with
    /// `CLAUDE_CODE_ENTRYPOINT=local-agent`, usually inside a VM that has no GPU.
    Cowork,
    /// OpenAI Codex: the agent in the ChatGPT desktop app, and the Codex CLI. Codex puts
    /// `CODEX_THREAD_ID` in the environment of every command it runs.
    Codex,
}

impl Harness {
    /// Prefix of the session's group key, e.g. `claude:5f3c1e2a`.
    pub fn tag(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Cowork => "cowork",
            Harness::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Owner {
    /// Free-form owner set by a person or script with `VRAMORAMA_OWNER`.
    pub label: Option<String>,
    /// The `vramorama run` lease the process was started under (inherited by its children).
    pub lease: Option<String>,
    /// `AI_AGENT`, a harness-neutral marker some agents set (Claude Code: `claude-code_<ver>_agent`).
    pub agent: Option<String>,
    /// Claude Code session id (also the transcript file name).
    pub claude: Option<String>,
    /// The Claude desktop app's id for that session (`local_<uuid>`), which names its session record.
    pub claude_host: Option<String>,
    /// `CLAUDE_CODE_ENTRYPOINT`: `cli`, `claude-desktop`, `local-agent` (Cowork), `sdk-ts`, ...
    pub claude_entry: Option<String>,
    /// `CLAUDE_CONFIG_DIR`, where a session that does not use `~/.claude` keeps its transcript.
    pub claude_config: Option<String>,
    /// Codex thread id.
    pub codex: Option<String>,
    /// Codex runs for a desktop app (the ChatGPT app) rather than as the CLI.
    pub codex_desktop: bool,
    /// `CODEX_HOME`, when set (default `~/.codex`).
    pub codex_home: Option<String>,
    /// The harness process whose exit ends the session: `CLAUDE_PID`, or for Codex the nearest
    /// `codex.exe` ancestor (filled in by `scan`, which sees the process tree).
    pub harness_pid: Option<u32>,
    /// When both a Claude and a Codex tag are present (one agent ran the other), the one nearer in
    /// the process tree. Without it Claude is assumed.
    pub nearest: Option<Harness>,
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
            lease: get(crate::ledger::LEASE_VAR),
            agent: get(AI_AGENT),
            claude: get(CLAUDE_SESSION),
            claude_host: get(CLAUDE_HOST_SESSION),
            claude_entry: get(CLAUDE_ENTRYPOINT),
            claude_config: get(CLAUDE_CONFIG_DIR),
            codex: get(CODEX_THREAD),
            codex_desktop: get(CODEX_ORIGINATOR).is_some_and(|o| o.to_ascii_lowercase().contains("desktop")),
            codex_home: get(CODEX_HOME),
            harness_pid: get(CLAUDE_PID).and_then(|v| v.parse().ok()),
            nearest: None,
        }
    }

    pub fn is_tagged(&self) -> bool {
        self.label.is_some()
            || self.claude.is_some()
            || self.codex.is_some()
            || self.agent.is_some()
            || self.lease.is_some()
    }

    /// The agent app whose session started the process.
    pub fn harness(&self) -> Option<Harness> {
        let claude = || {
            if self.claude_entry.as_deref() == Some(COWORK_ENTRYPOINT) { Harness::Cowork } else { Harness::Claude }
        };
        match (&self.claude, &self.codex) {
            (Some(_), Some(_)) if self.nearest == Some(Harness::Codex) => Some(Harness::Codex),
            (Some(_), _) => Some(claude()),
            (None, Some(_)) => Some(Harness::Codex),
            (None, None) => None,
        }
    }

    /// The session (Claude) or thread (Codex) id of that harness.
    pub fn session(&self) -> Option<&str> {
        match self.harness()? {
            Harness::Codex => self.codex.as_deref(),
            Harness::Claude | Harness::Cowork => self.claude.as_deref(),
        }
    }

    /// The agent session as a short stable key, e.g. `claude:5f3c1e2a` or `codex:f3c4933e`.
    pub fn agent_session_key(&self) -> Option<String> {
        Some(format!("{}:{}", self.harness()?.tag(), short(self.session()?)))
    }

    /// Group key for jobs: the label, else the agent session, else the agent, else the lease.
    pub fn key(&self) -> Option<String> {
        if let Some(l) = &self.label {
            return Some(l.clone());
        }
        self.agent_session_key()
            .or_else(|| self.agent.clone())
            .or_else(|| self.lease.as_ref().map(|l| format!("lease:{l}")))
    }

    /// Group key for agent sessions: the session even when the job has a label, else as `key`.
    pub fn session_key(&self) -> Option<String> {
        self.agent_session_key().or_else(|| self.key())
    }

    /// The app, as people know it.
    pub fn app(&self) -> Option<&'static str> {
        Some(match self.harness()? {
            Harness::Claude => "Claude Code",
            Harness::Cowork => "Claude Cowork",
            Harness::Codex if self.codex_desktop => "ChatGPT (Codex)",
            Harness::Codex => "Codex CLI",
        })
    }

    /// The app in one lowercase word, for the console.
    pub fn app_word(&self) -> Option<&'static str> {
        Some(match self.harness()? {
            Harness::Codex if self.codex_desktop => "chatgpt",
            h => h.tag(),
        })
    }
}

/// A short form of a session id. Claude Code ids are random (UUIDv4), so the first 8 digits do.
/// Codex thread ids are UUIDv7, whose first 8 digits are a timestamp shared by threads started
/// within the same minute or so, so their last 8 (random) digits are used instead.
pub fn short(session: &str) -> &str {
    let b = session.as_bytes();
    if b.len() == 36 && b[8] == b'-' && b[14] == b'7' {
        return &session[28..];
    }
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
            ("CLAUDE_CODE_HOST_SESSION_ID", "local_6e7f8a9b-1c2d-4e3f-8a4b-5c6d7e8f9a0b"),
            ("CLAUDE_CODE_ENTRYPOINT", "claude-desktop"),
            ("CLAUDE_PID", "41200"),
            ("AI_AGENT", "claude-code_2-1-293_agent"),
            ("VRAMORAMA_LEASE", "ab12cd34"),
        ]));
        assert_eq!(o.lease.as_deref(), Some("ab12cd34"));
        assert_eq!(o.harness(), Some(Harness::Claude));
        assert_eq!(o.session(), Some("5f3c1e2a-7d41-4b8e-9a6c-1e2f3a4b5c6d"));
        assert_eq!(o.claude_host.as_deref(), Some("local_6e7f8a9b-1c2d-4e3f-8a4b-5c6d7e8f9a0b"));
        assert_eq!(o.harness_pid, Some(41200));
        assert_eq!(o.key().as_deref(), Some("claude:5f3c1e2a"));
        assert_eq!(o.app(), Some("Claude Code"));
        assert!(o.is_tagged());
    }

    #[test]
    fn label_wins_the_job_key_but_not_the_session_key() {
        let o =
            Owner::from_env(&env(&[("vramorama_owner", "nightly-sweep"), ("CLAUDE_CODE_SESSION_ID", "abcdef12-3")]));
        assert_eq!(o.key().as_deref(), Some("nightly-sweep"));
        assert_eq!(o.session_key().as_deref(), Some("claude:abcdef12"));
        let o = Owner::from_env(&env(&[("VRAMORAMA_OWNER", "  "), ("PATH", "x")]));
        assert!(!o.is_tagged());
        assert_eq!((o.key(), o.session_key()), (None, None));
        let o = Owner::from_env(&env(&[("VRAMORAMA_LEASE", "ab12cd34")]));
        assert_eq!(o.key().as_deref(), Some("lease:ab12cd34"));
        assert_eq!(o.session_key().as_deref(), Some("lease:ab12cd34"));
        let o = Owner::from_env(&env(&[("VRAMORAMA_OWNER", "eval-server")]));
        assert_eq!(o.session_key().as_deref(), Some("eval-server"));
    }

    #[test]
    fn cowork_is_claude_code_with_the_local_agent_entrypoint() {
        let o = Owner::from_env(&env(&[
            ("CLAUDE_CODE_SESSION_ID", "9a8b7c6d-0000-4000-8000-000000000001"),
            ("CLAUDE_CODE_ENTRYPOINT", "local-agent"),
            ("CLAUDE_CONFIG_DIR", r"C:\Users\me\AppData\Roaming\Claude\local-agent-mode-sessions\a\b\local_x\.claude"),
        ]));
        assert_eq!(o.harness(), Some(Harness::Cowork));
        assert_eq!(o.key().as_deref(), Some("cowork:9a8b7c6d"));
        assert_eq!(o.app(), Some("Claude Cowork"));
        assert!(o.claude_config.as_deref().unwrap().ends_with(r"local_x\.claude"));
    }

    #[test]
    fn codex_threads_from_the_chatgpt_app_and_the_cli() {
        let desktop = Owner::from_env(&env(&[
            ("CODEX_THREAD_ID", "0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f"),
            ("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "Codex Desktop"),
        ]));
        assert_eq!(desktop.harness(), Some(Harness::Codex));
        // UUIDv7: the random tail, not the timestamp head, names the thread.
        assert_eq!(desktop.key().as_deref(), Some("codex:3c4d5e6f"));
        assert_eq!(desktop.app(), Some("ChatGPT (Codex)"));
        assert_eq!(desktop.app_word(), Some("chatgpt"));
        assert_eq!(desktop.harness_pid, None, "Codex has no pid variable; scan finds codex.exe");
        let cli = Owner::from_env(&env(&[("CODEX_THREAD_ID", "0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f")]));
        assert_eq!(cli.app(), Some("Codex CLI"));
        assert_eq!(cli.app_word(), Some("codex"));
    }

    #[test]
    fn nested_agents_use_the_nearer_harness() {
        // A Claude session ran Codex: Codex's commands carry both tags.
        let mut o = Owner::from_env(&env(&[
            ("CLAUDE_CODE_SESSION_ID", "5f3c1e2a-7d41-4b8e-9a6c-1e2f3a4b5c6d"),
            ("CODEX_THREAD_ID", "0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f"),
        ]));
        assert_eq!(o.harness(), Some(Harness::Claude), "without the process tree, Claude is assumed");
        o.nearest = Some(Harness::Codex);
        assert_eq!(o.harness(), Some(Harness::Codex));
        assert_eq!(o.session(), Some("0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f"));
    }

    #[test]
    fn short_ids() {
        assert_eq!(short("5f3c1e2a-7d41-4b8e-9a6c-1e2f3a4b5c6d"), "5f3c1e2a");
        assert_eq!(short("0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f"), "3c4d5e6f");
        assert_eq!(short("abc"), "abc");
    }
}
