//! Paying for MISSING craft ingredients with gems (`gemsPayment: true`).
//!
//! When a player is short of a recipe's gold or materials, the client offers to
//! "spend gems on the missing resources". Retail then used what the player HAD and
//! billed only the shortfall in gems. Two retail enchant starts show it (2026-06-07
//! snapshot, recipe `a4dfdf4f` = 35 000 gold + four materials, all held):
//!
//! | capture | gold before | gold after | gems before → after |
//! | --- | --- | --- | --- |
//! | 59598 | 26 533 | 0 | 14 897 → 14 769 (−128 = 8 467 missing gold) |
//! | 59601 | 0 | 0 | 14 739 → 14 214 (−525 = 35 000 missing gold) |
//!
//! The price is client data: `IngredientValueTable` (one asset, 72 entries, gold
//! among them) gives gems per unit, and `GetGemCostForIngredient` (RVA 0x1E5C2CC) is
//! `(int)ceil((float)quantity * gemValue)` — an f32 multiply, then `frintp`. The
//! client sums that per ingredient (`AddMissingResourceCost`). Both captures match:
//! `ceil(8467 × 0.015f) = 128`, `ceil(35000 × 0.015f) = 525`. The table is
//! extracted by `script/extract_ingredient_values.py` and compiled in.

use std::collections::HashMap;
use std::sync::OnceLock;

use uuid::Uuid;

use super::{consume_stackable, is_currency, EconomyError, GEMS};
use crate::user_data::{CompleteInventory, CompleteWallet, InventoryChangeTracker};

static INGREDIENT_VALUES_RAW: &str = include_str!("../ingredient_values.json");

fn gem_values() -> &'static HashMap<Uuid, f32> {
    static TABLE: OnceLock<HashMap<Uuid, f32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(rename = "gemValues")]
            gem_values: HashMap<Uuid, f64>,
        }
        // The JSON holds each f32 widened to f64; narrowing gets the exact f32 back.
        serde_json::from_str::<Raw>(INGREDIENT_VALUES_RAW)
            .map(|r| r.gem_values.into_iter().map(|(k, v)| (k, v as f32)).collect())
            .unwrap_or_default()
    })
}

/// Gems retail charged for `quantity` missing units of `ingredient`, or `None` when
/// the ingredient has no gem price (the client cannot buy it with gems either).
pub fn gem_cost_for_ingredient(ingredient: Uuid, quantity: u64) -> Option<u64> {
    if quantity == 0 {
        return Some(0);
    }
    let value = *gem_values().get(&ingredient)?;
    Some((quantity as f32 * value).ceil().max(0.0) as u64)
}

