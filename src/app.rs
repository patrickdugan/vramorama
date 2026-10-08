//! The Windows commands: ps, watch, trace, idle, reclaim.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use crate::Opts;
use crate::gpu::{self, Adapter, Sample, Sampler};
use crate::idle::{IDLE_UTIL, Verdict, classify};
use crate::json::{Value, obj};
use crate::owner::{Owner, short};
use crate::parse::{args_tail, basename, duration, ellipsize, needles, thousands};
use crate::procs;
use crate::scan::{Row, Scanner, adapter_json, is_host, pick_adapters};
use crate::sys::{filetime_to_unix, fmt_short, fmt_utc, now_filetime, unix_to_filetime};
use crate::transcripts::{self, Kind, Needle};

const MIB: u64 = 1 << 20;

pub fn ps(o: &Opts) -> Result<(), String> {
    let mut sampler = Sampler::open()?;
    sampler.prime();
    std::thread::sleep(Duration::from_millis(1000));
    let sample = sampler.sample();
    let table = procs::snapshot();
    let adapters = pick_adapters(gpu::adapters(), &sample, o.all);
    let luids: Vec<u64> = adapters.iter().map(|a| a.luid).collect();
    let rows = Scanner::default().rows(&sample, &table, &luids, o.min_mib);
    if o.json {
        let list = adapters.iter().map(|a| adapter_json(a, &sample, &rows)).collect();
        println!("{}", obj([("time", fmt_utc(now_filetime()).into()), ("adapters", Value::Arr(list))]));
        return Ok(());
    }
    let width = if o.wide { usize::MAX } else { crate::width() };
    print!("{}", render(&adapters, &sample, &rows, None, width));
    let untagged: Vec<u32> = rows.iter().filter(|r| !r.owner.is_tagged() && r.note.is_none()).map(|r| r.pid).collect();
    if o.trace {
        for pid in untagged {
            println!();
            trace(pid)?;
        }
    } else if !untagged.is_empty() {
        println!(
            "\nUntagged processes: `vramorama trace <PID>` searches agent transcripts for the session that launched them."
        );
    }
    Ok(())
}

pub fn watch(o: &Opts) -> Result<(), String> {
    let mut sampler = Sampler::open()?;
    sampler.prime();
    let dxgi = gpu::adapters();
    let mut scanner = Scanner::default();
    let mut peaks: HashMap<(u32, u64), u64> = HashMap::new();
    let mut marks: HashMap<String, Mark> = HashMap::new();
    let mut log = match &o.log {
        Some(p) => {
            Some(std::fs::OpenOptions::new().create(true).append(true).open(p).map_err(|e| format!("{p}: {e}"))?)
        }
        None => None,
    };
    let mut idle_since: HashMap<(u32, u64), u64> = HashMap::new();
    let since = now_filetime();
    loop {
        std::thread::sleep(Duration::from_secs_f64(o.interval));
        let sample = sampler.sample();
        let table = procs::snapshot();
        let adapters = pick_adapters(dxgi.clone(), &sample, o.all);
        let luids: Vec<u64> = adapters.iter().map(|a| a.luid).collect();
        let rows = scanner.rows(&sample, &table, &luids, o.min_mib);
        let now = now_filetime();
        for r in &rows {
            let key = (r.pid, r.luid);
            let p = peaks.entry(key).or_insert(0);
            *p = (*p).max(r.mib);
            if r.util.is_some_and(|u| u < IDLE_UTIL) {
                idle_since.entry(key).or_insert(now);
            } else {
                idle_since.remove(&key);
            }
        }
        update_marks(&mut marks, &rows, now);
        if let Some(f) = log.as_mut() {
            let list = adapters.iter().map(|a| adapter_json(a, &sample, &rows)).collect();
            let line =
                obj([("ts", filetime_to_unix(now).into()), ("t", fmt_utc(now).into()), ("adapters", Value::Arr(list))]);
            writeln!(f, "{line}").and_then(|_| f.flush()).map_err(|e| format!("writing log: {e}"))?;
        }
        if !o.quiet {
            let width = if o.wide { usize::MAX } else { crate::width() };
            let mut screen = String::from("\x1b[H\x1b[2J");
            screen.push_str(&format!(
                "vramorama watch: every {}s since {}{}  (Ctrl-C to stop)\n\n",
                o.interval,
                fmt_short(since),
                o.log.as_deref().map(|p| format!(", logging to {p}")).unwrap_or_default()
            ));
            let view = WatchView { peaks: &peaks, idle_since: &idle_since, now };
            screen.push_str(&render(&adapters, &sample, &rows, Some(&view), width));
            screen.push_str(&render_marks(&marks, width));
            print!("{screen}");
            std::io::stdout().flush().ok();
        }
    }
}

