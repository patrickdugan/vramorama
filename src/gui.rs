//! `vramorama gui`: a live page served by vramorama itself on 127.0.0.1, using only the standard
//! library. The page (gui.html) is embedded in the binary and loads nothing from the network.
//!
//! Access: the server listens on loopback only, rejects requests whose Host header is not that
//! loopback address (DNS rebinding), and the data endpoints need a random token that is printed
//! with the URL and kept in the URL fragment, which browsers never send to a server. The page
//! is read-only: it shows, traces and copies commands, and nothing in it can stop a process.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::app::{TraceInfo, command_text, judge, mark_key, owner_text, trace_info};
use crate::gpu::{self, Sampler};
use crate::idle::{IDLE_UTIL, Verdict};
use crate::json::{Value, obj};
use crate::ledger;
use crate::procs;
use crate::scan::{Row, Scanner, pick_adapters, row_json};
use crate::sys::{Lib, filetime_to_unix, fmt_utc, now_filetime, wide};
use crate::transcripts::Kind;

const PAGE: &str = include_str!("gui.html");
const MIB: u64 = 1 << 20;
const HISTORY_SECS: f64 = 3600.0;
/// An idle process gets a verdict once it has been idle this long.
const VERDICT_AFTER_SECS: f64 = 60.0;
const HEADROOM_MIB: u64 = 512;

pub struct GuiOpts {
    pub port: u16,
    pub open: bool,
    pub interval: f64,
    pub all: bool,
    pub min_mib: u64,
}

/// Totals per group (agent session, or job owner) since the page's server started.
struct Stats {
    label: String,
    peak: u64,
    peak_ts: f64,
    first_ts: f64,
    last_ts: f64,
    gib_h: f64,
    idle_gib_h: f64,
    /// What the group looked like when last seen (app, title, ...), kept after it leaves the card.
    meta: Vec<(String, Value)>,
}

/// Stats for one way of grouping processes.
#[derive(Default)]
struct Tally {
    stats: HashMap<String, Stats>,
    now: HashMap<String, u64>,
}

impl Tally {
    fn begin(&mut self) {
        self.now.clear();
    }

    fn add(&mut self, key: &str, label: &str, mib: u64, idle: bool, ts: f64, dt: f64) {
        *self.now.entry(key.to_string()).or_insert(0) += mib;
        let s = self.stats.entry(key.to_string()).or_insert_with(|| Stats {
            label: label.to_string(),
            peak: 0,
            peak_ts: ts,
            first_ts: ts,
            last_ts: ts,
            gib_h: 0.0,
            idle_gib_h: 0.0,
            meta: Vec::new(),
        });
        s.label = label.to_string();
        s.last_ts = ts;
        s.gib_h += mib as f64 / 1024.0 * dt / 3600.0;
        if idle {
            s.idle_gib_h += mib as f64 / 1024.0 * dt / 3600.0;
        }
    }

    fn end(&mut self, ts: f64) {
        for (k, mib) in &self.now {
            if let Some(s) = self.stats.get_mut(k) {
                if *mib > s.peak {
                    s.peak = *mib;
                    s.peak_ts = ts;
                }
            }
        }
    }

    /// Largest peak first.
    fn json(&self) -> Vec<Value> {
        let mut list: Vec<(&String, &Stats)> = self.stats.iter().collect();
        list.sort_by_key(|(k, s)| {
            (std::cmp::Reverse(self.now.get(*k).copied().unwrap_or(0)), std::cmp::Reverse(s.peak))
        });
        list.iter()
            .map(|(k, s)| {
                let mut kv: Vec<(String, Value)> = vec![
                    ("key".into(), k.as_str().into()),
                    ("label".into(), s.label.as_str().into()),
                    ("now_mib".into(), self.now.get(*k).copied().unwrap_or(0).into()),
                    ("peak_mib".into(), s.peak.into()),
                    ("peak_ts".into(), s.peak_ts.into()),
                    ("first_ts".into(), s.first_ts.into()),
                    ("last_ts".into(), s.last_ts.into()),
                    ("gib_hours".into(), s.gib_h.into()),
                    ("idle_gib_hours".into(), s.idle_gib_h.into()),
                ];
                kv.extend(s.meta.iter().cloned());
                Value::Obj(kv)
            })
            .collect()
    }

