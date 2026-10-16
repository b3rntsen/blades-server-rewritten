//! How loot rises with character level from 1 to 48 (report #398).
//!
//! HauDrauf (#398, 2026-10-16, translated): "It should rise with the player level,
//! like the offers in the shops — that's how the original was. Later you no longer
//! got Histcarp, Pearls etc., only things you can actually use. We'll test the
//! exact gradations." He approved the narrowing explicitly: low-tier items stop
//! appearing at higher levels (his standing #383/#398 rule is "never remove or
//! narrow without asking").
//!
//! The retail captures are too thin to drive this per source, so it is a rule, and
//! every number in it is a named constant below, to be tuned after his tests:
//!
//! * GEAR (weapons, armour, shields; never rings or necklaces, never artifacts): at
//!   level `L` only material tiers whose APK required level `R` satisfies
//!   `L - GEAR_WINDOW_BELOW <= R <= L + GEAR_WINDOW_ABOVE`, weighted by
//!   [`gear_weight`], a triangle peaking at `R = L`. An empty window (none today)
//!   falls back to the lowest tier. A piece the generator drew at a tier is moved
//!   to the drawn tier as the same kind of equipment ([`reshape_item`]).
//! * CRAFTING MATERIALS (ingredients, ingots and ores, gems and, while
//!   [`FADE_TOWN_MATERIALS`], town materials): each has an introduction level `I`
//!   ([`introduction_level`], from its APK tier). It can drop from
//!   `L >= I - MATERIAL_LOOKAHEAD`, at full weight up to `I`, then fading linearly
//!   to nothing at `I + MATERIAL_FADE_LEVELS` ([`material_weight`]). A faded stack
//!   is replaced by one drawn from the same source's own stacks at that level,
//!   weighted by how often the source pays them times their weight
//!   ([`reshape_stacks`]): the higher-tier materials take the faded share in
//!   proportion to what the source already paid them, at the quantities it paid.
//! * NEVER FADED: the seven effect (enchanting) ingredients
//!   ([`crate::util::dungeon::EFFECT_INGREDIENTS`]) and soul gems keep their own
//!   level rules, and their shares, untouched; so does the 8/17/75 jewellery
//!   split. Gold and every quantity keep their per-level values.
//!
//! Applied by every loot generator whose level input is 1-48: treasury chests (all
//! tiers), dungeon enemy and container loot (story quests, events, jobs),
//! story-quest rewards and the Abyss bonus material. Event rewards go through it
//! with their ladder and town materials exempt, which today leaves them as they
//! were (`EventQuestTemplate::apply_level_curve`). From level 49 every
//! source pays as level 100 ([`super::loot_level::effective_loot_level`]) and this
//! module is a no-op: [`applies`] is false.

use std::collections::HashMap;

use uuid::Uuid;

use super::loot_level::TOP_LOOT_FROM_LEVEL;
use super::store_bundles::mix;
use crate::economy::RewardGrant;
use crate::user_data::Item;

/// How many levels BELOW the character's level a gear tier may still be paid
/// (#398, HauDrauf 2026-10-16: "it should rise with the player level"). At level
/// 30 the lowest tier paid is the one unlocked at 20 or later (Elven, 23).
pub const GEAR_WINDOW_BELOW: i64 = 10;

/// How many levels ABOVE the character's level a gear tier may already be paid
/// (#398), the look-ahead the shops have (`store_bundles::LOOKAHEAD_LEVELS`).
pub const GEAR_WINDOW_ABOVE: i64 = 2;

/// How many levels before its introduction level a material may already drop
/// (#398).
pub const MATERIAL_LOOKAHEAD: i64 = 2;

/// Over how many levels after its introduction a material's weight fades from
/// full to nothing (#398, HauDrauf: "later you no longer got Histcarp, Pearls"):
/// Pearl (introduced at 1) is gone from 21, Histcarp (8) from 28.
pub const MATERIAL_FADE_LEVELS: i64 = 20;

/// A fixed reward (an event milestone, a story quest's reward) has no draw of
/// its own to take a replacement from, so a faded material in one is replaced
/// from what retail's dungeon tables (the enemy table and the level-keyed
/// containers) paid within this many levels of the character's (#398;
/// [`crate::util::dungeon::material_catalogue`]).
pub const MATERIAL_POOL_WINDOW: i64 = 2;

