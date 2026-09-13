//! Capture-derived static game definitions — the catalogs/templates the retail
//! server held that `parsed.json` does not (it ships as a 67-byte stub). Each type
//! deserializes verbatim from a JSON file extracted from `api_captures` by
//! `blades-capture/scripts/extract-static-data.py` and loaded at server start into
//! [`StaticData`]. Everything here is pure data — no IO, no DB — so it round-trips
//! in tests against captured fixtures.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::economy::RewardGrant;
use crate::features::challenges::ChallengeTemplate;
use crate::features::daily_reward::DailyRewardDef;
use crate::features::game_events::EventDef;
use crate::user_data::ItemSingleProperty;

/// One reward line of a global gift (`{itemTemplateId, quantity}`). The template
/// may be a currency UUID (Gold/Sigil/Gems), in which case claiming credits the
/// wallet rather than the backpack — see [`crate::features::gifts`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftItem {
    pub item_template_id: Uuid,
    pub quantity: u64,
}

/// A chest in a global-gift override. Retail identifies these by rarity only;
/// season-gift claims open them immediately instead of putting them in treasury.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftChest {
    pub rarity: u64,
}

/// A global gift definition (the captured `globalGiftOverride` block). Time-windowed
/// and claim-count-limited; `startTime`/`endTime` of 0 mean "no bound".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftDef {
    pub global_gift_id: Uuid,
    #[serde(default)]
    pub items: Vec<GiftItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chests: Vec<GiftChest>,
    pub start_time: i64,
    pub end_time: i64,
    pub claim_count_limit: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A news/announcement entry (`GET /announcements`). Server-authoritative list; the
/// `assetUrl` points at Bethesda's (now-defunct) CDN — harmless, the client just
/// fails to fetch the banner image. Carried verbatim from captures.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Announcement {
    pub id: String,
    pub r#type: String,
    pub start_time: i64,
    pub ttl: i64,
    pub asset_url: String,
}

/// One catalog bundle reference (`{id, quantity}`). The client renders the bundle's
/// item + price from its own asset data; the server only lists which are in stock.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShopBundleRef {
    pub id: Uuid,
    pub quantity: u64,
}

/// A shop's wallet line (its gold, e.g. for buybacks).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShopWalletEntry {
    pub currency_id: Uuid,
    pub balance: i64,
}

/// A representative catalog for a shop template (bundle list + the shop's wallet).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShopCatalogTemplate {
    #[serde(default)]
    pub bundles: Vec<ShopBundleRef>,
    #[serde(default)]
    pub wallet: Vec<ShopWalletEntry>,
}

/// Town vendor shop catalogs (capture-derived). `by_shop` routes a captured shopId to
/// its template; `by_template` holds a representative catalog per shop type; `default`
/// is the fallback template for an unseen shopId (so a shop is never empty/timing-out).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShopData {
    #[serde(default)]
    pub by_shop: HashMap<Uuid, Uuid>,
    #[serde(default)]
    pub by_template: HashMap<Uuid, ShopCatalogTemplate>,
    #[serde(default)]
    pub default: Option<Uuid>,
}

impl ShopData {
    /// The catalog template for a shop: its captured mapping, else the default.
    pub fn catalog_for(&self, shop_id: &Uuid) -> Option<&ShopCatalogTemplate> {
        let tid = self.by_shop.get(shop_id).or(self.default.as_ref())?;
        self.by_template.get(tid)
    }

    pub fn template_for(&self, shop_id: &Uuid) -> Option<Uuid> {
        self.by_shop.get(shop_id).copied().or(self.default)
    }
}

/// What buying one unit of a shop bundle costs + grants (capture-derived).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShopBundle {
    #[serde(default)]
    pub currency_id: Option<Uuid>,
    #[serde(default)]
    pub price: u64,
    #[serde(default)]
    pub grant: RewardGrant,
}

/// A craft recipe definition (capture-derived). Holds the `craftingTypeId` and the
/// verbatim `results` object (either `{"items":[...]}` or `{"stackableItems":{...}}`).
/// `duration_ms` is how long the job runs before `/finish` is needed (0 = instant).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recipe {
    pub crafting_type_id: Uuid,
    /// Verbatim captured `results` object — kept as raw `Value` to avoid re-modelling
    /// the items/stackableItems union; the craft handlers deserialize it at use time.
    pub results: Value,
    #[serde(default)]
    pub duration_ms: i64,
}

/// One observed enchant outcome — the `ENCHANTING` property set a recipe applied to an
/// item (+ the item's resulting `arcaneTier`). Retail rolls a random set from a pool;
/// we keep every distinct observed outcome and the server picks one deterministically.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnchantOutcome {
    #[serde(default)]
    pub enchanting: Vec<ItemSingleProperty>,
    /// Arcane tier the item ends at, applied to [`crate::user_data::Item::arcane_tier`]
    /// by the enchant branch of `apply_item_mod`. `None` leaves the item without one —
    /// retail omits the key rather than sending `arcaneTier: 0`.
    #[serde(default)]
    pub arcane_tier: Option<u64>,
}

/// A temper/enchant recipe — a `POST /crafts` request carrying an `itemId` that MODIFIES
/// an existing backpack item, rather than minting a new one (see [`Recipe`]).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemModRecipe {
    #[serde(default)]
    pub crafting_type_id: Uuid,
    #[serde(default)]
    pub duration_ms: i64,
    /// `"temper"` (the request's `temperingLevel` drives it) or `"enchant"` (one of
    /// `outcomes` is applied).
    #[serde(default)]
    pub kind: String,
    /// Observed enchant outcomes (enchant recipes only; empty for temper).
    #[serde(default)]
    pub outcomes: Vec<EnchantOutcome>,
}

/// One fixed floor entry for the abyss (floors 1–24, captured from prod).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssFixedSlice {
    pub dungeon_settings_id: Uuid,
    pub difficulty_level: u32,
}

/// One future-reward threshold: reaching `score` grants these stackable items.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssFutureRewardDef {
    pub score: u32,
    #[serde(default)]
    pub stackable_items: HashMap<Uuid, u64>,
}

