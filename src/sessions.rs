//! What each agent app calls a session, and where it keeps the session's title.
//!
//! - Claude Code keeps its transcript at `~/.claude/projects/<project>/<session>.jsonl` (under
//!   `CLAUDE_CONFIG_DIR` instead when that is set). The Claude desktop app also keeps a record per
//!   Code-tab session in `claude-code-sessions/<account>/<org>/local_<id>.json`, with the title
//!   its sidebar shows, and one per Cowork session in `local-agent-mode-sessions/...`.
//! - Codex (the agent in the ChatGPT desktop app, and the Codex CLI) names threads in
//!   `~/.codex/session_index.jsonl` and keeps each transcript in
//!   `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`.
//!
//! The Claude desktop app is an MSIX package, so Windows redirects files it creates under
//! `%APPDATA%` to `%LOCALAPPDATA%\Packages\Claude_<publisher id>\LocalCache\Roaming`. Processes
//! inside the package (every shell a desktop session starts) see them at the usual path; everyone
//! else (a terminal, the ChatGPT app) has to look in the package folder. Both are read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::json;
use crate::owner::{Harness, Owner};
use crate::transcripts;

/// A Claude desktop session record.
#[derive(Clone, Debug, PartialEq)]
pub struct DesktopSession {
    /// The desktop app's id, `local_<uuid>` (`CLAUDE_CODE_HOST_SESSION_ID` in its shells).
    pub id: String,
    /// The Claude Code session id (`CLAUDE_CODE_SESSION_ID`).
    pub cli_session: Option<String>,
    pub title: Option<String>,
    pub archived: bool,
    pub cowork: bool,
}

/// The Claude desktop app's data folders: `%APPDATA%\Claude`, and the same folder inside each
/// installed Claude package.
pub fn claude_user_data_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        out.push(PathBuf::from(appdata).join("Claude"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        if let Ok(rd) = std::fs::read_dir(PathBuf::from(local).join("Packages")) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("Claude_") {
                    out.push(e.path().join("LocalCache").join("Roaming").join("Claude"));
                }
            }
        }
    }
    out.retain(|p| p.is_dir());
    out
}

/// `path`, and when it is under `%APPDATA%`, where the Claude package's redirection would have put it.
pub fn with_package_paths(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    let (Some(appdata), Some(local)) = (std::env::var_os("APPDATA"), std::env::var_os("LOCALAPPDATA")) else {
        return out;
    };
    let (s, a) = (path.to_string_lossy(), appdata.to_string_lossy());
    let a = a.trim_end_matches('\\');
    let under =
        s.get(..a.len()).is_some_and(|head| head.eq_ignore_ascii_case(a)) && s.as_bytes().get(a.len()) == Some(&b'\\');
    let Some(rest) = s.get(a.len() + 1..).filter(|r| under && !r.is_empty()) else { return out };
    if let Ok(rd) = std::fs::read_dir(PathBuf::from(local).join("Packages")) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("Claude_") {
                out.push(e.path().join("LocalCache").join("Roaming").join(rest));
            }
        }
    }
    out
}

/// Every session record under the given data folders, deduplicated by id.
pub fn read_desktop_sessions(dirs: &[PathBuf]) -> Vec<DesktopSession> {
    let mut out: Vec<DesktopSession> = Vec::new();
    for dir in dirs {
        for (kind, cowork) in [("claude-code-sessions", false), ("local-agent-mode-sessions", true)] {
            // <kind>/<account>/<org>/local_<id>.json
            for account in subdirs(&dir.join(kind)) {
                for org in subdirs(&account) {
                    let Ok(rd) = std::fs::read_dir(&org) else { continue };
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if !(name.starts_with("local_") && name.ends_with(".json")) {
                            continue;
                        }
                        let Some(s) = std::fs::read_to_string(e.path()).ok().and_then(|t| parse_record(&t, cowork))
                        else {
                            continue;
                        };
                        if !out.iter().any(|o| o.id == s.id) {
                            out.push(s);
                        }
                    }
                }
            }
        }
    }
    out
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()
}

