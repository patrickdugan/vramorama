//! `vramorama run` and `vramorama leases`: wait for a VRAM lease, then start the job tagged with it.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::gpu::{self, Sampler};
use crate::ledger::{self, LEASE_VAR, Lease, State};
use crate::owner::{LABEL_VAR, Owner};
use crate::parse::{base64, duration, ellipsize, join_args, thousands};
use crate::procs;
use crate::scan::Scanner;

const MIB: u64 = 1 << 20;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SHARING_VIOLATION: i32 = 32;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const POLL: Duration = Duration::from_secs(2);

pub struct RunOpts {
    pub vram_mib: u64,
    pub owner: Option<String>,
    pub detach: bool,
    pub out: Option<String>,
    pub wait_secs: Option<f64>,
    pub settle_secs: f64,
    pub headroom_mib: u64,
    pub adapter: Option<String>,
    pub quiet: bool,
    pub argv: Vec<String>,
}

/// The ledger lock: the lock file opened with no sharing, so a second opener gets a sharing
/// violation. Windows releases it when the handle closes, including when the holder crashes.
struct Lock(#[allow(dead_code)] File);

fn lock(root: &Path) -> Result<Lock, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let path = root.join("ledger.lock");
    let start = Instant::now();
    loop {
        match OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(&path) {
            Ok(f) => return Ok(Lock(f)),
            Err(e)
                if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) && start.elapsed() < Duration::from_secs(30) =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
}

struct Card {
    luid: u64,
    name: String,
    total_mib: u64,
}

/// The adapter to budget: the one whose name contains `want`, else the one with the most
/// dedicated memory.
fn pick_card(want: Option<&str>) -> Result<Card, String> {
    let mut list: Vec<_> = gpu::adapters().into_iter().filter(|a| !a.software && a.dedicated > 0).collect();
    if let Some(w) = want {
        let w = w.to_ascii_lowercase();
        list.retain(|a| a.name.to_ascii_lowercase().contains(&w));
    }
    let a = list.into_iter().max_by_key(|a| a.dedicated).ok_or("no matching GPU adapter")?;
    Ok(Card { luid: a.luid, name: a.name, total_mib: a.dedicated / MIB })
}

/// Memory in use on the card, and how much of it each lease's processes hold.
struct View {
    used_mib: u64,
    by_lease: HashMap<String, u64>,
}

fn observe(sampler: &mut Sampler, scanner: &mut Scanner, card: &Card) -> View {
    let sample = sampler.sample();
    let table = procs::snapshot();
    let mut by_lease = HashMap::new();
    for r in scanner.rows(&sample, &table, &[card.luid], 1) {
        if let Some(id) = &r.owner.lease {
            *by_lease.entry(id.clone()).or_insert(0) += r.mib;
        }
    }
    View { used_mib: sample.adapter_used.get(&card.luid).copied().unwrap_or(0) / MIB, by_lease }
}

/// The process is running and is the one recorded (not a recycled pid). Unknown counts as yes,
/// which keeps a reservation rather than dropping it.
fn same_process(pid: u32, started: Option<u64>) -> bool {
    if procs::alive(pid, None) == Some(false) {
        return false;
    }
    match (started, procs::created(pid)) {
        (Some(want), Some(got)) => want == got,
        _ => true,
    }
}

/// A lease lives while its job runs, while any process tagged with it holds memory, and, before
/// the job starts, while the `run` process waiting for it is alive.
fn live(l: &Lease, by_lease: &HashMap<String, u64>) -> bool {
    if by_lease.get(&l.id).copied().unwrap_or(0) > 0 {
        return true;
    }
    match l.job_pid {
        Some(pid) => same_process(pid, l.job_started),
        None => same_process(l.runner_pid, l.runner_started),
    }
}

/// Drop dead leases from the ledger. Call with the lock held.
fn prune(root: &Path, by_lease: &HashMap<String, u64>) -> Vec<Lease> {
    let (keep, dead): (Vec<Lease>, Vec<Lease>) = ledger::read_all(root).into_iter().partition(|l| live(l, by_lease));
    for l in dead {
        ledger::remove(root, &l.id);
    }
    keep
}

fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Returns the job's exit code (attached), or 0 once a detached job has started.
pub fn run(o: &RunOpts) -> Result<i32, String> {
    let root = ledger::dir().ok_or("cannot find %LOCALAPPDATA% for the ledger")?;
    let card = pick_card(o.adapter.as_deref())?;
    if o.vram_mib + o.headroom_mib > card.total_mib {
        return Err(format!(
            "{} MiB plus {} MiB headroom can never fit on {} ({} MiB)",
            thousands(o.vram_mib),
            thousands(o.headroom_mib),
            card.name,
            thousands(card.total_mib)
        ));
    }
    let env: Vec<(String, String)> = std::env::vars().collect();
    let me_pid = std::process::id();
    let mut me = Lease {
        id: ledger::new_id(),
        state: State::Waiting,
        vram_mib: o.vram_mib,
        owner: o.owner.clone().or_else(|| Owner::from_env(&env).key()),
        cmd: join_args(&o.argv),
        cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        runner_pid: me_pid,
        runner_started: procs::created(me_pid),
        job_pid: None,
        job_started: None,
        created: unix_now(),
        admitted_at: None,
    };
    let mut sampler = Sampler::open()?;
    let mut scanner = Scanner::default();
    {
        let _l = lock(&root)?;
        ledger::write(&root, &me)?;
    }

    let start = Instant::now();
    let mut fits_since: Option<Instant> = None;
    let mut last_note = String::new();
    loop {
        let view = observe(&mut sampler, &mut scanner, &card);
        let waiting = {
            let _l = lock(&root)?;
            let leases = prune(&root, &view.by_lease);
            if !leases.iter().any(|l| l.id == me.id) {
                ledger::write(&root, &me)?; // removed by hand; put the request back
            }
            let outstanding = ledger::outstanding(&leases, &view.by_lease);
            let avail = ledger::available(card.total_mib, view.used_mib, outstanding, o.headroom_mib);
            if ledger::may_admit(&me, &leases, avail) {
                let since = *fits_since.get_or_insert_with(Instant::now);
                if since.elapsed().as_secs_f64() >= o.settle_secs {
                    me.state = State::Admitted;
                    me.admitted_at = Some(unix_now());
                    ledger::write(&root, &me)?;
                    None
                } else {
                    Some(format!(
                        "{} MiB free; confirming it stays free for {}s",
                        thousands(avail.max(0) as u64),
                        o.settle_secs
                    ))
                }
            } else {
                fits_since = None;
                let ahead = ledger::queue_position(&me, &leases);
                Some(format!(
                    "{} MiB available ({} in use, {} reserved by running jobs, {} headroom){}",
                    thousands(avail.max(0) as u64),
                    thousands(view.used_mib),
                    thousands(outstanding),
                    thousands(o.headroom_mib),
                    if ahead > 0 { format!("; {ahead} earlier request(s) queued") } else { String::new() }
                ))
            }
        };
        let Some(note) = waiting else { break };
        if !o.quiet && note != last_note {
            eprintln!("vramorama: lease {} waits for {} MiB on {}: {note}", me.id, thousands(o.vram_mib), card.name);
            last_note = note;
        }
        if o.wait_secs.is_some_and(|w| start.elapsed().as_secs_f64() > w) {
            let _l = lock(&root)?;
            ledger::remove(&root, &me.id);
            return Err(format!(
                "gave up after waiting {}s for {} MiB",
                o.wait_secs.unwrap_or(0.0),
                thousands(o.vram_mib)
            ));
        }
        std::thread::sleep(POLL);
    }
    if !o.quiet {
        eprintln!(
            "vramorama: lease {} admitted after {}; starting {}",
            me.id,
            duration(start.elapsed().as_secs_f64()),
            me.cmd
        );
    }

    let mut cmd = Command::new(&o.argv[0]);
    let mut tags = vec![(LEASE_VAR, me.id.clone())];
    if let Some(owner) = &o.owner {
        tags.push((LABEL_VAR, owner.clone()));
    }
    cmd.args(&o.argv[1..]).envs(tags.iter().map(|(k, v)| (*k, v.as_str())));
    let started = if o.detach {
        detach(&mut cmd, &o.argv, o.out.as_deref(), &tags)
    } else {
        cmd.spawn().map(Started::Attached).map_err(|e| e.to_string())
    };
    let started = match started {
        Ok(s) => s,
        Err(e) => {
            let _l = lock(&root)?;
            ledger::remove(&root, &me.id);
            return Err(format!("could not start {}: {e}", o.argv[0]));
        }
    };
    let pid = match &started {
        Started::Attached(c) => c.id(),
        Started::Detached { pid, .. } => *pid,
    };
    me.job_pid = Some(pid);
    me.job_started = procs::created(pid);
    {
        let _l = lock(&root)?;
        ledger::write(&root, &me)?;
    }
    match started {
        Started::Detached { pid, how } => {
            println!("lease {} pid {pid} ({how})", me.id);
            Ok(0)
        }
        Started::Attached(mut child) => {
            let status = child.wait().map_err(|e| e.to_string())?;
            let _l = lock(&root)?;
            ledger::remove(&root, &me.id);
            Ok(status.code().unwrap_or(1))
        }
    }
}

enum Started {
    Attached(Child),
    Detached { pid: u32, how: &'static str },
}

/// Start the job so it outlives this session: no console, its own process group, and outside the
/// caller's job object. Agent hosts often run shells in a job object that forbids breakaway; then
/// the job is started through WMI instead (its parent becomes the WMI host, outside any session),
/// passing the whole environment so the lease and session tags survive.
fn detach(cmd: &mut Command, argv: &[String], out: Option<&str>, tags: &[(&str, String)]) -> Result<Started, String> {
    let (stdout, stderr) = match out {
        Some(p) => {
            let f = OpenOptions::new().create(true).append(true).open(p).map_err(|e| format!("{p}: {e}"))?;
            (Stdio::from(f.try_clone().map_err(|e| e.to_string())?), Stdio::from(f))
        }
        None => (Stdio::null(), Stdio::null()),
    };
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Ok(c) => Ok(Started::Detached { pid: c.id(), how: "detached" }),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            // Close our handles on the log first: cmd.exe opens `>>` targets without write
            // sharing and would fail on the spot while we still hold them.
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let pid = wmi_launch(argv, out, tags)?;
            Ok(Started::Detached { pid, how: "started through WMI, outside this session" })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Runs in Windows PowerShell 5.1 (part of Windows). It inherits vramorama's environment plus the
/// job's tags and hands all of it to the new process: `EnvironmentVariables` replaces the
/// environment rather than adding to it.
const WMI_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$skip = @('VRAMORAMA_WMI_CMDLINE', 'VRAMORAMA_WMI_CWD')
$envList = [string[]](Get-ChildItem env: | Where-Object { $skip -notcontains $_.Name } | ForEach-Object { "$($_.Name)=$($_.Value)" })
$startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ EnvironmentVariables = $envList; ShowWindow = [uint16]0 }
$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $env:VRAMORAMA_WMI_CMDLINE; CurrentDirectory = $env:VRAMORAMA_WMI_CWD; ProcessStartupInformation = $startup }
"$($r.ReturnValue) $($r.ProcessId)"
"#;

fn wmi_launch(argv: &[String], out: Option<&str>, tags: &[(&str, String)]) -> Result<u32, String> {
    let line = join_args(argv);
    let line = match out {
        Some(p) => format!("cmd.exe /d /s /c \"{line} >> \"{p}\" 2>&1\""),
        None => line,
    };
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?.display().to_string();
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let ps = format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe");
    let script: Vec<u8> = WMI_SCRIPT.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let output = Command::new(&ps)
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &base64(&script)])
        .env("VRAMORAMA_WMI_CMDLINE", &line)
        .env("VRAMORAMA_WMI_CWD", &cwd)
        .envs(tags.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{ps}: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut words = text.split_whitespace();
    match (words.next(), words.next().and_then(|p| p.parse::<u32>().ok())) {
        (Some("0"), Some(pid)) => Ok(pid),
        _ => Err(format!("WMI launch failed: {} {}", text.trim(), String::from_utf8_lossy(&output.stderr).trim())),
    }
}

