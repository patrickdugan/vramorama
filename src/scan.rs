//! Join GPU counter samples with process details and owners into display rows.

use std::collections::HashMap;

use crate::gpu::{Adapter, Sample};
use crate::json::{Value, obj};
use crate::owner::{Harness, Owner, short};
use crate::parse::luid_string;
use crate::procs::{self, Entry};
use crate::sessions::Titles;

const MIB: u64 = 1 << 20;

#[derive(Clone)]
pub struct Row {
    pub pid: u32,
    pub luid: u64,
    pub mib: u64,
    /// Shared (system) memory mapped for the adapter; see `spill.rs`.
    pub shared_mib: u64,
    pub util: Option<f64>,
    pub name: String,
    pub started: Option<u64>,
    pub cmd: Option<String>,
    pub cwd: Option<String>,
    pub owner: Owner,
    /// The agent session's title, as its app shows it.
    pub title: Option<String>,
    /// Whether the agent harness that launched the job (Claude Code, Codex) is still running.
    pub harness_live: Option<bool>,
    /// For untagged processes, where the launch chain starts (see `origin`).
    pub root: Option<String>,
    pub note: Option<&'static str>,
}

impl Row {
    /// Agent-session group: key and short label (`claude 5f3c1e2a`), or the job's owner, or for
    /// untagged processes the process itself.
    pub fn session_group(&self) -> (String, String) {
        if let (Some(k), Some(word), Some(id)) =
            (self.owner.agent_session_key(), self.owner.app_word(), self.owner.session())
        {
            return (k, format!("{word} {}", short(id)));
        }
        match self.owner.session_key() {
            Some(k) => (k.clone(), k),
            None => {
                let k = format!("untagged {} {}", self.name, self.pid);
                (k.clone(), k)
            }
        }
    }
}

/// What does not change while a process lives, cached by (pid, start time).
#[derive(Clone)]
struct Fixed {
    name: String,
    started: Option<u64>,
    cmd: Option<String>,
    cwd: Option<String>,
    owner: Owner,
    root: Option<String>,
    note: Option<&'static str>,
}

#[derive(Default)]
pub struct Scanner {
    cache: HashMap<(u32, Option<u64>), Fixed>,
    pub titles: Titles,
}

impl Scanner {
    pub fn rows(&mut self, sample: &Sample, table: &HashMap<u32, Entry>, luids: &[u64], min_mib: u64) -> Vec<Row> {
        let mut rows = Vec::new();
        for u in &sample.procs {
            if !luids.contains(&u.luid) || u.dedicated < min_mib * MIB {
                continue;
            }
            let created = procs::created(u.pid);
            let fixed = match self.cache.get(&(u.pid, created)) {
                Some(f) => f.clone(),
                None => {
                    let f = self.fixed(u.pid, table);
                    self.cache.insert((u.pid, created), f.clone());
                    f
                }
            };
            let harness_live = fixed.owner.harness_pid.and_then(|hp| procs::alive(hp, fixed.started));
            let title = self.titles.title(&fixed.owner);
            rows.push(Row {
                pid: u.pid,
                luid: u.luid,
                mib: u.dedicated / MIB,
                shared_mib: u.shared / MIB,
                util: u.util,
                name: fixed.name,
                started: fixed.started,
                cmd: fixed.cmd,
                cwd: fixed.cwd,
                owner: fixed.owner,
                title,
                harness_live,
                root: fixed.root,
                note: fixed.note,
            });
        }
        rows.sort_by(|a, b| b.mib.cmp(&a.mib).then(a.pid.cmp(&b.pid)));
        rows
    }

    fn fixed(&mut self, pid: u32, table: &HashMap<u32, Entry>) -> Fixed {
        let d = procs::details(pid);
        let mut owner = d.env.as_deref().map(Owner::from_env).unwrap_or_default();
        if owner.codex.is_some() {
            resolve_harness(&mut owner, pid, table);
        }
        let root = if owner.is_tagged() { None } else { origin(pid, table) };
        Fixed {
            name: table.get(&pid).map(|e| e.exe.clone()).unwrap_or_else(|| "?".into()),
            started: d.created,
            cmd: d.cmdline,
            cwd: d.cwd,
            owner,
            root,
            note: d.note,
        }
    }
}

/// Fill in what a Codex tag leaves out, from the process tree: the `codex.exe` whose exit ends
/// the thread (Codex exports no pid), whether the ChatGPT app runs it, and, when a Claude tag is
/// present too, which of the two agents is nearer (the one that ran the command).
pub fn resolve_harness(owner: &mut Owner, pid: u32, table: &HashMap<u32, Entry>) {
    let (chain, _) = procs::ancestry(pid, table);
    let exe = |p: &u32| table.get(p).map(|e| e.exe.to_ascii_lowercase()).unwrap_or_default();
    let codex = chain.iter().position(|p| exe(p) == "codex.exe");
    let claude = chain.iter().position(|p| owner.harness_pid == Some(*p) || exe(p) == "claude.exe");
    owner.nearest = match (claude, codex) {
        (Some(c), Some(x)) if c < x => Some(Harness::Claude),
        (_, Some(_)) => Some(Harness::Codex),
        (Some(_), None) => Some(Harness::Claude),
        (None, None) => None,
    };
    if owner.harness() == Some(Harness::Codex) {
        owner.harness_pid = codex.map(|i| chain[i]);
        if let Some(i) = codex {
            owner.codex_desktop |= chain[i + 1..].iter().any(|p| exe(p) == "chatgpt.exe");
        }
    }
}