/// Per-owner running watermark: current and peak total VRAM across the owner's processes.
struct Mark {
    label: String,
    now: u64,
    peak: u64,
    peak_at: u64,
    first: u64,
    last: u64,
}

pub fn mark_key(r: &Row) -> (String, String) {
    match r.owner.key() {
        Some(k) => {
            let label = match (&r.owner.session, &r.title) {
                (Some(_), Some(t)) if r.owner.label.is_none() => format!("{k} “{t}”"),
                _ => k.clone(),
            };
            (k, label)
        }
        None => {
            let k = format!("untagged {} {}", r.name, r.pid);
            (k.clone(), k)
        }
    }
}

fn update_marks(marks: &mut HashMap<String, Mark>, rows: &[Row], now: u64) {
    for m in marks.values_mut() {
        m.now = 0;
    }
    for r in rows {
        let (key, label) = mark_key(r);
        let m = marks.entry(key).or_insert(Mark { label, now: 0, peak: 0, peak_at: now, first: now, last: now });
        m.now += r.mib;
        m.last = now;
    }
    for m in marks.values_mut() {
        if m.now > m.peak {
            m.peak = m.now;
            m.peak_at = now;
        }
    }
}

fn render_marks(marks: &HashMap<String, Mark>, width: usize) -> String {
    let mut list: Vec<&Mark> = marks.values().collect();
    list.sort_by_key(|m| std::cmp::Reverse(m.peak));
    let mut out =
        format!("\n{:<52} {:>9} {:>9}  {:<11}  {}\n", "WATERMARK BY OWNER", "NOW MiB", "PEAK MiB", "PEAK AT", "SEEN");
    for m in list {
        let seen = format!("{} – {}", fmt_short(m.first), &fmt_short(m.last)[6..]);
        let line = format!(
            "{:<52} {:>9} {:>9}  {:<11}  {}",
            ellipsize(&m.label, 52),
            if m.now > 0 { thousands(m.now) } else { "–".into() },
            thousands(m.peak),
            fmt_short(m.peak_at),
            seen
        );
        out.push_str(&ellipsize(&line, width));
        out.push('\n');
    }
    out
}

pub fn owner_text(r: &Row) -> String {
    let o: &Owner = &r.owner;
    let lease = o.lease.as_deref().map(|l| format!(" #{l}")).unwrap_or_default();
    if let Some(l) = &o.label {
        return format!("{l}{lease}");
    }
    if let Some(s) = &o.session {
        let state = match r.harness_live {
            Some(true) => " (live)",
            Some(false) => " (ended)",
            None => "",
        };
        let title = r.title.as_deref().map(|t| format!(" “{t}”")).unwrap_or_default();
        return format!("claude {}{state}{lease}{title}", short(s));
    }
    if let Some(a) = &o.agent {
        return format!("{a}{lease}");
    }
    if let Some(l) = &o.lease {
        return format!("lease {l}");
    }
    match (&r.root, r.note) {
        (_, Some(_)) => "unknown".into(),
        (Some(root), None) => format!("untagged, {root}"),
        (None, None) => "untagged".into(),
    }
}
pub fn command_text(r: &Row) -> String {
    let mut s = r.name.clone();
    if let Some(cwd) = &r.cwd {
        s.push_str(&format!(" [{}]", basename(cwd)));
    }
    match (&r.cmd, r.note) {
        (Some(c), _) => {
            let tail = args_tail(c);
            if !tail.is_empty() {
                s.push(' ');
                s.push_str(tail);
            }
        }
        (None, Some(note)) => s.push_str(&format!(" ({note})")),
        (None, None) => {}
    }
    s
}