/// Pay a recipe's inputs, buying whatever is missing with gems — ALL OR NOTHING.
///
/// Each line is taken from what the player holds (a currency from the wallet, a
/// material from the backpack) up to the amount needed; the rest is priced with
/// [`gem_cost_for_ingredient`] and the sum is debited in gems. Nothing moves until
/// every line has a price and the gems are there, so a refusal leaves the wallet and
/// backpack exactly as they were:
///
/// * a missing ingredient with no gem price → the same error [`super::pay_inputs`]
///   gives for that line (the player is simply short of it);
/// * not enough gems for the bill → `InsufficientFunds` for gems.
///
/// A gem input is billed in gems directly. Returns the gems charged for the missing
/// part (0 when nothing was missing — then this is exactly `pay_inputs`). Touched
/// stacks are recorded in `tracker`; a stack used up shows in the diff as removed.
/// Does not bump `backpackVersion` — the handler bumps once per request.
pub fn pay_inputs_with_gems(
    inputs: &[(Uuid, u64)],
    wallet: &mut CompleteWallet,
    inventory: &mut CompleteInventory,
    tracker: &mut InventoryChangeTracker,
) -> Result<u64, EconomyError> {
    // Sum repeated templates first, so a line listed twice is priced once.
    let mut merged: Vec<(Uuid, u64)> = Vec::new();
    for &(template, quantity) in inputs {
        if quantity == 0 {
            continue;
        }
        match merged.iter_mut().find(|m| m.0 == template) {
            Some(m) => m.1 += quantity,
            None => merged.push((template, quantity)),
        }
    }

    // Phase 1: decide what is used from stock and what is bought, mutating nothing.
    let mut used: Vec<(Uuid, u64)> = Vec::new();
    let mut gem_bill: u64 = 0;
    for &(template, needed) in &merged {
        if template == GEMS {
            gem_bill += needed;
            continue;
        }
        let have = if is_currency(template) {
            wallet.balance(template)
        } else {
            inventory.backpack.stackable_items.count(template)
        };
        let take = have.min(needed);
        let missing = needed - take;
        let cost = gem_cost_for_ingredient(template, missing).ok_or(if is_currency(template) {
            EconomyError::InsufficientFunds { currency: template, needed, have }
        } else {
            EconomyError::InsufficientStackable { template, needed, have }
        })?;
        gem_bill += cost;
        if take > 0 {
            used.push((template, take));
        }
    }
    let gems = wallet.balance(GEMS);
    if gems < gem_bill {
        return Err(EconomyError::InsufficientFunds { currency: GEMS, needed: gem_bill, have: gems });
    }

    // Phase 2: commit. Every amount was checked against the stock above.
    for &(template, take) in &used {
        if is_currency(template) {
            wallet.debit(template, take)?;
        } else {
            consume_stackable(inventory, template, take, tracker)?;
        }
    }
    wallet.debit(GEMS, gem_bill)?;
    Ok(gem_bill)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::GOLD;
    use crate::user_data::{Backpack, Loadout, Treasury};

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    const FROST_SALTS: &str = "05b4dd6b-796f-4088-a772-0e33ea3db976";
    const GRAND_SOUL_GEM: &str = "68d7941e-8c8d-47bf-9f66-becb058f1817";

    fn inventory(stacks: &[(&str, u64)]) -> CompleteInventory {
        let mut inv = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        };
        for (t, n) in stacks {
            inv.backpack.stackable_items.add(uuid(t), *n);
        }
        inv
    }

    fn wallet(gold: u64, gems: u64) -> CompleteWallet {
        let mut w = CompleteWallet::default();
        w.credit(GOLD, gold);
        w.credit(GEMS, gems);
        w
    }

    #[test]
    fn the_table_is_the_apk_asset() {
        assert_eq!(gem_values().len(), 72);
        assert_eq!(gem_values()[&GOLD], 0.015_f32);
        assert_eq!(gem_values()[&uuid(FROST_SALTS)], 5.0);
        assert_eq!(gem_values()[&uuid(GRAND_SOUL_GEM)], 16.0);
    }

    /// The two retail gem-paid enchants: the missing gold, priced per the client.
    #[test]
    fn missing_gold_is_priced_exactly_as_retail_charged_it() {
        assert_eq!(gem_cost_for_ingredient(GOLD, 8_467), Some(128), "capture 59598");
        assert_eq!(gem_cost_for_ingredient(GOLD, 35_000), Some(525), "capture 59601");
        assert_eq!(gem_cost_for_ingredient(GOLD, 0), Some(0));
        assert_eq!(gem_cost_for_ingredient(GOLD, 1), Some(1), "any shortfall costs a gem");
        assert_eq!(gem_cost_for_ingredient(uuid(FROST_SALTS), 17), Some(85));
        assert_eq!(gem_cost_for_ingredient(Uuid::nil(), 1), None, "unpriced");
    }

    #[test]
    fn held_stock_is_used_and_only_the_shortfall_is_bought() {
        // 99 Frost Salts needed, 40 held; gold plentiful.
        let mut w = wallet(100_000, 1_000);
        let mut inv = inventory(&[(FROST_SALTS, 40), (GRAND_SOUL_GEM, 9)]);
        let mut tr = InventoryChangeTracker::default();
        let gems = pay_inputs_with_gems(
            &[(GOLD, 42_000), (uuid(FROST_SALTS), 99), (uuid(GRAND_SOUL_GEM), 4)],
            &mut w,
            &mut inv,
            &mut tr,
        )
        .expect("gems cover the missing salts");
        assert_eq!(gems, 59 * 5);
        assert_eq!(w.balance(GEMS), 1_000 - 295);
        assert_eq!(w.balance(GOLD), 58_000);
        assert_eq!(inv.backpack.stackable_items.count(uuid(FROST_SALTS)), 0);
        assert_eq!(inv.backpack.stackable_items.count(uuid(GRAND_SOUL_GEM)), 5);
        assert!(tr.modified_backpack.stackable_items.contains(&uuid(FROST_SALTS)));
    }

    #[test]
    fn short_of_gems_is_refused_and_nothing_moves() {
        let mut w = wallet(100_000, 294);
        let mut inv = inventory(&[(FROST_SALTS, 40)]);
        let before = (serde_json::to_value(&w).unwrap(), inv.backpack.stackable_items.count(uuid(FROST_SALTS)));
        let mut tr = InventoryChangeTracker::default();
        let err = pay_inputs_with_gems(&[(GOLD, 42_000), (uuid(FROST_SALTS), 99)], &mut w, &mut inv, &mut tr)
            .expect_err("one gem short");
        assert_eq!(err, EconomyError::InsufficientFunds { currency: GEMS, needed: 295, have: 294 });
        assert_eq!(
            (serde_json::to_value(&w).unwrap(), inv.backpack.stackable_items.count(uuid(FROST_SALTS))),
            before
        );
        assert!(tr.modified_backpack.stackable_items.is_empty());
    }

    #[test]
    fn an_unpriced_missing_ingredient_is_refused_like_pay_inputs() {
        let unpriced = Uuid::from_u128(7);
        let mut w = wallet(100_000, 1_000_000);
        let mut inv = inventory(&[]);
        let err = pay_inputs_with_gems(&[(unpriced, 1)], &mut w, &mut inv, &mut InventoryChangeTracker::default())
            .expect_err("no gem price");
        assert_eq!(err, EconomyError::InsufficientStackable { template: unpriced, needed: 1, have: 0 });
        assert_eq!(w.balance(GEMS), 1_000_000);
    }
}
