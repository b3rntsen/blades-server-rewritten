//! Town-merchant money system — the pure half of `POST /shops/{id}`,
//! `/shops/{id}/purchase` and `/shops/{id}/sell`.
//!
//! # What retail did
//!
//! Tracker #30: *"Merchants don't have money — we should reverse engineer the
//! money system. 8h reset cycle on items on sale and amount they have to buy."*
//!
//! Measured over 1,720 retail shop opens (462 shop ids, 33 users, 2026-05-02 →
//! 2026-06-30) plus 1,467 sells and 1,517 purchases:
//!
//! * **`catalog.wallet` is a STATIC gold budget for the window.** Always exactly
//!   one entry and always Gold (1,720/1,720). It never changes while the catalog
//!   lives — of 446 catalogs opened two or more times, the wallet changed in
//!   **zero**.
//! * **`shop.revenue` is net cashflow**, not a balance: NEGATIVE when the
//!   merchant buys from the player, POSITIVE when the player buys. So buying from
//!   a merchant *replenishes* what it can spend on you.
//! * **Remaining buy capacity = `wallet + revenue`, floored at 0.** Never negative
//!   across all 1,720 opens; exactly 0 in 49.
//! * **A merchant with nothing left still takes the item and pays 0.** It does not
//!   refuse the sale. 739 of 1,466 buybacks had `price == 0`, and in all 739 the
//!   remaining budget was exactly 0 — zero exceptions. Corroborating: the sell
//!   response carries a `wallet` key in exactly 727 cases, and 1,466 − 739 = 727,
//!   so `wallet` is echoed iff the payout was non-zero.
//! * **The cycle is 10 hours, not 8, and it runs from first visit.**
//!   `expiration − start` is 36,000,000 ms for all 1,720 opens, with
//!   `expiration = floor((start + 36_000_000) / 1000) * 1000`. `start` equals the
//!   request time (median +77 ms) and its minute-of-hour is uniformly spread, so
//!   there is no wall-clock rotation boundary. `expired` was `false` in all
//!   1,720 — the server rerolls on read rather than serving a stale catalog.
//! * **`catalog.bundles` is the window's sale stock and `shop.sales` is what the
//!   player has already bought from it.** Remaining = `quantity − sold`;
//!   `sold <= quantity` held in 187 of 187 entries, and 174 of those were bought
//!   out exactly.
//! * **Buybacks last 5 minutes** (1,466/1,466).
//!
//! ## The sell price
//!
//! ```text
//! price = round(sellValue * temperMult(level))
//!       + SUM over properties.ENCHANTING of enchantValue[propertyId][tier]
//! ```
//!
//! `sellValue` is `ItemTemplate._sellValue`, authored per template (its ratio to
//! `_value` is 0.15 for 543 templates, 0.10 for 105 and 0.35 for 17, so it cannot
//! be computed). `temperMult(0) = 1.0` and `temperMult(L>=1)` is
//! `ItemTemplate._temperProperties[L-1]._value` — a multiplier, not an absolute.
//! The enchantment term is added at FACE value, not scaled by the sell fraction.
//! `properties.GRADING` contributes nothing.
//!
//! Validated against 508 unclamped retail buybacks: **478 exact (94.09%)**, one
//! off by a gold of rounding, and 29 off by a flat +200 confined to two tier-10
//! jewellery templates (Ebony Faerite Ring / Necklace) and only when unenchanted.
//! Every tempered cohort is a clean sweep (temper 1..10: 33/33, 21/21, 19/19,
//! 32/32, 22/23, 22/22, 28/28, 15/15, 10/10, 1/1). Stackables are simply
//! `sellValue` per unit: 44/44 exact.
//!
//! Everything here is pure: no DB, no clock (the caller passes `now_ms`), no HTTP.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::economy::{RewardGrant, GOLD};
use crate::user_data::{CompleteInventory, InventoryChangeTracker, Item};

/// Catalog refresh window. **MEASURED**: `expiration − start` was 36,000,000 ms
/// for all 1,720 retail shop opens. (The report said 8h; the wire says 10.)
pub const REFRESH_MS: i64 = 36_000 * 1000;

/// How long a buyback stays available. **MEASURED**: 5 minutes, 1,466/1,466.
pub const BUYBACK_MS: i64 = 300 * 1000;

/// Retail's expiration rule: the 10h mark truncated to whole seconds.
/// `expiration % 1000 == 0` held for all 1,720 opens while `start % 1000` took
/// 688 distinct values.
pub fn expiration_for(start_ms: i64) -> i64 {
    ((start_ms + REFRESH_MS) / 1000) * 1000
}

// ---------------------------------------------------------------------------
// Sell prices
// ---------------------------------------------------------------------------

