//! Supervisors: the process above a memory hog that starts it again when it dies.
//!
//! Stopping a 9 GiB model server does nothing when the Python driver that launched it restarts
//! it a moment later. What frees the RAM is stopping the *tree*, from the topmost process that
//! belongs to the job down. This module finds that process, and says whether anything on the way
//! up is a real supervisor or only a launcher (a `cmd /c` wrapper, a shell, `vramorama run`).

use std::collections::{HashMap, HashSet};

/// A process, as far as the process tree is concerned.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    /// Creation time (FILETIME). A parent that started after its child is a recycled pid.
    pub created: Option<u64>,
    pub ram_mib: u64,
}

/// What stopping a process for good takes.
#[derive(Clone, Debug, PartialEq)]
pub struct Supervision {
    /// The process to stop, with its whole tree, so nothing is left to start the job again.
    pub root: u32,
    /// From the process up to `root`, both included.
    pub chain: Vec<u32>,
    /// Processes above it, inside the job, that are more than a launcher: they can restart it.
    pub supervisors: Vec<u32>,
    /// `root` and every live process below it.
    pub tree: Vec<u32>,
    /// Private working set of the whole tree.
    pub tree_ram_mib: u64,
}

impl Supervision {
    /// Something above the process can start it again, so stopping it alone is futile.
    pub fn supervised(&self) -> bool {
        !self.supervisors.is_empty()
    }
}

fn exe_stem(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    n.strip_suffix(".exe").map(str::to_string).unwrap_or(n)
}

const SHELLS: &[&str] = &["cmd", "powershell", "pwsh", "bash", "sh", "zsh", "nu", "wsl"];

/// Processes that start a job without managing it: shells, and vramorama's own `run`.
fn is_launcher_name(stem: &str) -> bool {
    SHELLS.contains(&stem) || stem == "vramorama" || stem == "conhost"
}

/// Whether a shell was started to run one command (`cmd /c`, `pwsh -Command`, `bash -c`) rather
/// than to be typed into. The first is part of the job; the second is somebody's terminal.
fn runs_one_command(stem: &str, cmdline: &str) -> bool {
    let c = cmdline.to_ascii_lowercase();
    let has = |flags: &[&str]| c.split_whitespace().skip(1).any(|t| flags.contains(&t.trim_matches('"')));
    match stem {
        "cmd" => has(&["/c", "/r"]),
        "powershell" | "pwsh" => has(&["-c", "-command", "-f", "-file", "-ec", "-encodedcommand"]),
        "bash" | "sh" | "zsh" => has(&["-c"]),
        _ => false,
    }
}

/// Parents that are never part of a job: the system, terminals, and the agent apps themselves.
/// Stopping any of these would take far more than the job with it.
fn is_boundary(stem: &str, cmdline: Option<&str>) -> bool {
    match stem {
        "system" | "smss" | "csrss" | "wininit" | "winlogon" | "services" | "lsass" | "svchost" | "explorer"
        | "vmwp" | "vmcompute" | "vmms" | "wslservice" | "spoolsv" | "msmpeng" | "wmiprvse" | "taskhostw"
        | "sihost" | "fontdrvhost" | "dwm" | "windowsterminal" | "openconsole" | "code" | "cursor" | "claude"
        | "codex" | "chatgpt" | "electron" | "taskeng" | "taskmgr" | "wslhost" | "wsl" | "init" => true,
        s if SHELLS.contains(&s) => !cmdline.is_some_and(|c| runs_one_command(s, c)),
        _ => false,
    }
}

/// A parent whose creation time is later than its child's is a recycled pid, not the parent.
fn genuine_parent(child: &Node, parent: &Node) -> bool {
    parent.pid != child.pid && !matches!((parent.created, child.created), (Some(p), Some(c)) if p > c)
}

/// `pid` and every ancestor of it, which a stop command must never include (the caller's own shell).
pub fn lineage(pid: u32, nodes: &HashMap<u32, Node>) -> HashSet<u32> {
    let mut out = HashSet::from([pid]);
    let mut cur = pid;
    while let Some(parent) = nodes.get(&cur).and_then(|n| nodes.get(&n.ppid)) {
        if !out.insert(parent.pid) {
            break;
        }
        cur = parent.pid;
    }
    out
}

