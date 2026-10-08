//! Pure parsing helpers, kept free of OS calls so they are tested on every platform.

/// Parse a GPU performance-counter instance name.
///
/// `pid_8352_luid_0x00000000_0x00013F3A_phys_0` and the engine form
/// `pid_8352_luid_0x00000000_0x00013F3A_phys_0_eng_3_engtype_Compute` give
/// `(Some(8352), 0x0000_0000_0001_3F3A)`. Adapter instances (`luid_..._phys_0`) give `(None, luid)`.
pub fn parse_instance(name: &str) -> Option<(Option<u32>, u64)> {
    let pid = match name.strip_prefix("pid_") {
        Some(rest) => {
            let end = rest.find('_')?;
            Some(rest[..end].parse().ok()?)
        }
        None => None,
    };
    let at = name.find("luid_0x")? + "luid_0x".len();
    let rest = &name[at..];
    let (hi, rest) = rest.split_once("_0x")?;
    let lo = rest.split('_').next()?;
    let hi = u32::from_str_radix(hi, 16).ok()?;
    let lo = u32::from_str_radix(lo, 16).ok()?;
    Some((pid, luid_key(hi, lo)))
}

pub fn luid_key(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

pub fn luid_string(key: u64) -> String {
    format!("0x{:08X}_0x{:08X}", (key >> 32) as u32, key as u32)
}

/// Split a Windows environment block (UTF-16 `K=V\0K=V\0\0`) into pairs.
/// Entries whose name starts with `=` (per-drive current directories like `=C:=C:\x`) keep it.
pub fn parse_env_block(units: &[u16]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in units.split(|&c| c == 0) {
        if entry.is_empty() {
            break;
        }
        let s = String::from_utf16_lossy(entry);
        let split_at = s.char_indices().skip(1).find(|&(_, c)| c == '=').map(|(i, _)| i);
        match split_at {
            Some(i) => out.push((s[..i].to_string(), s[i + 1..].to_string())),
            None => out.push((s, String::new())),
        }
    }
    out
}

/// The command line with the program (argv[0]) removed, using the Windows quoting rule
/// for argv[0]: a leading `"` runs to the next `"`, otherwise argv[0] ends at whitespace.
pub fn args_tail(cmdline: &str) -> &str {
    let s = cmdline.trim_start();
    let rest = if let Some(q) = s.strip_prefix('"') {
        match q.find('"') {
            Some(end) => &q[end + 1..],
            None => "",
        }
    } else {
        match s.find(char::is_whitespace) {
            Some(end) => &s[end..],
            None => "",
        }
    };
    rest.trim()
}

/// Search strings for finding a command line inside an agent transcript.
///
/// Transcripts store the command the agent typed, which often differs from the final process
/// command line in argv[0] (a variable, a venv shim) and in interpreter flags like `-u`, so the
/// candidates drop those. Short candidates are discarded because they match too much.
pub fn needles(cmdline: &str) -> Vec<String> {
    const MIN: usize = 16;
    let tail = args_tail(cmdline);
    let mut out = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim();
        if s.len() >= MIN && !out.iter().any(|o: &String| o == s) {
            out.push(s.to_string());
        }
    };
    push(tail);
    let mut rest = tail;
    while let Some(tok) = rest.split_whitespace().next() {
        // Interpreter and shell switches: python -u, cmd /c.
        if (tok.starts_with('-') || tok.starts_with('/')) && tok.len() <= 3 {
            rest = rest[rest.find(tok).unwrap() + tok.len()..].trim_start();
        } else {
            break;
        }
    }
    push(rest);
    // Drop a trailing redirection, which agents often add around the real command.
    if let Some(i) = rest.find(" > ").or_else(|| rest.find(" >> ")) {
        push(&rest[..i]);
    }
    out
}

/// `2026-10-08T04:08:45.120Z` (or with a `+08:00` offset) to Unix seconds.
pub fn parse_iso_utc(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut frac = 0.0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        frac = format!("0.{}0", &r[..digits]).parse().ok()?;
        rest = &r[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hm = rest[1..].replace(':', "");
            if hm.len() != 4 {
                return None;
            }
            sign * (hm[..2].parse::<i64>().ok()? * 3600 + hm[2..].parse::<i64>().ok()? * 60)
        }
    };
    // Days since 1970-01-01 in the proleptic Gregorian calendar (Howard Hinnant's days_from_civil).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + se - offset) as f64 + frac)
}