/// The share a faded material loses goes to the materials still at this weight
/// or more (#398, "higher-tier materials take their share"): with the defaults,
/// the materials introduced within the last ten levels or about to be. A
/// fading material keeps its own share times its weight but gains nothing, so
/// Histcarp at 25 is retail's Histcarp share there times 0.15, not a share of
/// everything the town materials lost.
pub const MATERIAL_REPLACEMENT_MIN_WEIGHT: f64 = 0.5;

/// A treasury chest replaces faded materials from what retail's chests of its
/// tier paid at its level; where that leaves fewer than this many kinds to draw
/// (Wooden chests above level 40, which retail opened four times), it draws
/// from retail's dungeon materials at the level too
/// ([`crate::util::dungeon::material_catalogue`]), so one kind does not fill
/// every chest (#381).
pub const CHEST_POOL_MIN_KINDS: usize = 4;

/// Whether the town materials (Lumber, Limestone, Clay, Marble, Bronze, Seed,
/// Crystal, Fabric, Dye; `util::dungeon::TOWN_MATERIALS`) fade like every other
/// crafting material (#398). All nine are APK tier 1, so with `true` none drops
/// from level 21 on and their share goes to the materials of the character's
/// tier. `false` keeps them at retail's rates at every level (retail never faded
/// them: Lumber is 15-20% of dungeon enemy results at every level 1-100).
pub const FADE_TOWN_MATERIALS: bool = true;

/// A material's introduction level by its APK tier (#398): the level the gear of
/// the same tier unlocks at (`ItemTemplate.requiredLevel`), the ladder retail's
/// own event rewards step their materials at (#362, 409 observations, none
/// contradicting). Tiers 11-13 (Pyrine, Arentine, Firebreath Opal) have no gear;
/// theirs is the level retail's loot first listed them at, all above 48.
pub const MATERIAL_TIER_INTRODUCTION_LEVEL: [(u64, i64); 13] = [
    (1, 1),   // Iron, Pearl, Garlic, the town materials
    (2, 1),   // Steel, Leather, Topaz, Lavender
    (3, 8),   // Silver, Histcarp, Garnet, Scrib Chitin
    (4, 13),  // Orichalcum, Amethyst, Blue Dartwing
    (5, 18),  // Dwarven, Gold Ingot, Ruby, Ectoplasm
    (6, 23),  // Moonstone, Quicksilver, Sapphire, Moon Sugar
    (7, 28),  // Malachite, Emerald, Nightshade, Chaurus Chitin
    (8, 33),  // Ebony, Stalhrim, Diamond, Giant's Toe
    (9, 39),  // Daedra Heart, Atronite, Daedroth Tooth
    (10, 45), // Dragon Bone, Dragon Scale, Dragon Claw, Faerite
    (11, 54), // Pyrine
    (12, 75), // Arentine
    (13, 77), // Firebreath Opal
];

/// Whether the curve shapes loot rolled at `level`: 1-48. From 49 every source
/// pays as level 100 and nothing here applies.
pub fn applies(level: i64) -> bool {
    level < TOP_LOOT_FROM_LEVEL
}

// -- gear --------------------------------------------------------------------

/// The weight of the gear tier unlocked at `required_level` for a character of
/// `level` (#398): a triangle, 1 at `required_level == level`, falling linearly
/// to 0 one level past either edge of the window, so the edge tiers keep a small
/// weight (`1 / (GEAR_WINDOW_BELOW + 1)` ten below, `1 / (GEAR_WINDOW_ABOVE + 1)`
/// two above). 0 outside the window.
pub fn gear_weight(required_level: i64, level: i64) -> f64 {
    let level = level.max(1);
    let d = required_level - level;
    if d > GEAR_WINDOW_ABOVE || -d > GEAR_WINDOW_BELOW {
        return 0.0;
    }
    if d >= 0 {
        1.0 - d as f64 / (GEAR_WINDOW_ABOVE + 1) as f64
    } else {
        1.0 - (-d) as f64 / (GEAR_WINDOW_BELOW + 1) as f64
    }
}