/// `vramorama leases`: the ledger and what a new request could get right now.
pub fn leases(json_out: bool, adapter: Option<&str>, headroom_mib: u64) -> Result<(), String> {
    let root = ledger::dir().ok_or("cannot find %LOCALAPPDATA% for the ledger")?;
    let card = pick_card(adapter)?;
    let mut sampler = Sampler::open()?;
    let view = observe(&mut sampler, &mut Scanner::default(), &card);
    let leases = {
        let _l = lock(&root)?;
        prune(&root, &view.by_lease)
    };
    let outstanding = ledger::outstanding(&leases, &view.by_lease);
    let avail = ledger::available(card.total_mib, view.used_mib, outstanding, headroom_mib);
    if json_out {
        use crate::json::{Value, obj};
        let list = leases
            .iter()
            .map(|l| {
                let mut v = l.to_json();
                if let Value::Obj(kv) = &mut v {
                    kv.push(("using_mib".into(), view.by_lease.get(&l.id).copied().unwrap_or(0).into()));
                }
                v
            })
            .collect();
        let out = obj([
            ("adapter", card.name.as_str().into()),
            ("total_mib", card.total_mib.into()),
            ("used_mib", view.used_mib.into()),
            ("reserved_mib", outstanding.into()),
            ("headroom_mib", headroom_mib.into()),
            ("available_mib", (avail.max(0) as u64).into()),
            ("leases", Value::Arr(list)),
        ]);
        println!("{out}");
        return Ok(());
    }
    println!(
        "{}: {} MiB; {} in use, {} reserved by running jobs, {} headroom: {} available for a new lease",
        card.name,
        thousands(card.total_mib),
        thousands(view.used_mib),
        thousands(outstanding),
        thousands(headroom_mib),
        thousands(avail.max(0) as u64)
    );
    if leases.is_empty() {
        println!("  (no leases)");
        return Ok(());
    }
    let width = crate::width();
    println!(
        "{:<9} {:<9} {:>9} {:>9} {:>8} {:>6}  {:<20} COMMAND",
        "LEASE", "STATE", "VRAM MiB", "USING", "PID", "AGE", "OWNER"
    );
    let now = unix_now();
    for l in &leases {
        let state = if l.state == State::Admitted { "admitted" } else { "waiting" };
        let using = view.by_lease.get(&l.id).copied().unwrap_or(0);
        let age = (now - l.created).max(0.0) as u64;
        let line = format!(
            "{:<9} {:<9} {:>9} {:>9} {:>8} {:>6}  {:<20} {}",
            l.id,
            state,
            thousands(l.vram_mib),
            thousands(using),
            l.job_pid.map_or("–".into(), |p| p.to_string()),
            if age < 3600 { format!("{}m", age / 60) } else { format!("{}h{:02}m", age / 3600, age / 60 % 60) },
            ellipsize(l.owner.as_deref().unwrap_or("–"), 20),
            l.cmd
        );
        println!("{}", ellipsize(&line, width));
    }
    Ok(())
}
