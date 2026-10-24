//! Recipe id → how long retail's craft takes.
//!
//! From the APK's `Recipe._duration` / `TemperingRecipe._levels[]._duration` /
//! `ItemModifierRecipe._duration` (`recipe_durations.json`, compiled in, produced by
//! `script/extract_recipe_durations.py`). The client draws its recipe screen's craft time
//! from the same fields; the server must start the job with the same number or the
//! timer jumps the moment the craft begins (tracker #423: a 10-minute Steel Gauntlets
//! became a 5-hour job, from the authored `smith_craftables.json` approximation).
//!
//! Retail's rule, checked against every captured `POST /crafts`:
//!
//! * Smithing / Alchemy / DecorationCrafting / Enchanting — `seconds × batchSize`
//!   (231 starts, 11 of them Alchemy batches of 4–5).
//! * Tempering — `levelSeconds[temperingLevel - 1]`, the request's TARGET level
//!   (13 starts, levels 1–10).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use uuid::Uuid;

static RECIPE_DURATIONS_RAW: &str = include_str!("../recipe_durations.json");

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RecipeDuration {
    pub crafting_type: String,
    /// Seconds per craft (flat recipes).
    #[serde(default)]
    pub seconds: Option<u64>,
    /// Seconds per TARGET tempering level, index `level - 1` (Tempering only).
    #[serde(default)]
    pub level_seconds: Vec<u64>,
}

#[derive(Deserialize)]
struct Corpus {
    recipes: HashMap<Uuid, RecipeDuration>,
}

fn table() -> &'static HashMap<Uuid, RecipeDuration> {
    static TABLE: OnceLock<HashMap<Uuid, RecipeDuration>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<Corpus>(RECIPE_DURATIONS_RAW)
            .map(|c| c.recipes)
            .unwrap_or_default()
    })
}

/// Retail's duration, in milliseconds, for starting this recipe — or `None` when the
/// APK does not ship the recipe (or a tempering recipe is asked for a level it has no
/// row for), so the caller keeps its own fallback.
pub fn duration_ms(recipe_id: &Uuid, tempering_level: u64, batch_size: u32) -> Option<i64> {
    let r = table().get(recipe_id)?;
    let secs = if r.level_seconds.is_empty() {
        r.seconds? * u64::from(batch_size.max(1))
    } else {
        let idx = usize::try_from(tempering_level.checked_sub(1)?).ok()?;
        *r.level_seconds.get(idx)?
    };
    i64::try_from(secs).ok()?.checked_mul(1000)
}

pub fn len() -> usize {
    table().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Uuid {
        s.parse().unwrap()
    }

    /// Parsed explicitly so a broken file names itself instead of degrading to an
    /// empty table (which would silently restore the authored durations).
    #[test]
    fn the_table_loads_every_timed_recipe() {
        if let Err(e) = serde_json::from_str::<Corpus>(RECIPE_DURATIONS_RAW) {
            panic!("recipe_durations.json failed to parse: {e}");
        }
        // 617 Smithing + 140 Alchemy + 141 DecorationCrafting + 202 Enchanting + 571 Tempering.
        assert_eq!(len(), 1671);
    }

    /// #423: Gamermaus404's Steel Gauntlets (recipe f069258d) — the APK says 600 s.
    #[test]
    fn steel_gauntlets_take_ten_minutes() {
        assert_eq!(
            duration_ms(&u("f069258d-b765-4c68-9218-0522f131e69f"), 0, 1),
            Some(600_000)
        );
    }

    /// Retail captures 19341 and 6473: Smithing 32c68b36 completed 18 001 s after the
    /// response's Date header, 5004cd2f 21 s after it.
    #[test]
    fn captured_smithing_starts_match() {
        assert_eq!(
            duration_ms(&u("32c68b36-4a68-4c35-b39b-dcd9def5e650"), 0, 1),
            Some(18_000_000)
        );
        assert_eq!(
            duration_ms(&u("5004cd2f-12bf-49d8-97ca-544a1662c895"), 0, 1),
            Some(20_000)
        );
    }

    /// Retail capture 112652: Alchemy 7a9179ee at batch 5 completed 36 000 s out.
    #[test]
    fn flat_recipes_scale_with_batch_size() {
        let id = u("7a9179ee-2fef-4181-9ef9-6870c6c3d995");
        assert_eq!(duration_ms(&id, 0, 1), Some(7_200_000));
        assert_eq!(duration_ms(&id, 0, 5), Some(36_000_000));
        assert_eq!(
            duration_ms(&id, 0, 0),
            Some(7_200_000),
            "batch 0 is one craft"
        );
    }

    /// Retail captures 84722 (tL 10 → 17 400 s) and 227610-shaped (tL 1 → 900 s) on the
    /// same ladder; a temper to level 0 has no row.
    #[test]
    fn tempering_reads_the_target_level_row() {
        let id = u("b6e4dfaf-0d5b-470b-a26b-dd423222f435");
        assert_eq!(duration_ms(&id, 10, 1), Some(17_400_000));
        assert_eq!(duration_ms(&id, 1, 1), Some(900_000));
        assert_eq!(
            duration_ms(&id, 1, 5),
            Some(900_000),
            "batch does not scale a temper"
        );
        assert_eq!(duration_ms(&id, 0, 1), None);
        assert_eq!(duration_ms(&id, 11, 1), None);
    }

    #[test]
    fn an_unknown_recipe_has_no_duration() {
        assert_eq!(duration_ms(&Uuid::nil(), 0, 1), None);
    }
}
