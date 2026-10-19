//! GPU resets (Windows TDR) from the System event log.
//!
//! When a GPU operation runs longer than the two seconds Windows allows, the driver resets the card and
//! logs event 153 from `nvlddmkm` (NVIDIA; AMD logs 4101 from `amdkmdag` and others "Display"). Every
//! job on the card dies or hangs, which is the failure that kills long runs on a shared consumer GPU.
//! Read through the Windows event log API (`evt.rs`).

/// Unix time of an ISO-8601 UTC stamp like `2026-10-15T14:56:57.1234567Z`.
pub fn parse_iso_utc(s: &str) -> Option<f64> {
    let (date, time) = s.trim().trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':');
    let (h, mi) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let sec: f64 = t.next()?.parse().ok()?;
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60) as f64 + sec)
}

/// XPath for the GPU reset events logged in the last `within_secs` seconds.
pub fn reset_query(within_secs: u64) -> String {
    let ms = within_secs.saturating_mul(1000);
    format!(
        "*[System[(Provider[@Name='nvlddmkm'] and EventID=153 or Provider[@Name='amdkmdag'] and EventID=4101 or Provider[@Name='Display'] and EventID=4101) and TimeCreated[timediff(@SystemTime) <= {ms}]]]"
    )
}

/// Unix times of GPU reset events logged in the last `within_secs` seconds, newest first.
#[cfg(windows)]
pub fn recent_resets(within_secs: u64) -> Vec<f64> {
    let Ok(log) = crate::evt::EventLog::open() else { return Vec::new() };
    let Ok(xml) = log.read("System", &reset_query(within_secs), 200) else { return Vec::new() };
    xml.iter().filter_map(|x| crate::crash::parse_event(x)).map(|e| e.ts).collect()
}

#[cfg(not(windows))]
pub fn recent_resets(_within_secs: u64) -> Vec<f64> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_stamps() {
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_iso_utc("2000-03-01T00:00:00.000Z"), Some(951_868_800.0));
        // 2026-10-15 14:56:57 UTC.
        assert_eq!(parse_iso_utc("2026-10-15T14:56:57.5000000Z"), Some(1_792_076_217.5));
        assert_eq!(parse_iso_utc("nonsense"), None);
    }
}