/// The share of gear paid at each tier (as its unlock level) at `level`,
/// ascending, summing to 1. An empty window falls back to the lowest tier.
pub fn gear_tier_shares(level: i64) -> Vec<(u64, f64)> {
    let mut levels: Vec<u64> = super::legendary_gear::gear_tiers().into_iter().map(|(r, _)| r).collect();
    levels.dedup();
    let weights: Vec<(u64, f64)> = levels.iter().map(|r| (*r, gear_weight(*r as i64, level))).collect();
    let total: f64 = weights.iter().map(|(_, w)| w).sum();
    if total <= 0.0 {
        return levels.first().map(|r| vec![(*r, 1.0)]).unwrap_or_default();
    }
    weights.into_iter().filter(|(_, w)| *w > 0.0).map(|(r, w)| (r, w / total)).collect()
}

/// A uniform float in [0, 1) from a seed.
fn unit(seed: u64) -> f64 {
    (mix(seed) >> 11) as f64 / (1u64 << 53) as f64
}

/// Move a weapon, armour piece or shield to the tier [`gear_tier_shares`] draws
/// for `level` (#398). Untouched: jewellery, artifacts, templates outside the
/// loot pool, a draw that lands on the piece's own tier, and every level from 49.
/// `true` when the piece changed.
pub fn reshape_item(item: &mut Item, level: i64, seed: u64) -> bool {
    if !applies(level) || super::store_bundles::is_artifact(&item.item_template_id) {
        return false;
    }
    let Some((own, _)) = super::legendary_gear::gear_tier_of(&item.item_template_id) else {
        return false;
    };
    let shares = gear_tier_shares(level);
    let mut at = unit(seed ^ 0x0398_6EA2_7135_0398);
    let mut target = shares.last().map(|(r, _)| *r);
    for (r, share) in &shares {
        if at < *share {
            target = Some(*r);
            break;
        }
        at -= share;
    }
    let Some(target) = target.filter(|t| *t != own) else {
        return false;
    };
    // Level 1 has two APK tiers (Iron and Steel): either, evenly.
    let tiers: Vec<u64> = super::legendary_gear::gear_tiers()
        .into_iter()
        .filter(|(r, _)| *r == target)
        .map(|(_, t)| t)
        .collect();
    if tiers.is_empty() {
        return false;
    }
    let tier = tiers[(mix(seed ^ 0x7135_0398) % tiers.len() as u64) as usize];
    match super::legendary_gear::retier(item, target, tier, seed) {
        Some(moved) => {
            *item = moved;
            true
        }
        None => false,
    }
}

// -- crafting materials ------------------------------------------------------

static MATERIAL_TIERS_RAW: &str = include_str!("../material_tiers.json");

#[derive(serde::Deserialize)]
struct MaterialTiers {
    materials: HashMap<Uuid, MaterialTier>,
}

#[derive(serde::Deserialize)]
struct MaterialTier {
    tier: u64,
}

/// APK tier of every crafting material (`script/extract_material_tiers.py`).
fn material_tiers() -> &'static HashMap<Uuid, u64> {
    static TABLE: std::sync::OnceLock<HashMap<Uuid, u64>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<MaterialTiers>(MATERIAL_TIERS_RAW)
            .map(|t| t.materials.into_iter().map(|(id, m)| (id, m.tier)).collect())
            .unwrap_or_default()
    })
}

/// Whether `item` is a crafting material the curve fades: a material in the APK,
/// not an effect ingredient, not a soul gem, and not a town material unless
/// [`FADE_TOWN_MATERIALS`].
pub fn is_curve_material(item: &Uuid) -> bool {
    let raw = item.as_u128();
    material_tiers().contains_key(item)
        && !crate::util::dungeon::EFFECT_INGREDIENTS.contains(&raw)
        && !super::store_bundles::SOUL_GEMS.contains(item)
        && (FADE_TOWN_MATERIALS || !crate::util::dungeon::TOWN_MATERIALS.contains(&raw))
}

/// The level a curve material is introduced at (#398): its APK tier's entry in
/// [`MATERIAL_TIER_INTRODUCTION_LEVEL`]. `None` for anything else.
pub fn introduction_level(item: &Uuid) -> Option<i64> {
    if !is_curve_material(item) {
        return None;
    }
    let tier = *material_tiers().get(item)?;
    MATERIAL_TIER_INTRODUCTION_LEVEL
        .iter()
        .find(|(t, _)| *t == tier)
        .map(|(_, level)| *level)
}