/// What `watch` adds to the table: each process's peak, and since when it has been idle.
pub struct WatchView<'a> {
    peaks: &'a HashMap<(u32, u64), u64>,
    idle_since: &'a HashMap<(u32, u64), u64>,
    now: u64,
}

pub fn render(adapters: &[Adapter], sample: &Sample, rows: &[Row], watch: Option<&WatchView>, width: usize) -> String {
    const OWNER_W: usize = 44;
    let mut out = String::new();
    for a in adapters {
        let mine: Vec<&Row> = rows.iter().filter(|r| r.luid == a.luid).collect();
        let used = sample.adapter_used.get(&a.luid).copied().unwrap_or(0) / MIB;
        let listed: u64 = mine.iter().map(|r| r.mib).sum();
        let total = if a.dedicated > 0 { format!(" of {}", thousands(a.dedicated / MIB)) } else { String::new() };
        out.push_str(&format!(
            "{}: {} MiB{} in use; {} in the processes below, {} elsewhere\n",
            a.name,
            thousands(used),
            total,
            thousands(listed),
            thousands(used.saturating_sub(listed))
        ));
        if mine.is_empty() {
            out.push_str("  (no process holds memory above the --min threshold)\n\n");
            continue;
        }
        let peak_h = if watch.is_some() { format!(" {:>9} {:>6}", "PEAK MiB", "IDLE") } else { String::new() };
        out.push_str(&format!(
            "{:>7} {:>9}{peak_h} {:>5}  {:<11}  {:<OWNER_W$}  {}\n",
            "PID", "VRAM MiB", "GPU%", "STARTED", "OWNER", "COMMAND"
        ));
        for r in mine {
            let key = (r.pid, r.luid);
            let peak = match watch {
                Some(w) => format!(
                    " {:>9} {:>6}",
                    thousands(w.peaks.get(&key).copied().unwrap_or(r.mib)),
                    w.idle_since.get(&key).map_or("–".into(), |&t| duration((w.now - t) as f64 / 1e7))
                ),
                None => String::new(),
            };
            let util = r.util.map_or("–".into(), |u| format!("{u:.0}"));
            let line = format!(
                "{:>7} {:>9}{peak} {:>5}  {:<11}  {:<OWNER_W$}  {}",
                r.pid,
                thousands(r.mib),
                util,
                r.started.map(fmt_short).unwrap_or_default(),
                ellipsize(&owner_text(r), OWNER_W),
                command_text(r)
            );
            out.push_str(&ellipsize(&line, width));
            out.push('\n');
        }
        out.push('\n');
    }
    if adapters.is_empty() {
        out.push_str("No GPU adapters found.\n");
    }
    out
}

/// Everything `trace` finds out about a process, for the CLI printer and the GUI.
pub struct TraceInfo {
    pub pid: u32,
    pub exe: String,
    pub details: procs::Details,
    pub owner: Owner,
    pub title: Option<String>,
    pub transcript: Option<std::path::PathBuf>,
    /// The process and its live ancestors, nearest first, then the first parent that has exited.
    pub chain: Vec<(String, u32)>,
    pub exited: Option<u32>,
    pub tagged_ancestors: Vec<(String, u32, String)>,
    /// Transcripts read, and the time they had to be modified after; `None` when nothing was searched.
    pub search: Option<(usize, Option<f64>)>,
    /// Hits with the pid whose command line matched.
    pub hits: Vec<(transcripts::Hit, u32, String)>,
}

