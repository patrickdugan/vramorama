//! Summarise a `watch --log` file: who held how much VRAM, for how long.

use std::collections::{BTreeSet, HashMap};

use crate::json::{self, Value, obj};
use crate::parse::{args_tail, ellipsize, thousands};

#[derive(Debug, Default)]
pub struct Holder {
    pub key: String,
    pub title: Option<String>,
    pub first: f64,
    pub last: f64,
    pub peak_mib: f64,
    pub gib_hours: f64,
    pub max_util: f64,
    pub pids: BTreeSet<u64>,
    pub example_cmd: Option<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub samples: usize,
    pub start: f64,
    pub end: f64,
    pub peak_used_mib: f64,
    pub holders: Vec<Holder>,
    pub bad_lines: usize,
}

/// Group key for a logged process: its owner tag, or for untagged processes the executable and pid.
fn key_of(p: &Value) -> String {
    match p.str("owner") {
        Some(o) => o.to_string(),
        None => format!("untagged {} pid {}", p.str("name").unwrap_or("?"), p.f64("pid").unwrap_or(0.0)),
    }
}

pub fn summarise(log: &str) -> Report {
    let mut samples: Vec<Value> = Vec::new();
    let mut bad_lines = 0;
    for line in log.lines().filter(|l| !l.trim().is_empty()) {
        match json::parse(line) {
            Ok(v) if v.f64("ts").is_some() => samples.push(v),
            _ => bad_lines += 1,
        }
    }
    samples.sort_by(|a, b| a.f64("ts").unwrap().total_cmp(&b.f64("ts").unwrap()));
    let mut r = Report { samples: samples.len(), bad_lines, ..Default::default() };
    if samples.is_empty() {
        return r;
    }
    let ts: Vec<f64> = samples.iter().map(|s| s.f64("ts").unwrap()).collect();
    let mut gaps: Vec<f64> = ts.windows(2).map(|w| w[1] - w[0]).filter(|g| *g > 0.0).collect();
    gaps.sort_by(f64::total_cmp);
    let typical = gaps.get(gaps.len() / 2).copied().unwrap_or(0.0);
    r.start = ts[0];
    r.end = *ts.last().unwrap();

    let mut by_key: HashMap<String, Holder> = HashMap::new();
    for (i, s) in samples.iter().enumerate() {
        // Weight each sample by the time until the next one, capped so a sleep or a stopped
        // watcher does not count as hours of use.
        let dt = ts.get(i + 1).map_or(typical, |next| (next - ts[i]).min(3.0 * typical));
        let mut used = 0.0;
        for a in s.arr("adapters") {
            used += a.f64("used_mib").unwrap_or(0.0);
            for p in a.arr("procs") {
                let mib = p.f64("mib").unwrap_or(0.0);
                let key = key_of(p);
                let h = by_key.entry(key.clone()).or_insert_with(|| Holder { key, first: ts[i], ..Default::default() });
                h.last = ts[i];
                h.peak_mib = h.peak_mib.max(mib);
                h.gib_hours += mib / 1024.0 * dt / 3600.0;
                h.max_util = h.max_util.max(p.f64("util").unwrap_or(0.0));
                if let Some(pid) = p.f64("pid") {
                    h.pids.insert(pid as u64);
                }
                if h.title.is_none() {
                    h.title = p.str("title").map(str::to_string);
                }
                if h.example_cmd.is_none() {
                    let name = p.str("name").unwrap_or("?");
                    h.example_cmd = p.str("cmd").map(|c| format!("{name} {}", args_tail(c)).trim_end().to_string());
                }
            }
        }
        r.peak_used_mib = r.peak_used_mib.max(used);
    }
    r.holders = by_key.into_values().collect();
    r.holders.sort_by(|a, b| b.gib_hours.total_cmp(&a.gib_hours));
    r
}

fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}h{:02}m", s / 3600, s / 60 % 60)
}