/// A material's weight at `level`, as a factor on what the source pays it at
/// (#398): 0 before `I - MATERIAL_LOOKAHEAD`, 1 up to `I`, then linearly down to
/// 0 at `I + MATERIAL_FADE_LEVELS`. 1 for everything the curve leaves alone, and
/// at every level from 49.
pub fn material_weight(item: &Uuid, level: i64) -> f64 {
    if !applies(level) {
        return 1.0;
    }
    let Some(introduced) = introduction_level(item) else {
        return 1.0;
    };
    let level = level.max(1);
    if introduced > level + MATERIAL_LOOKAHEAD {
        return 0.0;
    }
    if level <= introduced {
        return 1.0;
    }
    (1.0 - (level - introduced) as f64 / MATERIAL_FADE_LEVELS as f64).max(0.0)
}

/// One stack a source pays at a level: (item, quantity, how often).
pub type PoolStack = (Uuid, u64, f64);

/// Every curve-material stack in a set of weighted loot results (`{"n", "loot":
/// {"stackableItems"}}`), weighted by the results' `n`: a source's own pool.
pub fn pool_from_results<'a>(results: impl IntoIterator<Item = &'a serde_json::Value>) -> Vec<PoolStack> {
    let mut out: Vec<PoolStack> = Vec::new();
    for r in results {
        let n = r["n"].as_u64().unwrap_or(0) as f64;
        if n <= 0.0 {
            continue;
        }
        for (id, qty) in r["loot"]["stackableItems"].as_object().into_iter().flatten() {
            let (Ok(item), Some(qty)) = (Uuid::parse_str(id), qty.as_u64()) else { continue };
            if qty == 0 || !is_curve_material(&item) {
                continue;
            }
            match out.iter_mut().find(|s| s.0 == item && s.1 == qty) {
                Some(s) => s.2 += n,
                None => out.push((item, qty, n)),
            }
        }
    }
    out.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    out
}

/// The candidates a faded stack is replaced from: the pool's curve materials at
/// [`MATERIAL_REPLACEMENT_MIN_WEIGHT`] or more at `level`, each weighted by how
/// often the source pays it times its weight; if none is that fresh, every one
/// still dropping.
fn replacement_candidates(pool: &[PoolStack], level: i64) -> Vec<PoolStack> {
    let weighed: Vec<(PoolStack, f64)> = pool
        .iter()
        .filter(|(id, _, _)| is_curve_material(id))
        .map(|(id, q, n)| {
            let w = material_weight(id, level);
            ((*id, *q, n * w), w)
        })
        .filter(|((_, _, nw), _)| *nw > 0.0)
        .collect();
    let fresh: Vec<PoolStack> = weighed
        .iter()
        .filter(|(_, w)| *w >= MATERIAL_REPLACEMENT_MIN_WEIGHT)
        .map(|(c, _)| *c)
        .collect();
    if fresh.is_empty() {
        weighed.into_iter().map(|(c, _)| c).collect()
    } else {
        fresh
    }
}

/// The distinct curve materials in `pool` at [`MATERIAL_REPLACEMENT_MIN_WEIGHT`]
/// or more at `level`: what a faded stack can be replaced with.
pub fn replacement_kinds(pool: &[PoolStack], level: i64) -> usize {
    let mut ids: Vec<Uuid> = pool
        .iter()
        .filter(|(id, _, n)| *n > 0.0 && is_curve_material(id) && material_weight(id, level) >= MATERIAL_REPLACEMENT_MIN_WEIGHT)
        .map(|c| c.0)
        .collect();
    ids.sort();
    ids.dedup();
    ids.len()
}

