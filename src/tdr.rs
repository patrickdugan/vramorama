//! GPU resets (Windows TDR) from the System event log.
//!
//! When a GPU operation runs longer than the two seconds Windows allows, the driver resets the card and
//! logs event 153 from `nvlddmkm` (NVIDIA; AMD logs 4101 from `amdkmdag` and others "Display"). Every
//! job on the card dies or hangs, which is the failure that kills long runs on a shared consumer GPU.
//! Read with `wevtutil`, which ships with Windows, so no event-log API has to be linked.

use std::process::Command;

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

/// The `SystemTime` attributes of every event in `wevtutil ... /f:xml` output.
pub fn times_in(xml: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("SystemTime='") {
        rest = &rest[i + "SystemTime='".len()..];
        if let Some(end) = rest.find('\'') {
            out.extend(parse_iso_utc(&rest[..end]));
            rest = &rest[end..];
        }
    }
    out
}

/// Unix times of GPU reset events logged in the last `within_secs` seconds, newest first.
pub fn recent_resets(within_secs: u64) -> Vec<f64> {
    let ms = within_secs.saturating_mul(1000);
    let query = format!(
        "*[System[(Provider[@Name='nvlddmkm'] and EventID=153 or Provider[@Name='amdkmdag'] and EventID=4101 \
or Provider[@Name='Display'] and EventID=4101) and TimeCreated[timediff(@SystemTime) <= {ms}]]]"
    );
    let Ok(out) =
        Command::new("wevtutil").args(["qe", "System", &format!("/q:{query}"), "/c:50", "/rd:true", "/f:xml"]).output()
    else {
        return Vec::new();
    };
    times_in(&String::from_utf8_lossy(&out.stdout))
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

    #[test]
    fn times_from_event_xml() {
        let xml = "<Event><System><TimeCreated SystemTime='2026-10-15T14:56:57.0000000Z'/></System></Event>\
<Event><System><TimeCreated SystemTime='2026-10-15T10:03:58.0000000Z'/></System></Event>";
        assert_eq!(times_in(xml), vec![1_792_076_217.0, 1_792_058_638.0]);
        assert!(times_in("").is_empty());
    }
}