/// The APK-derived sell-price tables (`item_sell_values.json` +
/// `enchant_values.json`).
#[derive(Debug, Clone, Default)]
pub struct SellPrices {
    /// `itemTemplateId` -> base sell value (`ItemTemplate._sellValue`).
    sell_value: HashMap<Uuid, u64>,
    /// `itemTemplateId` -> multiplier for temper levels 1..=10.
    temper_mult: HashMap<Uuid, Vec<f64>>,
    /// `propertyId` -> tier -> gold the enchantment adds at face value.
    enchant: HashMap<Uuid, HashMap<u64, u64>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SellValueRow {
    sell_value: u64,
    #[serde(default)]
    temper_mult: Vec<f64>,
}

impl SellPrices {
    /// Parse the two JSON documents, skipping their `_meta` provenance blocks and
    /// any row that does not deserialize (a partially readable table still prices
    /// most items; a hard error would take the server down at startup).
    pub fn from_json(sell_values: &serde_json::Value, enchant_values: &serde_json::Value) -> Self {
        let mut sell_value = HashMap::new();
        let mut temper_mult = HashMap::new();
        if let Some(map) = sell_values.as_object() {
            for (key, row) in map {
                let Ok(template) = Uuid::parse_str(key) else {
                    continue;
                };
                let Ok(row) = serde_json::from_value::<SellValueRow>(row.clone()) else {
                    continue;
                };
                sell_value.insert(template, row.sell_value);
                if !row.temper_mult.is_empty() {
                    temper_mult.insert(template, row.temper_mult);
                }
            }
        }

        let mut enchant = HashMap::new();
        if let Some(map) = enchant_values.as_object() {
            for (key, row) in map {
                let Ok(property) = Uuid::parse_str(key) else {
                    continue;
                };
                let Some(tiers) = row.as_object() else {
                    continue;
                };
                let mut parsed = HashMap::new();
                for (tier, v) in tiers {
                    if let (Ok(t), Some(gold)) = (tier.parse::<u64>(), v.as_u64()) {
                        parsed.insert(t, gold);
                    }
                }
                if !parsed.is_empty() {
                    enchant.insert(property, parsed);
                }
            }
        }

        SellPrices {
            sell_value,
            temper_mult,
            enchant,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sell_value.is_empty()
    }

    pub fn template_count(&self) -> usize {
        self.sell_value.len()
    }

    pub fn enchant_property_count(&self) -> usize {
        self.enchant.len()
    }

    /// Multiplier on `sellValue` for a temper level. Level 0 is 1.0; a level past
    /// the authored ladder clamps to its top rather than falling back to 1.0,
    /// which would under-pay a heavily tempered item.
    fn temper_multiplier(&self, template: Uuid, level: u64) -> f64 {
        if level == 0 {
            return 1.0;
        }
        match self.temper_mult.get(&template) {
            Some(ladder) if !ladder.is_empty() => {
                let idx = ((level - 1) as usize).min(ladder.len() - 1);
                let m = ladder[idx];
                if m > 0.0 {
                    m
                } else {
                    1.0
                }
            }
            // Untemperable, or a template with no ladder: temper cannot raise the
            // price, so pay the base value.
            _ => 1.0,
        }
    }

    /// Gold the merchant offers for one instanced item, before the budget clamp.
    pub fn item_price(&self, item: &Item) -> u64 {
        let Some(base) = self.sell_value.get(&item.item_template_id) else {
            // Retail marks unsellable templates with `_sellValue = 0`; an absent
            // row means the same thing.
            return 0;
        };
        let scaled = (*base as f64
            * self.temper_multiplier(item.item_template_id, item.tempering_level))
        .round() as u64;
        // GRADING contributes nothing (measured: residual 0 over 188 graded
        // records); only ENCHANTING adds, and at face value.
        let enchant: u64 = item
            .properties
            .enchanting
            .iter()
            .filter_map(|p| {
                self.enchant
                    .get(&p.id)
                    .and_then(|t| t.get(&p.tier))
                    .copied()
            })
            .sum();
        scaled.saturating_add(enchant)
    }

    /// Gold the merchant offers for `count` units of a stackable, before the
    /// budget clamp. Measured: exactly `sellValue` per unit, 44/44.
    pub fn stackable_price(&self, template: Uuid, count: u64) -> u64 {
        self.sell_value
            .get(&template)
            .copied()
            .unwrap_or(0)
            .saturating_mul(count)
    }
}

// ---------------------------------------------------------------------------
// Per-shop, per-window merchant state
// ---------------------------------------------------------------------------

/// One buyback slot: an item the player sold that they can buy back for 5 minutes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Buyback {
    pub id: Uuid,
    pub shop_id: Uuid,
    /// Set for an instanced item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<BuybackItem>,
    /// Set for a stackable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stackable_item: Option<BuybackStackable>,
    pub expiration: i64,
    /// What the merchant actually paid — 0 when its budget was exhausted.
    pub price: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BuybackItem {
    pub id: Uuid,
    #[serde(flatten)]
    pub item: Item,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BuybackStackable {
    pub item_template_id: Uuid,
    pub count: u64,
}

/// A merchant's state for one 10-hour window, persisted per shop in
/// `server_state.shops`. Rolled on the first visit of a window and replaced when
/// the window elapses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct MerchantWindow {
    /// The catalog instance id. The client binds shop↔catalog by id, so
    /// `shop.catalogId` MUST equal `catalog.id` or it renders an empty list.
    pub catalog_id: Uuid,
    /// Informational template id echoed to the client.
    pub template_id: Uuid,
    pub start_ms: i64,
    pub expiration_ms: i64,
    /// The rolled sale stock for this window: bundle id -> quantity stocked.
    pub bundles: Vec<(Uuid, u64)>,
    /// Generated item grants for bundles whose static definition is only a bare
    /// template. Used for Enchanter jewelry: roll once when the catalog is
    /// generated, then grant exactly that item when bought.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub generated_grants: HashMap<Uuid, RewardGrant>,
    /// The merchant's STATIC gold budget for this window.
    pub wallet_gold: u64,
    /// Net cashflow this window: negative when the merchant bought from the
    /// player, positive when the player bought.
    pub revenue_gold: i64,
    /// Bundle id -> units the player has bought from this window.
    pub sales: HashMap<Uuid, u64>,
    /// Live buyback slots (each expires 5 minutes after the sale).
    pub buybacks: Vec<Buyback>,
}

impl Default for MerchantWindow {
    fn default() -> Self {
        MerchantWindow {
            catalog_id: Uuid::nil(),
            template_id: Uuid::nil(),
            start_ms: 0,
            expiration_ms: 0,
            bundles: Vec::new(),
            generated_grants: HashMap::new(),
            wallet_gold: 0,
            revenue_gold: 0,
            sales: HashMap::new(),
            buybacks: Vec::new(),
        }
    }
}

impl MerchantWindow {
    /// Is this window still the current one? Retail never served an expired
    /// catalog (`expired` was false in all 1,720 opens) — it rerolled on read.
    pub fn is_live(&self, now_ms: i64) -> bool {
        self.expiration_ms > now_ms && self.catalog_id != Uuid::nil()
    }

