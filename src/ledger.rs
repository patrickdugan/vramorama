//! The lease ledger: one JSON file per lease in a shared directory, changed only while holding the
//! ledger lock (see `run.rs`). A lease reserves VRAM for a job from the moment it is admitted until
//! the job and every process that inherited its `VRAMORAMA_LEASE` tag have exited.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::json::{self, Value, obj};

pub const LEASE_VAR: &str = "VRAMORAMA_LEASE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Queued behind earlier leases or waiting for memory.
    Waiting,
    /// Admitted: its reservation counts against the card.
    Admitted,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub id: String,
    pub state: State,
    pub vram_mib: u64,
    /// Owner key at request time (label, `claude:<id>`, or agent), for display.
    pub owner: Option<String>,
    pub cmd: String,
    pub cwd: Option<String>,
    /// The `vramorama run` process holding the request.
    pub runner_pid: u32,
    pub runner_started: Option<u64>,
    /// The job, once started.
    pub job_pid: Option<u32>,
    pub job_started: Option<u64>,
    /// Unix seconds.
    pub created: f64,
    pub admitted_at: Option<f64>,
}

impl Lease {
    pub fn to_json(&self) -> Value {
        obj([
            ("id", self.id.as_str().into()),
            ("state", if self.state == State::Admitted { "admitted" } else { "waiting" }.into()),
            ("vram_mib", self.vram_mib.into()),
            ("owner", self.owner.clone().into()),
            ("cmd", self.cmd.as_str().into()),
            ("cwd", self.cwd.clone().into()),
            ("runner_pid", self.runner_pid.into()),
            ("runner_started", self.runner_started.map(|t| t.to_string()).into()),
            ("job_pid", self.job_pid.into()),
            ("job_started", self.job_started.map(|t| t.to_string()).into()),
            ("created", self.created.into()),
            ("admitted_at", self.admitted_at.into()),
        ])
    }

    pub fn from_json(v: &Value) -> Option<Lease> {
        let ft = |k: &str| v.str(k).and_then(|s| s.parse::<u64>().ok());
        Some(Lease {
            id: v.str("id")?.to_string(),
            state: match v.str("state")? {
                "admitted" => State::Admitted,
                "waiting" => State::Waiting,
                _ => return None,
            },
            vram_mib: v.f64("vram_mib")? as u64,
            owner: v.str("owner").map(str::to_string),
            cmd: v.str("cmd").unwrap_or_default().to_string(),
            cwd: v.str("cwd").map(str::to_string),
            runner_pid: v.f64("runner_pid")? as u32,
            runner_started: ft("runner_started"),
            job_pid: v.f64("job_pid").map(|p| p as u32),
            job_started: ft("job_started"),
            created: v.f64("created")?,
            admitted_at: v.f64("admitted_at"),
        })
    }
}

/// `9G`, `9GB`, `9GiB`, `9.5g`, `9000M`, `9000MiB`, or a bare number of MiB.
pub fn parse_size_mib(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_lowercase();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("not a size: {s}"))?;
    let scale = match unit.trim() {
        "" | "m" | "mb" | "mib" => 1.0,
        "g" | "gb" | "gib" => 1024.0,
        _ => return Err(format!("unknown unit in {s}; use M or G")),
    };
    let mib = (n * scale).round();
    if !(1.0..=1_048_576.0).contains(&mib) {
        return Err(format!("size out of range: {s}"));
    }
    Ok(mib as u64)
}

/// Memory admitted leases have reserved but their processes have not allocated yet (a model
/// still loading). Counting it is what stops a second job starting on memory the first is about
/// to take.
pub fn outstanding(leases: &[Lease], usage: &HashMap<String, u64>) -> u64 {
    leases
        .iter()
        .filter(|l| l.state == State::Admitted)
        .map(|l| l.vram_mib.saturating_sub(usage.get(&l.id).copied().unwrap_or(0)))
        .sum()
}

/// MiB a new admission may take: card total minus what is in use (by anyone, leased or not),
/// minus outstanding reservations, minus headroom. Negative when over-committed.
pub fn available(total: u64, used: u64, outstanding: u64, headroom: u64) -> i64 {
    total as i64 - used as i64 - outstanding as i64 - headroom as i64
}

/// First come, first served: `me` goes when it fits and nothing that queued earlier is still
/// waiting. A big request at the head of the queue holds back smaller ones behind it, so it
/// cannot be starved.
pub fn may_admit(me: &Lease, leases: &[Lease], available: i64) -> bool {
    let ahead = leases.iter().any(|l| l.id != me.id && l.state == State::Waiting && earlier(l, me));
    !ahead && me.vram_mib as i64 <= available
}

pub fn queue_position(me: &Lease, leases: &[Lease]) -> usize {
    leases.iter().filter(|l| l.id != me.id && l.state == State::Waiting && earlier(l, me)).count()
}

fn earlier(a: &Lease, b: &Lease) -> bool {
    (a.created, &a.id) < (b.created, &b.id)
}

