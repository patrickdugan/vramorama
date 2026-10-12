//! GPU memory per agent session, and per agent app: the view that answers "which session holds
//! how much?". A process belongs to the session whose tags it inherited (see `owner.rs`); a job's
//! `VRAMORAMA_OWNER` label names the job inside its session instead of hiding the session.

use std::collections::HashMap;

use crate::idle::IDLE_UTIL;
use crate::json::{Value, obj};
use crate::owner::Harness;
use crate::procs::Entry;
use crate::scan::Row;

/// The processes of one agent session (or, for processes started outside any session, of one
/// label, lease or untagged process).
#[derive(Clone, Debug)]
pub struct Group {
    pub key: String,
    /// `claude 5f3c1e2a`, `chatgpt 3c4d5e6f`, a label, or `untagged python.exe 8352`.
    pub label: String,
    pub harness: Option<Harness>,
    pub app: Option<&'static str>,
    pub session: Option<String>,
    pub title: Option<String>,
    pub live: Option<bool>,
    pub mib: u64,
    /// Memory held by processes doing no GPU work right now.
    pub idle_mib: u64,
    pub pids: Vec<u32>,
    /// Labels and leases of the jobs inside the session.
    pub jobs: Vec<String>,
}

pub fn group(rows: &[Row]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for r in rows {
        let (key, label) = r.session_group();
        let i = *at.entry(key.clone()).or_insert_with(|| {
            out.push(Group {
                key: key.clone(),
                label,
                harness: r.owner.harness(),
                app: r.owner.app(),
                session: r.owner.session().map(str::to_string),
                title: r.title.clone(),
                live: r.harness_live,
                mib: 0,
                idle_mib: 0,
                pids: Vec::new(),
                jobs: Vec::new(),
            });
            out.len() - 1
        });
        let g = &mut out[i];
        g.mib += r.mib;
        if r.util.is_some_and(|u| u < IDLE_UTIL) {
            g.idle_mib += r.mib;
        }
        if !g.pids.contains(&r.pid) {
            g.pids.push(r.pid);
        }
        // A session is live while any of its harness processes is.
        g.live = match (g.live, r.harness_live) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), _) | (_, Some(false)) => Some(false),
            _ => None,
        };
        if g.title.is_none() {
            g.title = r.title.clone();
        }
        let job = match (&r.owner.label, &r.owner.lease) {
            (Some(l), Some(id)) if r.owner.session().is_some() => Some(format!("{l} #{id}")),
            (Some(l), None) if r.owner.session().is_some() => Some(l.clone()),
            (None, Some(id)) => Some(format!("lease {id}")),
            _ => None,
        };
        if let Some(j) = job.filter(|j| !g.jobs.contains(j)) {
            g.jobs.push(j);
        }
    }
    out.sort_by(|a, b| b.mib.cmp(&a.mib).then_with(|| a.key.cmp(&b.key)));
    out
}

pub fn group_json(g: &Group) -> Value {
    obj([
        ("key", g.key.as_str().into()),
        ("label", g.label.as_str().into()),
        ("harness", g.harness.map(|h| h.tag()).into()),
        ("app", g.app.into()),
        ("session", g.session.clone().into()),
        ("title", g.title.clone().into()),
        ("live", g.live.into()),
        ("mib", g.mib.into()),
        ("idle_mib", g.idle_mib.into()),
        ("pids", Value::Arr(g.pids.iter().map(|&p| p.into()).collect())),
        ("jobs", Value::Arr(g.jobs.iter().map(|j| j.as_str().into()).collect())),
    ])
}

/// One agent app on this machine and what its sessions hold.
#[derive(Clone, Debug, PartialEq)]
pub struct App {
    pub name: &'static str,
    pub harness: Option<&'static str>,
    /// Whether the app (or for Cowork, its VM service) is running; `None` when not checked.
    pub running: Option<bool>,
    pub sessions: usize,
    pub mib: u64,
    pub note: Option<&'static str>,
}

const COWORK_NOTE: &str = "Cowork sessions run in a virtual machine with no GPU access, so they hold no VRAM here.";