    /// Gold the merchant can still spend buying from the player:
    /// `wallet + revenue`, floored at 0.
    pub fn remaining_budget(&self) -> u64 {
        (self.wallet_gold as i64 + self.revenue_gold).max(0) as u64
    }

    /// Units of a bundle still in stock this window.
    pub fn remaining_stock(&self, bundle: Uuid) -> u64 {
        let stocked = self
            .bundles
            .iter()
            .find(|(id, _)| *id == bundle)
            .map(|(_, q)| *q)
            .unwrap_or(0);
        stocked.saturating_sub(self.sales.get(&bundle).copied().unwrap_or(0))
    }

    /// Drop buyback slots whose 5 minutes are up.
    pub fn expire_buybacks(&mut self, now_ms: i64) {
        self.buybacks.retain(|b| b.expiration > now_ms);
    }

    /// The `shop.revenue` wire array — empty when nothing has moved, matching the
    /// captured `"revenue": []` on a fresh catalog.
    pub fn revenue_wire(&self) -> Vec<(Uuid, i64)> {
        if self.revenue_gold == 0 {
            Vec::new()
        } else {
            vec![(GOLD, self.revenue_gold)]
        }
    }
}

// ---------------------------------------------------------------------------
// Rolling a window
// ---------------------------------------------------------------------------

/// The merchant-gold band for one `(buildingTypeId, level)`.
///
/// **MEASURED** per cell from retail `catalog.wallet` joined to
/// `GET /towns/current` building levels; the ±1/9 band is **INFERRED** from four
/// independent high-n cells agreeing on a half-width of 11.09–11.25%. Per-cell
/// provenance (MEASURED / INTERPOLATED / AUTHORED plus observation counts) is
/// carried in `shop_stock.json` itself.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MerchantGoldBand {
    #[serde(default)]
    pub base_gold: u64,
    #[serde(default)]
    pub band_min: u64,
    #[serde(default)]
    pub band_max: u64,
}

impl MerchantGoldBand {
    /// Draw this window's budget. Deterministic in `(shop_id, start_ms)` so a
    /// reopen inside the window yields the identical wallet — which is what retail
    /// showed (0 of 446 repeat-opened catalogs changed their wallet).
    pub fn roll(&self, shop_id: &Uuid, start_ms: i64) -> u64 {
        let lo = if self.band_min > 0 {
            self.band_min
        } else {
            self.base_gold
        };
        let hi = if self.band_max > lo {
            self.band_max
        } else {
            lo
        };
        if hi <= lo {
            return lo;
        }
        let mut rng = SplitMix64(seed(shop_id, start_ms));
        lo + rng.next() % (hi - lo + 1)
    }
}

/// FNV-1a 64-bit over the shop id plus the window start — a stable,
/// dependency-free seed.
fn seed(shop_id: &Uuid, start_ms: i64) -> u64 {
    fnv(&[shop_id.as_bytes().as_slice(), &start_ms.to_le_bytes()])
}

fn fnv(chunks: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in chunks {
        for b in *chunk {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// A buyback slot's id. Derived rather than random: `blades_lib` does not enable
/// uuid's `v4` feature (only the server crate does), and a derived id keeps the
/// sell path deterministic and testable. Unique per `(shop, sold thing, instant)`,
/// which is all the client needs — it is an opaque handle.
fn buyback_id(shop_id: &Uuid, key: &Uuid, now_ms: i64, nth: u64) -> Uuid {
    let hi = fnv(&[
        shop_id.as_bytes().as_slice(),
        key.as_bytes().as_slice(),
        &now_ms.to_le_bytes(),
        &nth.to_le_bytes(),
    ]);
    let lo = fnv(&[
        &nth.to_le_bytes(),
        key.as_bytes().as_slice(),
        shop_id.as_bytes().as_slice(),
    ]);
    Uuid::from_u64_pair(hi, lo)
}

struct SplitMix64(u64);
impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

// ---------------------------------------------------------------------------
// Selling to the merchant
// ---------------------------------------------------------------------------

/// One line of a sell: what left the player's inventory and what they were paid.
#[derive(Debug, Clone, PartialEq)]
pub enum SoldLine {
    Item {
        id: Uuid,
        paid: u64,
    },
    Stackable {
        template: Uuid,
        count: u64,
        paid: u64,
    },
}

#[derive(Debug, Default, PartialEq)]
pub struct SellOutcome {
    pub sold: Vec<SoldLine>,
    /// Item / stackable ids the request named but the backpack did not hold.
    pub unknown: Vec<Uuid>,
    pub gold_paid: u64,
    /// New buyback slots, in the order retail returns them.
    pub buybacks: Vec<Buyback>,
}

/// Sell backpack items and stackables to a merchant.
///
/// Retail semantics, all measured: the merchant pays
/// `min(price, remaining_budget)` and **takes the item either way** — an
/// exhausted merchant does not refuse the sale, it pays 0. Each line pushes a
/// buyback slot that lives for 5 minutes.
/// Why a buyback failed. Each maps to a distinct client-visible outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuybackError {
    /// No slot with that id in this shop's window.
    NoSuchSlot,
    /// The five-minute window closed before the player asked.
    Expired,
    /// The player cannot afford what the merchant paid them.
    InsufficientGold,
}

/// What a successful buyback returned.
#[derive(Debug, Clone, PartialEq)]
pub struct BuybackOutcome {
    pub slot: Buyback,
    /// Gold actually taken back off the player.
    pub gold_spent: u64,
}

/// Buy back an item the player sold. **This is the half that was never written.**
///
/// `sell_to_shop` creates buyback slots, the client shows them, and the client then
/// calls `POST …/shops/{shop_id}/buybacks/{buyback_id}` — a route that did not
/// exist, so every attempt answered 404. Selling something by accident was
/// therefore irreversible, which is exactly how report #163 lost six items: the
/// player sold them, tried to buy them back, got nothing, and the five-minute
/// window closed while the slots sat there unusable.
///
/// The transaction is the exact inverse of the sale in `apply_sell`: the player
/// pays back precisely what the merchant paid them (`slot.price`, often 0 once the
/// merchant's budget is spent), the merchant's `revenue_gold` is restored, and the
/// item or stack goes back into the backpack. The slot is consumed either way.
pub fn apply_buyback(
    window: &mut MerchantWindow,
    buyback_id: Uuid,
    inventory: &mut CompleteInventory,
    wallet: &mut crate::user_data::CompleteWallet,
    tracker: &mut InventoryChangeTracker,
    now_ms: i64,
) -> Result<BuybackOutcome, BuybackError> {
    // Expiry is checked on the slot itself BEFORE pruning, so an expired slot
    // reports `Expired` rather than vanishing into `NoSuchSlot` — the player asked
    // for something that existed and has just run out, and the two cases deserve
    // different answers.
    let idx = window
        .buybacks
        .iter()
        .position(|b| b.id == buyback_id)
        .ok_or(BuybackError::NoSuchSlot)?;
    if window.buybacks[idx].expiration <= now_ms {
        window.buybacks.remove(idx);
        return Err(BuybackError::Expired);
    }

    let price = window.buybacks[idx].price;
    if price > 0 && wallet.balance(GOLD) < price {
        return Err(BuybackError::InsufficientGold);
    }

    let slot = window.buybacks.remove(idx);
    if price > 0 {
        // `try_pay` is the same debit the shop's own purchase path uses, so an
        // overdraft is impossible even if the balance check above ever drifts.
        wallet
            .try_pay(&[crate::economy::Price {
                currency_id: GOLD,
                quantity: price,
            }])
            .map_err(|_| BuybackError::InsufficientGold)?;
    }
    // The merchant gets its money back, mirroring the `-= paid` in `apply_sell`.
    window.revenue_gold += price as i64;

    if let Some(bi) = slot.item.clone() {
        tracker.modified_backpack.items.insert(bi.id);
        inventory.backpack.items.0.insert(bi.id, bi.item);
    }
    if let Some(bs) = slot.stackable_item.clone() {
        inventory
            .backpack
            .stackable_items
            .add(bs.item_template_id, bs.count);
        tracker
            .modified_backpack
            .stackable_items
            .insert(bs.item_template_id);
    }

    Ok(BuybackOutcome {
        slot,
        gold_spent: price,
    })
}

pub fn apply_sell(
    prices: &SellPrices,
    window: &mut MerchantWindow,
    shop_id: Uuid,
    items: &[Uuid],
    stackables: &HashMap<Uuid, u64>,
    inventory: &mut CompleteInventory,
    wallet: &mut crate::user_data::CompleteWallet,
    tracker: &mut InventoryChangeTracker,
    now_ms: i64,
) -> SellOutcome {
    let mut outcome = SellOutcome::default();
    window.expire_buybacks(now_ms);

    let mut nth: u64 = 0;
    for item_id in items {
        let Some(item) = inventory.backpack.items.0.remove(item_id) else {
            outcome.unknown.push(*item_id);
            continue;
        };
        nth += 1;
        let asking = prices.item_price(&item);
        let paid = asking.min(window.remaining_budget());
        window.revenue_gold -= paid as i64;
        wallet.credit(GOLD, paid);
        tracker.modified_backpack.items.insert(*item_id);
        outcome.sold.push(SoldLine::Item { id: *item_id, paid });
        outcome.gold_paid = outcome.gold_paid.saturating_add(paid);
        outcome.buybacks.push(Buyback {
            id: buyback_id(&shop_id, item_id, now_ms, nth),
            shop_id,
            item: Some(BuybackItem { id: *item_id, item }),
            stackable_item: None,
            expiration: now_ms + BUYBACK_MS,
            price: paid,
        });
    }

    // Deterministic order so the response and the budget clamp are reproducible.
    let mut stack_keys: Vec<(&Uuid, &u64)> = stackables.iter().collect();
    stack_keys.sort_by_key(|(t, _)| **t);
    for (template, count) in stack_keys {
        if inventory
            .backpack
            .stackable_items
            .remove(*template, *count)
            .is_err()
        {
            outcome.unknown.push(*template);
            continue;
        }
        nth += 1;
        let asking = prices.stackable_price(*template, *count);
        let paid = asking.min(window.remaining_budget());
        window.revenue_gold -= paid as i64;
        wallet.credit(GOLD, paid);
        tracker.modified_backpack.stackable_items.insert(*template);
        outcome.sold.push(SoldLine::Stackable {
            template: *template,
            count: *count,
            paid,
        });
        outcome.gold_paid = outcome.gold_paid.saturating_add(paid);
        outcome.buybacks.push(Buyback {
            id: buyback_id(&shop_id, template, now_ms, nth),
            shop_id,
            item: None,
            stackable_item: Some(BuybackStackable {
                item_template_id: *template,
                count: *count,
            }),
            expiration: now_ms + BUYBACK_MS,
            price: paid,
        });
    }

    window.buybacks.extend(outcome.buybacks.iter().cloned());
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_data::{
        Backpack, CompleteWallet, ItemPropertiesAll, ItemSingleProperty, Loadout, Treasury,
    };
    use serde_json::json;

    const TPL: Uuid = Uuid::from_u128(0x11);
    const TPL_STACK: Uuid = Uuid::from_u128(0x22);
    const TPL_UNSELLABLE: Uuid = Uuid::from_u128(0x33);
    const ENCH: Uuid = Uuid::from_u128(0x44);
    const SHOP: Uuid = Uuid::from_u128(0x55);

    fn prices() -> SellPrices {
        // Real shapes: a tier-10 weapon at sellValue 635 with the measured temper
        // ladder, a material at 190, and the shared enchant tier ladder.
        let sell = json!({
            "_meta": {"_source": "test"},
            TPL.to_string(): {
                "sellValue": 635,
                "temperMult": [2.25, 2.6, 3.1, 3.7, 4.16, 5.0, 6.1, 7.4, 9.0, 12.03]
            },
            TPL_STACK.to_string(): { "sellValue": 190 },
            TPL_UNSELLABLE.to_string(): { "sellValue": 0 },
        });
        let ench = json!({
            "_meta": {"_source": "test"},
            ENCH.to_string(): {
                "1": 268, "2": 736, "3": 1318, "4": 1941, "5": 2566,
                "6": 3209, "7": 4008, "8": 4961, "9": 6137, "10": 7591
            },
        });
        SellPrices::from_json(&sell, &ench)
    }

    fn item(temper: u64, enchants: &[(Uuid, u64)]) -> Item {
        Item {
            item_template_id: TPL,
            tempering_level: temper,
            durability: 100.0,
            grade: None,
            arcane_tier: None,
            properties: ItemPropertiesAll {
                enchanting: enchants
                    .iter()
                    .map(|(id, tier)| ItemSingleProperty {
                        id: *id,
                        tier: *tier,
                    })
                    .collect(),
                grading: vec![ItemSingleProperty {
                    id: Uuid::from_u128(0x99),
                    tier: 5,
                }],
            },
        }
    }

    fn inventory() -> CompleteInventory {
        CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        }
    }

    fn window(gold: u64) -> MerchantWindow {
        MerchantWindow {
            catalog_id: Uuid::from_u128(0xc1),
            template_id: Uuid::from_u128(0xd1),
            start_ms: 1_000_000,
            expiration_ms: expiration_for(1_000_000),
            bundles: vec![(Uuid::from_u128(0xb1), 5)],
            generated_grants: HashMap::new(),
            wallet_gold: gold,
            revenue_gold: 0,
            sales: HashMap::new(),
            buybacks: Vec::new(),
        }
    }

    // -- the cycle ---------------------------------------------------------

    #[test]
    fn the_window_is_ten_hours_truncated_to_whole_seconds() {
        // MEASURED: expiration - start == 36_000_000 ms and expiration % 1000 == 0.
        let start = 1_782_655_411_159i64;
        let exp = expiration_for(start);
        assert_eq!(exp % 1000, 0, "expiration is truncated to whole seconds");
        assert_eq!(exp, 1_782_691_411_000);
        assert!(exp - start <= REFRESH_MS && exp - start > REFRESH_MS - 1000);
        assert_eq!(REFRESH_MS, 36_000_000, "10 hours, not 8");
    }

    #[test]
    fn a_window_is_live_until_it_expires_then_not() {
        let w = window(1000);
        assert!(w.is_live(w.start_ms + 1));
        assert!(w.is_live(w.expiration_ms - 1));
        assert!(!w.is_live(w.expiration_ms));
        assert!(!w.is_live(w.expiration_ms + 1));
    }

    #[test]
    fn a_default_window_is_never_live() {
        assert!(!MerchantWindow::default().is_live(0));
    }

    // -- the money system -------------------------------------------------

    /// The reported defect: merchants had NO money, so selling paid nothing.
    #[test]
    fn a_merchant_has_a_gold_budget_drawn_from_its_measured_band() {
        let band = MerchantGoldBand {
            base_gold: 24_438,
            band_min: 21_723,
            band_max: 27_153,
        };
        let mut seen = std::collections::HashSet::new();
        for i in 0..64 {
            let g = band.roll(&Uuid::from_u128(i), 1_000_000);
            assert!(
                g >= band.band_min && g <= band.band_max,
                "rolled {g} outside the measured band"
            );
            seen.insert(g);
        }
        assert!(seen.len() > 1, "the budget is randomized per roll");
        assert!(!seen.contains(&0), "a merchant is never broke at roll time");
    }

    #[test]
    fn the_same_shop_and_window_always_rolls_the_same_budget() {
        // Retail: 0 of 446 repeat-opened catalogs changed their wallet.
        let band = MerchantGoldBand {
            base_gold: 1000,
            band_min: 889,
            band_max: 1111,
        };
        let a = band.roll(&SHOP, 42_000);
        let b = band.roll(&SHOP, 42_000);
        assert_eq!(a, b);
        assert_ne!(
            a,
            band.roll(&SHOP, 42_000 + REFRESH_MS),
            "next window rerolls"
        );
    }

    #[test]
    fn remaining_budget_is_wallet_plus_revenue_floored_at_zero() {
        let mut w = window(1000);
        assert_eq!(w.remaining_budget(), 1000);
        w.revenue_gold = -400; // bought 400 worth from the player
        assert_eq!(w.remaining_budget(), 600);
        w.revenue_gold = -5000; // clamped in practice, but never negative
        assert_eq!(w.remaining_budget(), 0);
        w.revenue_gold = 250; // the player bought something — budget replenishes
        assert_eq!(w.remaining_budget(), 1250);
    }

    #[test]
    fn revenue_is_omitted_from_the_wire_until_something_moves() {
        let mut w = window(1000);
        assert!(
            w.revenue_wire().is_empty(),
            "fresh catalog reports revenue: []"
        );
        w.revenue_gold = -35;
        assert_eq!(w.revenue_wire(), vec![(GOLD, -35)]);
    }

    // -- the sell price ---------------------------------------------------

    #[test]
    fn sell_price_is_the_templates_sell_value_at_temper_zero() {
        let p = prices();
        let mut it = item(0, &[]);
        it.properties.enchanting.clear();
        assert_eq!(p.item_price(&it), 635);
    }

    #[test]
    fn temper_scales_the_sell_price_by_the_measured_multiplier() {
        let p = prices();
        // 635 * 2.25 = 1428.75 -> 1429 (the worked retail example).
        let mut it = item(1, &[]);
        it.properties.enchanting.clear();
        assert_eq!(p.item_price(&it), 1429);
        // 635 * 12.03 = 7639.05 -> 7639
        let mut it10 = item(10, &[]);
        it10.properties.enchanting.clear();
        assert_eq!(p.item_price(&it10), 7639);
    }

    #[test]
    fn enchantments_add_at_face_value_and_grading_adds_nothing() {
        let p = prices();
        // The fixture item always carries a GRADING property; it must not count.
        let plain = {
            let mut i = item(0, &[]);
            i.properties.enchanting.clear();
            i
        };
        assert_eq!(p.item_price(&plain), 635);
        // One tier-3 enchantment: +1318 at face value, not 0.15 * 1318.
        assert_eq!(p.item_price(&item(0, &[(ENCH, 3)])), 635 + 1318);
        // Three tier-1 enchantments sum to the +804 offset seen in retail.
        assert_eq!(
            p.item_price(&item(0, &[(ENCH, 1), (ENCH, 1), (ENCH, 1)])),
            635 + 804
        );
        // Temper and enchantment combine: round(sellValue*mult) + sum(enchants).
        assert_eq!(p.item_price(&item(1, &[(ENCH, 3)])), 1429 + 1318);
    }

    #[test]
    fn an_unknown_or_zero_value_template_is_worth_nothing() {
        let p = prices();
        let mut it = item(0, &[]);
        it.properties.enchanting.clear();
        it.item_template_id = TPL_UNSELLABLE;
        assert_eq!(p.item_price(&it), 0);
        it.item_template_id = Uuid::from_u128(0xdead);
        assert_eq!(p.item_price(&it), 0);
    }

    #[test]
    fn a_temper_level_past_the_ladder_clamps_to_its_top() {
        let p = prices();
        let mut it = item(99, &[]);
        it.properties.enchanting.clear();
        assert_eq!(
            p.item_price(&it),
            7639,
            "clamps to the temper-10 multiplier"
        );
    }

    #[test]
    fn stackables_are_sell_value_per_unit() {
        let p = prices();
        assert_eq!(p.stackable_price(TPL_STACK, 1), 190);
        assert_eq!(p.stackable_price(TPL_STACK, 7), 1330);
        assert_eq!(p.stackable_price(Uuid::from_u128(0xdead), 3), 0);
    }

    #[test]
    fn meta_keys_do_not_become_rows() {
        let p = prices();
        assert_eq!(p.template_count(), 3);
        assert_eq!(p.enchant_property_count(), 1);
    }

    // -- selling ----------------------------------------------------------

    /// The headline defect: with a funded merchant, selling actually pays.
    #[test]
    fn selling_pays_the_player_and_draws_down_the_merchants_budget() {
        let p = prices();
        let mut w = window(10_000);
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tracker = InventoryChangeTracker::default();

        let id = Uuid::from_u128(0xb1);
        let mut it = item(0, &[]);
        it.properties.enchanting.clear();
        inv.backpack.items.0.insert(id, it);
        inv.backpack.stackable_items.add(TPL_STACK, 4);

        let mut stacks = HashMap::new();
        stacks.insert(TPL_STACK, 4);
        let out = apply_sell(
            &p,
            &mut w,
            SHOP,
            &[id],
            &stacks,
            &mut inv,
            &mut wallet,
            &mut tracker,
            2_000_000,
        );

        // 635 for the item + 4 * 190 for the stack.
        assert_eq!(out.gold_paid, 635 + 760);
        assert_eq!(wallet.balance(GOLD), 1395);
        assert_eq!(
            w.revenue_gold, -1395,
            "revenue goes negative, wallet does not move"
        );
        assert_eq!(w.wallet_gold, 10_000, "the wallet is a static budget");
        assert_eq!(w.remaining_budget(), 8_605);
        assert!(
            !inv.backpack.items.0.contains_key(&id),
            "item left the backpack"
        );
        assert_eq!(inv.backpack.stackable_items.count(TPL_STACK), 0);
        assert_eq!(out.buybacks.len(), 2);
        assert!(out.unknown.is_empty());
    }

    /// Measured retail behaviour, and the counter-intuitive one: an exhausted
    /// merchant still TAKES the item and pays 0 rather than refusing the sale.
    /// 739 of 1466 buybacks had price 0, and all 739 had exactly 0 remaining.
    #[test]
    fn an_exhausted_merchant_takes_the_item_and_pays_zero() {
        let p = prices();
        let mut w = window(400); // less than the 635 asking price
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tracker = InventoryChangeTracker::default();

        let a = Uuid::from_u128(0xb1);
        let b = Uuid::from_u128(0xb2);
        for id in [a, b] {
            let mut it = item(0, &[]);
            it.properties.enchanting.clear();
            inv.backpack.items.0.insert(id, it);
        }

        let out = apply_sell(
            &p,
            &mut w,
            SHOP,
            &[a, b],
            &HashMap::new(),
            &mut inv,
            &mut wallet,
            &mut tracker,
            2_000_000,
        );

        // First sale is CLAMPED to the remaining 400; the second pays nothing.
        assert_eq!(out.gold_paid, 400);
        assert_eq!(wallet.balance(GOLD), 400);
        assert_eq!(w.remaining_budget(), 0);
        assert!(
            !inv.backpack.items.0.contains_key(&a) && !inv.backpack.items.0.contains_key(&b),
            "both items were taken even though the second paid nothing"
        );
        let paid: Vec<u64> = out
            .sold
            .iter()
            .map(|l| match l {
                SoldLine::Item { paid, .. } => *paid,
                SoldLine::Stackable { paid, .. } => *paid,
            })
            .collect();
        assert_eq!(paid, vec![400, 0]);
        assert_eq!(
            out.buybacks[1].price, 0,
            "a zero-price buyback slot still exists"
        );
    }

    #[test]
    fn buying_replenishes_what_the_merchant_can_spend() {
        // Retail capture 5027: 18 units bought -> revenue +4500.
        let mut w = window(1000);
        w.revenue_gold = -1000; // fully drained by earlier sales
        assert_eq!(w.remaining_budget(), 0);
        w.revenue_gold += 4500; // the player buys
        assert_eq!(w.remaining_budget(), 4500);
    }

    #[test]
    fn stock_runs_down_as_the_player_buys_and_never_goes_negative() {
        let mut w = window(1000);
        let b = Uuid::from_u128(0xb1);
        assert_eq!(w.remaining_stock(b), 5);
        w.sales.insert(b, 2);
        assert_eq!(w.remaining_stock(b), 3);
        w.sales.insert(b, 5);
        assert_eq!(w.remaining_stock(b), 0);
        w.sales.insert(b, 99); // can't happen, but must not underflow
        assert_eq!(w.remaining_stock(b), 0);
        assert_eq!(w.remaining_stock(Uuid::from_u128(0xffff)), 0);
    }

    #[test]
    fn buybacks_expire_after_five_minutes() {
        let p = prices();
        let mut w = window(10_000);
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tracker = InventoryChangeTracker::default();
        inv.backpack.stackable_items.add(TPL_STACK, 1);
        let mut stacks = HashMap::new();
        stacks.insert(TPL_STACK, 1);
        let now = 2_000_000;
        apply_sell(
            &p,
            &mut w,
            SHOP,
            &[],
            &stacks,
            &mut inv,
            &mut wallet,
            &mut tracker,
            now,
        );
        assert_eq!(w.buybacks.len(), 1);
        assert_eq!(w.buybacks[0].expiration, now + 300_000);
        w.expire_buybacks(now + 299_999);
        assert_eq!(w.buybacks.len(), 1);
        w.expire_buybacks(now + 300_001);
        assert!(w.buybacks.is_empty());
    }

    #[test]
    fn selling_something_you_do_not_have_is_reported_not_fatal() {
        let p = prices();
        let mut w = window(10_000);
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tracker = InventoryChangeTracker::default();
        let ghost = Uuid::from_u128(0xdead);
        let mut stacks = HashMap::new();
        stacks.insert(TPL_STACK, 3); // none held
        let out = apply_sell(
            &p,
            &mut w,
            SHOP,
            &[ghost],
            &stacks,
            &mut inv,
            &mut wallet,
            &mut tracker,
            2_000_000,
        );
        assert_eq!(out.unknown.len(), 2);
        assert!(out.sold.is_empty());
        assert_eq!(out.gold_paid, 0);
        assert_eq!(w.revenue_gold, 0);
    }

    #[test]
    fn a_buyback_round_trips_through_json_in_the_captured_shape() {
        let b = Buyback {
            id: Uuid::from_u128(0x1),
            shop_id: SHOP,
            item: None,
            stackable_item: Some(BuybackStackable {
                item_template_id: TPL_STACK,
                count: 1,
            }),
            expiration: 1_778_436_717_394,
            price: 35,
        };
        let j = serde_json::to_value(&b).unwrap();
        let obj = j.as_object().unwrap();
        assert!(obj.contains_key("stackableItem"), "camelCase stackableItem");
        assert!(
            !obj.contains_key("item"),
            "instanced item omitted when absent"
        );
        assert!(obj.contains_key("shopId") && obj.contains_key("expiration"));
        assert_eq!(obj["price"], 35);
        let back: Buyback = serde_json::from_value(j).unwrap();
        assert_eq!(back, b);
    }

    /// The committed APK/capture-derived tables must load and be complete enough
    /// that a merchant can actually price what players carry.
    #[test]
    fn committed_tables_load_and_price_real_items() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/");
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_reader(std::io::BufReader::new(
                std::fs::File::open(format!("{dir}{name}")).expect("file is committed"),
            ))
            .expect("file parses")
        };
        let p = SellPrices::from_json(&read("item_sell_values.json"), &read("enchant_values.json"));
        assert!(
            p.template_count() >= 1000,
            "sell-value table covers {} templates",
            p.template_count()
        );
        assert!(
            p.enchant_property_count() >= 60,
            "enchant table covers {} properties",
            p.enchant_property_count()
        );
        // The flat 50-gold placeholder this replaces was ~75x below the retail
        // median of 3738; the real table must be nowhere near that flat.
        let distinct: std::collections::HashSet<u64> = p.sell_value.values().copied().collect();
        assert!(
            distinct.len() > 200,
            "only {} distinct sell values — the table looks flat",
            distinct.len()
        );
        let max = p.sell_value.values().copied().max().unwrap_or(0);
        assert!(
            max > 50_000,
            "top sell value is only {max}; expected retail scale"
        );
        // Every temper ladder must be usable: 10 entries, all positive.
        for (tpl, ladder) in p.temper_mult.iter() {
            assert_eq!(ladder.len(), 10, "template {tpl} has a short temper ladder");
            assert!(
                ladder.iter().all(|m| *m > 0.0),
                "template {tpl} has a zero temper multiplier"
            );
        }
    }

