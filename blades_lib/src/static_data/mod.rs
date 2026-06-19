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
}
