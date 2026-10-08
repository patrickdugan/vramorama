//! Agent transcripts on disk: session titles, and finding which session typed a command line.
//!
//! Claude Code keeps one JSONL file per session at `~/.claude/projects/<project>/<session>.jsonl`,
//! where `<session>` equals the `CLAUDE_CODE_SESSION_ID` its shells export. Codex keeps
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::json::{escape, string_at};
use crate::parse::parse_iso_utc;

pub fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

pub fn roots() -> Vec<(&'static str, PathBuf)> {
    let Some(h) = home() else { return Vec::new() };
    vec![("claude", h.join(".claude").join("projects")), ("codex", h.join(".codex").join("sessions"))]
}

/// The transcript of a Claude Code session, if it is on this machine.
pub fn claude_transcript(session: &str) -> Option<PathBuf> {
    if session.is_empty() || session.contains(['/', '\\', '.']) {
        return None;
    }
    let projects = home()?.join(".claude").join("projects");
    std::fs::read_dir(projects).ok()?.flatten().map(|d| d.path().join(format!("{session}.jsonl"))).find(|p| p.is_file())
}

/// The latest title recorded in a transcript (Claude Code writes `custom-title` records).
pub fn title_in(content: &str) -> Option<String> {
    ["\"customTitle\":", "\"aiTitle\":"].iter().find_map(|key| {
        let at = content.rfind(key)? + key.len();
        string_at(content, at).filter(|t| !t.is_empty())
    })
}

pub fn session_title(session: &str) -> Option<String> {
    let text = std::fs::read(claude_transcript(session)?).ok()?;
    title_in(&String::from_utf8_lossy(&text))
}

/// A command line to look for, and when the process that ran it started (Unix seconds).
pub struct Needle {
    pub text: String,
    pub started: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// In a tool call the agent made before the process started: this session ran it.
    Launched,
    /// Only in tool output, or typed after the process had already started.
    Mentioned,
}

#[derive(Debug)]
pub struct Hit {
    pub harness: &'static str,
    pub session: String,
    pub path: PathBuf,
    pub title: Option<String>,
    pub needle: String,
    pub line: usize,
    pub kind: Kind,
    /// Seconds from the tool call to the process start, when both are known.
    pub lead: Option<f64>,
}

/// Tool calls are logged when the model emits them, before the tool runs; allow for clock
/// granularity between the transcript and the process start time.
const SKEW_SECS: f64 = 5.0;

/// Find transcripts that contain any of the needles (raw command-line text, matched in its
/// JSON-escaped form since transcripts store commands inside JSON strings). Only files modified at
/// or after `since_unix` are read: the launching session wrote to its transcript when it ran the
/// command. Returns the number of files read and the hits: launches first, closest to the process
/// start first, then mentions, newest file first.
pub fn search(needles: &[Needle], since_unix: Option<f64>) -> (usize, Vec<Hit>) {
    search_in(&roots(), needles, since_unix)
}