/// Where stopping `pid` has to start. `protect` names processes the walk must stop below (the
/// caller's own lineage). `cmd_of` reads a process's command line; it is only asked about shells,
/// to tell a `cmd /c` wrapper from somebody's terminal, and about a parent with the same name,
/// to tell a venv's launcher shim (same command line) from a driver (a different one).
pub fn supervision(
    pid: u32,
    nodes: &HashMap<u32, Node>,
    protect: &HashSet<u32>,
    cmd_of: &dyn Fn(u32) -> Option<String>,
) -> Supervision {
    let mut chain = vec![pid];
    let mut supervisors = Vec::new();
    let mut cur = pid;
    while let Some(node) = nodes.get(&cur) {
        let Some(parent) = nodes.get(&node.ppid) else { break };
        if chain.len() > 64 || !genuine_parent(node, parent) || protect.contains(&parent.pid) {
            break;
        }
        let stem = exe_stem(&parent.name);
        let same_name = stem == exe_stem(&node.name);
        let cmdline = if SHELLS.contains(&stem.as_str()) || same_name { cmd_of(parent.pid) } else { None };
        if is_boundary(&stem, cmdline.as_deref()) {
            break;
        }
        // A venv's python.exe starts the real python.exe with the same command line: a shim,
        // not a supervisor. A parent with the same name and another command line is a driver.
        let shim = same_name && cmdline.is_some() && cmdline == cmd_of(node.pid);
        if !is_launcher_name(&stem) && !shim && !(same_name && cmdline.is_none()) {
            supervisors.push(parent.pid);
        }
        chain.push(parent.pid);
        cur = parent.pid;
    }
    let root = *chain.last().unwrap();
    let tree = descendants(root, nodes);
    let tree_ram_mib = tree.iter().filter_map(|p| nodes.get(p)).map(|n| n.ram_mib).sum();
    Supervision { root, chain, supervisors, tree, tree_ram_mib }
}