/// One `difficultyCurve` row: the `difficulty_level` assigned to a 1-based `floor`.
/// Floors 1–24 are authoritative (the captured ladder); 25+ are an authored ramp.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssDifficultyEntry {
    pub floor: u32,
    pub difficulty_level: u32,
}

/// A monster family's power tier (`1`=weak/shallow .. `9`=tough/deep). Keyed in the
/// file by a lowercased family token (e.g. `"goblin"`, `"dremora"`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssMonsterTier {
    #[serde(default)]
    pub tier: u32,
}

/// A depth band: for floors `floor_min..=floor_max`, the eligible monster tiers and
/// their relative spawn weight (`tierWeights` keyed by the tier as a STRING). Deeper
/// bands weight higher tiers so weak monsters appear shallow, tough monsters deep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssDepthBand {
    pub floor_min: u32,
    pub floor_max: u32,
    /// `{ "<tier>": weight }` — the JSON keys the tier as a string.
    #[serde(default)]
    pub tier_weights: HashMap<String, u32>,
}

/// One entry of the `dungeonPool`: an abyss dungeon-setting the client can render.
/// `monsters[0]` (a mixed-case family token) resolves to a tier via `monster_tiers`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssDungeonDef {
    #[serde(default)]
    pub handle: String,
    #[serde(default)]
    pub environment: String,
    #[serde(default)]
    pub monsters: Vec<String>,
    #[serde(default)]
    pub is_boss: bool,
    #[serde(default)]
    pub enemy_count: u32,
}

/// Static abyss definitions loaded from `deploy/static/abyss.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssStaticData {
    /// The 24 fixed floors served verbatim for every run (indices 0–23).
    #[serde(default)]
    pub fixed_slices: Vec<AbyssFixedSlice>,
    /// Pool of dungeon-settings UUIDs used to extend the run beyond floor 24
    /// (all at difficultyLevel 100, cycled via `(seed + floor) % pool.len()`).
    /// Fallback when `dungeon_pool`/`depth_bands` are absent.
    #[serde(default)]
    pub random_pool: Vec<Uuid>,
    /// Score thresholds that trigger in-run reward grants.
    #[serde(default)]
    pub future_rewards: Vec<AbyssFutureRewardDef>,
    /// Captured `algorithmVersion` baked into every generated run.
    #[serde(default)]
    pub algorithm_version: u32,
    /// How many floors the prod server pre-generated per run (informational).
    #[serde(default)]
    pub total_pregen_floors: u32,
    /// Per-floor difficulty ramp (`[{floor, difficultyLevel}]`, 150 rows). Deep
    /// floors read their difficulty from here; absent → the legacy hard-coded 100.
    #[serde(default)]
    pub difficulty_curve: Vec<AbyssDifficultyEntry>,
    /// `familyKey -> {tier}` — every monster family grouped to a power tier.
    #[serde(default)]
    pub monster_tiers: HashMap<String, AbyssMonsterTier>,
    /// Floor→eligible-tier weighting bands (weak shallow, tough deep).
    #[serde(default)]
    pub depth_bands: Vec<AbyssDepthBand>,
    /// Every abyss dungeon-setting the client can render, keyed by its UUID.
    #[serde(default)]
    pub dungeon_pool: HashMap<Uuid, AbyssDungeonDef>,
    /// `AbyssScaling_Backend` scalars. Only `slices_count` and
    /// `slices_count_above_player_level` are measured; the rest are still guessed.
    #[serde(default)]
    pub scaling_backend: AbyssScalingBackend,
    /// `AbyssScaling._abyssScalingCurve` — the gold/XP bonus step curve.
    #[serde(default)]
    pub scaling_curve: AbyssScalingCurve,
    /// `AbyssScaling._perFloorData` — the 150 per-floor base gold/XP rows.
    #[serde(default)]
    pub per_floor_rewards: AbyssPerFloorRewards,
    /// `AbyssScaling._sameLevelKillScore` / `_underLeveledKillScore` /
    /// `_overLeveledKillScore` — in-run score per kill.
    #[serde(default)]
    pub kill_scores: AbyssKillScores,
}

/// `AbyssScaling_Backend` scalars from `abyss.json`'s `scalingBackend`.
///
/// `slices_count_above_player_level` is `AbyssScaling._slicesCountAbovePlayerLevel`,
/// measured from the APK at **20**. The file shipped `2` for months. The other three
/// live in `AbyssScaling_Backend`, a server-only asset absent from the APK, and are
/// still guesses — nothing in this crate reads them yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssScalingBackend {
    #[serde(default = "default_slices_count")]
    pub slices_count: u32,
    #[serde(default)]
    pub max_difficulty: u32,
    #[serde(default = "default_slices_above_player_level")]
    pub slices_count_above_player_level: u32,
    #[serde(default)]
    pub hardcore_slice_difficulty_offset: i32,
    #[serde(default)]
    pub minimum_success_rating: f64,
}

fn default_slices_count() -> u32 {
    150
}

/// The measured `_slicesCountAbovePlayerLevel`. Also the `Default`, so a stale
/// `abyss.json` cannot silently reinstate the old `2`.
fn default_slices_above_player_level() -> u32 {
    20
}

impl Default for AbyssScalingBackend {
    fn default() -> Self {
        Self {
            slices_count: default_slices_count(),
            max_difficulty: 400,
            slices_count_above_player_level: default_slices_above_player_level(),
            hardcore_slice_difficulty_offset: 10,
            minimum_success_rating: 0.0,
        }
    }
}

/// One `_abyssScalingCurve` breakpoint: at `difficulty_offset` and above (until the
/// next breakpoint), rewards are multiplied by these. Gold and XP are always equal in
/// the source data, but they are separate fields in the asset so they stay separate here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssScalingCurveEntry {
    pub difficulty_offset: i32,
    pub gold_bonus_multiplier: f64,
    #[serde(rename = "xpBonusMultiplier")]
    pub xp_bonus_multiplier: f64,
}

/// `AbyssScaling._abyssScalingCurve`, wrapped so the JSON can carry `_source`/`_note`
/// siblings next to `entries`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssScalingCurve {
    #[serde(default)]
    pub entries: Vec<AbyssScalingCurveEntry>,
}

