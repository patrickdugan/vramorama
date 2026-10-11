//! Spill: on Windows, when a discrete card's dedicated memory is full, the driver does not fail the
//! next allocation. It places it in shared (system) memory instead, behind PCIe ("sysmem
//! fallback"). Nothing errors and nothing logs it; every kernel touching that memory runs many
//! times slower, so jobs that would have died with "out of memory" crawl for hours instead.
//!
//! The `GPU Process Memory\Shared Usage` counter shows it per process. A CUDA context alone holds
//! about 80 MiB there and pinned host buffers add some more, so a process counts as spilled only
//! when it holds a lot of shared memory *and* the card's dedicated memory is nearly full.

/// Shared memory a process must hold on a discrete card before it counts as spilled.
pub const SPILL_MIN_MIB: u64 = 512;
/// The card counts as full when this little dedicated memory is left.
pub const FULL_MARGIN_MIB: u64 = 1024;

/// Whether a process holding `shared_mib` of shared memory on a card with `total_mib` dedicated,
/// of which `used_mib` is in use, has spilled. Integrated GPUs (no dedicated memory) use shared
/// memory by design and never count.
pub fn spilled(shared_mib: u64, total_mib: u64, used_mib: u64) -> bool {
    total_mib > 0 && shared_mib >= SPILL_MIN_MIB && used_mib + FULL_MARGIN_MIB >= total_mib
}

/// One line for the console and the GUI: how much spilled, by whom, and what to do.
pub fn warning(spills: &[(u32, u64)]) -> Option<String> {
    if spills.is_empty() {
        return None;
    }
    let total: u64 = spills.iter().map(|(_, m)| m).sum();
    let who: Vec<String> =
        spills.iter().map(|(pid, m)| format!("{pid} ({} MiB)", crate::parse::thousands(*m))).collect();
    Some(format!(
        "{} MiB spilled into system memory by pid {}: dedicated memory is full, so these jobs run many times \
slower without failing. Stop or pause one job; start GPU jobs with `vramorama run` so they queue instead.",
        crate::parse::thousands(total),
        who.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_needs_a_full_card_and_real_shared_use() {
        // The jam of 2026-10-11: 15,027 of 15,915 MiB in use, a job with 3.8 GiB in shared memory.
        assert!(spilled(3850, 15915, 15027));
        // A CUDA context's usual shared footprint is not a spill.
        assert!(!spilled(80, 15915, 15500));
        // Pinned buffers on a card with room to spare are not a spill either.
        assert!(!spilled(2000, 15915, 6455));
        // Integrated GPUs live in shared memory.
        assert!(!spilled(4000, 0, 0));
        assert!(spilled(SPILL_MIN_MIB, 16000, 16000 - FULL_MARGIN_MIB));
        assert!(!spilled(SPILL_MIN_MIB, 16000, 16000 - FULL_MARGIN_MIB - 1));
    }

    #[test]
    fn warning_names_the_processes() {
        assert_eq!(warning(&[]), None);
        let w = warning(&[(22488, 3850), (41220, 1200)]).unwrap();
        assert!(w.starts_with("5,050 MiB spilled into system memory by pid 22488 (3,850 MiB), 41220 (1,200 MiB)"));
        assert!(w.contains("vramorama run"));
    }
}