    /// A group seen with no memory of this kind yet (a session holding only RAM): listed with zeros.
    fn touch(&mut self, key: &str, label: &str, ts: f64) {
        self.stats.entry(key.to_string()).or_insert_with(|| Stats {
            label: label.to_string(),
            peak: 0,
            peak_ts: ts,
            first_ts: ts,
            last_ts: ts,
            gib_h: 0.0,
            idle_gib_h: 0.0,
            meta: Vec::new(),
        });
    }

    /// Current values of at least `min` MiB, for one timeline point.
    fn point(&self, min: u64) -> Value {
        Value::Obj(self.now.iter().filter(|(_, v)| **v >= min.max(1)).map(|(k, v)| (k.clone(), (*v).into())).collect())
    }
}

/// Fields of a session group that change from sample to sample; the rest is kept as metadata.
const LIVE_FIELDS: &[&str] =
    &["key", "label", "mib", "idle_mib", "pids", "ram_mib", "commit_mib", "ram_procs", "agent_mib"];

/// Sessions for the page: the GPU tally (which lists every group seen), with each group's RAM now
/// (zero once it is gone) and its RAM peak and GiB-hours.
fn session_list(sessions: &Tally, ram: &Tally, groups: &[crate::agents::Group]) -> Vec<Value> {
    sessions
        .json()
        .into_iter()
        .map(|mut v| {
            let key = v.str("key").unwrap_or_default().to_string();
            let g = groups.iter().find(|g| g.key == key);
            let r = ram.stats.get(&key);
            if let Value::Obj(kv) = &mut v {
                let live = |f: fn(&crate::agents::Group) -> u64| g.map_or(0, f);
                kv.push(("idle_mib".into(), live(|g| g.idle_mib).into()));
                kv.push((
                    "pids".into(),
                    Value::Arr(g.map(|g| g.pids.iter().map(|&p| p.into()).collect()).unwrap_or_default()),
                ));
                kv.push(("ram_mib".into(), live(|g| g.ram_mib).into()));
                kv.push(("agent_mib".into(), live(|g| g.agent_mib).into()));
                kv.push(("commit_mib".into(), live(|g| g.commit_mib).into()));
                kv.push(("ram_procs".into(), live(|g| g.ram_procs as u64).into()));
                kv.push(("ram_peak_mib".into(), r.map_or(0, |s| s.peak).into()));
                kv.push(("ram_gib_hours".into(), r.map_or(0.0, |s| s.gib_h).into()));
            }
            v
        })
        .collect()
}

/// RAM processes are listed on the page from this size up (all of them count in the totals).
const RAM_PROC_MIN_MIB: u64 = 64;
/// Sessions get a line on the RAM timeline from this much RAM up, as they get a bar segment.
const SESSION_MIN_RAM_MIB: u64 = 256;

#[derive(Default)]
struct Shared {
    /// The current state, already serialised.
    now: String,
    /// One point per sample: (unix time, serialised point).
    points: VecDeque<(f64, String)>,
}

pub fn gui(o: GuiOpts) -> Result<(), String> {
    let token = random_token()?;
    let mut sampler = Sampler::open()?;
    sampler.prime();
    let listener = TcpListener::bind(("127.0.0.1", o.port)).map_err(|e| format!("127.0.0.1:{}: {e}", o.port))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let shared = Arc::new(Mutex::new(Shared::default()));
    {
        let shared = Arc::clone(&shared);
        let (interval, all, min_mib) = (o.interval, o.all, o.min_mib);
        std::thread::spawn(move || sample_loop(sampler, &shared, interval, all, min_mib));
    }
    let url = format!("http://127.0.0.1:{port}/#t={token}");
    println!("vramorama gui: {url}");
    println!("Serving on 127.0.0.1 only; the token in the link is needed to read data. Ctrl-C to stop.");
    if o.open {
        open_browser(&url);
    }
    let host_ok = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let shared = Arc::clone(&shared);
        let (token, host_ok) = (token.clone(), host_ok.clone());
        std::thread::spawn(move || {
            let _ = handle(stream, &shared, &token, &host_ok);
        });
    }
    Ok(())
}

