//! The Windows commands: ps, watch, trace, idle, reclaim.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use crate::Opts;
use crate::gpu::{self, Adapter, Sample, Sampler};
use crate::idle::{IDLE_UTIL, Verdict, classify};
use crate::json::{Value, obj};
use crate::owner::{Harness, Owner};
use crate::parse::{args_tail, basename, duration, ellipsize, needles, thousands};
use crate::procs;
use crate::scan::{Row, Scanner, adapter_json, is_host, pick_adapters};
use crate::sessions::{self, Titles};
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
    // Enough samples to see processes take turns, at any interval.
    let mut thrash = crate::thrash::Detector::new((12.0 * o.interval).max(60.0));
    loop {
        std::thread::sleep(Duration::from_secs_f64(o.interval));
        let sample = sampler.sample();
        let table = procs::snapshot();
        let adapters = pick_adapters(dxgi.clone(), &sample, o.all);
        let luids: Vec<u64> = adapters.iter().map(|a| a.luid).collect();
        let mut rows = scanner.rows(&sample, &table, &luids, o.min_mib);
        crate::scan::attach_ram(&mut rows, &procs::memory());
        let now = now_filetime();
        feed_thrash(&mut thrash, &adapters, &sample, &rows, filetime_to_unix(now));
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
            for a in &adapters {
                if let Some(t) = thrash.check(a.luid) {
                    screen.push_str(&format!("WARNING {}: {}\n", a.name, thrash_warning(&t, &rows)));
                }
            }
            screen.push_str(&render_marks(&marks, width));
            print!("{screen}");
            std::io::stdout().flush().ok();
        }
    }
}

/// Add one sample of every shown adapter to a thrash detector.
pub fn feed_thrash(det: &mut crate::thrash::Detector, adapters: &[Adapter], sample: &Sample, rows: &[Row], ts: f64) {
    for a in adapters {
        let used = sample.adapter_used.get(&a.luid).copied().unwrap_or(0) / MIB;
        let mine = rows.iter().filter(|r| r.luid == a.luid).map(|r| (r.pid, r.mib, r.ram.map(|m| m.private_mib)));
        det.push(a.luid, ts, used, a.dedicated / MIB, mine);
    }
}

/// A process in a thrash warning: `python.exe 25184 [lr-sweep, claude a91d07c4]`.
pub fn thrash_name(pid: u32, rows: &[Row]) -> String {
    let Some(r) = rows.iter().find(|r| r.pid == pid) else { return format!("pid {pid}") };
    let mut who: Vec<String> = r.owner.label.iter().cloned().collect();
    if r.owner.session().is_some() || who.is_empty() {
        who.push(r.session_group().1);
    }
    format!("{} {pid} [{}]", r.name, who.join(", "))
}

pub fn thrash_warning(t: &crate::thrash::Thrash, rows: &[Row]) -> String {
    crate::thrash::warning(t, |pid| thrash_name(pid, rows))
}

pub fn thrash_json(t: &crate::thrash::Thrash, rows: &[Row]) -> Value {
    let procs = t
        .procs
        .iter()
        .map(|p| {
            let r = rows.iter().find(|r| r.pid == p.pid);
            obj([
                ("pid", p.pid.into()),
                ("name", thrash_name(p.pid, rows).into()),
                ("low_mib", p.low_mib.into()),
                ("high_mib", p.high_mib.into()),
                ("reversals", (p.reversals as u64).into()),
                ("ram_antiphase", p.ram_antiphase.into()),
                ("session_key", r.map(|r| r.session_group().0).into()),
                ("cmd", r.and_then(|r| r.cmd.clone()).into()),
            ])
        })
        .collect();
    obj([
        ("procs", Value::Arr(procs)),
        ("period_secs", t.period_secs.into()),
        ("window_secs", t.window_secs.into()),
        ("samples", (t.samples as u64).into()),
        ("full_share", t.full_share.into()),
        ("warning", thrash_warning(t, rows).into()),
    ])
}