/// Standard base64 with padding (for PowerShell's `-EncodedCommand`).
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= c.len() { T[(n >> shift & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Join arguments into a Windows command line (CommandLineToArgvW quoting).
pub fn join_args(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty() && !a.contains([' ', '\t', '"']) {
                return a.clone();
            }
            let mut q = String::from("\"");
            let mut slashes = 0;
            for ch in a.chars() {
                match ch {
                    '\\' => slashes += 1,
                    '"' => {
                        q.push_str(&"\\".repeat(slashes * 2 + 1));
                        q.push('"');
                        slashes = 0;
                    }
                    c => {
                        q.push_str(&"\\".repeat(slashes));
                        q.push(c);
                        slashes = 0;
                    }
                }
            }
            q.push_str(&"\\".repeat(slashes * 2));
            q.push('"');
            q
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// "4s", "12m", "3h05m"
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// Shorten for a fixed-width column, marking the cut.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".chars().take(max).collect();
    }
    let mut t: String = s.chars().take(max - 1).collect();
    t.push('…');
    t
}

/// Last path component of a Windows or POSIX path.
pub fn basename(path: &str) -> &str {
    path.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or(path)
}

/// 8,776
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names() {
        assert_eq!(parse_instance("pid_8352_luid_0x00000000_0x00013F3A_phys_0"), Some((Some(8352), 0x13F3A)));
        assert_eq!(
            parse_instance("pid_19004_luid_0x00000001_0x00015D78_phys_0_eng_3_engtype_Compute"),
            Some((Some(19004), (1u64 << 32) | 0x15D78))
        );
        assert_eq!(parse_instance("luid_0x00000000_0x00016E7D_phys_0"), Some((None, 0x16E7D)));
        assert_eq!(parse_instance("_Total"), None);
        assert_eq!(parse_instance("pid_x_luid_0x0_0x1_phys_0"), None);
        assert_eq!(luid_string(0x13F3A), "0x00000000_0x00013F3A");
    }

    #[test]
    fn env_blocks() {
        let raw: Vec<u16> =
            "=C:=C:\\work\0PATH=C:\\bin\0CLAUDE_CODE_SESSION_ID=abc\0EMPTY=\0\0junk\0".encode_utf16().collect();
        let env = parse_env_block(&raw);
        assert_eq!(env.len(), 4);
        assert_eq!(env[0], ("=C:".into(), "C:\\work".into()));
        assert_eq!(env[2], ("CLAUDE_CODE_SESSION_ID".into(), "abc".into()));
        assert_eq!(env[3], ("EMPTY".into(), "".into()));
    }

    #[test]
    fn argv0_is_stripped() {
        assert_eq!(args_tail(r#""C:\Program Files\x\python.exe" -u a.py --k v"#), "-u a.py --k v");
        assert_eq!(args_tail(r"C:\venv\python.exe -u scripts\run_queue.py"), r"-u scripts\run_queue.py");
        assert_eq!(args_tail("llama-server.exe"), "");
        assert_eq!(args_tail(r#""unterminated"#), "");
    }

    #[test]
    fn transcript_needles() {
        let n =
            needles(r"C:\Users\p\.venvs\jq\Scripts\python.exe -u scripts\run_queue.py --queue configs\queues\u6.yaml");
        assert_eq!(
            n,
            vec![
                r"-u scripts\run_queue.py --queue configs\queues\u6.yaml".to_string(),
                r"scripts\run_queue.py --queue configs\queues\u6.yaml".to_string(),
            ]
        );
        let n = needles(r"cmd.exe /c C:\py.exe -u scripts\run_queue.py --queue q.yaml > out\u6.out 2>&1");
        assert!(n.contains(&r"C:\py.exe -u scripts\run_queue.py --queue q.yaml".to_string()));
        assert!(needles("python.exe -u x.py").is_empty());
    }

    #[test]
    fn base64_and_quoting() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar!"), "Zm9vYmFyIQ==");
        let argv: Vec<String> =
            ["python.exe", "-c", r#"print("a b")"#, r"C:\dir with space\", ""].map(String::from).into();
        assert_eq!(join_args(&argv), r#"python.exe -c "print(\"a b\")" "C:\dir with space\\" """#);
    }

    #[test]
    fn iso_timestamps() {
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_iso_utc("2000-03-01T00:00:00Z"), Some(951_868_800.0));
        assert_eq!(parse_iso_utc("2026-10-08T04:08:48Z"), Some(1_791_432_528.0));
        assert_eq!(parse_iso_utc("2026-10-08T12:08:48+08:00"), Some(1_791_432_528.0));
        let t = parse_iso_utc("2026-10-08T04:08:45.120Z").unwrap();
        assert!((t - 1_791_432_525.12).abs() < 1e-6);
        assert_eq!(parse_iso_utc("2026-13-08T04:08:48Z"), None);
        assert_eq!(parse_iso_utc("yesterday"), None);
    }

    #[test]
    fn formatting() {
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
        assert_eq!(basename(r"C:\Users\p\GitHub\bench\"), "bench");
        assert_eq!(basename("/home/p/x"), "x");
        assert_eq!(thousands(8776), "8,776");
        assert_eq!(thousands(16303000), "16,303,000");
        assert_eq!(thousands(12), "12");
    }
}