fn sample_loop(mut sampler: Sampler, shared: &Mutex<Shared>, interval: f64, all: bool, min_mib: u64) {
    let dxgi = gpu::adapters();
    let mut scanner = Scanner::default();
    let mut peaks: HashMap<(u32, u64), u64> = HashMap::new();
    let mut idle_since: HashMap<(u32, u64), f64> = HashMap::new();
    let mut owners = Tally::default();
    let mut sessions = Tally::default();
    // RAM per session: its jobs plus the agent's own process.
    let mut ram_tally = Tally::default();
    // Tags of the session that asked for each lease, looked up once per lease.
    let mut lease_owners: HashMap<String, Option<crate::owner::Owner>> = HashMap::new();
    let started = filetime_to_unix(now_filetime());
    let mut last_ts: Option<f64> = None;
    loop {
        std::thread::sleep(Duration::from_secs_f64(interval));
        let sample = sampler.sample();
        let table = procs::snapshot();
        let adapters = pick_adapters(dxgi.clone(), &sample, all);
        let luids: Vec<u64> = adapters.iter().map(|a| a.luid).collect();
        let mut rows = scanner.rows(&sample, &table, &luids, min_mib);
        let mem = procs::memory();
        let sys = procs::system_memory();
        crate::scan::attach_ram(&mut rows, &mem);
        let ram_rows = scanner.ram_rows(&table, &mem);
        let ft = now_filetime();
        let ts = filetime_to_unix(ft);
        let dt = last_ts.map_or(interval, |l| (ts - l).min(3.0 * interval));
        last_ts = Some(ts);

        // Per process: peak, idle streak, verdict. Per owner and per agent session: totals.
        owners.begin();
        sessions.begin();
        let mut procs_json: HashMap<u64, Vec<Value>> = HashMap::new();
        for r in &rows {
            let key = (r.pid, r.luid);
            let peak = peaks.entry(key).or_insert(0);
            *peak = (*peak).max(r.mib);
            let idle = r.util.is_some_and(|u| u < IDLE_UTIL);
            if idle {
                idle_since.entry(key).or_insert(ts);
            } else {
                idle_since.remove(&key);
            }
            let idle_secs = idle_since.get(&key).map(|s| ts - s);
            let (verdict, why) = match idle_secs {
                Some(s) if s >= VERDICT_AFTER_SECS => {
                    let (v, why) = judge(r);
                    (if v == Verdict::Stale { "stale" } else { "idle" }, why)
                }
                Some(_) => ("idle", String::new()),
                None => ("active", String::new()),
            };
            let (okey, olabel) = mark_key(r);
            let (skey, slabel) = r.session_group();
            owners.add(&okey, &olabel, r.mib, idle, ts, dt);
            sessions.add(&skey, &slabel, r.mib, idle, ts, dt);
            procs_json.entry(r.luid).or_default().push(proc_json(r, *peak, idle_secs, verdict, &why, okey, olabel));
        }
        owners.end(ts);
        sessions.end(ts);
        ram_tally.begin();
        let mut ram_json: Vec<Value> = Vec::new();
        for r in &ram_rows {
            let (skey, slabel) = r.session_group();
            let mib = r.ram.map_or(0, |m| m.private_mib);
            ram_tally.add(&skey, &slabel, mib, false, ts, dt);
            sessions.touch(&skey, &slabel, ts);
            if mib >= RAM_PROC_MIN_MIB && ram_json.len() < 200 {
                let (okey, olabel) = mark_key(r);
                let vram: u64 = rows.iter().filter(|g| g.pid == r.pid).map(|g| g.mib).sum();
                let mut v = proc_json(r, 0, None, "", "", okey, olabel);
                if let Value::Obj(kv) = &mut v {
                    kv.push(("vram_mib".into(), vram.into()));
                }
                ram_json.push(v);
            }
        }
        ram_tally.end(ts);
        let groups = crate::agents::group(&rows, &ram_rows);
        for g in &groups {
            if let (Some(s), Value::Obj(kv)) = (sessions.stats.get_mut(&g.key), crate::agents::group_json(g)) {
                // What does not change from sample to sample; `session_list` adds the rest.
                s.meta = kv.into_iter().filter(|(k, _)| !LIVE_FIELDS.contains(&k.as_str())).collect();
            }
        }
        let apps: Vec<Value> = crate::agents::apps(&groups, &table).iter().map(crate::agents::app_json).collect();
        let memory = sys.as_ref().map(|s| {
            let mut v = crate::scan::system_memory_json(s);
            let sum = |f: &dyn Fn(&Row) -> bool| -> u64 {
                ram_rows.iter().filter(|r| f(r)).map(|r| r.ram.map_or(0, |m| m.private_mib)).sum()
            };
            if let Value::Obj(kv) = &mut v {
                kv.push(("jobs_mib".into(), sum(&|r| !r.agent_proc && r.owner.is_tagged()).into()));
                kv.push(("agents_mib".into(), sum(&|r| r.agent_proc).into()));
                kv.push(("untagged_big_mib".into(), sum(&|r| !r.agent_proc && !r.owner.is_tagged()).into()));
            }
            v
        });
        let ram_used = sys.map(|s| (s.total - s.avail.min(s.total)) / MIB);

        // Leases: memory per lease from the processes' tags; the ledger is only read here.
        let mut by_lease: HashMap<String, u64> = HashMap::new();
        for r in &rows {
            if let Some(id) = &r.owner.lease {
                *by_lease.entry(id.clone()).or_insert(0) += r.mib;
            }
        }
        let leases = crate::run::live_leases(&by_lease);
        let legacy: Vec<String> = crate::run::legacy_leases(&by_lease).into_iter().map(|l| l.id).collect();
        let outstanding = ledger::outstanding(&leases, &by_lease, crate::sys::filetime_to_unix(ft));

        let adapter_list: Vec<Value> = adapters
            .iter()
            .map(|a| {
                let used = sample.adapter_used.get(&a.luid).copied().unwrap_or(0) / MIB;
                let total = a.dedicated / MIB;
                obj([
                    ("name", a.name.as_str().into()),
                    ("luid", crate::parse::luid_string(a.luid).into()),
                    ("total_mib", total.into()),
                    ("used_mib", used.into()),
                    ("spill_warning", crate::spill::warning(&crate::scan::spills(a, &sample, &rows)).into()),
                    ("available_mib", (ledger::available(total, used, outstanding, HEADROOM_MIB).max(0) as u64).into()),
                    ("procs", Value::Arr(procs_json.remove(&a.luid).unwrap_or_default())),
                ])
            })
            .collect();
        // Lease tags on running processes that this ledger has never heard of: jobs queued in a
        // ledger somewhere else (an older vramorama, another VRAMORAMA_HOME).
        let mut untracked: Vec<(&String, &u64)> =
            by_lease.iter().filter(|(id, _)| !leases.iter().any(|l| &l.id == *id)).collect();
        untracked.sort();
        let untracked: Vec<Value> =
            untracked.iter().map(|(id, mib)| obj([("id", id.as_str().into()), ("mib", (**mib).into())])).collect();
        let lease_json: Vec<Value> = leases
            .iter()
            .map(|l| {
                let mut v = l.to_json();
                if let Value::Obj(kv) = &mut v {
                    kv.push(("using_mib".into(), by_lease.get(&l.id).copied().unwrap_or(0).into()));
                    kv.push(("legacy_ledger".into(), legacy.contains(&l.id).into()));
                    let owner = lease_owners.entry(l.id.clone()).or_insert_with(|| crate::run::lease_owner(l)).clone();
                    let session = l.session.clone().or_else(|| owner.as_ref().and_then(|o| o.agent_session_key()));
                    if let Some((_, slot)) = kv.iter_mut().find(|(k, _)| k == "session") {
                        *slot = session.into();
                    }
                    kv.push(("session_title".into(), owner.and_then(|o| scanner.titles.title(&o)).into()));
                }
                v
            })
            .collect();
        let used_total: u64 =
            adapters.iter().map(|a| sample.adapter_used.get(&a.luid).copied().unwrap_or(0) / MIB).sum();
        let now = obj([
            ("version", env!("CARGO_PKG_VERSION").into()),
            ("time", fmt_utc(ft).into()),
            ("ts", ts.into()),
            ("started", started.into()),
            ("interval", interval.into()),
            ("headroom_mib", HEADROOM_MIB.into()),
            ("reserved_mib", outstanding.into()),
            ("adapters", Value::Arr(adapter_list)),
            ("owners", Value::Arr(owners.json())),
            ("sessions", Value::Arr(session_list(&sessions, &ram_tally, &groups))),
            ("apps", Value::Arr(apps)),
            ("memory", memory.into()),
            ("ram_procs", Value::Arr(ram_json)),
            ("leases", Value::Arr(lease_json)),
            ("untracked_leases", Value::Arr(untracked)),
            ("ledger", ledger::dir().map(|d| d.display().to_string()).into()),
        ]);
        let point = obj([
            ("ts", ts.into()),
            ("used", used_total.into()),
            ("o", owners.point(1)),
            ("s", sessions.point(1)),
            ("ru", ram_used.into()),
            ("r", ram_tally.point(SESSION_MIN_RAM_MIB)),
        ]);
        let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
        s.now = now.to_string();
        s.points.push_back((ts, point.to_string()));
        while s.points.front().is_some_and(|(t, _)| ts - t > HISTORY_SECS) {
            s.points.pop_front();
        }
    }
}

