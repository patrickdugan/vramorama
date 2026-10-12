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
    /// System memory the process holds, when it was read.
    pub ram: Option<Ram>,
    /// The agent's own process (Claude Code's `claude.exe`), not a job it started.
    pub agent_proc: bool,
}

/// A process's system memory, in MiB.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ram {
    /// Private working set, as Task Manager's "Memory" column.
    pub private_mib: u64,
    pub ws_mib: u64,
    pub peak_ws_mib: u64,
    /// Private bytes committed (RAM or page file), what counts against the commit limit.
    pub commit_mib: u64,
}

impl Ram {
    pub fn of(m: &procs::Mem) -> Ram {
        Ram {
            private_mib: m.private_ws / MIB,
            ws_mib: m.ws / MIB,
            peak_ws_mib: m.peak_ws / MIB,
            commit_mib: m.commit / MIB,
        }
    }
}

/// Fill in each row's system memory from the kernel's process list (skipping recycled pids).
pub fn attach_ram(rows: &mut [Row], mem: &HashMap<u32, procs::Mem>) {
    for r in rows {
        r.ram = mem.get(&r.pid).filter(|m| r.started.is_none_or(|s| s == m.created)).map(Ram::of);
    }
}

/// Claude sessions that have a Claude Code process running now (from `ram_rows`' agent processes).
pub fn live_agent_sessions(ram_rows: &[Row]) -> std::collections::HashSet<String> {
    ram_rows.iter().filter(|r| r.agent_proc).filter_map(|r| r.owner.claude.clone()).collect()
}

/// A Claude session can outlive the `claude.exe` its jobs were started from: resuming it, or the
/// desktop app restarting, runs it in a new one. A job whose `CLAUDE_PID` has exited still belongs
/// to a live session while another Claude Code process runs that session.
pub fn revive(rows: &mut [Row], live: &std::collections::HashSet<String>) {
    for r in rows {
        let claude = matches!(r.owner.harness(), Some(Harness::Claude | Harness::Cowork));
        if claude && r.harness_live == Some(false) && r.owner.claude.as_ref().is_some_and(|c| live.contains(c)) {
            r.harness_live = Some(true);
        }
    }
}

/// Untagged processes are listed in the RAM view only from this size up: big enough for a job
/// started outside any session (or WSL's VM), small enough to leave out browser tabs.
pub const BIG_UNTAGGED_MIB: u64 = 1024;

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
                let label = known_process(&self.name).map_or_else(|| k.clone(), |(short, _)| short.to_string());
                (k, label)
            }
        }
    }
}

/// System processes that hold memory on behalf of many others, which no tag can say: a short
/// name and a longer description.
pub fn known_process(exe: &str) -> Option<(&'static str, &'static str)> {
    Some(match exe.to_ascii_lowercase().as_str() {
        "vmmemwsl" => ("WSL 2 virtual machine", "WSL 2 virtual machine (everything running inside WSL)"),
        "vmmem" => ("Hyper-V virtual machine", "Hyper-V virtual machine"),
        "memory compression" => ("Windows memory compression", "Windows memory compression (pages of other processes)"),
        _ => return None,
    })
}