/// One `_perFloorData` row: the base gold/XP for clearing the floor at wire index
/// `floor`, before the scaling-curve multiplier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssPerFloorReward {
    pub floor: u32,
    pub base_gold_reward: u64,
    /// `baseXPReward` — the game's own capitalisation, which `rename_all = "camelCase"`
    /// would mangle to `baseXpReward`.
    #[serde(rename = "baseXPReward")]
    pub base_xp_reward: u64,
}

/// `AbyssScaling._perFloorData` (150 rows, indices 0–149).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssPerFloorRewards {
    #[serde(default)]
    pub entries: Vec<AbyssPerFloorReward>,
}

/// `AbyssScaling`'s three kill-score fields.
///
/// Values and list lengths are read from the APK. The index alignment
/// (`levelDelta > 0 → under[delta-1]`, `levelDelta < 0 → over[-delta-1]`) is backed by
/// wire evidence: a captured floor-43 run at `initialPlayerLevel` 45 (`levelDelta` -2)
/// scored 16.0, which `over[1] = 8` reaches as `8 × 2` (a boss `killScoreMultiplier`)
/// while `over[2] = 5` cannot reach under any of the 0.33 / 1.0 / 2.0 multipliers.
/// See [`AbyssStaticData::kill_score`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbyssKillScores {
    #[serde(default)]
    pub same_level_kill_score: i64,
    #[serde(default)]
    pub under_leveled_kill_score: Vec<i64>,
    #[serde(default)]
    pub over_leveled_kill_score: Vec<i64>,
}

// ── Built-in fallbacks ──────────────────────────────────────────────────────
//
// `deploy/static/` is a bind-mounted data directory on the box: merging this repo
// ships CODE but not DATA. Without these, a code-only deploy would read an
// `abyss.json` that has no `perFloorRewards.entries` and grant players ZERO gold and
// ZERO XP for a completed run. The fallbacks reproduce the shipped table exactly (a
// unit test asserts file == fallback across every floor, offset and level delta), so the
// only difference between "data deployed" and "not yet" is where the numbers came from.

/// `_perFloorData[i] = (10 + 8i, 10 + 2i)`, where `i` is the wire `floorIndex`. Exactly
/// linear over all 150 rows, and confirmed to the unit against five single-floor retail
/// `/end` captures (floorIndex 49 → 402/108, 90 → 730/190, 118 → 954/246, 147 →
/// 1186/304, 149 → 1202/308, before the multiplier). The rows are the game data; this is
/// the observation about them, used only when the file has no rows.
fn fallback_base_rewards(floor_index: u32) -> (u64, u64) {
    let i = floor_index.min(FALLBACK_LAST_FLOOR_INDEX) as u64;
    (10 + 8 * i, 10 + 2 * i)
}

/// `_perFloorData` has 150 rows, indices 0–149; past the end the last row repeats.
const FALLBACK_LAST_FLOOR_INDEX: u32 = 149;

/// `_abyssScalingCurve` breakpoints: `(difficultyOffset, multiplier)`. Gold == XP.
/// There is no negative-offset breakpoint — below offset 0 the multiplier is 1.0.
const FALLBACK_CURVE: [(i32, f64); 6] = [
    (0, 1.25),
    (2, 2.0),
    (4, 3.0),
    (6, 4.0),
    (10, 5.0),
    (14, 6.0),
];

const FALLBACK_SAME_LEVEL_KILL_SCORE: i64 = 10;
/// `_underLeveledKillScore` — 100 entries; the tail repeats the last value.
const FALLBACK_UNDER_LEVELED_HEAD: [i64; 6] = [12, 15, 19, 24, 30, 40];
const FALLBACK_UNDER_LEVELED_TAIL: i64 = 30;
const FALLBACK_UNDER_LEVELED_LEN: usize = 100;
/// `_overLeveledKillScore` — 60 entries; the tail repeats the last value.
const FALLBACK_OVER_LEVELED_HEAD: [i64; 6] = [10, 8, 5, 4, 3, 2];
const FALLBACK_OVER_LEVELED_TAIL: i64 = 1;
const FALLBACK_OVER_LEVELED_LEN: usize = 60;

/// The built-in `_underLeveledKillScore` list, materialised.
pub fn fallback_under_leveled_kill_score() -> Vec<i64> {
    let mut v = FALLBACK_UNDER_LEVELED_HEAD.to_vec();
    v.resize(FALLBACK_UNDER_LEVELED_LEN, FALLBACK_UNDER_LEVELED_TAIL);
    v
}

/// The built-in `_overLeveledKillScore` list, materialised.
pub fn fallback_over_leveled_kill_score() -> Vec<i64> {
    let mut v = FALLBACK_OVER_LEVELED_HEAD.to_vec();
    v.resize(FALLBACK_OVER_LEVELED_LEN, FALLBACK_OVER_LEVELED_TAIL);
    v
}

/// Index a kill-score list, repeating the last entry past its end (both lists end in a
/// long flat tail, so a delta beyond the table keeps that tail's value rather than
/// falling off to zero).
fn kill_score_at(list: &[i64], index: usize, fallback: &[i64]) -> Option<i64> {
    let list = if list.is_empty() { fallback } else { list };
    if list.is_empty() {
        return None;
    }
    Some(*list.get(index).unwrap_or_else(|| list.last().unwrap()))
}

impl AbyssStaticData {
    /// The difficulty level for a 1-based `floor`: `difficulty_curve[floor-1]` when the
    /// curve is present, else `fallback` (the legacy hard-coded deep-floor difficulty).
    pub fn difficulty_for_floor(&self, floor: u32, fallback: u32) -> u32 {
        self.difficulty_curve
            .iter()
            .find(|e| e.floor == floor)
            .map(|e| e.difficulty_level)
            .unwrap_or(fallback)
    }