/// `root` and every live process below it, parents before children.
pub fn descendants(root: u32, nodes: &HashMap<u32, Node>) -> Vec<u32> {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for n in nodes.values() {
        let genuine = nodes.get(&n.ppid).is_some_and(|p| genuine_parent(n, p));
        if genuine && n.pid != n.ppid {
            kids.entry(n.ppid).or_default().push(n.pid);
        }
    }
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() && out.len() < 4096 {
        if let Some(k) = kids.get(&out[i]) {
            let mut k = k.clone();
            k.sort_unstable();
            for p in k {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        i += 1;
    }
    out
}

/// The command that stops a whole tree. vramorama prints it; whoever runs it decides.
pub fn kill_tree_command(root: u32) -> String {
    format!("taskkill /PID {root} /T /F")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(pid: u32, ppid: u32, name: &str, created: u64, ram_mib: u64) -> (u32, Node) {
        (pid, Node { pid, ppid, name: name.into(), created: Some(created), ram_mib })
    }

    /// The jam that prompted this module: wmiprvse > cmd > vramorama > cmd > python > llama-server.
    fn jam() -> HashMap<u32, Node> {
        HashMap::from([
            node(1, 0, "wmiprvse.exe", 1, 20),
            node(23168, 1, "cmd.exe", 10, 5),
            node(67004, 23168, "vramorama.exe", 11, 10),
            node(71172, 67004, "cmd.exe", 12, 5),
            node(18852, 71172, "python.exe", 20, 60),
            node(34564, 18852, "llama-server.exe", 30, 9017),
            node(42096, 18852, "python.exe", 31, 100),
        ])
    }

    fn cmds(pid: u32) -> Option<String> {
        match pid {
            23168 => Some(r#"cmd /c ""C:\vramorama.exe" run -- cmd /c job.cmd""#.into()),
            71172 => Some(r#""cmd" /c C:\job.cmd"#.into()),
            _ => None,
        }
    }

    #[test]
    fn a_respawning_model_server_is_stopped_from_the_top_of_its_job() {
        let s = supervision(34564, &jam(), &HashSet::new(), &cmds);
        assert_eq!(s.chain, vec![34564, 18852, 71172, 67004, 23168]);
        assert_eq!(s.root, 23168);
        assert_eq!(s.supervisors, vec![18852]);
        assert!(s.supervised());
        assert_eq!(s.tree, vec![23168, 67004, 71172, 18852, 34564, 42096]);
        assert_eq!(s.tree_ram_mib, 5 + 10 + 5 + 60 + 9017 + 100);
        assert_eq!(kill_tree_command(s.root), "taskkill /PID 23168 /T /F");
    }

    #[test]
    fn a_launcher_alone_is_not_a_supervisor() {
        let nodes = HashMap::from([
            node(1, 0, "claude.exe", 1, 200),
            node(2, 1, "pwsh.exe", 2, 80),
            node(3, 2, "python.exe", 3, 2000),
        ]);
        let s =
            supervision(3, &nodes, &HashSet::new(), &|p| (p == 2).then(|| "pwsh -NoProfile -Command train.py".into()));
        assert_eq!(s.root, 2, "the per-command shell is part of the job; the agent is not");
        assert!(!s.supervised());
    }

    #[test]
    fn somebodys_terminal_is_a_boundary() {
        let nodes = HashMap::from([node(1, 0, "cmd.exe", 1, 5), node(2, 1, "python.exe", 2, 900)]);
        let s = supervision(2, &nodes, &HashSet::new(), &|_| Some("cmd.exe".into()));
        assert_eq!(s.root, 2);
        let unreadable = supervision(2, &nodes, &HashSet::new(), &|_| None);
        assert_eq!(unreadable.root, 2, "when the shell cannot be read, assume it is a terminal");
    }

    #[test]
    fn a_venv_shim_is_not_a_supervisor_but_a_python_driver_is() {
        let nodes = HashMap::from([
            node(1, 0, "explorer.exe", 1, 100),
            node(2, 1, "python.exe", 2, 30),
            node(3, 2, "python.exe", 3, 40),
            node(4, 3, "python.exe", 4, 3000),
        ]);
        // 3 is the venv's real python started by shim 2 (same command line); 4 is a job it runs.
        let shim = |p: u32| match p {
            2 | 3 => Some("python.exe -u run_queue.py".to_string()),
            _ => Some("python.exe train.py".to_string()),
        };
        let s = supervision(4, &nodes, &HashSet::new(), &shim);
        assert_eq!(s.chain, vec![4, 3, 2]);
        assert_eq!(s.supervisors, vec![3], "the queue runner restarts train.py; the shim above it does not");
        let alone = supervision(3, &nodes, &HashSet::new(), &shim);
        assert!(!alone.supervised());
        let unreadable = supervision(4, &nodes, &HashSet::new(), &|_| None);
        assert!(!unreadable.supervised(), "without command lines, same-name parents are not guessed at");
    }

    #[test]
    fn the_callers_own_lineage_is_never_stopped() {
        let nodes = jam();
        let protect = lineage(71172, &nodes);
        assert!(protect.contains(&23168) && protect.contains(&67004));
        let s = supervision(34564, &nodes, &protect, &cmds);
        assert_eq!(s.root, 18852);
        assert!(!s.tree.contains(&71172));
    }

    #[test]
    fn a_recycled_parent_pid_ends_the_walk() {
        let nodes = HashMap::from([node(10, 0, "python.exe", 500, 10), node(11, 10, "llama-server.exe", 100, 9000)]);
        let s = supervision(11, &nodes, &HashSet::new(), &|_| None);
        assert_eq!(s.chain, vec![11]);
        assert_eq!(s.tree, vec![11]);
    }

    #[test]
    fn the_agent_and_the_system_are_never_inside_a_tree() {
        let nodes = HashMap::from([
            node(1, 0, "Claude.exe", 1, 400),
            node(2, 1, "node.exe", 2, 90),
            node(3, 2, "llama-server.exe", 3, 9000),
        ]);
        let s = supervision(3, &nodes, &HashSet::new(), &|_| None);
        assert_eq!(s.root, 2);
        assert_eq!(s.supervisors, vec![2]);
        assert!(!s.tree.contains(&1));
    }
}
