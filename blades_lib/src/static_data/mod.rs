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
}