pub fn parse_record(text: &str, cowork: bool) -> Option<DesktopSession> {
    let v = json::parse(text).ok()?;
    let nonempty = |k: &str| v.str(k).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    Some(DesktopSession {
        id: nonempty("sessionId")?,
        cli_session: nonempty("cliSessionId"),
        title: nonempty("title"),
        archived: matches!(v.get("isArchived"), Some(json::Value::Bool(true))),
        cowork,
    })
}

/// Codex's home: `CODEX_HOME` from the process, else `~/.codex`.
pub fn codex_home(from_env: Option<&str>) -> Option<PathBuf> {
    from_env.map(PathBuf::from).or_else(|| transcripts::home().map(|h| h.join(".codex")))
}

/// Thread id -> name from `session_index.jsonl` (later lines rename earlier ones).
pub fn parse_codex_index(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let Ok(v) = json::parse(line) else { continue };
        if let (Some(id), Some(name)) = (v.str("id"), v.str("thread_name").map(str::trim).filter(|n| !n.is_empty())) {
            out.insert(id.to_string(), name.to_string());
        }
    }
    out
}

/// The rollout (transcript) of a Codex thread: `<codex home>/sessions/YYYY/MM/DD/rollout-*-<thread>.jsonl`.
pub fn codex_rollout(thread: &str, codex_home_env: Option<&str>) -> Option<PathBuf> {
    if thread_from_rollout(thread) != Some(thread) {
        return None;
    }
    let suffix = format!("{thread}.jsonl");
    let mut days = Vec::new();
    for y in subdirs(&codex_home(codex_home_env)?.join("sessions")) {
        for m in subdirs(&y) {
            days.extend(subdirs(&m));
        }
    }
    days.sort();
    days.iter().rev().find_map(|d| {
        std::fs::read_dir(d).ok()?.flatten().map(|e| e.path()).find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("rollout-") && n.to_string_lossy().ends_with(&suffix))
        })
    })
}

/// The thread id at the end of a Codex rollout file name
/// (`rollout-2026-10-12T14-06-50-0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f`).
pub fn thread_from_rollout(stem: &str) -> Option<&str> {
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    let b = id.as_bytes();
    let dashes = [8, 13, 18, 23].iter().all(|&i| b[i] == b'-');
    let hex = b.iter().enumerate().all(|(i, c)| [8, 13, 18, 23].contains(&i) || c.is_ascii_hexdigit());
    (dashes && hex).then_some(id)
}

/// Session titles, re-read at most once a minute so that a title set after a session started
/// (Claude names sessions after their first turn) still shows up in a long-running `gui`.
#[derive(Default)]
pub struct Titles {
    desktop: Vec<DesktopSession>,
    codex: HashMap<PathBuf, HashMap<String, String>>,
    loaded: Option<Instant>,
    /// Titles read from Claude transcripts, for sessions with no desktop record (the CLI).
    transcript: HashMap<String, (Option<String>, Instant)>,
}

const REFRESH: Duration = Duration::from_secs(60);
const TRANSCRIPT_REFRESH: Duration = Duration::from_secs(300);

impl Titles {
    fn refresh(&mut self) {
        if self.loaded.is_some_and(|t| t.elapsed() < REFRESH) {
            return;
        }
        self.desktop = read_desktop_sessions(&claude_user_data_dirs());
        self.codex.clear();
        self.loaded = Some(Instant::now());
    }

    /// The desktop record of a Claude or Cowork session.
    pub fn desktop(&mut self, o: &Owner) -> Option<DesktopSession> {
        let cli = o.claude.as_deref()?;
        self.refresh();
        let by_host = o.claude_host.as_deref().and_then(|h| self.desktop.iter().find(|d| d.id == h));
        by_host.or_else(|| self.desktop.iter().find(|d| d.cli_session.as_deref() == Some(cli))).cloned()
    }