    /// SELLING MUST BE REVERSIBLE. The whole of report #163.
    ///
    /// `apply_sell` created buyback slots and the client rendered them, but the
    /// route they post to was never written, so every attempt answered 404. A
    /// player sold six items by accident, could not take them back, and lost them
    /// when the five-minute window closed.
    ///
    /// Round trip: sell an item, buy it straight back, and the character must be
    /// where it started — same item in the pack, same gold, slot consumed.
    #[test]
    fn a_sold_item_can_be_bought_straight_back() {
        let now = 2_000_000i64;
        let mut w = window(100_000);
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tracker = InventoryChangeTracker::default();
        let id = Uuid::from_u128(0xABCD);
        inv.backpack.items.0.insert(id, item(3, &[]));

        let gold_before = wallet.balance(GOLD);
        let out = apply_sell(
            &prices(),
            &mut w,
            SHOP,
            &[id],
            &Default::default(),
            &mut inv,
            &mut wallet,
            &mut tracker,
            now,
        );
        assert_eq!(out.buybacks.len(), 1, "selling opens a buyback slot");
        assert!(
            !inv.backpack.items.0.contains_key(&id),
            "the item left the pack"
        );
        let paid = out.buybacks[0].price;
        let slot_id = out.buybacks[0].id;

        let back = apply_buyback(
            &mut w,
            slot_id,
            &mut inv,
            &mut wallet,
            &mut tracker,
            now + 1,
        )
        .expect("the slot must be claimable");

        assert_eq!(
            back.gold_spent, paid,
            "you pay back exactly what you were paid"
        );
        assert!(inv.backpack.items.0.contains_key(&id), "the item came back");
        assert_eq!(wallet.balance(GOLD), gold_before, "and the gold nets out");
        assert!(w.buybacks.is_empty(), "the slot is consumed");
    }