/// Fade the curve materials in `stacks` for `level` (#398). Each is kept with
/// probability [`material_weight`]; otherwise it is replaced by a stack drawn
/// from `pool` (the source's stacks at this level), among the materials at
/// [`MATERIAL_REPLACEMENT_MIN_WEIGHT`] or more, weighted by how often the source
/// pays them times their weight, at the pool's quantity. So a fading material
/// keeps its own share times its weight, and the share it loses goes to the
/// materials of the newer tiers. Effect ingredients, soul gems, currencies and everything else
/// are not touched. A pool with nothing to draw keeps the stack (the fallback).
/// Deterministic in `seed`.
pub fn reshape_stacks(stacks: &mut HashMap<Uuid, u64>, level: i64, pool: &[PoolStack], seed: u64) {
    if !applies(level) {
        return;
    }
    let mut faded: Vec<(Uuid, f64)> = stacks
        .keys()
        .filter(|id| is_curve_material(id))
        .map(|id| (*id, material_weight(id, level)))
        .filter(|(_, w)| *w < 1.0)
        .collect();
    if faded.is_empty() {
        return;
    }
    faded.sort_by_key(|(id, _)| *id);
    let candidates = replacement_candidates(pool, level);
    let total: f64 = candidates.iter().map(|c| c.2).sum();
    for (id, weight) in faded {
        let key = (id.as_u128() as u64) ^ ((id.as_u128() >> 64) as u64);
        if unit(seed ^ key ^ 0x0398_FADE_0398_FADE) < weight || total <= 0.0 {
            continue;
        }
        let qty = stacks.remove(&id).unwrap_or(0);
        // A material the stack already holds is drawn again, then added to.
        let mut drawn = None;
        for attempt in 0..8u64 {
            let mut at = unit(seed ^ key.rotate_left(17) ^ (attempt + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)) * total;
            let mut pick = candidates.last().copied();
            for c in &candidates {
                if at < c.2 {
                    pick = Some(*c);
                    break;
                }
                at -= c.2;
            }
            drawn = pick;
            if pick.is_some_and(|p| !stacks.contains_key(&p.0)) {
                break;
            }
        }
        match drawn {
            Some((new, q, _)) => *stacks.entry(new).or_insert(0) += q,
            None => {
                stacks.insert(id, qty);
            }
        }
    }
}

