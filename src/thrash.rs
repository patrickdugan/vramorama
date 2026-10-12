//! Thrash: processes taking turns on a full card.
//!
//! When the jobs on a card need more memory than it has, WDDM does not fail the allocation. Besides
//! spilling (`spill.rs`), its video memory manager can make the processes take turns: before one
//! runs, the other's allocations are evicted to system memory, and they come back when it runs
//! again. Each process's dedicated memory then swings by gigabytes every few seconds, opposite to
//! the other's, while the card stays full, and the evicted memory shows up in their RAM. Every swap
//! moves gigabytes over PCIe, so both jobs run far slower than either would alone, and nothing
//! errors. Seen 2026-10-12: two training jobs swung between about 4 and 12 GB every few seconds on
//! a 16 GB card for hours, with under 1 GB of RAM left.
//!
//! Detection needs a window of samples: the card full for most of it, and at least two processes
//! whose memory swings by `MIN_SWING_MIB` or more, reverses direction `MIN_REVERSALS` times, and
//! moves opposite to the other's (correlation of their changes at most `MAX_CORR`). A job loading
//! its model only grows; a job that allocates and frees every step on its own does not move
//! opposite to anyone; neither counts.

use std::collections::{HashMap, VecDeque};

/// A process's memory must range over at least this much in the window.
pub const MIN_SWING_MIB: u64 = 1024;
/// A change between samples smaller than this is noise, not a move.
const STEP_MIB: f64 = 256.0;
/// Up, down, up, down: at least this many changes of direction.
pub const MIN_REVERSALS: usize = 3;
/// Two processes take turns when their changes correlate at most this much (−1 is perfect antiphase).
const MAX_CORR: f64 = -0.5;
/// A process's RAM moving opposite to its VRAM (its evicted memory) correlates at most this much.
const RAM_CORR: f64 = -0.3;
/// The card counts as full within this much of its capacity...
const FULL_MARGIN_MIB: u64 = 1024;
/// ...for at least this share of the samples.
const FULL_SHARE: f64 = 0.5;
/// Fewer samples than this say nothing.
pub const MIN_SAMPLES: usize = 8;

#[derive(Clone, Debug, Default)]
struct Snap {
    ts: f64,
    used: u64,
    total: u64,
    /// pid -> (dedicated MiB, RAM MiB when known)
    procs: HashMap<u32, (u64, Option<u64>)>,
}

/// One process taking part, over the window.
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub low_mib: u64,
    pub high_mib: u64,
    pub reversals: usize,
    /// Its RAM moved opposite to its VRAM: the evicted memory went there.
    pub ram_antiphase: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Thrash {
    pub procs: Vec<Proc>,
    /// Roughly how long one round of turns takes.
    pub period_secs: Option<f64>,
    pub window_secs: f64,
    pub samples: usize,
    /// Share of the samples in which the card was full.
    pub full_share: f64,
}

/// A sliding window of samples per adapter.
pub struct Detector {
    window: f64,
    cards: HashMap<u64, VecDeque<Snap>>,
}

impl Detector {
    pub fn new(window_secs: f64) -> Detector {
        Detector { window: window_secs, cards: HashMap::new() }
    }

    /// Add one sample of an adapter: memory in use and capacity, and each process's dedicated
    /// memory (and RAM, when read), all in MiB.
    pub fn push(
        &mut self,
        luid: u64,
        ts: f64,
        used: u64,
        total: u64,
        procs: impl IntoIterator<Item = (u32, u64, Option<u64>)>,
    ) {
        let q = self.cards.entry(luid).or_default();
        let mut snap = Snap { ts, used, total, procs: HashMap::new() };
        for (pid, mib, ram) in procs {
            let e = snap.procs.entry(pid).or_insert((0, ram));
            e.0 += mib;
        }
        q.push_back(snap);
        while q.front().is_some_and(|s| ts - s.ts > self.window) {
            q.pop_front();
        }
    }

    pub fn check(&self, luid: u64) -> Option<Thrash> {
        detect(&self.cards.get(&luid)?.iter().collect::<Vec<_>>())
    }
}