    /// The item returns INTACT — same tempering and enchantments, not a fresh one.
    #[test]
    fn the_returned_item_is_the_same_item() {
        let now = 2_000_000i64;
        let mut w = window(100_000);
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tr = InventoryChangeTracker::default();
        let id = Uuid::from_u128(0xBEEF);
        let original = item(7, &[(ENCH, 9)]);
        inv.backpack.items.0.insert(id, original.clone());

        let out = apply_sell(
            &prices(),
            &mut w,
            SHOP,
            &[id],
            &Default::default(),
            &mut inv,
            &mut wallet,
            &mut tr,
            now,
        );
        apply_buyback(
            &mut w,
            out.buybacks[0].id,
            &mut inv,
            &mut wallet,
            &mut tr,
            now + 1,
        )
        .expect("claimable");

        assert_eq!(
            inv.backpack.items.0.get(&id),
            Some(&original),
            "tempering and enchantments must survive the round trip"
        );
    }

    /// A stackable round-trips too, by count.
    #[test]
    fn a_sold_stack_comes_back_in_full() {
        let now = 2_000_000i64;
        let mut w = window(100_000);
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tr = InventoryChangeTracker::default();
        inv.backpack.stackable_items.add(TPL_STACK, 25);

        let mut want = std::collections::HashMap::new();
        want.insert(TPL_STACK, 10u64);
        let out = apply_sell(
            &prices(),
            &mut w,
            SHOP,
            &[],
            &want,
            &mut inv,
            &mut wallet,
            &mut tr,
            now,
        );
        assert_eq!(inv.backpack.stackable_items.count(TPL_STACK), 15);

        apply_buyback(
            &mut w,
            out.buybacks[0].id,
            &mut inv,
            &mut wallet,
            &mut tr,
            now + 1,
        )
        .expect("claimable");
        assert_eq!(
            inv.backpack.stackable_items.count(TPL_STACK),
            25,
            "the stack is whole again"
        );
    }

