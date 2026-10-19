//! `vramorama crashes` and `vramorama doctor`: read-only crash forensics and risk checks.

use std::io::BufRead;
use std::path::PathBuf;

use crate::Opts;
use crate::crash;
use crate::doctor;
use crate::sys::fmt_local_unix;

const DEFAULT_DAYS: f64 = 30.0;
const DEFAULT_LAST: usize = 10;
/// Watch logs found in the ledger folder: this many newest files.
const MAX_AUTO_LOGS: usize = 8;

fn unix_now() -> f64 {
    crate::sys::filetime_to_unix(crate::sys::now_filetime())
}

/// Watch logs to line crashes up with: the one named with `--log`, else the `.jsonl` files in the ledger
/// folder (`~/.vramorama`), newest first.
fn watch_logs(explicit: Option<&str>) -> Result<Vec<PathBuf>, String> {
    if let Some(p) = explicit {
        let path = PathBuf::from(p);
        if !path.is_file() {
            return Err(format!("{p}: no such watch log"));
        }
        return Ok(vec![path]);
    }
    let Some(dir) = crate::ledger::dir() else { return Ok(Vec::new()) };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("jsonl")))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|a| std::cmp::Reverse(a.0));
    Ok(files.into_iter().take(MAX_AUTO_LOGS).map(|(_, p)| p).collect())
}

/// Read the event logs and analyse them, and line the crashes up with the watch logs.
pub fn report(days: f64, log: Option<&str>) -> Result<(crash::Report, Vec<PathBuf>), String> {
    let since = unix_now() - days * 86_400.0;
    let events = crate::sysinfo::read_log(days)?;
    let mut r = crash::analyse(&events, days, since);
    let paths = watch_logs(log)?;
    let windows = crash::watch_windows(&r);
    let mut samples = Vec::new();
    let mut span: Option<(f64, f64)> = None;
    for p in &paths {
        let Ok(f) = std::fs::File::open(p) else { continue };
        let (s, sp) = crash::load_watch_lines(std::io::BufReader::new(f).lines().map_while(Result::ok), &windows);
        samples.extend(s);
        span = match (span, sp) {
            (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
            (x, None) | (None, x) => x,
        };
    }
    samples.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    crash::attach_watch(&mut r, &samples, span);
    Ok((r, paths))
}

pub fn crashes(o: &Opts) -> Result<(), String> {
    let days = o.days.unwrap_or(DEFAULT_DAYS);
    let (r, logs) = report(days, o.log.as_deref())?;
    if o.json {
        println!("{}", crash::to_json(&r));
        return Ok(());
    }
    let fmt = |t: f64| fmt_local_unix(t);
    print!("{}", crash::render(&r, o.last.unwrap_or(DEFAULT_LAST), &fmt));
    if logs.is_empty() {
        println!(
            "\nNo watch log found. To see what held the GPU and RAM in the minutes before the next crash, keep one running:\n  vramorama watch --quiet --log \"{}\"",
            crate::ledger::dir().map_or("watch.jsonl".to_string(), |d| d.join("watch.jsonl").display().to_string())
        );
    } else {
        println!(
            "\nWatch log(s) read: {}",
            logs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

pub fn doctor(o: &Opts) -> Result<(), String> {
    let days = o.days.unwrap_or(DEFAULT_DAYS);
    // The crash record is part of the picture; without event log access the other checks still run.
    let r = report(days, o.log.as_deref()).map(|(r, _)| r);
    let facts = crate::sysinfo::facts(r.as_ref().ok());
    let findings = doctor::advise(&facts);
    if o.json {
        println!("{}", doctor::to_json(&findings, &facts));
        return Ok(());
    }
    println!("vramorama doctor: read-only checks; it changes no setting (as of {})\n", fmt_local_unix(facts.now));
    print!("{}", doctor::render(&findings, crate::width()));
    if let Err(e) = r {
        println!("\nThe event log could not be read ({e}), so crash history and GPU resets are missing above.");
    }
    Ok(())
}

/// How many of the latest incidents the GUI card lists.
const GUI_INCIDENTS: usize = 6;

/// The risk checks and the latest crashes as one JSON document, for the GUI card.
pub fn health_json() -> crate::json::Value {
    use crate::json::{Value, obj};
    let report = report(DEFAULT_DAYS, None).map(|(r, _)| r);
    let facts = crate::sysinfo::facts(report.as_ref().ok());
    let findings = doctor::advise(&facts);
    let incidents: Vec<Value> = report
        .as_ref()
        .map(|r| {
            r.incidents
                .iter()
                .take(GUI_INCIDENTS)
                .map(|i| {
                    let blamed = i.blamed.as_ref().map(|(m, f)| match f {
                        Some(f) => format!("{m}!{f}"),
                        None => m.clone(),
                    });
                    obj([
                        ("crashed", i.crashed.into()),
                        ("title", crash::bugcheck_title(i).into()),
                        ("verdict", i.verdict.cause.tag().into()),
                        ("explanation", i.verdict.summary.as_str().into()),
                        ("standby", i.standby.is_some().into()),
                        ("blamed", blamed.into()),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    obj([
        ("as_of", facts.now.into()),
        ("days", DEFAULT_DAYS.into()),
        ("reboots", report.as_ref().map_or(0, |r| r.incidents.len() as u64).into()),
        ("error", report.as_ref().err().cloned().into()),
        ("findings", doctor::to_json(&findings, &facts).get("findings").cloned().unwrap_or(Value::Null)),
        ("incidents", Value::Arr(incidents)),
    ])
}