/// `vramorama thrash`: watch the card for `window` seconds, sampling every second, and say whether
/// processes are taking turns on it (see `thrash.rs`).
pub fn thrash(o: &Opts, window: f64) -> Result<(), String> {
    let mut sampler = Sampler::open()?;
    sampler.prime();
    let dxgi = gpu::adapters();
    let mut scanner = Scanner::default();
    let mut det = crate::thrash::Detector::new(window + 5.0);
    let (mut adapters, mut rows) = (Vec::new(), Vec::new());
    if !o.json {
        eprintln!("vramorama: watching for {window} s...");
    }
    for _ in 0..(window.ceil() as u32) {
        std::thread::sleep(Duration::from_secs(1));
        let sample = sampler.sample();
        let table = procs::snapshot();
        adapters = pick_adapters(dxgi.clone(), &sample, o.all);
        let luids: Vec<u64> = adapters.iter().map(|a| a.luid).collect();
        rows = scanner.rows(&sample, &table, &luids, o.min_mib);
        crate::scan::attach_ram(&mut rows, &procs::memory());
        feed_thrash(&mut det, &adapters, &sample, &rows, filetime_to_unix(now_filetime()));
    }
    if o.json {
        let list = adapters
            .iter()
            .map(|a| {
                obj([
                    ("name", a.name.as_str().into()),
                    ("thrash", det.check(a.luid).map(|t| thrash_json(&t, &rows)).into()),
                ])
            })
            .collect();
        println!("{}", obj([("window_secs", window.into()), ("adapters", Value::Arr(list))]));
        return Ok(());
    }
    let width = if o.wide { usize::MAX } else { crate::width() };
    for a in &adapters {
        let Some(t) = det.check(a.luid) else {
            println!("{}: no thrash in {window} s (no two processes took turns on the card)", a.name);
            continue;
        };
        println!(
            "{}: THRASHING. {} processes took turns on the card over {:.0} s (full {:.0}% of the time){}",
            a.name,
            t.procs.len(),
            t.window_secs,
            t.full_share * 100.0,
            t.period_secs.map(|s| format!(", a round about every {s:.0} s")).unwrap_or_default()
        );
        println!("{:>7} {:>17} {:>6}  {:<10}  {:<44}  COMMAND", "PID", "VRAM MiB LOW–HIGH", "TURNS", "RAM", "OWNER");
        for p in &t.procs {
            let Some(r) = rows.iter().find(|r| r.pid == p.pid) else { continue };
            let line = format!(
                "{:>7} {:>17} {:>6}  {:<10}  {:<44}  {}",
                p.pid,
                format!("{}–{}", thousands(p.low_mib), thousands(p.high_mib)),
                p.reversals,
                if p.ram_antiphase { "opposite" } else { "–" },
                ellipsize(&owner_text(r), 44),
                command_text(r)
            );
            println!("{}", ellipsize(&line, width));
        }
        println!(
            "\nEach turn moves gigabytes between VRAM and system memory, so these jobs run far slower than either would\n\
             alone. RAM \"opposite\": the process's RAM rose as its VRAM fell, where the evicted memory went. vramorama\n\
             stops nothing; pausing or stopping one of them would let the other run at full speed."
        );
    }
    Ok(())
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
            let label = match (r.owner.session(), &r.title) {
                (Some(_), Some(t)) if r.owner.label.is_none() => format!("{} “{t}”", r.session_group().1),
                (Some(_), None) if r.owner.label.is_none() => r.session_group().1,
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

/// The agent session in a few words: `claude 5f3c1e2a (live) “Night queue”`.
pub fn session_text(r: &Row) -> Option<String> {
    r.owner.session()?;
    let state = match r.harness_live {
        Some(true) => " (live)",
        Some(false) => " (ended)",
        None => "",
    };
    let title = r.title.as_deref().map(|t| format!(" “{t}”")).unwrap_or_default();
    Some(format!("{}{state}{title}", r.session_group().1))
}

pub fn owner_text(r: &Row) -> String {
    let o: &Owner = &r.owner;
    let lease = o.lease.as_deref().map(|l| format!(" #{l}")).unwrap_or_default();
    if let Some(l) = &o.label {
        // The label names the job; the session says who started it.
        return match session_text(r) {
            Some(s) => format!("{l}{lease} · {s}"),
            None => format!("{l}{lease}"),
        };
    }
    if let Some(s) = session_text(r) {
        return format!("{s}{lease}");
    }
    if let Some(a) = &o.agent {
        return format!("{a}{lease}");
    }
    if let Some(l) = &o.lease {
        return format!("lease {l}");
    }
    if let Some((_, what)) = crate::scan::known_process(&r.name) {
        return what.into();
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
        if let Some(w) = crate::spill::warning(&crate::scan::spills(a, sample, rows)) {
            out.push_str(&format!("  WARNING {w}\n"));
        }
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
    let mut owner = d.env.as_deref().map(Owner::from_env).unwrap_or_default();
    if owner.codex.is_some() {
        crate::scan::resolve_harness(&mut owner, pid, &table);
    }
    let (chain, exited) = procs::ancestry(pid, &table);
    let mut titles = Titles::default();
    let mut info = TraceInfo {
        pid,
        exe: entry.exe.clone(),
        title: titles.title(&owner),
        transcript: session_transcript(&owner),
        owner,
        chain: std::iter::once(pid).chain(chain.iter().copied()).map(|p| (table[&p].exe.clone(), p)).collect(),
        exited,
        tagged_ancestors: Vec::new(),
        search: None,
        hits: Vec::new(),
        details: d,
    };
    if info.owner.session().is_some() {
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
        .map(|mut h| {
            // The app's own name for the session beats a title found in the transcript.
            if let Some(t) = titles.for_hit(h.harness, &h.session) {
                h.title = Some(t);
            }
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
    if let (Some(s), Some(app)) = (t.owner.session(), t.owner.app()) {
        let what = if t.owner.harness() == Some(Harness::Codex) { "thread" } else { "session" };
        println!("  tagged    {app} {what} {s}{}", t.title.as_ref().map(|t| format!(" “{t}”")).unwrap_or_default());
        if let Some(l) = &t.owner.label {
            println!("  job       {l}");
        }
        if let Some(p) = &t.transcript {
            println!("  transcript {}", p.display());
        }
        return Ok(());
    }
    if let Some(l) = t.owner.label.as_ref().or(t.owner.agent.as_ref()) {
        println!("  tagged    {l}");
    } else {
        println!(
            "  tagged    no (no {} / CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID / AI_AGENT in its environment)",
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
            "  {:<9} {} {} {}{}{lead}",
            if h.kind == Kind::Launched { "launched" } else { "mentioned" },
            h.harness,
            if h.harness == "codex" { "thread" } else { "session" },
            h.session,
            h.title.as_deref().map(|t| format!(" “{t}”")).unwrap_or_default()
        );
        println!("            {}:{}", h.path.display(), h.line);
        println!("            via {via_exe} {via}: {}", h.needle);
    }
    Ok(())
}

/// Where the session that started a process keeps its transcript.
fn session_transcript(o: &Owner) -> Option<std::path::PathBuf> {
    match o.harness()? {
        Harness::Codex => sessions::codex_rollout(o.codex.as_deref()?, o.codex_home.as_deref()),
        Harness::Claude | Harness::Cowork => {
            transcripts::claude_transcript(o.claude.as_deref()?, o.claude_config.as_deref())
        }
    }
}

/// One look at the machine: the GPU processes, every process in an agent session (and big
/// untagged ones) with its RAM, and the system's memory. Without GPU counters only RAM is seen.
struct Look {
    adapters: Vec<Adapter>,
    vram_used: u64,
    vram_total: u64,
    gpu: Vec<Row>,
    ram: Vec<Row>,
    sys: Option<procs::SysMem>,
    table: HashMap<u32, procs::Entry>,
}

fn look(o: &Opts) -> Look {
    let mut sampler = Sampler::open().ok();
    if let Some(s) = sampler.as_mut() {
        s.prime();
        std::thread::sleep(Duration::from_millis(1000));
    }
    let table = procs::snapshot();
    let mem = procs::memory();
    let mut scanner = Scanner::default();
    let (mut adapters, mut gpu, mut vram_used) = (Vec::new(), Vec::new(), 0);
    if let Some(s) = sampler.as_mut() {
        let sample = s.sample();
        adapters = pick_adapters(gpu::adapters(), &sample, o.all);
        let luids: Vec<u64> = adapters.iter().map(|a| a.luid).collect();
        gpu = scanner.rows(&sample, &table, &luids, o.min_mib);
        crate::scan::attach_ram(&mut gpu, &mem);
        vram_used = adapters.iter().map(|a| sample.adapter_used.get(&a.luid).copied().unwrap_or(0) / MIB).sum();
    }
    let ram = scanner.ram_rows(&table, &mem);
    let vram_total = adapters.iter().map(|a| a.dedicated / MIB).sum();
    Look { adapters, vram_used, vram_total, gpu, ram, sys: procs::system_memory(), table }
}

fn print_header(l: &Look) {
    if !l.adapters.is_empty() {
        let names: Vec<&str> = l.adapters.iter().map(|a| a.name.as_str()).collect();
        println!("{}: {} of {} MiB in use", names.join(", "), thousands(l.vram_used), thousands(l.vram_total));
    }
    if let Some(s) = &l.sys {
        let used = (s.total - s.avail.min(s.total)) / MIB;
        let commit = (s.commit_limit - s.commit_avail.min(s.commit_limit)) / MIB;
        println!(
            "RAM: {} of {} MiB in use ({} available); {} of {} MiB committed",
            thousands(used),
            thousands(s.total / MIB),
            thousands(s.avail / MIB),
            thousands(commit),
            thousands(s.commit_limit / MIB)
        );
    }
}

/// A session is listed when it holds GPU memory or at least this much RAM (`--min-ram`).
const SESSION_MIN_RAM_MIB: u64 = 256;

/// `vramorama sessions`: GPU memory and RAM per agent session (Claude Code, Cowork, the ChatGPT
/// app's Codex, the Codex CLI), and per app.
pub fn sessions(o: &Opts) -> Result<(), String> {
    let l = look(o);
    let groups = crate::agents::group(&l.gpu, &l.ram);
    let apps = crate::agents::apps(&groups, &l.table);
    if o.json {
        println!(
            "{}",
            obj([
                ("time", fmt_utc(now_filetime()).into()),
                ("used_mib", l.vram_used.into()),
                ("total_mib", l.vram_total.into()),
                ("memory", l.sys.as_ref().map(crate::scan::system_memory_json).into()),
                ("sessions", Value::Arr(groups.iter().map(crate::agents::group_json).collect())),
                ("apps", Value::Arr(apps.iter().map(crate::agents::app_json).collect())),
            ])
        );
        return Ok(());
    }
    print_header(&l);
    println!();
    let min_ram = o.min_ram.unwrap_or(SESSION_MIN_RAM_MIB);
    let (shown, hidden): (Vec<&crate::agents::Group>, Vec<&crate::agents::Group>) =
        groups.iter().partition(|g| g.mib > 0 || g.ram_mib + g.agent_mib >= min_ram);
    if shown.is_empty() {
        println!("No session holds GPU memory or {min_ram} MiB of RAM.");
    } else {
        let width = if o.wide { usize::MAX } else { crate::width() };
        println!(
            "{:<26} {:<16} {:<6} {:>9} {:>6} {:>9} {:>7} {:>5}  TITLE / JOBS",
            "SESSION", "APP", "STATE", "VRAM MiB", "SHARE", "RAM MiB", "AGENT", "PROCS"
        );
        for g in &shown {
            let state = match g.live {
                Some(true) => "live",
                Some(false) => "ended",
                None => "–",
            };
            let mut what: Vec<String> = g.title.iter().map(|t| format!("“{t}”")).collect();
            what.extend(g.jobs.iter().cloned());
            let dash = |v: u64| if v > 0 { thousands(v) } else { "–".into() };
            let line = format!(
                "{:<26} {:<16} {:<6} {:>9} {:>5.0}% {:>9} {:>7} {:>5}  {}",
                ellipsize(&g.label, 26),
                g.app.unwrap_or("–"),
                state,
                dash(g.mib),
                if l.vram_total > 0 { g.mib as f64 * 100.0 / l.vram_total as f64 } else { 0.0 },
                dash(g.ram_mib),
                dash(g.agent_mib),
                g.ram_procs.max(g.pids.len()),
                what.join(" · ")
            );
            println!("{}", ellipsize(&line, width));
        }
    }
    if !hidden.is_empty() {
        let mib: u64 = hidden.iter().map(|g| g.ram_mib + g.agent_mib).sum();
        println!(
            "  and {} more holding no GPU memory and under {min_ram} MiB of RAM each ({} MiB in all; --min-ram 0 lists them)",
            hidden.len(),
            thousands(mib)
        );
    }
    println!(
        "\nRAM is each process's private working set (Task Manager's Memory column); AGENT is the agent's own process.\n"
    );
    for a in &apps {
        let state = match a.running {
            Some(true) if a.sessions == 0 => " (running; no session holds GPU memory or RAM)",
            Some(false) => " (not running)",
            _ => "",
        };
        println!(
            "  {:<24} VRAM {:>7} MiB in {} · RAM {:>7} MiB in jobs + {} agent · {} {}{state}",
            a.name,
            thousands(a.mib),
            a.gpu_sessions,
            thousands(a.ram_mib),
            thousands(a.agent_mib),
            a.sessions,
            if a.harness.is_some() { "session(s)" } else { "group(s)" }
        );
        if let (Some(n), 0) = (a.note, a.gpu_sessions) {
            println!("  {:<24} {n}", "");
        }
    }
    Ok(())
}

/// `vramorama ram`: the processes of agent sessions by RAM, with the agents' own processes and
/// untagged processes of 1 GiB or more.
pub fn ram(o: &Opts) -> Result<(), String> {
    let l = look(o);
    let min = if o.min_given { o.min_mib } else { 64 };
    let rows: Vec<&Row> = l.ram.iter().filter(|r| r.ram.is_some_and(|m| m.private_mib >= min)).collect();
    let vram = |pid: u32| -> u64 { l.gpu.iter().filter(|g| g.pid == pid).map(|g| g.mib).sum() };
    if o.json {
        let list = rows
            .iter()
            .map(|r| {
                let mut j = crate::scan::row_json(r);
                if let Value::Obj(kv) = &mut j {
                    kv.push(("session_key".into(), r.session_group().0.into()));
                    kv.push(("vram_mib".into(), vram(r.pid).into()));
                }
                j
            })
            .collect();
        println!(
            "{}",
            obj([
                ("time", fmt_utc(now_filetime()).into()),
                ("memory", l.sys.as_ref().map(crate::scan::system_memory_json).into()),
                ("processes", Value::Arr(list)),
            ])
        );
        return Ok(());
    }
    print_header(&l);
    let sum = |f: &dyn Fn(&Row) -> bool| -> (u64, usize) {
        let m: Vec<&Row> = l.ram.iter().filter(|r| f(r)).collect();
        (m.iter().map(|r| r.ram.map_or(0, |x| x.private_mib)).sum(), m.len())
    };
    let (jobs, jobs_n) = sum(&|r| !r.agent_proc && r.owner.is_tagged());
    let (agents, agents_n) = sum(&|r| r.agent_proc);
    let (big, big_n) = sum(&|r| !r.agent_proc && !r.owner.is_tagged());
    println!(
        "Agent sessions' jobs: {} MiB in {jobs_n} processes; the agents themselves: {} MiB in {agents_n}; \
         untagged processes of 1 GiB or more: {} MiB in {big_n}\n",
        thousands(jobs),
        thousands(agents),
        thousands(big)
    );
    if rows.is_empty() {
        println!("No process in an agent session holds {min} MiB of RAM or more.");
        return Ok(());
    }
    let width = if o.wide { usize::MAX } else { crate::width() };
    println!(
        "{:>7} {:>8} {:>8} {:>8}  {:<11}  {:<44}  COMMAND",
        "PID", "RAM MiB", "COMMIT", "VRAM", "STARTED", "OWNER"
    );
    for r in rows {
        let m = r.ram.unwrap_or_default();
        let owner = if r.agent_proc {
            format!("agent: {}", session_text(r).unwrap_or_else(|| owner_text(r)))
        } else {
            owner_text(r)
        };
        let v = vram(r.pid);
        let line = format!(
            "{:>7} {:>8} {:>8} {:>8}  {:<11}  {:<44}  {}",
            r.pid,
            thousands(m.private_mib),
            thousands(m.commit_mib),
            if v > 0 { thousands(v) } else { "–".into() },
            r.started.map(fmt_short).unwrap_or_default(),
            ellipsize(&owner, 44),
            command_text(r)
        );
        println!("{}", ellipsize(&line, width));
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
    // A job whose claude.exe exited is not stale while its session runs on in another one.
    let live = crate::scan::live_agent_sessions(&scanner.ram_rows(&procs::snapshot(), &procs::memory()));
    crate::scan::revive(&mut rows, &live);
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