/// What does not change while a process lives, cached by (pid, start time).
#[derive(Clone)]
struct Fixed {
    name: String,
    started: Option<u64>,
    cmd: Option<String>,
    cwd: Option<String>,
    owner: Owner,
    /// `origin` for untagged processes, worked out the first time a row needs it (it walks the
    /// process tree, which is not worth doing for every process on the machine).
    root: Option<Option<String>>,
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
            let fixed = self.fixed(u.pid, created, table, true);
            let mut r = self.row(u.pid, fixed);
            (r.luid, r.mib, r.shared_mib, r.util) = (u.luid, u.dedicated / MIB, u.shared / MIB, u.util);
            rows.push(r);
        }
        rows.sort_by(|a, b| b.mib.cmp(&a.mib).then(a.pid.cmp(&b.pid)));
        rows
    }

    /// Every process in an agent session (its tags say so), the agent's own Claude Code process
    /// (attributed to its session through the `CLAUDE_PID` its shells carry, or through the desktop
    /// app's session record), and untagged processes holding at least `BIG_UNTAGGED_MIB` of private
    /// memory, with their system memory. Largest first.
    pub fn ram_rows(&mut self, table: &HashMap<u32, Entry>, mem: &HashMap<u32, procs::Mem>) -> Vec<Row> {
        // Forget processes that have exited, so a long-running `gui` does not keep every shell
        // it ever saw.
        self.cache.retain(|(pid, created), _| mem.get(pid).is_some_and(|m| created.is_none_or(|c| c == m.created)));
        let mut fixed: Vec<(u32, &procs::Mem, Fixed)> = Vec::new();
        for (&pid, m) in mem {
            if pid <= 4 {
                continue; // System Idle Process and System
            }
            let f = self.fixed(pid, Some(m.created), table, false);
            fixed.push((pid, m, f));
        }
        // Claude Code's own process carries the desktop app's session id but not its own session
        // id; the shells it starts name it in CLAUDE_PID.
        let mut agents: HashMap<u32, Owner> = HashMap::new();
        for (_, _, f) in &fixed {
            if let (Some(hp), Some(Harness::Claude | Harness::Cowork)) = (f.owner.harness_pid, f.owner.harness()) {
                agents.entry(hp).or_insert_with(|| Owner { label: None, lease: None, ..f.owner.clone() });
            }
        }
        let mut rows = Vec::new();
        for (pid, m, mut f) in fixed {
            // Claude Code marks its own process AI_AGENT=claude-code_<version>_harness.
            let mut agent_proc = f.owner.agent.as_deref().is_some_and(|a| a.ends_with("_harness"));
            if f.owner.claude.is_none() && f.owner.codex.is_none() {
                if let Some(o) = agents.get(&pid) {
                    f.owner = o.clone();
                    agent_proc = true;
                } else if agent_proc {
                    // A Claude Code process with no shell running: its desktop session record names it.
                    if let Some(d) = f.owner.claude_host.as_deref().and_then(|h| self.titles.by_host(h)) {
                        f.owner.claude = d.cli_session;
                        f.owner.claude_entry = Some(if d.cowork { "local-agent" } else { "claude-desktop" }.into());
                    }
                }
            }
            if !(f.owner.is_tagged() || agent_proc) && m.private_ws < BIG_UNTAGGED_MIB * MIB {
                continue;
            }
            if !f.owner.is_tagged() && f.root.is_none() {
                f = self.fixed(pid, Some(m.created), table, true);
            }
            let mut r = self.row(pid, f);
            r.ram = Some(Ram::of(m));
            r.agent_proc = agent_proc;
            if agent_proc {
                r.harness_live = Some(true);
            }
            rows.push(r);
        }
        let live = live_agent_sessions(&rows);
        revive(&mut rows, &live);
        rows.sort_by(|a, b| {
            let p = |r: &Row| r.ram.map_or(0, |m| m.private_mib);
            p(b).cmp(&p(a)).then(a.pid.cmp(&b.pid))
        });
        rows
    }

    fn row(&mut self, pid: u32, f: Fixed) -> Row {
        let harness_live = f.owner.harness_pid.and_then(|hp| procs::alive(hp, f.started));
        Row {
            pid,
            luid: 0,
            mib: 0,
            shared_mib: 0,
            util: None,
            name: f.name,
            started: f.started,
            cmd: f.cmd,
            cwd: f.cwd,
            title: self.titles.title(&f.owner),
            owner: f.owner,
            harness_live,
            root: f.root.flatten(),
            note: f.note,
            ram: None,
            agent_proc: false,
        }
    }

    /// The cached facts about a process, read on first sight; `root` also works out where an
    /// untagged process's launch chain starts.
    fn fixed(&mut self, pid: u32, created: Option<u64>, table: &HashMap<u32, Entry>, root: bool) -> Fixed {
        let f = self.cache.entry((pid, created)).or_insert_with(|| {
            let d = procs::details(pid);
            let mut owner = d.env.as_deref().map(Owner::from_env).unwrap_or_default();
            if owner.codex.is_some() {
                resolve_harness(&mut owner, pid, table);
            }
            Fixed {
                name: table.get(&pid).map(|e| e.exe.clone()).unwrap_or_else(|| "?".into()),
                started: d.created,
                cmd: d.cmdline,
                cwd: d.cwd,
                root: owner.is_tagged().then_some(None),
                owner,
                note: d.note,
            }
        });
        if root && f.root.is_none() {
            f.root = Some(origin(pid, table));
        }
        f.clone()
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
        ("ram_mib", r.ram.map(|m| m.private_mib).into()),
        ("ws_mib", r.ram.map(|m| m.ws_mib).into()),
        ("commit_mib", r.ram.map(|m| m.commit_mib).into()),
        ("agent_proc", r.agent_proc.into()),
    ])
}

/// System memory, in MiB, for the JSON outputs.
pub fn system_memory_json(s: &procs::SysMem) -> Value {
    obj([
        ("total_mib", (s.total / MIB).into()),
        ("used_mib", ((s.total - s.avail.min(s.total)) / MIB).into()),
        ("available_mib", (s.avail / MIB).into()),
        ("commit_mib", ((s.commit_limit - s.commit_avail.min(s.commit_limit)) / MIB).into()),
        ("commit_limit_mib", (s.commit_limit / MIB).into()),
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