    /// THE CONTROL: the five-minute window is still enforced, and an expired slot
    /// says so rather than silently succeeding or pretending it never existed.
    /// Without this the fix would hand players a permanent undo button, which is
    /// not what retail did — buybacks last 5 minutes, 1466/1466.
    #[test]
    fn an_expired_slot_is_refused_and_named_as_expired() {
        let now = 2_000_000i64;
        let mut w = window(100_000);
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tr = InventoryChangeTracker::default();
        let id = Uuid::from_u128(0xFEED);
        inv.backpack.items.0.insert(id, item(1, &[]));
        let out = apply_sell(
            &prices(),
            &mut w,
            SHOP,
            &[id],
            &Default::default(),
            &mut inv,
            &mut wallet,
            &mut tr,
            now,
        );

        let err = apply_buyback(
            &mut w,
            out.buybacks[0].id,
            &mut inv,
            &mut wallet,
            &mut tr,
            now + BUYBACK_MS + 1,
        )
        .expect_err("an expired slot must be refused");
        assert_eq!(err, BuybackError::Expired);
        assert!(
            !inv.backpack.items.0.contains_key(&id),
            "and the item stays sold"
        );
    }

    /// An unknown slot id is `NoSuchSlot`, distinct from `Expired` — the two are
    /// different answers to the player and must not be conflated.
    #[test]
    fn an_unknown_slot_is_not_reported_as_expired() {
        let mut w = window(100_000);
        let mut inv = inventory();
        let mut wallet = CompleteWallet::default();
        let mut tr = InventoryChangeTracker::default();
        let err = apply_buyback(
            &mut w,
            Uuid::from_u128(0x1234),
            &mut inv,
            &mut wallet,
            &mut tr,
            1,
        )
        .expect_err("no such slot");
        assert_eq!(err, BuybackError::NoSuchSlot);
    }
}
