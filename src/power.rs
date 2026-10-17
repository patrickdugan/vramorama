//! Power cap: a laptop GPU can be held to a small fraction of its rated power (seen 2026-10-15 on an
//! RTX 5080 Laptop: an enforced limit of 15 W against a default of 80 W, usually after Modern
//! Standby). Everything on the card then runs roughly twenty times slower, and one long kernel can
//! pass the two seconds Windows allows before it resets the GPU (a TDR, `nvlddmkm` event 153). A
//! reset kills the jobs of every session on the card, not just the one that tipped it over.
//!
//! The signal is the enforced power limit against the driver's default limit. Clock readings are no
//! use: under WDDM `nvidia-smi` reports about 200 MHz even while the card draws 100 W.

/// A card counts as capped when its enforced limit is at most this fraction of its default limit.
pub const CAPPED_BELOW: f64 = 0.5;

#[derive(Clone, Debug, PartialEq)]
pub struct PowerState {
    pub name: String,
    pub draw_w: f64,
    pub enforced_w: f64,
    pub default_w: f64,
    pub util_pct: u32,
}

impl PowerState {
    pub fn capped(&self) -> bool {
        self.default_w > 0.0 && self.enforced_w > 0.0 && self.enforced_w <= self.default_w * CAPPED_BELOW
    }
}

/// One line for the console and the GUI, or None when the card is not capped.
pub fn warning(p: &PowerState) -> Option<String> {
    p.capped().then(|| {
        format!(
            "{} is power-capped: enforced limit {:.0} W against a default of {:.0} W (drawing {:.0} W at {}% use). \
Jobs run many times slower and a long kernel can trip the 2 s Windows GPU timeout, which resets the card for \
every session. Do not start GPU jobs until it clears (waking the machine, or a power-mode change, usually does it).",
            p.name, p.enforced_w, p.default_w, p.draw_w, p.util_pct
        )
    })
}

pub fn to_json(p: &PowerState) -> crate::json::Value {
    use crate::json::obj;
    obj([
        ("name", p.name.as_str().into()),
        ("draw_w", p.draw_w.into()),
        ("enforced_limit_w", p.enforced_w.into()),
        ("default_limit_w", p.default_w.into()),
        ("util_pct", u64::from(p.util_pct).into()),
        ("capped", p.capped().into()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(enforced: f64, default: f64) -> PowerState {
        PowerState {
            name: "NVIDIA GeForce RTX 5080 Laptop GPU".into(),
            draw_w: 19.0,
            enforced_w: enforced,
            default_w: default,
            util_pct: 100,
        }
    }

    #[test]
    fn capped_means_enforced_limit_far_below_default() {
        // 2026-10-15: 15 W enforced against an 80 W default.
        assert!(state(15.0, 80.0).capped());
        // The healthy state of the same card: 100 W enforced, above the default.
        assert!(!state(100.6, 80.0).capped());
        assert!(state(40.0, 80.0).capped());
        assert!(!state(41.0, 80.0).capped());
        // Unreadable limits never count as capped.
        assert!(!state(0.0, 80.0).capped());
        assert!(!state(15.0, 0.0).capped());
    }

    #[test]
    fn warning_only_when_capped() {
        assert_eq!(warning(&state(100.0, 80.0)), None);
        let w = warning(&state(15.0, 80.0)).unwrap();
        assert!(w.contains("15 W against a default of 80 W"));
        assert!(w.contains("2 s Windows GPU timeout"));
    }
}