pub fn trace_info(pid: u32) -> Result<TraceInfo, String> {
    let table = procs::snapshot();
    let entry = table.get(&pid).ok_or(format!("no process {pid}"))?;
    let d = procs::details(pid);
    let owner = d.env.as_deref().map(Owner::from_env).unwrap_or_default();
    let (chain, exited) = procs::ancestry(pid, &table);
    let mut info = TraceInfo {
        pid,
        exe: entry.exe.clone(),
        title: owner.session.as_deref().and_then(transcripts::session_title),
        transcript: owner.session.as_deref().and_then(transcripts::claude_transcript),
        owner,
        chain: std::iter::once(pid).chain(chain.iter().copied()).map(|p| (table[&p].exe.clone(), p)).collect(),
        exited,
        tagged_ancestors: Vec::new(),
        search: None,
        hits: Vec::new(),
        details: d,
    };
    if info.owner.session.is_some() {
        return Ok(info);
    }
    // Each candidate command line, with the process that ran it and when that process started.
    let mut wanted: Vec<(u32, Needle)> = Vec::new();
    for p in std::iter::once(pid).chain(chain.iter().copied()) {
        if is_host(&table[&p].exe) {
            break;
        }
        let pd = if p == pid { info.details.clone() } else { procs::details(p) };
        if p != pid {
            let o = pd.env.as_deref().map(Owner::from_env).unwrap_or_default();
            if let Some(k) = o.key() {
                info.tagged_ancestors.push((table[&p].exe.clone(), p, k));
            }
        }
        let Some(cmd) = pd.cmdline else { continue };
        let started = pd.created.map(filetime_to_unix);
        for text in needles(&cmd) {
            if !wanted.iter().any(|(_, n)| n.text == text) {
                wanted.push((p, Needle { text, started }));
            }
        }
    }
    if wanted.is_empty() {
        return Ok(info);
    }
    // The launching session wrote to its transcript when it ran the command, so older files can be skipped.
    let earliest = wanted.iter().filter_map(|(_, n)| n.started).min_by(f64::total_cmp);
    let since_unix = earliest.map(|s| s - 300.0);
    let list: Vec<Needle> = wanted.iter().map(|(_, n)| Needle { text: n.text.clone(), started: n.started }).collect();
    let (scanned, hits) = transcripts::search(&list, since_unix);
    info.search = Some((scanned, since_unix));
    info.hits = hits
        .into_iter()
        .map(|h| {
            let via = wanted.iter().find(|(_, n)| n.text == h.needle).map_or(pid, |(p, _)| *p);
            let via_exe = table.get(&via).map_or_else(|| "?".into(), |e| e.exe.clone());
            (h, via, via_exe)
        })
        .collect();
    Ok(info)
}

pub fn trace(pid: u32) -> Result<(), String> {
    let t = trace_info(pid)?;
    let d = &t.details;
    println!(
        "pid {pid} {}  started {}{}",
        t.exe,
        d.created.map(fmt_short).unwrap_or("?".into()),
        d.cwd.as_deref().map(|c| format!("  in {c}")).unwrap_or_default()
    );
    if let Some(c) = &d.cmdline {
        println!("  command   {c}");
    }
    if let Some(note) = d.note {
        println!("  note      {note}");
    }
    if let Some(s) = &t.owner.session {
        println!(
            "  tagged    Claude Code session {s}{}",
            t.title.as_ref().map(|t| format!(" “{t}”")).unwrap_or_default()
        );
        if let Some(p) = &t.transcript {
            println!("  transcript {}", p.display());
        }
        return Ok(());
    }
    if let Some(l) = t.owner.label.as_ref().or(t.owner.agent.as_ref()) {
        println!("  tagged    {l}");
    } else {
        println!(
            "  tagged    no (no {} / CLAUDE_CODE_SESSION_ID / AI_AGENT in its environment)",
            crate::owner::LABEL_VAR
        );
    }
    let mut line: Vec<String> = t.chain.iter().map(|(exe, p)| format!("{exe} {p}")).collect();
    if let Some(g) = t.exited {
        line.push(format!("[exited {g}]"));
    }
    println!("  ancestry  {}", line.join(" <- "));
    for (exe, p, k) in &t.tagged_ancestors {
        println!("  ancestor  {exe} {p} is tagged {k}");
    }
    let Some((scanned, since_unix)) = t.search else {
        println!("  search    nothing distinctive to search for in these command lines");
        return Ok(());
    };
    println!(
        "  search    {scanned} transcripts modified since {}",
        since_unix.map_or("ever".into(), |s| fmt_short(unix_to_filetime(s)))
    );
    if t.hits.is_empty() {
        println!("  result    no transcript contains these command lines; likely started by hand or by a script");
    } else if t.hits[0].0.kind != Kind::Launched {
        println!("  result    no session typed this command before the process started; the sessions below only");
        println!("            mention it (the launcher may have built it from variables: check the earliest)");
    }
    for (h, via, via_exe) in &t.hits {
        let lead = match (h.kind, h.lead) {
            (Kind::Launched, Some(l)) => format!(" (tool call {} before the process started)", duration(l)),
            _ => String::new(),
        };
        println!(
            "  {:<9} {} session {}{}{lead}",
            if h.kind == Kind::Launched { "launched" } else { "mentioned" },
            h.harness,
            h.session,
            h.title.as_deref().map(|t| format!(" “{t}”")).unwrap_or_default()
        );
        println!("            {}:{}", h.path.display(), h.line);
        println!("            via {via_exe} {via}: {}", h.needle);
    }
    Ok(())
}