/// Totals per agent app, in a fixed order, plus everything outside agent sessions. Apps with no
/// sessions are listed when they are running, so "the ChatGPT app holds nothing" is visible.
pub fn apps(groups: &[Group], table: &HashMap<u32, Entry>) -> Vec<App> {
    let running = |exe: &str| table.values().any(|e| e.exe.eq_ignore_ascii_case(exe));
    let sum = |name: &str| {
        let mine: Vec<&Group> = groups.iter().filter(|g| g.app == Some(name)).collect();
        (mine.len(), mine.iter().map(|g| g.mib).sum::<u64>())
    };
    let mut out = Vec::new();
    for (name, harness, exe, note) in [
        ("Claude Code", "claude", "claude.exe", None),
        ("Claude Cowork", "cowork", "cowork-svc.exe", Some(COWORK_NOTE)),
        ("ChatGPT (Codex)", "codex", "chatgpt.exe", None),
        ("Codex CLI", "codex", "codex.exe", None),
    ] {
        let (sessions, mib) = sum(name);
        let up = running(exe);
        // codex.exe also runs inside the ChatGPT app; list the CLI only when it has sessions.
        if sessions == 0 && (!up || name == "Codex CLI") {
            continue;
        }
        out.push(App { name, harness: Some(harness), running: Some(up), sessions, mib, note });
    }
    let other: Vec<&Group> = groups.iter().filter(|g| g.app.is_none()).collect();
    if !other.is_empty() {
        out.push(App {
            name: "Outside agent sessions",
            harness: None,
            running: None,
            sessions: other.len(),
            mib: other.iter().map(|g| g.mib).sum(),
            note: None,
        });
    }
    out
}

pub fn app_json(a: &App) -> Value {
    obj([
        ("name", a.name.into()),
        ("harness", a.harness.into()),
        ("running", a.running.into()),
        ("sessions", (a.sessions as u64).into()),
        ("mib", a.mib.into()),
        ("note", a.note.into()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner::Owner;

    fn row(pid: u32, mib: u64, util: f64, owner: Owner, live: Option<bool>) -> Row {
        Row {
            pid,
            luid: 1,
            mib,
            shared_mib: 0,
            util: Some(util),
            name: "python.exe".into(),
            started: None,
            cmd: None,
            cwd: None,
            owner,
            title: None,
            harness_live: live,
            root: None,
            note: None,
        }
    }

    fn claude(id: &str) -> Owner {
        Owner { claude: Some(id.into()), harness_pid: Some(1), ..Default::default() }
    }

    #[test]
    fn labelled_jobs_count_against_their_session() {
        let mut job = claude("a91d07c4-2b3e-4f5a-8c9d-0e1f2a3b4c5d");
        job.label = Some("lr-sweep".into());
        job.lease = Some("234228b0".into());
        let mut helper = claude("a91d07c4-2b3e-4f5a-8c9d-0e1f2a3b4c5d");
        helper.lease = Some("234228b0".into());
        let codex = Owner {
            codex: Some("0190c3a1-7b2e-7c4d-9e8f-1a2b3c4d5e6f".into()),
            codex_desktop: true,
            ..Default::default()
        };
        let rows = vec![
            row(10, 9000, 95.0, job, Some(true)),
            row(11, 400, 0.0, helper, Some(true)),
            row(20, 6000, 0.0, codex, None),
            row(30, 60, 0.0, Owner::default(), None),
            row(40, 100, 0.0, Owner { label: Some("eval-server".into()), ..Default::default() }, None),
        ];
        let g = group(&rows);
        assert_eq!(
            g.iter().map(|g| g.key.as_str()).collect::<Vec<_>>(),
            ["claude:a91d07c4", "codex:3c4d5e6f", "eval-server", "untagged python.exe 30"]
        );
        let s = &g[0];
        assert_eq!((s.mib, s.idle_mib, s.pids.len()), (9400, 400, 2));
        assert_eq!(s.jobs, ["lr-sweep #234228b0", "lease 234228b0"]);
        assert_eq!((s.label.as_str(), s.app, s.live), ("claude a91d07c4", Some("Claude Code"), Some(true)));
        assert_eq!((g[1].label.as_str(), g[1].app), ("chatgpt 3c4d5e6f", Some("ChatGPT (Codex)")));
        assert!(g[2].jobs.is_empty(), "a label outside a session is the group itself");

        let table = HashMap::from([
            (1, Entry { ppid: 0, exe: "claude.exe".into() }),
            (2, Entry { ppid: 0, exe: "ChatGPT.exe".into() }),
            (3, Entry { ppid: 0, exe: "cowork-svc.exe".into() }),
            (4, Entry { ppid: 2, exe: "codex.exe".into() }),
        ]);
        let a = apps(&g, &table);
        let names: Vec<&str> = a.iter().map(|a| a.name).collect();
        assert_eq!(names, ["Claude Code", "Claude Cowork", "ChatGPT (Codex)", "Outside agent sessions"]);
        assert_eq!((a[0].sessions, a[0].mib), (1, 9400));
        assert_eq!((a[1].sessions, a[1].running, a[1].note.is_some()), (0, Some(true), true));
        assert_eq!((a[2].sessions, a[2].mib), (1, 6000));
        assert_eq!((a[3].sessions, a[3].mib), (2, 160));
        // Nothing running and nothing held: the app is not listed.
        assert!(apps(&g[2..], &HashMap::new()).iter().all(|a| a.harness.is_none()));
    }
}