    /// The power tier of a dungeon's primary monster family, via `monster_tiers`.
    /// The dungeon families are mixed-case (`"DragonAncientFire"`); the tier keys are
    /// lowercased (`"dragonancientfire"`). Exact lowercased match first, else the
    /// longest tier-key that PREFIXES the family (so composite families like
    /// `"GoblinSkeleton"` fall back to `"goblin"`). `None` if nothing matches.
    pub fn dungeon_tier(&self, dungeon: &AbyssDungeonDef) -> Option<u32> {
        let family = dungeon.monsters.first()?.to_ascii_lowercase();
        if let Some(t) = self.monster_tiers.get(&family) {
            return Some(t.tier);
        }
        self.monster_tiers
            .iter()
            .filter(|(k, _)| family.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, t)| t.tier)
    }

    /// The depth band covering a 1-based `floor`, if any.
    pub fn band_for_floor(&self, floor: u32) -> Option<&AbyssDepthBand> {
        self.depth_bands
            .iter()
            .find(|b| floor >= b.floor_min && floor <= b.floor_max)
    }

    /// `(base_gold, base_xp)` for clearing the floor at wire `floor_index`, BEFORE the
    /// scaling-curve multiplier (`AbyssScaling.GetBaseGoldAndXPRewards`).
    ///
    /// Exact row first; past the last row (index 149) the last row repeats. With no
    /// table at all, [`fallback_base_rewards`].
    pub fn base_rewards_for_floor(&self, floor_index: u32) -> (u64, u64) {
        let rows = &self.per_floor_rewards.entries;
        if rows.is_empty() {
            return fallback_base_rewards(floor_index);
        }
        let row = rows
            .iter()
            .find(|r| r.floor == floor_index)
            .or_else(|| rows.iter().max_by_key(|r| r.floor));
        match row {
            Some(r) => (r.base_gold_reward, r.base_xp_reward),
            None => fallback_base_rewards(floor_index),
        }
    }

    /// `(gold_multiplier, xp_multiplier)` for a `difficulty_offset`, which is the
    /// SLICE'S generated `difficultyLevel` minus the run's `initialPlayerLevel` — not
    /// the floor index minus it. The two diverge as soon as a run starts below the
    /// player's level.
    ///
    /// A STEP function: the highest breakpoint whose offset is `<=` the argument. The
    /// asset has no negative-offset breakpoint, so a negative offset matches nothing and
    /// the multiplier is **1.0** — the unmultiplied base reward, measured (a single-floor
    /// run on floor 49 at `initialPlayerLevel` 59, offset -10, paid exactly 402/108).
    /// It is neither a penalty (the `{-5, 0.5, 0.5}` row this file used to carry was an
    /// invention) nor a clamp up to the offset-0 row's ×1.25. Above the last breakpoint
    /// the curve plateaus (×6).
    pub fn multiplier_for_offset(&self, difficulty_offset: i32) -> (f64, f64) {
        let entries = &self.scaling_curve.entries;
        if entries.is_empty() {
            let mut m = 1.0;
            for (off, mult) in FALLBACK_CURVE {
                if difficulty_offset >= off {
                    m = mult;
                }
            }
            return (m, m);
        }
        match entries
            .iter()
            .filter(|e| e.difficulty_offset <= difficulty_offset)
            .max_by_key(|e| e.difficulty_offset)
        {
            Some(e) => (e.gold_bonus_multiplier, e.xp_bonus_multiplier),
            // Below every breakpoint: no bonus, base reward only.
            None => (1.0, 1.0),
        }
    }

    /// The in-run score for ONE kill at `level_delta = enemyLevel - initialPlayerLevel`,
    /// before the enemy's own `killScoreMultiplier` (`AbyssScaling.GetKillScore`).
    /// Past the end of either list its last entry repeats. See [`AbyssKillScores`] for
    /// the evidence behind the index alignment.
    pub fn kill_score(&self, level_delta: i32) -> i64 {
        let k = &self.kill_scores;
        match level_delta.cmp(&0) {
            std::cmp::Ordering::Equal => {
                if k.same_level_kill_score != 0 {
                    k.same_level_kill_score
                } else {
                    FALLBACK_SAME_LEVEL_KILL_SCORE
                }
            }
            std::cmp::Ordering::Greater => kill_score_at(
                &k.under_leveled_kill_score,
                (level_delta - 1) as usize,
                &fallback_under_leveled_kill_score(),
            )
            .unwrap_or(FALLBACK_SAME_LEVEL_KILL_SCORE),
            std::cmp::Ordering::Less => kill_score_at(
                &k.over_leveled_kill_score,
                (-level_delta - 1) as usize,
                &fallback_over_leveled_kill_score(),
            )
            .unwrap_or(FALLBACK_SAME_LEVEL_KILL_SCORE),
        }
    }
}

/// The APK-extracted `recipeId -> CraftingType` table (`recipe_crafting_types.json`).
///
/// The client keys a `CraftingStation` by **CraftingType**, never by recipe. Echo
/// anything else in a craft job's `craftingTypeId` and `GetCraftingStation()` returns
/// null: the town-build coroutine never completes and the player is stuck on the
/// loading screen with no client-side way out (report #34).
///
/// Until this table existed there was no recipe→station mapping on the server at all
/// beyond the ~34 captured `recipes.json` rows, so an un-captured recipe could only be
/// guessed at. The table is walked out of the APK's own `RecipeData._recipeMap` (each
/// `RecipeActionMapping` pairs a `UidCraftingTypePointer` with the `RecipeList` whose
/// recipes belong to it), so it covers every recipe the client ships — 2,978 across the
/// 7 crafting types. Nothing in it is authored: a recipe absent here is absent from the
/// shipped client data, and the caller's existing fallback still applies.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeCraftingTypes {
    /// The 7 `CraftingType` definitions (Smithing, Alchemy, Enchanting, Tempering,
    /// DecorationCrafting, Salvaging, Repairing).
    #[serde(default)]
    pub crafting_types: Vec<CraftingTypeDef>,
    /// `recipeId -> {craftingTypeId, …}`.
    #[serde(default)]
    pub recipes: HashMap<Uuid, RecipeCraftingType>,
}

impl RecipeCraftingTypes {
    /// The `CraftingType` uuid a recipe belongs to, or `None` when the recipe is not in
    /// the shipped client data. Never returns the recipe id.
    pub fn crafting_type_of(&self, recipe_id: &Uuid) -> Option<Uuid> {
        self.recipes.get(recipe_id).map(|r| r.crafting_type_id)
    }