pub fn search_in(roots: &[(&'static str, PathBuf)], needles: &[Needle], since_unix: Option<f64>) -> (usize, Vec<Hit>) {
    let escaped: Vec<(String, &Needle)> = needles.iter().map(|n| (escape(&n.text), n)).collect();
    let mut files = Vec::new();
    for (harness, root) in roots {
        collect_jsonl(root, harness, since_unix, &mut files, 0);
    }
    files.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut hits = Vec::new();
    for (harness, path, _) in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let mut best: Option<(Kind, Option<f64>, usize, &Needle)> = None;
        for (esc, needle) in &escaped {
            for (at, _) in text.match_indices(esc.as_str()).take(200) {
                let record = line_around(&text, at);
                let ts = record_time(record);
                let before_start = match (ts, needle.started) {
                    (Some(t), Some(s)) => t <= s + SKEW_SECS,
                    _ => true,
                };
                let kind = if is_tool_call(record) && before_start { Kind::Launched } else { Kind::Mentioned };
                let lead = ts.zip(needle.started).map(|(t, s)| s - t);
                let better = match best {
                    None => true,
                    Some((Kind::Mentioned, _, _, _)) if kind == Kind::Launched => true,
                    Some((k, l, a, _)) if k == kind => match kind {
                        Kind::Launched => lead.unwrap_or(f64::MAX) < l.unwrap_or(f64::MAX),
                        Kind::Mentioned => at < a,
                    },
                    _ => false,
                };
                if better {
                    best = Some((kind, lead, at, needle));
                }
            }
        }
        if let Some((kind, lead, at, needle)) = best {
            hits.push(Hit {
                harness,
                session: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                path: path.clone(),
                title: title_in(&text),
                needle: needle.text.clone(),
                line: text[..at].bytes().filter(|&b| b == b'\n').count() + 1,
                kind,
                lead,
            });
        }
    }
    // Stable sort: mentions keep newest-file-first order.
    hits.sort_by(|a, b| match (a.kind, b.kind) {
        (Kind::Launched, Kind::Launched) => a.lead.unwrap_or(f64::MAX).total_cmp(&b.lead.unwrap_or(f64::MAX)),
        (Kind::Launched, Kind::Mentioned) => std::cmp::Ordering::Less,
        (Kind::Mentioned, Kind::Launched) => std::cmp::Ordering::Greater,
        (Kind::Mentioned, Kind::Mentioned) => std::cmp::Ordering::Equal,
    });
    (files.len(), hits)
}

fn line_around(text: &str, at: usize) -> &str {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    &text[start..end]
}

/// A transcript record of the agent calling a tool (Claude Code `tool_use`, Codex `function_call`)
/// as opposed to a tool's output coming back.
fn is_tool_call(record: &str) -> bool {
    (record.contains("\"type\":\"tool_use\"") || record.contains("\"type\":\"function_call\""))
        && !record.contains("\"type\":\"tool_result\"")
}

/// The record's `"timestamp"` (both Claude Code and Codex write one per line), as Unix seconds.
fn record_time(record: &str) -> Option<f64> {
    const KEY: &str = "\"timestamp\":\"";
    let at = record.find(KEY)? + KEY.len();
    let end = record[at..].find('"')? + at;
    parse_iso_utc(&record[at..end])
}
fn collect_jsonl(
    dir: &Path,
    harness: &'static str,
    since: Option<f64>,
    out: &mut Vec<(&'static str, PathBuf, f64)>,
    depth: usize,
) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth < 6 {
                collect_jsonl(&path, harness, since, out, depth + 1);
            }
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            let mtime =
                meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64());
            if since.is_none_or(|s| mtime >= s) {
                out.push((harness, path, mtime));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_title_wins() {
        let t = "{\"type\":\"custom-title\",\"customTitle\":\"First\"}\n{\"type\":\"custom-title\",\"customTitle\":\"Eval-sweep \\\"Qwen\\\" research\"}\n";
        assert_eq!(title_in(t).as_deref(), Some("Eval-sweep \"Qwen\" research"));
        assert_eq!(title_in("{\"type\":\"user\"}"), None);
    }

    fn needle(text: &str, started: Option<f64>) -> Vec<Needle> {
        vec![Needle { text: text.to_string(), started }]
    }

    #[test]
    fn launch_must_precede_the_process() {
        let dir = std::env::temp_dir().join(format!("vramorama-launch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Process started 2026-10-08T04:08:48Z = 1791432528.
        let started = Some(1_791_432_528.0);
        let seen = r#"{"type":"user","timestamp":"2026-10-08T04:30:00Z","message":{"content":[{"type":"tool_result","content":"PID 1 python.exe -u hold.py 768 45"}]}}"#;
        let launch = r#"{"type":"assistant","timestamp":"2026-10-08T04:08:45.120Z","message":{"content":[{"type":"tool_use","input":{"command":"python.exe -u hold.py 768 45"}}]}}"#;
        let later = r#"{"type":"assistant","timestamp":"2026-10-08T06:00:00Z","message":{"content":[{"type":"tool_use","input":{"content":"assert needle == '-u hold.py 768 45'"}}]}}"#;
        std::fs::write(dir.join("watcher.jsonl"), format!("{seen}\n")).unwrap();
        std::fs::write(dir.join("tests-writer.jsonl"), format!("{later}\n")).unwrap();
        std::fs::write(dir.join("launcher.jsonl"), format!("{seen}\n{launch}\n")).unwrap();
        let (n, hits) = search_in(&[("claude", dir.clone())], &needle("-u hold.py 768 45", started), None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!((n, hits.len()), (3, 3));
        assert_eq!((hits[0].session.as_str(), hits[0].kind, hits[0].line), ("launcher", Kind::Launched, 2));
        assert!((hits[0].lead.unwrap() - 2.88).abs() < 1e-6);
        assert!(hits[1..].iter().all(|h| h.kind == Kind::Mentioned));
    }
    #[test]
    fn rejects_path_like_session_ids() {
        assert_eq!(claude_transcript("../../etc/passwd"), None);
        assert_eq!(claude_transcript(""), None);
    }

    #[test]
    fn needles_match_json_escaped_commands() {
        let dir = std::env::temp_dir().join(format!("vramorama-test-{}", std::process::id()));
        let proj = dir.join(".claude").join("projects").join("C--work-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","input":{"command":"$py -u scripts\\run_queue.py --queue configs\\queues\\u6.yaml"}}]}}"#;
        std::fs::write(proj.join("5f3c1e2a-0000.jsonl"), format!("{{\"customTitle\":\"Night queue\"}}\n{line}\n"))
            .unwrap();
        let roots = [("claude", dir.join(".claude").join("projects"))];
        let (n, hits) = search_in(&roots, &needle(r"scripts\run_queue.py --queue configs\queues\u6.yaml", None), None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(n, 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, "5f3c1e2a-0000");
        assert_eq!(hits[0].title.as_deref(), Some("Night queue"));
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].kind, Kind::Launched);
    }
}
