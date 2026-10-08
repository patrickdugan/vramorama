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

/// Per-owner totals since the page's server started.
struct OwnerStats {
    label: String,
    peak: u64,
    peak_ts: f64,
    first_ts: f64,
    last_ts: f64,
    gib_h: f64,
    idle_gib_h: f64,
}

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
    let mut owners: HashMap<String, OwnerStats> = HashMap::new();
    let started = filetime_to_unix(now_filetime());
    let mut last_ts: Option<f64> = None;
    loop {
        std::thread::sleep(Duration::from_secs_f64(interval));
        let sample = sampler.sample();
        let table = procs::snapshot();
        let adapters = pick_adapters(dxgi.clone(), &sample, all);
        let luids: Vec<u64> = adapters.iter().map(|a| a.luid).collect();
        let rows = scanner.rows(&sample, &table, &luids, min_mib);
        let ft = now_filetime();
        let ts = filetime_to_unix(ft);
        let dt = last_ts.map_or(interval, |l| (ts - l).min(3.0 * interval));
        last_ts = Some(ts);

        // Per process: peak, idle streak, verdict. Per owner: totals.
        let mut now_by_owner: HashMap<String, u64> = HashMap::new();
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
            *now_by_owner.entry(okey.clone()).or_insert(0) += r.mib;
            let s = owners.entry(okey.clone()).or_insert_with(|| OwnerStats {
                label: olabel.clone(),
                peak: 0,
                peak_ts: ts,
                first_ts: ts,
                last_ts: ts,
                gib_h: 0.0,
                idle_gib_h: 0.0,
            });
            s.label = olabel.clone();
            s.last_ts = ts;
            s.gib_h += r.mib as f64 / 1024.0 * dt / 3600.0;
            if idle {
                s.idle_gib_h += r.mib as f64 / 1024.0 * dt / 3600.0;
            }
            procs_json.entry(r.luid).or_default().push(proc_json(r, *peak, idle_secs, verdict, &why, okey, olabel));
        }
        for (k, mib) in &now_by_owner {
            if let Some(s) = owners.get_mut(k) {
                if *mib > s.peak {
                    s.peak = *mib;
                    s.peak_ts = ts;
                }
            }
        }

        // Leases: memory per lease from the processes' tags; the ledger is only read here.
        let mut by_lease: HashMap<String, u64> = HashMap::new();
        for r in &rows {
            if let Some(id) = &r.owner.lease {
                *by_lease.entry(id.clone()).or_insert(0) += r.mib;
            }
        }
        let leases = crate::run::live_leases(&by_lease);
        let outstanding = ledger::outstanding(&leases, &by_lease);

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
                    ("available_mib", (ledger::available(total, used, outstanding, HEADROOM_MIB).max(0) as u64).into()),
                    ("procs", Value::Arr(procs_json.remove(&a.luid).unwrap_or_default())),
                ])
            })
            .collect();
        let mut owner_list: Vec<(&String, &OwnerStats)> = owners.iter().collect();
        owner_list.sort_by_key(|(_, s)| std::cmp::Reverse(s.peak));
        let owner_json: Vec<Value> = owner_list
            .iter()
            .map(|(k, s)| {
                obj([
                    ("key", k.as_str().into()),
                    ("label", s.label.as_str().into()),
                    ("now_mib", now_by_owner.get(*k).copied().unwrap_or(0).into()),
                    ("peak_mib", s.peak.into()),
                    ("peak_ts", s.peak_ts.into()),
                    ("first_ts", s.first_ts.into()),
                    ("last_ts", s.last_ts.into()),
                    ("gib_hours", s.gib_h.into()),
                    ("idle_gib_hours", s.idle_gib_h.into()),
                ])
            })
            .collect();
        let lease_json: Vec<Value> = leases
            .iter()
            .map(|l| {
                let mut v = l.to_json();
                if let Value::Obj(kv) = &mut v {
                    kv.push(("using_mib".into(), by_lease.get(&l.id).copied().unwrap_or(0).into()));
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
            ("owners", Value::Arr(owner_json)),
            ("leases", Value::Arr(lease_json)),
        ]);
        let point = obj([
            ("ts", ts.into()),
            ("used", used_total.into()),
            ("o", Value::Obj(now_by_owner.into_iter().map(|(k, v)| (k, v.into())).collect())),
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
        kv.push(("peak_mib".into(), peak.into()));
        kv.push(("idle_secs".into(), idle_secs.into()));
        kv.push(("verdict".into(), verdict.into()));
        kv.push(("why".into(), why.into()));
        kv.push(("owner_key".into(), okey.into()));
        kv.push(("owner_label".into(), olabel.into()));
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
        ("session", t.owner.session.clone().into()),
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