    /// The item template a recipe mints, or `None` when the shipped client data has no
    /// output for it (Enchanting / Tempering / Repairing / Salvaging — see
    /// [`RecipeCraftingType::output_item_template_id`]) or the recipe is not in the
    /// table at all. Never returns the recipe id: recipe uuids and item-template uuids
    /// are disjoint namespaces, and confusing the two is what put a recipe id in a
    /// `stackableItems` key and hung town load.
    pub fn output_template_of(&self, recipe_id: &Uuid) -> Option<Uuid> {
        self.recipes.get(recipe_id)?.output_item_template_id
    }

    /// The uuid of the crafting type with the given `editorName` (e.g. `"Smithing"`).
    pub fn type_by_name(&self, editor_name: &str) -> Option<Uuid> {
        self.crafting_types
            .iter()
            .find(|t| t.editor_name == editor_name)
            .map(|t| t.crafting_type_id)
    }

    /// The `results` shape retail pairs with this CraftingType, or `None` when the type
    /// is not in the loaded table (an unloaded/absent table therefore disables every
    /// check built on this, which is the safe direction).
    ///
    /// `craftingTypeId` and `results` are NOT independent fields: across all 482 craft
    /// records in the retail captures (`https://blades.bgs.services/.../crafts`) the
    /// pairing is total and without exception —
    ///
    /// | CraftingType       | `results` shape  | retail records |
    /// |--------------------|------------------|----------------|
    /// | Smithing           | `items`          | 26 / 26        |
    /// | Enchanting         | `items`          | 297 / 297      |
    /// | Tempering          | `items`          | 51 / 51        |
    /// | Alchemy            | `stackableItems` | 100 / 100      |
    /// | DecorationCrafting | `stackableItems` | 8 / 8          |
    ///
    /// which is the game's own division: the benches that mint or mutate a piece of
    /// GEAR hand back one instanced item, while the Alchemist and the Workshop hand
    /// back a count of a stackable template. Salvaging and Repairing are unobserved in
    /// the captures (neither ever created a *job*) but are item-input benches, so they
    /// classify with the instanced side.
    ///
    /// The two stackable types are exactly the non-item-input types other than
    /// Smithing; `is_item_input_crafting` alone cannot separate Smithing from Alchemy,
    /// so the split is by name, pinned by
    /// `every_crafting_type_classifies_as_the_captures_show_it`.
    pub fn result_shape_of_type(&self, crafting_type_id: &Uuid) -> Option<CraftResultShape> {
        let def = self
            .crafting_types
            .iter()
            .find(|t| t.crafting_type_id == *crafting_type_id)?;
        Some(match def.editor_name.as_str() {
            "Alchemy" | "DecorationCrafting" => CraftResultShape::Stackable,
            "" => return None,
            _ => CraftResultShape::Instanced,
        })
    }
}

/// What a completed craft hands back, per CraftingType. See
/// [`RecipeCraftingTypes::result_shape_of_type`] for the retail evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraftResultShape {
    /// `{"items":[{id, itemTemplateId, temperingLevel, durability}]}` — one instanced
    /// piece of gear.
    Instanced,
    /// `{"stackableItems":{"<itemTemplateId>": n}}` — a count of a stackable template.
    Stackable,
}

/// One row of `recipe_crafting_types.json.craftingTypes`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CraftingTypeDef {
    pub crafting_type_id: Uuid,
    #[serde(default)]
    pub editor_name: String,
    #[serde(default)]
    pub is_item_input_crafting: bool,
    #[serde(default)]
    pub recipe_count: u32,
}

/// One row of `recipe_crafting_types.json.recipes`. `nameKey` / `name` are carried for
/// human debugging (the next "which bench is this?" report), not used by any logic.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeCraftingType {
    pub crafting_type_id: Uuid,
    #[serde(default)]
    pub crafting_type: String,
    #[serde(default)]
    pub name_key: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// The item template this recipe OUTPUTS, from the APK's
    /// `Recipe._outputs[]._itemTemplate` (`RecipeOutput` has no other serialized field,
    /// so the quantity is the constant 1).
    ///
    /// Present for exactly the three stations whose recipes mint a NEW item — Smithing
    /// (617/617), Alchemy (140/140), DecorationCrafting (141/141). `None` for
    /// Enchanting, whose outputs are item *properties* with a null template pointer, and
    /// for Tempering / Repairing / Salvaging, which mutate their input item and ship no
    /// `_outputs` at all. `None` therefore means "the shipped client data has no output
    /// for this recipe", never "we did not look", so a caller must read it as a reason to
    /// keep its existing fallback rather than to invent a result.
    #[serde(default)]
    pub output_item_template_id: Option<Uuid>,
}

/// One craftable the Forge's Smithing station can mint, from `smith_craftables.json`.
/// The craftable LIST is client-side (its RecipeManager, gated by forge level); the
/// server's job is to MINT the picked item at its `itemTemplateId` + `gradeIndex`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmithCraftable {
    pub item_template_id: Uuid,
    #[serde(default)]
    pub grade_index: u32,
    /// The captured recipe id if this exact item was captured, else `None` (the client
    /// sends its own recipeId and the server mints from `itemTemplateId` + grade).
    #[serde(default)]
    pub recipe_id: Option<Uuid>,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The Forge / Smithy craftables catalog, resolved at load into fast lookups: a smith
/// craft `POST /crafts` resolves against these by its `recipeId` first, then (for the
/// common un-captured recipe, where the client sends its own id) by `itemTemplateId`.
/// The single `smithing_crafting_type_id` is always echoed for a forge craft — never
/// the recipe id (echoing recipe_id hangs the client, fix e5659c9).
#[derive(Debug, Clone, Default)]
pub struct SmithCraftables {
    /// The Smithing station's craftingTypeId (echoed for every forge craft).
    pub smithing_crafting_type_id: Option<Uuid>,
    /// The Forge building typeId (so a craft's `buildingId` can be recognised).
    pub forge_building_type_id: Option<Uuid>,
    /// Captured recipeId -> craftable (only the items that carry a real recipe id).
    pub by_recipe: HashMap<Uuid, SmithCraftable>,
    /// itemTemplateId -> craftable (every craftable; the un-captured-recipe path).
    pub by_template: HashMap<Uuid, SmithCraftable>,
}