fn proc_json(
    r: &Row,
    peak: u64,
    idle_secs: Option<f64>,
    verdict: &str,
    why: &str,
    okey: String,
    olabel: String,
) -> Value {
    let mut v = row_json(r);
    if let Value::Obj(kv) = &mut v {
        let (skey, slabel) = r.session_group();
        kv.push(("peak_mib".into(), peak.into()));
        kv.push(("idle_secs".into(), idle_secs.into()));
        kv.push(("verdict".into(), verdict.into()));
        kv.push(("why".into(), why.into()));
        kv.push(("owner_key".into(), okey.into()));
        kv.push(("owner_label".into(), olabel.into()));
        kv.push(("group_key".into(), skey.into()));
        kv.push(("group_label".into(), slabel.into()));
        kv.push(("owner_text".into(), owner_text(r).into()));
        kv.push(("command_text".into(), command_text(r).into()));
    }
    v
}

fn trace_json(t: &TraceInfo) -> Value {
    let d = &t.details;
    let hits = t
        .hits
        .iter()
        .map(|(h, via, via_exe)| {
            obj([
                ("kind", if h.kind == Kind::Launched { "launched" } else { "mentioned" }.into()),
                ("harness", h.harness.into()),
                ("session", h.session.as_str().into()),
                ("title", h.title.clone().into()),
                ("path", h.path.display().to_string().into()),
                ("line", (h.line as u64).into()),
                ("lead_secs", h.lead.into()),
                ("via_pid", (*via).into()),
                ("via_exe", via_exe.as_str().into()),
                ("needle", h.needle.as_str().into()),
            ])
        })
        .collect();
    obj([
        ("pid", t.pid.into()),
        ("exe", t.exe.as_str().into()),
        ("started", d.created.map(fmt_utc).into()),
        ("cwd", d.cwd.clone().into()),
        ("cmd", d.cmdline.clone().into()),
        ("note", d.note.into()),
        ("owner", t.owner.key().into()),
        ("session", t.owner.session().map(str::to_string).into()),
        ("app", t.owner.app().into()),
        ("job", t.owner.label.clone().into()),
        ("title", t.title.clone().into()),
        ("transcript", t.transcript.as_ref().map(|p| p.display().to_string()).into()),
        (
            "chain",
            Value::Arr(
                t.chain.iter().map(|(exe, p)| obj([("exe", exe.as_str().into()), ("pid", (*p).into())])).collect(),
            ),
        ),
        ("exited_parent", t.exited.into()),
        (
            "tagged_ancestors",
            Value::Arr(
                t.tagged_ancestors
                    .iter()
                    .map(|(exe, p, k)| {
                        obj([("exe", exe.as_str().into()), ("pid", (*p).into()), ("owner", k.as_str().into())])
                    })
                    .collect(),
            ),
        ),
        ("searched", t.search.map(|(n, _)| n as u64).into()),
        ("since", t.search.and_then(|(_, s)| s).into()),
        ("hits", Value::Arr(hits)),
    ])
}

