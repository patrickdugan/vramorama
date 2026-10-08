//! The Windows commands: ps, watch, trace.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use crate::Opts;
use crate::gpu::{self, Adapter, Sample, Sampler};
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
            let p = peaks.entry((r.pid, r.luid)).or_insert(0);
            *p = (*p).max(r.mib);
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
            screen.push_str(&render(&adapters, &sample, &rows, Some(&peaks), width));
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

fn mark_key(r: &Row) -> (String, String) {
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

fn owner_text(r: &Row) -> String {
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
fn command_text(r: &Row) -> String {
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

pub fn render(
    adapters: &[Adapter],
    sample: &Sample,
    rows: &[Row],
    peaks: Option<&HashMap<(u32, u64), u64>>,
    width: usize,
) -> String {
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
        let peak_h = if peaks.is_some() { format!(" {:>9}", "PEAK MiB") } else { String::new() };
        out.push_str(&format!(
            "{:>7} {:>9}{peak_h} {:>5}  {:<11}  {:<OWNER_W$}  {}\n",
            "PID", "VRAM MiB", "GPU%", "STARTED", "OWNER", "COMMAND"
        ));
        for r in mine {
            let peak = match peaks {
                Some(p) => format!(" {:>9}", thousands(p.get(&(r.pid, r.luid)).copied().unwrap_or(r.mib))),
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

pub fn trace(pid: u32) -> Result<(), String> {
    let table = procs::snapshot();
    let entry = table.get(&pid).ok_or(format!("no process {pid}"))?;
    let d = procs::details(pid);
    println!(
        "pid {pid} {}  started {}{}",
        entry.exe,
        d.created.map(fmt_short).unwrap_or("?".into()),
        d.cwd.as_deref().map(|c| format!("  in {c}")).unwrap_or_default()
    );
    if let Some(c) = &d.cmdline {
        println!("  command   {c}");
    }
    if let Some(note) = d.note {
        println!("  note      {note}");
    }
    let owner = d.env.as_deref().map(Owner::from_env).unwrap_or_default();
    if let Some(s) = &owner.session {
        let title = transcripts::session_title(s);
        println!("  tagged    Claude Code session {s}{}", title.map(|t| format!(" “{t}”")).unwrap_or_default());
        if let Some(p) = transcripts::claude_transcript(s) {
            println!("  transcript {}", p.display());
        }
        return Ok(());
    }
    if let Some(l) = owner.label.as_ref().or(owner.agent.as_ref()) {
        println!("  tagged    {l}");
    } else {
        println!(
            "  tagged    no (no {} / CLAUDE_CODE_SESSION_ID / AI_AGENT in its environment)",
            crate::owner::LABEL_VAR
        );
    }

    let (chain, gone) = procs::ancestry(pid, &table);
    let mut line: Vec<String> =
        std::iter::once(pid).chain(chain.iter().copied()).map(|p| format!("{} {p}", table[&p].exe)).collect();
    if let Some(g) = gone {
        line.push(format!("[exited {g}]"));
    }
    println!("  ancestry  {}", line.join(" <- "));

    // Each candidate command line, with the process that ran it and when that process started.
    let mut wanted: Vec<(u32, Needle)> = Vec::new();
    for p in std::iter::once(pid).chain(chain.iter().copied()) {
        if is_host(&table[&p].exe) {
            break;
        }
        let pd = if p == pid { d.clone() } else { procs::details(p) };
        if p != pid {
            let o = pd.env.as_deref().map(Owner::from_env).unwrap_or_default();
            if let Some(k) = o.key() {
                println!("  ancestor  {} {p} is tagged {k}", table[&p].exe);
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
        println!("  search    nothing distinctive to search for in these command lines");
        return Ok(());
    }
    // The launching session wrote to its transcript when it ran the command, so older files can be skipped.
    let earliest = wanted.iter().filter_map(|(_, n)| n.started).min_by(f64::total_cmp);
    let since_unix = earliest.map(|s| s - 300.0);
    let list: Vec<Needle> = wanted.iter().map(|(_, n)| Needle { text: n.text.clone(), started: n.started }).collect();
    let (scanned, hits) = transcripts::search(&list, since_unix);
    println!(
        "  search    {scanned} transcripts modified since {}",
        since_unix.map_or("ever".into(), |s| fmt_short(unix_to_filetime(s)))
    );
    if hits.is_empty() {
        println!("  result    no transcript contains these command lines; likely started by hand or by a script");
    } else if hits[0].kind != Kind::Launched {
        println!("  result    no session typed this command before the process started; the sessions below only");
        println!("            mention it (the launcher may have built it from variables: check the earliest)");
    }
    for h in &hits {
        let via = wanted.iter().find(|(_, n)| n.text == h.needle).map_or(pid, |(p, _)| *p);
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
        println!("            via {} {via}: {}", table[&via].exe, h.needle);
    }
    Ok(())
}