impl SmithCraftables {
    /// Resolve a smith craftable for a `POST /crafts` request: by the captured recipe
    /// id first, else treat the request's `recipe_id` as an `itemTemplateId` (the
    /// client sends its own recipe/template id for the un-captured common case).
    pub fn resolve(&self, request_recipe_id: &Uuid) -> Option<&SmithCraftable> {
        self.by_recipe
            .get(request_recipe_id)
            .or_else(|| self.by_template.get(request_recipe_id))
    }

    /// Build the resolved lookups from the raw `smith_craftables.json` shape. A partial
    /// or empty file degrades to empty lookups (the smith craft then keeps the lenient
    /// placeholder path rather than failing).
    pub fn from_raw(raw: SmithCraftablesFile) -> Self {
        let mut by_recipe = HashMap::new();
        let mut by_template = HashMap::new();
        for level in raw.levels.values() {
            for item in &level.newly_unlocked_items {
                if let Some(rid) = item.recipe_id {
                    by_recipe.insert(rid, item.clone());
                }
                // Last-writer-wins per template is fine — a template appears once.
                by_template.insert(item.item_template_id, item.clone());
            }
        }
        SmithCraftables {
            smithing_crafting_type_id: raw.forge.smithing_crafting_type_id,
            forge_building_type_id: raw.forge.building_type_id,
            by_recipe,
            by_template,
        }
    }
}

/// Raw `smith_craftables.json` deserialization shape (the loader transforms it into the
/// resolved [`SmithCraftables`]). Only the fields the server needs are modelled; the
/// rich `_meta` / material-ladder blocks are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmithCraftablesFile {
    #[serde(default)]
    pub forge: SmithForge,
    #[serde(default)]
    pub levels: HashMap<String, SmithLevel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmithForge {
    #[serde(default)]
    pub building_type_id: Option<Uuid>,
    #[serde(default)]
    pub smithing_crafting_type_id: Option<Uuid>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmithLevel {
    #[serde(default)]
    pub newly_unlocked_items: Vec<SmithCraftable>,
}

/// One quest in the daily-rotation pool (`quests_daily.json.dailyQuestPool`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyQuestDef {
    pub quest_id: Uuid,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub dungeon_id: Option<Uuid>,
    #[serde(default)]
    pub objective_count: u32,
}

/// The per-skull enemy-level offset applied on top of the player level (`levelScaling`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnemyLevelScaling {
    /// `skull(as string) -> signed level offset`.
    #[serde(default)]
    pub offset_by_skull: HashMap<String, i64>,
    #[serde(default)]
    pub default_skull: u32,
}

/// The `levelScaling` table: how enemy/difficulty level + XP scale with player level.
/// Fixes the `generate_quest_data` stub that hard-coded level 1 / 1000 XP.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestLevelScaling {
    #[serde(default)]
    pub enemy_level_from_player_level: EnemyLevelScaling,
}

impl QuestLevelScaling {
    /// The enemy/difficulty level for a `player_level` at the default skull:
    /// `clamp(player_level + offset, 1, 100)`. With no table loaded, degrades to the
    /// player's own level (never the old hard-coded 1).
    pub fn enemy_level(&self, player_level: i64) -> i64 {
        let sk = &self.enemy_level_from_player_level;
        let offset = sk
            .offset_by_skull
            .get(&sk.default_skull.to_string())
            .copied()
            .unwrap_or(0);
        (player_level + offset).clamp(1, 100)
    }

    /// XP granted per enemy for a given enemy level: `base(100) * enemy_level`
    /// (`givenXpFormula`). Replaces the flat 1000.
    pub fn given_xp(&self, enemy_level: i64) -> u64 {
        (100 * enemy_level.max(1)) as u64
    }
}

/// One per-day selection rule (`selection.perDay[]`): pick `count` quests of `category`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySelectionRule {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub count: u32,
}

/// The deterministic daily-selection config (`selection`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySelection {
    #[serde(default)]
    pub reset_hour_utc: u64,
    #[serde(default)]
    pub reset_minute_utc: u64,
    #[serde(default)]
    pub per_day: Vec<DailySelectionRule>,
}

/// Daily-rotation quest model (`deploy/static/quests_daily.json`). Adds a curated
/// rotation pool, a deterministic date-keyed selection, and the level-scaling table
/// the quest generator reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestsDailyData {
    #[serde(default)]
    pub daily_quest_pool: Vec<DailyQuestDef>,
    #[serde(default)]
    pub level_scaling: QuestLevelScaling,
    #[serde(default)]
    pub selection: DailySelection,
    /// The nil-dungeon quests that MUST be excluded from any dungeon rotation (they
    /// would otherwise error in `generate_quest_data`).
    #[serde(default)]
    pub non_dungeon_quests: Vec<DailyQuestDef>,
}

impl QuestsDailyData {
    /// The set of nil-dungeon quest ids, for exclusion + graceful handling.
    pub fn non_dungeon_ids(&self) -> std::collections::HashSet<Uuid> {
        self.non_dungeon_quests.iter().map(|q| q.quest_id).collect()
    }
}

/// One milestone payout as retail's `/complete` body actually granted it.
///
/// The client is shown the tier under `rewards[]` with the gem currency listed as a
/// `stackableItems` entry; the `/complete` body moves that same gem count into
/// `currencies`. Rather than re-deriving which uuid is a currency, this is the
/// granting form taken verbatim from the captured responses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayableTier {
    #[serde(default)]
    pub reward: crate::economy::RewardGrant,
    #[serde(default)]
    pub observations: u64,
}

/// One event-quest TEMPLATE, keyed by `gldQuestId` (`event_quests.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventQuestTemplate {
    #[serde(default)]
    pub gld_quest_id: Uuid,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub objective_ids: Vec<Uuid>,
    /// The five milestone rewards, in wire form (what the client displays).
    #[serde(default)]
    pub rewards: Vec<crate::economy::RewardGrant>,
    /// The bonus paid alongside the last milestone.
    #[serde(default)]
    pub final_reward: Option<crate::economy::RewardGrant>,
    /// The granting form of each milestone, keyed by completion index as a string
    /// (`"0".."4"`). Every one of the 39 committed templates has all five observed.
    #[serde(default)]
    pub payable_rewards: HashMap<String, PayableTier>,
}