/// [`reshape_stacks`] and [`reshape_item`] over a whole reward: its stackables
/// from `pool`, each weapon, armour piece and shield by its slot. `keep` exempts
/// stackables a source already levels by its own rule (an event's material
/// ladders).
pub fn reshape_grant(grant: &mut RewardGrant, level: i64, pool: &[PoolStack], seed: u64, keep: &dyn Fn(&Uuid) -> bool) {
    if !applies(level) {
        return;
    }
    let kept: Vec<(Uuid, u64)> = grant
        .stackable_items
        .iter()
        .filter(|(id, _)| keep(id))
        .map(|(id, q)| (*id, *q))
        .collect();
    for (id, _) in &kept {
        grant.stackable_items.remove(id);
    }
    reshape_stacks(&mut grant.stackable_items, level, pool, seed);
    for (id, q) in kept {
        *grant.stackable_items.entry(id).or_insert(0) += q;
    }
    for (slot, reward) in grant.items.iter_mut().enumerate() {
        reshape_item(&mut reward.item, level, mix(seed ^ 0x0398_17E3 ^ (slot as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_name(name: &str) -> Uuid {
        #[derive(serde::Deserialize)]
        struct Raw {
            materials: HashMap<Uuid, serde_json::Value>,
        }
        let raw: Raw = serde_json::from_str(MATERIAL_TIERS_RAW).unwrap();
        *raw.materials
            .iter()
            .find(|(_, m)| m["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not in material_tiers.json"))
            .0
    }

    #[test]
    fn the_material_table_loads_every_apk_material() {
        assert_eq!(material_tiers().len(), 68);
        assert!(MATERIAL_TIER_INTRODUCTION_LEVEL.iter().all(|(t, _)| (1..=13).contains(t)));
        for (_, tier) in material_tiers() {
            assert!(MATERIAL_TIER_INTRODUCTION_LEVEL.iter().any(|(t, _)| t == tier), "tier {tier}");
        }
    }

    /// Every material the APK calls a soul gem is in `SOUL_GEMS`, so none is faded.
    #[test]
    fn soul_gems_and_effect_ingredients_are_never_faded() {
        #[derive(serde::Deserialize)]
        struct Raw {
            materials: HashMap<Uuid, serde_json::Value>,
        }
        let raw: Raw = serde_json::from_str(MATERIAL_TIERS_RAW).unwrap();
        let gems: Vec<Uuid> = raw
            .materials
            .iter()
            .filter(|(_, m)| m["name"].as_str().unwrap().ends_with("Soul Gem"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(gems.len(), 10);
        for g in gems.iter().copied().chain(crate::util::dungeon::EFFECT_INGREDIENTS.iter().map(|i| Uuid::from_u128(*i))) {
            assert!(!is_curve_material(&g), "{g}");
            for level in 1..=100 {
                assert_eq!(material_weight(&g, level), 1.0);
            }
        }
    }

    /// The window and the triangle hold at every level 1-48.
    #[test]
    fn gear_shares_stay_in_the_window_at_every_level() {
        for level in 1..=48i64 {
            let shares = gear_tier_shares(level);
            let total: f64 = shares.iter().map(|s| s.1).sum();
            assert!((total - 1.0).abs() < 1e-9, "level {level}: {total}");
            for (r, share) in &shares {
                let r = *r as i64;
                assert!(*share > 0.0);
                assert!(level - GEAR_WINDOW_BELOW <= r && r <= level + GEAR_WINDOW_ABOVE, "level {level}: tier {r}");
            }
            // The triangle peaks at the level: shares rise towards it from below and
            // fall away from it above.
            for pair in shares.windows(2) {
                let (lo, hi) = (pair[0], pair[1]);
                if (hi.0 as i64) <= level {
                    assert!(lo.1 < hi.1, "level {level}: {shares:?}");
                }
                if (lo.0 as i64) >= level {
                    assert!(lo.1 > hi.1, "level {level}: {shares:?}");
                }
            }
        }
        assert_eq!(gear_tier_shares(1), vec![(1, 1.0)]);
        assert_eq!(gear_weight(30, 30), 1.0);
        assert_eq!(gear_weight(41, 30), 0.0);
        assert_eq!(gear_weight(33, 30), 0.0);
        assert_eq!(gear_weight(19, 30), 0.0);
    }

    /// The fade holds at every level 1-48 for every curve material.
    #[test]
    fn materials_fade_after_their_introduction_at_every_level() {
        for (item, _) in material_tiers() {
            let Some(intro) = introduction_level(item) else { continue };
            for level in 1..=48i64 {
                let w = material_weight(item, level);
                if level < intro - MATERIAL_LOOKAHEAD {
                    assert_eq!(w, 0.0, "{item} at {level}");
                } else if level <= intro {
                    assert_eq!(w, 1.0, "{item} at {level}");
                } else if level >= intro + MATERIAL_FADE_LEVELS {
                    assert_eq!(w, 0.0, "{item} at {level}");
                } else {
                    assert!(w > 0.0 && w < 1.0, "{item} at {level}");
                    assert!(w < material_weight(item, level - 1), "{item} at {level}: not fading");
                }
            }
            for level in 49..=100 {
                assert_eq!(material_weight(item, level), 1.0);
            }
        }
        // HauDrauf's examples: Pearl and Histcarp are gone by ~25.
        assert_eq!(material_weight(&by_name("Pearl"), 21), 0.0);
        assert_eq!(material_weight(&by_name("Histcarp"), 28), 0.0);
        assert!(material_weight(&by_name("Histcarp"), 20) > 0.0);
        // Ebony is not there before its gear and is at full weight when it unlocks.
        assert_eq!(material_weight(&by_name("Ebony Ingot"), 30), 0.0);
        assert_eq!(material_weight(&by_name("Ebony Ingot"), 33), 1.0);
    }

    #[test]
    fn a_faded_stack_is_replaced_from_the_pool_and_the_rest_is_kept() {
        let pearl = by_name("Pearl");
        let ebony = by_name("Ebony Ingot");
        let malachite = by_name("Malachite Ingot");
        let frost = Uuid::from_u128(crate::util::dungeon::EFFECT_INGREDIENTS[0]);
        let gem = super::super::store_bundles::SOUL_GEMS[0];
        let pool = vec![(pearl, 1, 100.0), (ebony, 2, 10.0), (malachite, 3, 10.0)];
        for seed in 0..200u64 {
            let mut s = HashMap::from([(pearl, 1u64), (frost, 4), (gem, 1)]);
            reshape_stacks(&mut s, 33, &pool, seed);
            assert!(!s.contains_key(&pearl), "Pearl at 33");
            assert_eq!(s[&frost], 4);
            assert_eq!(s[&gem], 1);
            assert!(s.get(&ebony) == Some(&2) || s.get(&malachite) == Some(&3), "{s:?}");
            // From 49: untouched.
            let mut s = HashMap::from([(pearl, 1u64)]);
            reshape_stacks(&mut s, 49, &pool, seed);
            assert_eq!(s, HashMap::from([(pearl, 1u64)]));
        }
        // Nothing to draw: kept.
        let mut s = HashMap::from([(pearl, 1u64)]);
        reshape_stacks(&mut s, 40, &[(pearl, 1, 5.0)], 1);
        assert_eq!(s, HashMap::from([(pearl, 1u64)]));
    }
}