fn detect(snaps: &[&Snap]) -> Option<Thrash> {
    let n = snaps.len();
    if n < MIN_SAMPLES {
        return None;
    }
    let full = snaps.iter().filter(|s| s.total > 0 && s.used + FULL_MARGIN_MIB >= s.total).count();
    let full_share = full as f64 / n as f64;
    if full_share < FULL_SHARE {
        return None;
    }
    // Processes seen in most of the window, with their memory per sample (None when absent).
    let mut pids: Vec<u32> = snaps.iter().flat_map(|s| s.procs.keys().copied()).collect();
    pids.sort_unstable();
    pids.dedup();
    let series = |pid: u32| -> Vec<Point> { snaps.iter().map(|s| s.procs.get(&pid).copied()).collect() };
    let mut swinging: Vec<(Proc, Vec<Point>)> = Vec::new();
    for pid in pids {
        let s = series(pid);
        if s.iter().filter(|x| x.is_some()).count() * 5 < n * 4 {
            continue;
        }
        let v: Vec<u64> = s.iter().flatten().map(|x| x.0).collect();
        let (low, high) = (*v.iter().min()?, *v.iter().max()?);
        let reversals = reversals(&v);
        if high - low >= MIN_SWING_MIB && reversals >= MIN_REVERSALS {
            swinging.push((Proc { pid, low_mib: low, high_mib: high, reversals, ram_antiphase: false }, s));
        }
    }
    // Keep the processes that move opposite to at least one other swinging process.
    let mut taking_turns = vec![false; swinging.len()];
    for i in 0..swinging.len() {
        for j in i + 1..swinging.len() {
            let (a, b) = deltas_together(&swinging[i].1, &swinging[j].1);
            if correlation(&a, &b).is_some_and(|c| c <= MAX_CORR) {
                taking_turns[i] = true;
                taking_turns[j] = true;
            }
        }
    }
    let mut procs: Vec<Proc> = Vec::new();
    for ((mut p, s), turns) in swinging.into_iter().zip(taking_turns) {
        if !turns {
            continue;
        }
        let (dv, dr) = deltas_pair(&s);
        p.ram_antiphase = correlation(&dv, &dr).is_some_and(|c| c <= RAM_CORR);
        procs.push(p);
    }
    if procs.len() < 2 {
        return None;
    }
    procs.sort_by(|a, b| b.high_mib.cmp(&a.high_mib).then(a.pid.cmp(&b.pid)));
    let span = snaps[n - 1].ts - snaps[0].ts;
    let most = procs.iter().map(|p| p.reversals).max().unwrap_or(0);
    Some(Thrash {
        period_secs: (most > 0 && span > 0.0).then(|| 2.0 * span / most as f64),
        procs,
        window_secs: span,
        samples: n,
        full_share,
    })
}

/// Changes of direction in a series, ignoring moves smaller than `STEP_MIB`.
fn reversals(v: &[u64]) -> usize {
    let mut last = 0i8;
    let mut count = 0;
    for w in v.windows(2) {
        let d = w[1] as f64 - w[0] as f64;
        if d.abs() < STEP_MIB {
            continue;
        }
        let sign = if d > 0.0 { 1 } else { -1 };
        if last != 0 && sign != last {
            count += 1;
        }
        last = sign;
    }
    count
}

/// A process in one sample: (VRAM MiB, RAM MiB when known), or absent.
type Point = Option<(u64, Option<u64>)>;
type Series = [Point];

/// The VRAM changes of two processes between consecutive samples where both are present.
fn deltas_together(a: &Series, b: &Series) -> (Vec<f64>, Vec<f64>) {
    let (mut da, mut db) = (Vec::new(), Vec::new());
    for i in 1..a.len().min(b.len()) {
        if let (Some(a0), Some(a1), Some(b0), Some(b1)) = (a[i - 1], a[i], b[i - 1], b[i]) {
            da.push(a1.0 as f64 - a0.0 as f64);
            db.push(b1.0 as f64 - b0.0 as f64);
        }
    }
    (da, db)
}

/// A process's VRAM and RAM changes between consecutive samples where both are known.
fn deltas_pair(s: &Series) -> (Vec<f64>, Vec<f64>) {
    let (mut dv, mut dr) = (Vec::new(), Vec::new());
    for w in s.windows(2) {
        if let (Some((v0, Some(r0))), Some((v1, Some(r1)))) = (w[0], w[1]) {
            dv.push(v1 as f64 - v0 as f64);
            dr.push(r1 as f64 - r0 as f64);
        }
    }
    (dv, dr)
}

/// Pearson correlation; `None` for fewer than 4 pairs or a flat series.
fn correlation(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len().min(b.len());
    if n < 4 {
        return None;
    }
    let (ma, mb) = (a[..n].iter().sum::<f64>() / n as f64, b[..n].iter().sum::<f64>() / n as f64);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (x, y) = (a[i] - ma, b[i] - mb);
        sab += x * y;
        saa += x * x;
        sbb += y * y;
    }
    (saa > 0.0 && sbb > 0.0).then(|| sab / (saa * sbb).sqrt())
}