/// Serve one request. Only GET; the connection is closed after the response.
fn handle(mut stream: TcpStream, shared: &Mutex<Shared>, token: &str, host_ok: &[String]) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk)?;
        if n == 0 || buf.len() > 16 * 1024 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
    let mut headers: HashMap<String, String> = HashMap::new();
    for l in lines.take_while(|l| !l.is_empty()) {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    if !headers.get("host").is_some_and(|h| host_ok.iter().any(|ok| ok.eq_ignore_ascii_case(h))) {
        return respond(&mut stream, 403, "text/plain", b"forbidden host");
    }
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", b"GET only");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let param =
        |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('=')).map(str::to_string);
    if path == "/" || path == "/index.html" {
        return respond(&mut stream, 200, "text/html; charset=utf-8", PAGE.as_bytes());
    }
    if path == "/favicon.ico" {
        return respond(&mut stream, 204, "text/plain", b"");
    }
    if !path.starts_with("/api/") {
        return respond(&mut stream, 404, "text/plain", b"not found");
    }
    if !headers.get("x-vramorama-token").is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes())) {
        return respond(&mut stream, 401, "text/plain", b"missing or wrong token");
    }
    match path {
        "/api/state" => {
            let since: f64 = param("since").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let body = {
                let s = shared.lock().unwrap_or_else(|p| p.into_inner());
                let pts: Vec<&str> = s.points.iter().filter(|(t, _)| *t > since).map(|(_, p)| p.as_str()).collect();
                let now = if s.now.is_empty() { "null" } else { s.now.as_str() };
                format!("{{\"now\":{now},\"points\":[{}]}}", pts.join(","))
            };
            respond(&mut stream, 200, "application/json", body.as_bytes())
        }
        "/api/trace" => {
            let Some(pid) = param("pid").and_then(|p| p.parse::<u32>().ok()) else {
                return respond(&mut stream, 400, "text/plain", b"pid required");
            };
            let body = match trace_info(pid) {
                Ok(t) => trace_json(&t).to_string(),
                Err(e) => obj([("error", e.into())]).to_string(),
            };
            respond(&mut stream, 200, "application/json", body.as_bytes())
        }
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; \
script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src data:; base-uri 'none'; \
form-action 'none'; frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 128 bits from the system RNG (BCryptGenRandom), as hex.
fn random_token() -> Result<String, String> {
    type GenRandom = unsafe extern "system" fn(isize, *mut u8, u32, u32) -> i32;
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x2;
    let lib = Lib::load("bcrypt.dll").ok_or("bcrypt.dll not found")?;
    let gen_random: GenRandom = unsafe { lib.get("BCryptGenRandom") }.ok_or("BCryptGenRandom not found")?;
    let mut bytes = [0u8; 16];
    if unsafe { gen_random(0, bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) } != 0 {
        return Err("the system random number generator failed".into());
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Hand the URL to the default browser (ShellExecuteW). Failure is not an error: the URL is printed.
fn open_browser(url: &str) {
    type ShellExecuteW = unsafe extern "system" fn(isize, *const u16, *const u16, *const u16, *const u16, i32) -> isize;
    const SW_SHOWNORMAL: i32 = 1;
    let Some(shell) = Lib::load("shell32.dll") else { return };
    let Some(exec) = (unsafe { shell.get::<ShellExecuteW>("ShellExecuteW") }) else { return };
    let (op, file) = (wide("open"), wide(url));
    unsafe { exec(0, op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