impl EventQuestTemplate {
    /// What the `completion`-th completion of this quest pays.
    ///
    /// Prefers the observed `/complete` body for that index; falls back to the wire
    /// tier when a corpus is missing that index (none of the committed templates
    /// are, but a hand-edited file could be). Returns `None` past the last tier —
    /// retail's instances are exhausted after five completions.
    pub fn payout(&self, completion: usize) -> Option<crate::economy::RewardGrant> {
        if completion >= self.rewards.len().max(self.payable_rewards.len()) {
            return None;
        }
        if let Some(tier) = self.payable_rewards.get(&completion.to_string()) {
            return Some(tier.reward.clone());
        }
        self.rewards.get(completion).cloned()
    }

    /// How many times an instance of this quest can be completed.
    pub fn milestone_count(&self) -> usize {
        self.rewards.len().max(self.payable_rewards.len())
    }
}

/// `event_quests.json` — the capture-derived event-quest template table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventQuestsData {
    #[serde(default)]
    pub templates: HashMap<Uuid, EventQuestTemplate>,
}

/// `global_shop_free.json` — the offers retail gave away for nothing.
///
/// A wrapper struct rather than a bare `Vec`, so the file keeps a named key and
/// can grow fields (a per-offer daily cap, say) without changing its shape.
/// `Default` is an empty list, which is the safe direction: a missing file
/// means nothing is free, not everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeProductIds {
    #[serde(default)]
    pub free_product_ids: Vec<Uuid>,
}

impl FreeProductIds {
    pub fn contains(&self, id: &Uuid) -> bool {
        self.free_product_ids.contains(id)
    }
}

/// All capture-derived static definitions, loaded once at startup. Fields are added
/// per feature; each is independently optional (a missing/!invalid data file leaves
/// its field empty rather than failing startup).
#[derive(Debug, Clone, Default)]
pub struct StaticData {
    /// Global gifts, keyed by `globalGiftId`.
    pub gifts: HashMap<Uuid, GiftDef>,
    /// News entries served by `GET /announcements`.
    pub announcements: Vec<Announcement>,
    /// The global-shop override catalog (`{globalShopOverrides: {...}}`), served
    /// verbatim by `GET /catalogoverrides/globalshop`. Opaque JSON — special/limited
    /// offers with adjusted prices; the base catalog lives in the client's bundles.
    pub global_shop_overrides: Value,
    /// Admin-authored global-shop windows (`{globalShopOverrides: {...}}`), applied
    /// ON TOP of the replayed catalogue and never shifted.
    ///
    /// Separate from [`Self::global_shop_overrides`] on purpose. The replay shift
    /// anchors on the latest `activeEndDate` in the catalogue, so folding an
    /// authored future window into that file would make the anchor land past `now`
    /// and switch the shift off for all 547 retail offers at once — emptying the
    /// shop to author one entry. Kept apart, an authored window means exactly the
    /// dates someone typed.
    pub global_shop_authored: Value,
    /// The IAP fulfillment overrides (`{fulfillmentOverrides: {...}}`), served
    /// verbatim by `GET /catalogoverrides/iap`. Real-money SKUs — priced placeholders
    /// only (all `isActive:false` in captures); we never run a purchase flow.
    pub iap: Value,
    /// What each global-shop product grants when bought (`globalShopProductId` ->
    /// reward), derived from purchase captures. Price validation is independent:
    /// it uses [`Self::global_shop_prices`] or a currently active override, so an
    /// unknown product can be priced but not fulfilled.
    pub global_shop_grants: HashMap<Uuid, RewardGrant>,
    /// Authoritative base prices from the APK's `GlobalShopProductsCatalog`.
    ///
    /// The captured override catalogue records Bethesda's shutdown promotion,
    /// where every price was reduced to one. It is authoritative only while a
    /// replayed/authored override is active; always-available products use this
    /// table instead of trusting the client's `expectedPrices` request field.
    pub global_shop_prices: HashMap<Uuid, Vec<crate::economy::Price>>,
    /// APK-derived contents for offers the purchase captures never covered.
    ///
    /// `global_shop_grants` above knows 159 of the storefront's 547 offers,
    /// because an offer's contents only ever appeared in a purchase RESPONSE —
    /// so we know exactly the offers somebody happened to buy while we were
    /// capturing. The other 388 `404`d on purchase.
    ///
    /// This is the fallback, joined from the APK
    /// (`GlobalShopProductsCatalog._itemBundleGenerationDataId` ->
    /// `ItemBundleGenerationDataList`) and identity-checked against the captures
    /// on the 146 offers where both exist: contents and `townXP` agreed 146/146.
    ///
    /// It carries TEMPLATE + QUANTITY only, because retail rolled per-purchase
    /// stats (`durability`, `grade`, `arcaneTier`, `properties`) at purchase
    /// time. That is why each offer carries a [`OfferContentsKind`]: only
    /// `Literal` can be granted from this file without inventing item stats.
    pub global_shop_offer_contents: HashMap<Uuid, OfferContents>,
    /// The global-shop offers retail gave away for nothing — the store's free
    /// daily item. Mined from captured purchases retail answered 200 to with an
    /// all-zero `expectedPrices` (`scripts/build-shop-static.py`).
    ///
    /// This has to be an allowlist rather than "permit a zero price", because
    /// the price is client-supplied: accepting any zero would hand out every
    /// paid offer for free.
    pub global_shop_free: FreeProductIds,
    /// Latest shipped per-level offer, keyed by the level just reached.
    ///
    /// Only products with both an authoritative APK price and a captured grant
    /// belong here. Advertising an offer we cannot fulfil would replace a price
    /// spinner with a charged-but-empty or reconnect failure.
    pub level_up_offers: HashMap<u16, Uuid>,
    /// Challenge templates (objective + reward) the active set is generated from.
    pub challenge_templates: Vec<ChallengeTemplate>,
    /// Daily login reward rotation pool.
    pub daily_rewards: Vec<DailyRewardDef>,
    /// Representative chest-loot bundles (one is picked per chest by id), since per-tier
    /// loot tables aren't captured.
    pub chest_loots: Vec<RewardGrant>,
    /// Daily / Sigil quest event library (a rotating few are surfaced as active).
    pub game_events: Vec<EventDef>,
    /// Representative salvage yield per `recipeId` (`recipeId` -> {material -> count}),
    /// since the real yield is randomised.
    pub salvage_recipes: HashMap<Uuid, HashMap<Uuid, u64>>,
    /// Town vendor shop catalogs (open-shop), routed by shopId/template.
    pub shop_data: ShopData,
    /// What each shop bundle costs + grants when bought (`bundleId` -> price/grant).
    pub shop_bundles: HashMap<Uuid, ShopBundle>,
    /// Craft recipes keyed by `recipeId` (capture-derived from `POST /crafts`).
    pub recipes: HashMap<Uuid, Recipe>,
    /// Temper/enchant recipes keyed by `recipeId` — the `POST /crafts` requests that
    /// carry an `itemId` and modify an existing item (vs `recipes`, which mint a new one).
    pub item_mod_recipes: HashMap<Uuid, ItemModRecipe>,
    /// APK-extracted `recipeId -> CraftingType` (`recipe_crafting_types.json`). The
    /// authoritative answer to "which bench does this recipe belong to", covering every
    /// recipe the client ships — not just the captured ones.
    pub recipe_crafting_types: RecipeCraftingTypes,
    /// Capture-derived quest completion rewards, keyed by quest/`gldQuestId` UUID.
    /// Used by `POST /quests/{id}/complete` to grant the reward without re-running the
    /// quest logic. Lenient: an unknown quest id returns an empty reward.
    pub quest_rewards: HashMap<Uuid, crate::economy::RewardGrant>,
    /// Abyss static definitions (floor list + random pool + future rewards).
    pub abyss: AbyssStaticData,
    /// Forge / Smithy craftables catalog (`smith_craftables.json`), resolved to
    /// by-recipe / by-template lookups. Lets a smith craft mint the REAL item at its
    /// grade instead of the lenient placeholder stackable.
    pub smith_craftables: SmithCraftables,
    /// Daily-rotation quest model + level-scaling table (`quests_daily.json`).
    pub quests_daily: QuestsDailyData,
    /// Event-quest ("Sigil") templates keyed by `gldQuestId` (`event_quests.json`):
    /// objective ids, wire version, and the five milestone rewards each instance
    /// pays. Without this an event quest can be advertised but not paid.
    pub event_quests: EventQuestsData,
}