/// One line for consoles and the page: who is taking turns, and what it costs.
pub fn warning(t: &Thrash, name: impl Fn(u32) -> String) -> String {
    let who: Vec<String> = t
        .procs
        .iter()
        .map(|p| {
            format!(
                "{} ({}–{} MiB)",
                name(p.pid),
                crate::parse::thousands(p.low_mib),
                crate::parse::thousands(p.high_mib)
            )
        })
        .collect();
    let every = t.period_secs.map(|s| format!(" about every {s:.0} s")).unwrap_or_default();
    format!(
        "thrashing: {} are taking turns on the full card{every}. Each turn moves gigabytes between VRAM and system memory, \
so they run far slower than either would alone; pausing one would let the other run at full speed.",
        who.join(" and ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: u64 = 16_000;

    /// Feed `n` samples 2 s apart; `procs(i)` gives (pid, VRAM, RAM) at sample i.
    fn run(
        n: usize,
        used: impl Fn(usize) -> u64,
        procs: impl Fn(usize) -> Vec<(u32, u64, Option<u64>)>,
    ) -> Option<Thrash> {
        let mut d = Detector::new(60.0);
        for i in 0..n {
            d.push(1, i as f64 * 2.0, used(i), CARD, procs(i));
        }
        d.check(1)
    }

    /// Two jobs swapping every two samples, as seen on 2026-10-12.
    fn turns(i: usize) -> Vec<(u32, u64, Option<u64>)> {
        let a_up = (i / 2) % 2 == 0;
        let (a, b) = if a_up { (11_700, 4_000) } else { (4_000, 11_700) };
        vec![(25184, a, Some(16_000 - a)), (31264, b, Some(16_000 - b)), (900, 60, None)]
    }

    #[test]
    fn two_jobs_taking_turns_on_a_full_card() {
        let t = run(20, |_| 15_800, turns).expect("thrash");
        assert_eq!(t.procs.iter().map(|p| p.pid).collect::<Vec<_>>().len(), 2);
        assert!(t.procs.iter().all(|p| p.ram_antiphase && p.high_mib - p.low_mib == 7_700));
        assert!(t.procs.iter().all(|p| p.pid != 900), "a steady bystander is not part of it");
        let period = t.period_secs.unwrap();
        assert!((6.0..=10.0).contains(&period), "{period}");
        assert_eq!(t.full_share, 1.0);
        let w = warning(&t, |p| format!("pid {p}"));
        assert!(w.contains("pid 25184 (4,000–11,700 MiB) and pid 31264"), "{w}");
    }

    #[test]
    fn not_thrash() {
        // The same swapping on a card with room to spare is not memory pressure.
        assert_eq!(run(20, |_| 9_000, turns), None);
        // Too few samples.
        assert_eq!(run(MIN_SAMPLES - 1, |_| 15_800, turns), None);
        // A job loading its model next to a steady one: it only grows.
        assert_eq!(run(20, |_| 15_800, |i| vec![(1, 2_000 + i as u64 * 500, None), (2, 6_000, None)]), None);
        // Two jobs that each allocate and free every step, in step with each other.
        let together = |i: usize| {
            let m = if i % 2 == 0 { 7_500 } else { 5_000 };
            vec![(1, m, None), (2, m, None)]
        };
        assert_eq!(run(20, |_| 15_800, together), None);
        // One job swinging alone against a steady one.
        let alone = |i: usize| vec![(1, if (i / 2) % 2 == 0 { 9_000 } else { 4_000 }, None), (2, 6_000, None)];
        assert_eq!(run(20, |_| 15_800, alone), None);
    }

    #[test]
    fn small_moves_are_noise() {
        assert_eq!(reversals(&[1000, 1100, 1000, 1100, 1000]), 0);
        assert_eq!(reversals(&[1000, 3000, 1000, 3000, 1000]), 3);
        assert_eq!(correlation(&[1.0, -1.0, 1.0, -1.0], &[-1.0, 1.0, -1.0, 1.0]), Some(-1.0));
        assert_eq!(correlation(&[1.0, 1.0, 1.0, 1.0], &[-1.0, 1.0, -1.0, 1.0]), None);
    }

    #[test]
    fn window_slides() {
        let mut d = Detector::new(10.0);
        for i in 0..30 {
            d.push(1, i as f64 * 2.0, 15_800, CARD, turns(i));
        }
        // Only the last 10 s (6 samples) are kept: too few to say anything.
        assert_eq!(d.check(1), None);
        assert_eq!(d.check(2), None, "an adapter never seen");
    }
}
