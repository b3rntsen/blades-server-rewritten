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
use crate::user_data::ItemSingleProperty;
use crate::features::challenges::ChallengeTemplate;
use crate::features::daily_reward::DailyRewardDef;
use crate::features::game_events::EventDef;

/// One reward line of a global gift (`{itemTemplateId, quantity}`). The template
/// may be a currency UUID (Gold/Sigil/Gems), in which case claiming credits the
/// wallet rather than the backpack — see [`crate::features::gifts`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftItem {
    pub item_template_id: Uuid,
    pub quantity: u64,
}

/// A global gift definition (the captured `globalGiftOverride` block). Time-windowed
/// and claim-count-limited; `startTime`/`endTime` of 0 mean "no bound".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftDef {
    pub global_gift_id: Uuid,
    #[serde(default)]
    pub items: Vec<GiftItem>,
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
    /// Arcane tier the item ends at. Not modelled on [`crate::user_data::Item`] (the
    /// server drops `arcaneTier` for every item), kept here for completeness.
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
    /// Optional scaling tables (values authored/guessed). Carried verbatim so a
    /// future reward-scaling pass can read them; unused by the current handler.
    #[serde(default)]
    pub scaling_backend: Value,
    #[serde(default)]
    pub scaling_curve: Value,
    #[serde(default)]
    pub per_floor_rewards: Value,
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
    /// The IAP fulfillment overrides (`{fulfillmentOverrides: {...}}`), served
    /// verbatim by `GET /catalogoverrides/iap`. Real-money SKUs — priced placeholders
    /// only (all `isActive:false` in captures); we never run a purchase flow.
    pub iap: Value,
    /// What each global-shop product grants when bought (`globalShopProductId` ->
    /// reward), derived from purchase captures. The price comes from the client's
    /// `expectedPrices` (the base price list lives in the client bundles), so an
    /// unknown product can be priced but not fulfilled.
    pub global_shop_grants: HashMap<Uuid, RewardGrant>,
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
}