/// How an offer's APK-derived contents may be granted.
///
/// The distinction is the whole reason this file is usable at all: 398 of the
/// offers contain gear whose stats retail rolled at purchase time, and writing
/// those down would ship invented stats that look authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfferContentsKind {
    /// Currencies and stackables only — nothing to roll, so it can be granted
    /// verbatim. 92 of the 547 storefront offers.
    Literal,
    /// Contains gear or jewellery. The template is known; the instance is not.
    NeedsRoll,
    /// Contents come from a chest, which the server rolls (`chest_loots.json`).
    ChestRoll,
    /// Contains a template the extractor could not place in a bucket.
    Unclassified,
    /// A `kind` this build does not know. Deserialized rather than rejected so a
    /// newer data file cannot stop the server booting — and treated as
    /// ungrantable, which is the safe direction.
    #[serde(other)]
    Unknown,
}

/// One entry of an offer's contents.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferContentEntry {
    pub item_template_id: Uuid,
    pub quantity: u64,
    /// Which wire bucket this template lands in — `currencies`,
    /// `stackableItems`, `items`, or `unknown`. Determined by the extractor from
    /// the captured purchase responses (the bucket a template actually landed
    /// in IS its type) plus `item_durability.json` for gear never seen sold.
    pub bucket: String,
}

/// An offer's APK-derived contents.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferContents {
    pub kind: OfferContentsKind,
    #[serde(default)]
    pub contents: Vec<OfferContentEntry>,
    #[serde(default)]
    pub town_xp: u64,
}

/// The shape of `global_shop_offer_contents.json`.
#[derive(Debug, Clone, Default)]
pub struct OfferContentsFile {
    /// Keyed by product id.
    pub offers: HashMap<Uuid, OfferContents>,
    /// Ids in the file that are **not** UUIDs, kept verbatim so the loader can say
    /// which ones it dropped. Empty in the normal case.
    pub unparseable_ids: Vec<String>,
}

/// Deserialized leniently on purpose (report #93).
///
/// The shipped file carries one product id that is not a UUID —
/// `53c6f124-3603-4100-ba9a-e2fe23969f7p`, 36 characters with a `p` where the last
/// hex digit belongs. It arrives that way from the game data itself
/// (`global_shop_overrides.json` holds the identical string), so a regeneration will
/// not clean it up.
///
/// With `HashMap<Uuid, _>` that single key failed the whole map, serde aborted the
/// file, and `static_loader` fell back to `default()` — so **all 541 offers were
/// discarded** and every purchase was back to having no contents to grant. The
/// server did say so, once, in a startup WARN nobody was reading:
///
/// ```text
/// [static] invalid "/data/static/global_shop_offer_contents.json":
///   UUID parsing failed: invalid character: found `p` at 36 ...; using default
/// ```
///
/// One unusable key now costs us that key rather than the file: it lands in
/// [`Self::unparseable_ids`] and the other 540 load.
impl<'de> Deserialize<'de> for OfferContentsFile {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            offers: HashMap<String, OfferContents>,
        }
        let raw = Raw::deserialize(d)?;
        let mut offers = HashMap::with_capacity(raw.offers.len());
        let mut unparseable_ids = Vec::new();
        for (k, v) in raw.offers {
            match Uuid::parse_str(&k) {
                Ok(id) => {
                    offers.insert(id, v);
                }
                Err(_) => unparseable_ids.push(k),
            }
        }
        unparseable_ids.sort();
        Ok(OfferContentsFile {
            offers,
            unparseable_ids,
        })
    }
}