pub fn render(r: &Report, width: usize) -> String {
    let mut out = String::new();
    if r.samples == 0 {
        out.push_str("no samples in log\n");
        return out;
    }
    out.push_str(&format!(
        "{} samples over {}; peak VRAM in use {} MiB{}\n\n",
        r.samples,
        hms(r.end - r.start),
        thousands(r.peak_used_mib as u64),
        if r.bad_lines > 0 { format!(" ({} unreadable lines skipped)", r.bad_lines) } else { String::new() }
    ));
    out.push_str(&format!(
        "{:<34} {:>8} {:>9} {:>5} {:>7}  {}\n",
        "OWNER", "GiB·h", "PEAK MiB", "GPU%", "HELD", "WHAT"
    ));
    for h in &r.holders {
        let what = h.title.clone().or_else(|| h.example_cmd.clone()).unwrap_or_default();
        let line = format!(
            "{:<34} {:>8.2} {:>9} {:>5.0} {:>7}  {}",
            ellipsize(&h.key, 34),
            h.gib_hours,
            thousands(h.peak_mib as u64),
            h.max_util,
            hms(h.last - h.first),
            what
        );
        out.push_str(&ellipsize(&line, width));
        out.push('\n');
    }
    out
}

pub fn to_json(r: &Report) -> Value {
    let holders = r
        .holders
        .iter()
        .map(|h| {
            obj([
                ("owner", h.key.as_str().into()),
                ("title", h.title.clone().into()),
                ("gib_hours", ((h.gib_hours * 1000.0).round() / 1000.0).into()),
                ("peak_mib", h.peak_mib.into()),
                ("max_util", h.max_util.into()),
                ("first_ts", h.first.into()),
                ("last_ts", h.last.into()),
                ("pids", Value::Arr(h.pids.iter().map(|&p| Value::Num(p as f64)).collect())),
                ("cmd", h.example_cmd.clone().into()),
            ])
        })
        .collect();
    obj([
        ("samples", (r.samples as u64).into()),
        ("start_ts", r.start.into()),
        ("end_ts", r.end.into()),
        ("peak_used_mib", r.peak_used_mib.into()),
        ("holders", Value::Arr(holders)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: f64, procs: &str) -> String {
        format!(r#"{{"ts":{ts},"adapters":[{{"name":"GPU","used_mib":9000,"procs":[{procs}]}}]}}"#)
    }

    #[test]
    fn integrates_per_owner_and_caps_gaps() {
        let q =
            r#"{"pid":8352,"mib":8192,"util":90,"name":"python.exe","owner":"claude:5f3c1e2a","title":"Night queue"}"#;
        let l = r#"{"pid":500,"mib":1024,"util":0,"name":"llama-server.exe","owner":null}"#;
        let log = [
            line(0.0, &format!("{q},{l}")),
            line(60.0, &format!("{q},{l}")),
            line(120.0, l),
            // 10-hour gap (laptop asleep): counts as at most 3 typical intervals.
            line(36120.0, l),
            "not json".to_string(),
        ]
        .join("\n");
        let r = summarise(&log);
        assert_eq!(r.samples, 4);
        assert_eq!(r.bad_lines, 1);
        let q = r.holders.iter().find(|h| h.key == "claude:5f3c1e2a").unwrap();
        assert!((q.gib_hours - 8.0 * 120.0 / 3600.0).abs() < 1e-9);
        assert_eq!(q.title.as_deref(), Some("Night queue"));
        let l = r.holders.iter().find(|h| h.key == "untagged llama-server.exe pid 500").unwrap();
        // 60 + 60 + min(36000, 180) + 60 (last sample uses the typical interval) seconds at 1 GiB.
        assert!((l.gib_hours - 360.0 / 3600.0).abs() < 1e-9, "{}", l.gib_hours);
        assert_eq!(l.last - l.first, 36120.0);
        assert!(render(&r, 200).contains("claude:5f3c1e2a"));
    }

    #[test]
    fn empty_log() {
        assert_eq!(summarise("").samples, 0);
        assert!(render(&summarise(""), 80).contains("no samples"));
    }
}