/// Each process seen in the last sample of a window, with its busiest engine reading over the window.
type Observed = Vec<(Row, f64)>;

/// Watch the card for `window` seconds.
fn observe_window(window: f64, all: bool, min_mib: u64) -> Result<(Vec<Adapter>, Observed), String> {
    let mut sampler = Sampler::open()?;
    sampler.prime();
    let dxgi = gpu::adapters();
    let mut scanner = Scanner::default();
    let mut busiest: HashMap<(u32, u64), f64> = HashMap::new();
    let (mut adapters, mut rows) = (Vec::new(), Vec::new());
    for _ in 0..(window.max(2.0).ceil() as u32) {
        std::thread::sleep(Duration::from_secs(1));
        let sample = sampler.sample();
        let table = procs::snapshot();
        adapters = pick_adapters(dxgi.clone(), &sample, all);
        let luids: Vec<u64> = adapters.iter().map(|a| a.luid).collect();
        rows = scanner.rows(&sample, &table, &luids, min_mib);
        for r in &rows {
            let b = busiest.entry((r.pid, r.luid)).or_insert(0.0);
            *b = b.max(r.util.unwrap_or(0.0));
        }
    }
    let observed = rows
        .into_iter()
        .map(|r| {
            let b = busiest[&(r.pid, r.luid)];
            (r, b)
        })
        .collect();
    Ok((adapters, observed))
}

/// Verdict for an idle row; processes vramorama cannot inspect are never judged stale.
pub fn judge(r: &Row) -> (Verdict, String) {
    if r.note.is_some() || is_host(&r.name) {
        return (Verdict::Idle, "a system or protected process".into());
    }
    classify(&r.owner, r.harness_live, r.root.as_deref())
}

/// `vramorama idle`: processes that held memory but did no GPU work for the whole window.
pub fn idle(o: &Opts, window: f64) -> Result<(), String> {
    let (adapters, rows) = observe_window(window, o.all, o.min_mib)?;
    let mut idle: Vec<(&Row, Verdict, String)> = rows
        .iter()
        .filter(|(_, busiest)| *busiest < IDLE_UTIL)
        .map(|(r, _)| {
            let (v, why) = judge(r);
            (r, v, why)
        })
        .collect();
    idle.sort_by(|a, b| (b.1 == Verdict::Stale).cmp(&(a.1 == Verdict::Stale)).then(b.0.mib.cmp(&a.0.mib)));
    if o.json {
        let list = idle
            .iter()
            .map(|(r, v, why)| {
                let mut j = crate::scan::row_json(r);
                if let Value::Obj(kv) = &mut j {
                    kv.push(("verdict".into(), if *v == Verdict::Stale { "stale" } else { "idle" }.into()));
                    kv.push(("why".into(), why.as_str().into()));
                }
                j
            })
            .collect();
        println!("{}", obj([("window_secs", window.into()), ("idle", Value::Arr(list))]));
        return Ok(());
    }
    let names: Vec<&str> = adapters.iter().map(|a| a.name.as_str()).collect();
    let total: u64 = idle.iter().map(|(r, _, _)| r.mib).sum();
    let stale: u64 = idle.iter().filter(|(_, v, _)| *v == Verdict::Stale).map(|(r, _, _)| r.mib).sum();
    println!(
        "{}: {} process(es) held {} MiB without using the GPU for {}s; {} MiB of it looks stale",
        names.join(", "),
        idle.len(),
        thousands(total),
        window,
        thousands(stale)
    );
    if idle.is_empty() {
        return Ok(());
    }
    let width = if o.wide { usize::MAX } else { crate::width() };
    println!("{:>7} {:>9}  {:<11}  {:<7}  {:<44}  WHY / COMMAND", "PID", "VRAM MiB", "STARTED", "VERDICT", "OWNER");
    for (r, v, why) in &idle {
        let line = format!(
            "{:>7} {:>9}  {:<11}  {:<7}  {:<44}  {why}: {}",
            r.pid,
            thousands(r.mib),
            r.started.map(fmt_short).unwrap_or_default(),
            if *v == Verdict::Stale { "stale" } else { "idle" },
            ellipsize(&owner_text(r), 44),
            command_text(r)
        );
        println!("{}", ellipsize(&line, width));
    }
    if stale > 0 {
        println!("\n`vramorama reclaim` checks the stale ones again and prints the command that would free them.");
    }
    Ok(())
}

