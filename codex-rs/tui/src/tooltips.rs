use codex_features::FEATURES;
use codex_protocol::account::PlanType;
use lazy_static::lazy_static;
use rand::Rng;

const IS_MACOS: bool = cfg!(target_os = "macos");

const APP_TOOLTIP: &str = "Try the **Desktop app**. Run 'codex app' or visit https://chatgpt.com/codex?app-landing-page=true";
const FAST_TOOLTIP: &str =
    "*New* Use **/fast** to enable our fastest inference with increased plan usage.";
const OTHER_TOOLTIP: &str = "*New* Build faster with the **Desktop app**. Run 'codex app' or visit https://chatgpt.com/codex?app-landing-page=true";
const OTHER_TOOLTIP_NON_MAC: &str = "*New* Build faster with Codex.";
const FREE_GO_TOOLTIP: &str =
    "*New* For a limited time, Codex is included in your plan for free – let’s build together.";

const RAW_TOOLTIPS: &str = include_str!("../tooltips.txt");

lazy_static! {
    static ref TOOLTIPS: Vec<&'static str> = RAW_TOOLTIPS
        .lines()
        .map(str::trim)
        .filter(|line| {
            if line.is_empty() || line.starts_with('#') {
                return false;
            }
            if !IS_MACOS && line.contains("codex app") {
                return false;
            }
            true
        })
        .collect();
    static ref ALL_TOOLTIPS: Vec<&'static str> = {
        let mut tips = Vec::new();
        tips.extend(TOOLTIPS.iter().copied());
        tips.extend(experimental_tooltips());
        tips
    };
}

fn experimental_tooltips() -> Vec<&'static str> {
    FEATURES
        .iter()
        .filter_map(|spec| spec.stage.experimental_announcement())
        .collect()
}

/// Pick a random tooltip to show to the user when starting Codex.
pub(crate) fn get_tooltip(plan: Option<PlanType>, fast_mode_enabled: bool) -> Option<String> {
    let mut rng = rand::rng();

    // Leave small chance for a random tooltip to be shown.
    if rng.random_ratio(8, 10) {
        match plan {
            Some(plan_type)
                if matches!(
                    plan_type,
                    PlanType::Plus | PlanType::Enterprise | PlanType::Pro | PlanType::ProLite
                ) || plan_type.is_team_like()
                    || plan_type.is_business_like() =>
            {
                if let Some(tooltip) = pick_paid_tooltip(&mut rng, fast_mode_enabled) {
                    return Some(tooltip.to_string());
                }
            }
            Some(PlanType::Go) | Some(PlanType::Free) => {
                return Some(FREE_GO_TOOLTIP.to_string());
            }
            _ => {
                let tooltip = if IS_MACOS {
                    OTHER_TOOLTIP
                } else {
                    OTHER_TOOLTIP_NON_MAC
                };
                return Some(tooltip.to_string());
            }
        }
    }

    pick_tooltip(&mut rng).map(str::to_string)
}

fn paid_app_tooltip() -> Option<&'static str> {
    if IS_MACOS { Some(APP_TOOLTIP) } else { None }
}

/// Paid users spend most startup sessions in a dedicated promo slot rather than the
/// generic random tip pool. Keep this business logic explicit: we currently split
/// that slot between the app promo and Fast mode, but suppress the Fast promo once
/// the user already has Fast mode enabled.
fn pick_paid_tooltip<R: Rng + ?Sized>(
    rng: &mut R,
    fast_mode_enabled: bool,
) -> Option<&'static str> {
    if fast_mode_enabled || rng.random_bool(0.5) {
        paid_app_tooltip()
    } else {
        Some(FAST_TOOLTIP)
    }
}

fn pick_tooltip<R: Rng + ?Sized>(rng: &mut R) -> Option<&'static str> {
    if ALL_TOOLTIPS.is_empty() {
        None
    } else {
        ALL_TOOLTIPS
            .get(rng.random_range(0..ALL_TOOLTIPS.len()))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn random_tooltip_returns_some_tip_when_available() {
        let mut rng = StdRng::seed_from_u64(42);
        assert!(pick_tooltip(&mut rng).is_some());
    }

    #[test]
    fn random_tooltip_is_reproducible_with_seed() {
        let expected = {
            let mut rng = StdRng::seed_from_u64(7);
            pick_tooltip(&mut rng)
        };

        let mut rng = StdRng::seed_from_u64(7);
        assert_eq!(expected, pick_tooltip(&mut rng));
        assert_eq!(expected, pick_tooltip(&mut rng));
    }

    #[test]
    fn paid_tooltip_pool_rotates_between_promos() {
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..32 {
            let mut rng = StdRng::seed_from_u64(seed);
            seen.insert(pick_paid_tooltip(
                &mut rng, /*fast_mode_enabled*/ false,
            ));
        }

        let expected = std::collections::BTreeSet::from([paid_app_tooltip(), Some(FAST_TOOLTIP)]);
        assert_eq!(seen, expected);
    }

    #[test]
    fn paid_tooltip_pool_skips_fast_when_fast_mode_is_enabled() {
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..8 {
            let mut rng = StdRng::seed_from_u64(seed);
            seen.insert(pick_paid_tooltip(&mut rng, /*fast_mode_enabled*/ true));
        }

        let expected = std::collections::BTreeSet::from([paid_app_tooltip()]);
        assert_eq!(seen, expected);
        assert!(!seen.contains(&Some(FAST_TOOLTIP)));
    }
}