    /// What the session's app calls it.
    pub fn title(&mut self, o: &Owner) -> Option<String> {
        match o.harness()? {
            Harness::Codex => self.codex_title(o.codex.as_deref()?, o.codex_home.as_deref()),
            Harness::Claude | Harness::Cowork => {
                if let Some(t) = self.desktop(o).and_then(|d| d.title) {
                    return Some(t);
                }
                let id = o.claude.as_deref()?;
                if let Some((t, at)) = self.transcript.get(id) {
                    if at.elapsed() < TRANSCRIPT_REFRESH {
                        return t.clone();
                    }
                }
                let t = transcripts::session_title(id, o.claude_config.as_deref());
                self.transcript.insert(id.to_string(), (t.clone(), Instant::now()));
                t
            }
        }
    }

    pub fn codex_title(&mut self, thread: &str, codex_home_env: Option<&str>) -> Option<String> {
        self.refresh();
        let home = codex_home(codex_home_env)?;
        let names = self.codex.entry(home.clone()).or_insert_with(|| {
            std::fs::read_to_string(home.join("session_index.jsonl")).map(|t| parse_codex_index(&t)).unwrap_or_default()
        });
        names.get(thread).cloned()
    }

    /// A title for a transcript `trace` found, by harness name and session id.
    pub fn for_hit(&mut self, harness: &str, session: &str) -> Option<String> {
        match harness {
            "codex" => self.codex_title(session, None),
            _ => {
                self.refresh();
                self.desktop.iter().find(|d| d.cli_session.as_deref() == Some(session)).and_then(|d| d.title.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_records() {
        let root = std::env::temp_dir().join(format!("vramorama-desktop-{}", std::process::id()));
        let code = root.join("claude-code-sessions").join("acct").join("org");
        let cowork = root.join("local-agent-mode-sessions").join("acct").join("org");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::create_dir_all(&cowork).unwrap();
        std::fs::write(
            code.join("local_6e7f8a9b.json"),
            r#"{"sessionId":"local_6e7f8a9b","cliSessionId":"a91d07c4-2b3e","cwd":"C:\\work","title":"Reranker \"theory\"","isArchived":false}"#,
        )
        .unwrap();
        std::fs::write(
            cowork.join("local_77aa.json"),
            r#"{"sessionId":"local_77aa","cliSessionId":"9a8b","title":" "}"#,
        )
        .unwrap();
        std::fs::write(code.join("deleted_x"), "13 bytes here").unwrap();
        std::fs::write(code.join("local_broken.json"), "{").unwrap();
        // The same folder seen twice (inside and outside the MSIX package) is read once.
        let got = read_desktop_sessions(&[root.clone(), root.clone()]);
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(got.len(), 2);
        let s = got.iter().find(|s| s.id == "local_6e7f8a9b").unwrap();
        assert_eq!(s.cli_session.as_deref(), Some("a91d07c4-2b3e"));
        assert_eq!(s.title.as_deref(), Some("Reranker \"theory\""));
        assert!(!s.cowork && !s.archived);
        let c = got.iter().find(|s| s.id == "local_77aa").unwrap();
        assert!(c.cowork);
        assert_eq!(c.title, None, "a blank title is no title");
    }

    #[test]
    fn codex_index_later_lines_win() {
        let idx = concat!(
            r#"{"id":"0190b2f0-1111-7222-8333-444455556666","thread_name":"Sync state","updated_at":"2026-09-25T23:57:51Z"}"#,
            "\n",
            "garbage\n",
            r#"{"id":"0190b2f0-1111-7222-8333-444455556666","thread_name":"Sync the eval notes"}"#,
            "\n"
        );
        let m = parse_codex_index(idx);
        assert_eq!(m.len(), 1);
        assert_eq!(m["0190b2f0-1111-7222-8333-444455556666"], "Sync the eval notes");
    }

    #[test]
    fn rollout_names() {
        assert_eq!(
            thread_from_rollout("rollout-2026-10-12T14-06-50-0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f"),
            Some("0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f")
        );
        assert_eq!(thread_from_rollout("5f3c1e2a-0000"), None);
        assert_eq!(thread_from_rollout("rollout-2026-10-12T14-06-50-zzzzzzzz-7b2e-7c4d-9e8f-1a2b3c4d5e6f"), None);
    }
}