/// `vramorama reclaim [PID...]`: check idle holders and print the commands that would free their
/// memory. vramorama never stops a process itself; whoever runs the printed command decides.
/// A process is cleared only if it did no GPU work during the window, is not a system process,
/// and nobody appears to want it (`idle::classify` says stale); `force` also clears idle
/// processes whose owner may still want them, with a warning. Without PIDs, every stale holder
/// is checked. Exit code 1 when any named PID is not cleared.
pub fn reclaim(pids: &[u32], force: bool, window: f64) -> Result<i32, String> {
    let (_, rows) = observe_window(window, true, 1)?;
    let pids: Vec<u32> = if pids.is_empty() {
        // Small holders (browsers, chat apps) are never worth reclaiming unasked.
        let mut stale: Vec<u32> = rows
            .iter()
            .filter(|(r, busiest)| r.mib >= 256 && *busiest < IDLE_UTIL && judge(r).0 == Verdict::Stale)
            .map(|(r, _)| r.pid)
            .collect();
        stale.dedup();
        if stale.is_empty() {
            println!("No stale GPU memory holders (checked for {window}s).");
        }
        stale
    } else {
        pids.to_vec()
    };
    let mut code = 0;
    let mut cleared = Vec::new();
    for pid in pids {
        let mine: Vec<&(Row, f64)> = rows.iter().filter(|(r, _)| r.pid == pid).collect();
        let Some((r, _)) = mine.first() else {
            println!("{pid}: holds no GPU memory; nothing to free");
            code = 1;
            continue;
        };
        let mib: u64 = mine.iter().map(|(r, _)| r.mib).sum();
        let busiest = mine.iter().map(|(_, b)| *b).fold(0.0, f64::max);
        let (verdict, why) = judge(r);
        let refuse = if r.note.is_some() || is_host(&r.name) {
            Some("it is a system or protected process".to_string())
        } else if busiest >= IDLE_UTIL {
            Some(format!("it used the GPU ({busiest:.0}%) during the last {window}s"))
        } else if verdict != Verdict::Stale && !force {
            Some(format!("{why}; add --force to include it anyway"))
        } else {
            None
        };
        match refuse {
            Some(reason) => {
                println!("{pid} {}: leave it: {reason}", r.name);
                code = 1;
            }
            None => {
                let warn =
                    if verdict == Verdict::Stale { String::new() } else { " WARNING: may still be wanted".into() };
                println!("{pid} {}: {} MiB, idle, {why}{warn}", r.name, thousands(mib));
                println!("    {}", command_text(r));
                cleared.push((pid, mib, r.started));
            }
        }
    }
    if !cleared.is_empty() {
        let total: u64 = cleared.iter().map(|(_, m, _)| m).sum();
        let ids: Vec<String> = cleared.iter().map(|(p, _, _)| p.to_string()).collect();
        println!("\nTo free {} MiB, run (PowerShell): Stop-Process -Id {}", thousands(total), ids.join(","));
        let starts: Vec<String> =
            cleared.iter().map(|(p, _, s)| format!("{p} started {}", s.map(fmt_short).unwrap_or("?".into()))).collect();
        println!("vramorama never stops processes itself. Pids get reused, so check first that they are still the");
        println!("processes it checked ({}).", starts.join(", "));
    }
    Ok(code)
}