/// `%LOCALAPPDATA%\vramorama`, or `VRAMORAMA_HOME` when set.
pub fn dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("VRAMORAMA_HOME") {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("vramorama"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".vramorama")))
}

pub fn leases_dir(root: &Path) -> PathBuf {
    root.join("leases")
}

/// Every readable lease, oldest first. Unreadable files are skipped (a writer may have died
/// mid-write; it only ever replaces whole files by rename, so this is rare).
pub fn read_all(root: &Path) -> Vec<Lease> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(leases_dir(root)) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") {
            if let Some(l) =
                std::fs::read_to_string(&p).ok().and_then(|t| json::parse(&t).ok()).and_then(|v| Lease::from_json(&v))
            {
                out.push(l);
            }
        }
    }
    out.sort_by(|a, b| a.created.total_cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    out
}

pub fn write(root: &Path, lease: &Lease) -> Result<(), String> {
    let dir = leases_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{}.tmp", lease.id));
    let dst = dir.join(format!("{}.json", lease.id));
    std::fs::write(&tmp, lease.to_json().to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))
}

pub fn remove(root: &Path, id: &str) {
    std::fs::remove_file(leases_dir(root).join(format!("{id}.json"))).ok();
}

/// 8 random hex digits, from the standard library's per-process random hash keys.
pub fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.write_u32(std::process::id());
    format!("{:08x}", h.finish() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(id: &str, state: State, vram: u64, created: f64) -> Lease {
        Lease {
            id: id.into(),
            state,
            vram_mib: vram,
            owner: Some("claude:5f3c1e2a".into()),
            cmd: r#"python.exe -u train.py --lr 1e-4 "a b""#.into(),
            cwd: None,
            runner_pid: 100,
            runner_started: Some(133_000_000_000_000_000),
            job_pid: None,
            job_started: None,
            created,
            admitted_at: None,
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size_mib("9G"), Ok(9216));
        assert_eq!(parse_size_mib("9.5gib"), Ok(9728));
        assert_eq!(parse_size_mib("9000M"), Ok(9000));
        assert_eq!(parse_size_mib(" 512 "), Ok(512));
        assert!(parse_size_mib("9T").is_err());
        assert!(parse_size_mib("0").is_err());
        assert!(parse_size_mib("lots").is_err());
    }

    #[test]
    fn json_round_trip() {
        let mut l = lease("ab12cd34", State::Admitted, 9216, 1_791_432_528.5);
        l.job_pid = Some(4242);
        l.job_started = Some(134_045_000_000_000_123);
        l.admitted_at = Some(1_791_432_530.0);
        let back = Lease::from_json(&json::parse(&l.to_json().to_string()).unwrap()).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn reservations_count_until_allocated() {
        let a = lease("a", State::Admitted, 9000, 1.0);
        let b = lease("b", State::Admitted, 2000, 2.0);
        let w = lease("w", State::Waiting, 4000, 3.0);
        let usage = HashMap::from([("a".to_string(), 6000u64), ("b".to_string(), 2500u64)]);
        // a has 3000 still to come; b is over its reservation (counts 0 outstanding, its real use is in `used`).
        assert_eq!(outstanding(&[a.clone(), b.clone(), w.clone()], &usage), 3000);
        // 16000 total, 8500 in use, 3000 outstanding, 512 headroom.
        assert_eq!(available(16000, 8500, 3000, 512), 3988);
        assert!(!may_admit(&w, &[a.clone(), b.clone(), w.clone()], 3988));
        assert!(may_admit(&w, &[a, b, w.clone()], 4000));
    }

    #[test]
    fn first_come_first_served() {
        let big = lease("big", State::Waiting, 12000, 1.0);
        let small = lease("small", State::Waiting, 1000, 2.0);
        let all = [big.clone(), small.clone()];
        assert!(!may_admit(&small, &all, 8000), "small must not jump the queue");
        assert!(!may_admit(&big, &all, 8000));
        assert!(may_admit(&big, &all, 12000));
        assert_eq!(queue_position(&small, &all), 1);
        assert_eq!(queue_position(&big, &all), 0);
    }

    #[test]
    fn ledger_files() {
        let root = std::env::temp_dir().join(format!("vramorama-ledger-{}", std::process::id()));
        let a = lease("aaaaaaaa", State::Waiting, 100, 2.0);
        let b = lease("bbbbbbbb", State::Admitted, 200, 1.0);
        write(&root, &a).unwrap();
        write(&root, &b).unwrap();
        std::fs::write(leases_dir(&root).join("junk.json"), "{not json").unwrap();
        let got = read_all(&root);
        assert_eq!(got.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["bbbbbbbb", "aaaaaaaa"]);
        remove(&root, "bbbbbbbb");
        assert_eq!(read_all(&root).len(), 1);
        std::fs::remove_dir_all(&root).ok();
        let id = new_id();
        assert_eq!(id.len(), 8);
        assert_ne!(id, new_id());
    }
}