/// Processes that host or launch jobs rather than being one: desktop, terminals, agent apps, and
/// the system launchers behind WMI and Task Scheduler.
pub const HOSTS: &[&str] = &[
    "explorer.exe",
    "windowsterminal.exe",
    "openconsole.exe",
    "conhost.exe",
    "claude.exe",
    "chatgpt.exe",
    "code.exe",
    "codex.exe",
    "svchost.exe",
    "services.exe",
    "wininit.exe",
    "wmiprvse.exe",
    "taskeng.exe",
    "taskhostw.exe",
];

pub fn is_host(exe: &str) -> bool {
    HOSTS.contains(&exe.to_ascii_lowercase().as_str())
}

/// Where an untagged process's launch chain starts: "via WmiPrvSE.exe 35976" when a host
/// launched it, "from cmd.exe 33612 (parent exited)" when the chain is cut.
fn origin(pid: u32, table: &HashMap<u32, Entry>) -> Option<String> {
    let (chain, gone) = procs::ancestry(pid, table);
    if let Some(&host) = chain.iter().find(|&&p| table.get(&p).is_some_and(|e| is_host(&e.exe))) {
        return Some(format!("via {} {host}", table[&host].exe));
    }
    let &top = chain.last()?;
    let exited = if gone.is_some() { " (parent exited)" } else { "" };
    Some(format!("from {} {top}{exited}", table[&top].exe))
}

/// Adapters to show: real (not software) ones with at least 1 GiB of dedicated memory, or every
/// adapter with `all`. Falls back to the adapters the counters mention when DXGI is unavailable.
pub fn pick_adapters(dxgi: Vec<Adapter>, sample: &Sample, all: bool) -> Vec<Adapter> {
    let mut out: Vec<Adapter> = dxgi.into_iter().filter(|a| all || (!a.software && a.dedicated >= 1 << 30)).collect();
    if out.is_empty() {
        let mut luids: Vec<u64> = sample.adapter_used.keys().copied().collect();
        luids.sort();
        out = luids
            .into_iter()
            .map(|luid| Adapter {
                luid,
                name: format!("adapter {}", luid_string(luid)),
                vendor: 0,
                dedicated: 0,
                software: false,
            })
            .collect();
    }
    out
}

/// Processes on `a` whose allocations spilled into system memory, with how much: (pid, MiB).
pub fn spills(a: &Adapter, sample: &Sample, rows: &[Row]) -> Vec<(u32, u64)> {
    let total = a.dedicated / MIB;
    let used = sample.adapter_used.get(&a.luid).copied().unwrap_or(0) / MIB;
    let mut out: Vec<(u32, u64)> = rows
        .iter()
        .filter(|r| r.luid == a.luid && crate::spill::spilled(r.shared_mib, total, used))
        .map(|r| (r.pid, r.shared_mib))
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.1));
    out
}

pub fn row_json(r: &Row) -> Value {
    obj([
        ("pid", r.pid.into()),
        ("mib", r.mib.into()),
        ("shared_mib", r.shared_mib.into()),
        ("util", r.util.map(|u| (u * 10.0).round() / 10.0).into()),
        ("name", r.name.as_str().into()),
        ("started", r.started.map(crate::sys::fmt_utc).into()),
        ("owner", r.owner.key().into()),
        ("session_key", r.owner.session_key().into()),
        ("label", r.owner.label.clone().into()),
        ("lease", r.owner.lease.clone().into()),
        ("harness", r.owner.harness().map(|h| h.tag()).into()),
        ("app", r.owner.app().into()),
        ("session", r.owner.session().map(str::to_string).into()),
        ("desktop_session", r.owner.claude_host.clone().into()),
        ("title", r.title.clone().into()),
        ("agent", r.owner.agent.clone().into()),
        ("harness_live", r.harness_live.into()),
        ("root", r.root.clone().into()),
        ("cwd", r.cwd.clone().into()),
        ("cmd", r.cmd.clone().into()),
        ("note", r.note.into()),
    ])
}

pub fn adapter_json(a: &Adapter, sample: &Sample, rows: &[Row]) -> Value {
    let mine: Vec<Value> = rows.iter().filter(|r| r.luid == a.luid).map(row_json).collect();
    obj([
        ("luid", luid_string(a.luid).into()),
        ("name", a.name.as_str().into()),
        ("vendor", format!("0x{:04X}", a.vendor).into()),
        ("total_mib", (a.dedicated / MIB).into()),
        ("used_mib", sample.adapter_used.get(&a.luid).map(|b| b / MIB).into()),
        ("shared_mib", sample.adapter_shared.get(&a.luid).map(|b| b / MIB).into()),
        ("spilled", spills_json(&spills(a, sample, rows))),
        ("procs", Value::Arr(mine)),
    ])
}

pub fn spills_json(spills: &[(u32, u64)]) -> Value {
    Value::Arr(spills.iter().map(|(pid, mib)| obj([("pid", (*pid).into()), ("shared_mib", (*mib).into())])).collect())
}
