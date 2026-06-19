//! Shared economy primitives — currencies, wallet debit/credit, reward grants and
//! inventory mutation — used by every town/RPG endpoint (shops, crafting, chests,
//! quests, events, challenges, abyss, gifts, the global store, town building).
//!
//! The three global currencies and the uniform `reward` shape are taken verbatim
//! from captured retail traffic (`api_captures`):
//!
//! ```jsonc
//! "reward": {
//!   "currencies":     { "<currencyId>": <n> },   // object, NOT the wallet array
//!   "stackableItems": { "<templateId>": <n> },
//!   "items":          [ { "id", "itemTemplateId", "temperingLevel", "durability", "properties" } ],
//!   "characterXp":    <n>,
//!   "townXp":         <n>
//! }
//! ```
//!
//! Wallets debit/credit for real and fail on insufficient funds; XP is added to the
//! character's running `experience` (the client levels up explicitly via `/levelup`,
//! so this never auto-levels). `townXp` is echoed in the reward but the town object
//! itself is opaque JSON mutated by the town handlers, not here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::user_data::{
    CompleteCharacter, CompleteInventory, CompleteWallet, InventoryChangeTracker, Item, WalletEntry,
};

/// **Gold** — the primary soft currency (quest/loot rewards, town vendors, building
/// upgrades, crafting/repair costs).
pub const GOLD: Uuid = Uuid::from_u128(0xf8d27767_a85e_4fd6_a5bb_bf8a13d0daa2);
/// **Sigil** — the event currency (event-quest rewards; spent in the global event shop).
pub const SIGIL: Uuid = Uuid::from_u128(0xc64bcb53_41f4_41ba_892a_fe2cca423caa);
/// **Gems** — the green / premium currency (craft & chest speed-ups, gem-priced
/// offers, inventory-level upgrades). Earned in-game (gifts, some rewards); buying
/// gems with real money is out of scope (IAP is a priced placeholder only).
pub const GEMS: Uuid = Uuid::from_u128(0x470c8f58_a8dd_4c07_8c92_843b785e1139);

/// Errors from spending/granting. Mapped to a `BladeApiError` envelope at the
/// handler boundary (the server crate owns the HTTP status / service-id mapping).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EconomyError {
    #[error("insufficient funds: need {needed} of currency {currency}, have {have}")]
    InsufficientFunds {
        currency: Uuid,
        needed: u64,
        have: u64,
    },
    /// The client's `expectedPrices` did not match what the server would charge —
    /// the catalog moved under the player (reject rather than silently overcharge).
    #[error("price mismatch between client expectation and server catalog")]
    PriceMismatch,
    /// A referenced instanced item (by id) is not in the backpack/loadout.
    #[error("item {0} not found in inventory")]
    ItemNotFound(Uuid),
    /// Not enough of a stackable material to consume.
    #[error("not enough of stackable {template}: need {needed}, have {have}")]
    InsufficientStackable {
        template: Uuid,
        needed: u64,
        have: u64,
    },
}

/// A single currency price — the captured `{currencyId, quantity}` shape used in
/// shop `expectedPrices`, `catalogoverrides` `prices`, and recipe costs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Price {
    pub currency_id: Uuid,
    pub quantity: u64,
}

impl Price {
    pub fn new(currency_id: Uuid, quantity: u64) -> Self {
        Price {
            currency_id,
            quantity,
        }
    }
}

impl CompleteWallet {
    /// Balance of a currency (0 if the player has never held it).
    pub fn balance(&self, currency: Uuid) -> u64 {
        self.0.get(&currency).map(|e| e.balance).unwrap_or(0)
    }

    /// Add `amount` of a currency, creating the entry if needed.
    pub fn credit(&mut self, currency: Uuid, amount: u64) {
        self.0
            .entry(currency)
            .or_insert(WalletEntry { balance: 0 })
            .balance += amount;
    }

    /// Subtract `amount` of a currency, erroring (without mutating) if the balance
    /// is too low.
    pub fn debit(&mut self, currency: Uuid, amount: u64) -> Result<(), EconomyError> {
        let have = self.balance(currency);
        if have < amount {
            return Err(EconomyError::InsufficientFunds {
                currency,
                needed: amount,
                have,
            });
        }
        self.0
            .entry(currency)
            .or_insert(WalletEntry { balance: 0 })
            .balance -= amount;
        Ok(())
    }

    /// Pay a set of prices atomically: verify every line is affordable first, then
    /// debit them all (so a partially-affordable multi-currency price never leaves
    /// the wallet half-charged).
    pub fn try_pay(&mut self, prices: &[Price]) -> Result<(), EconomyError> {
        for p in prices {
            let have = self.balance(p.currency_id);
            if have < p.quantity {
                return Err(EconomyError::InsufficientFunds {
                    currency: p.currency_id,
                    needed: p.quantity,
                    have,
                });
            }
        }
        for p in prices {
            self.debit(p.currency_id, p.quantity)
                .expect("affordability checked above");
        }
        Ok(())
    }
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// An instanced (non-stackable) item granted by a reward — `{id, itemTemplateId,
/// temperingLevel, durability, properties}`. The `id` is the new item instance id;
/// the rest flattens [`Item`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardItem {
    pub id: Uuid,
    #[serde(flatten)]
    pub item: Item,
}

/// The uniform `reward` block returned by quest/event/challenge completion, chest
/// collection, shop/global-shop purchase, gift claim, salvage, etc. Empty
/// collections and zero XP are omitted so each endpoint's wire matches its captures.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardGrant {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub currencies: HashMap<Uuid, u64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub stackable_items: HashMap<Uuid, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<RewardItem>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub character_xp: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub town_xp: u64,
}

impl RewardGrant {
    pub fn is_empty(&self) -> bool {
        self.currencies.is_empty()
            && self.stackable_items.is_empty()
            && self.items.is_empty()
            && self.character_xp == 0
            && self.town_xp == 0
    }

    /// Convenience: a reward of only currencies.
    pub fn currencies(currencies: HashMap<Uuid, u64>) -> Self {
        RewardGrant {
            currencies,
            ..Default::default()
        }
    }
}

/// Apply a reward to a player: credit currencies, grant stackables and instanced
/// items into the backpack, and add character XP. Records every touched item in
/// `tracker` so the caller's `generate_client_update` emits the diff. Does **not**
/// bump `backpackVersion`/`treasuryVersion` — the handler bumps once per request
/// (matching the captured single-increment-per-mutation behaviour).
pub fn apply_reward(
    reward: &RewardGrant,
    wallet: &mut CompleteWallet,
    inventory: &mut CompleteInventory,
    character: &mut CompleteCharacter,
    tracker: &mut InventoryChangeTracker,
) {
    for (currency, amount) in &reward.currencies {
        wallet.credit(*currency, *amount);
    }
    for (template, count) in &reward.stackable_items {
        inventory.backpack.stackable_items.add(*template, *count);
        tracker.modified_backpack.stackable_items.insert(*template);
    }
    for ri in &reward.items {
        inventory.backpack.items.0.insert(ri.id, ri.item.clone());
        tracker.modified_backpack.items.insert(ri.id);
    }
    character.experience += reward.character_xp;
}

/// Consume `count` of a stackable material from the backpack, erroring if there is
/// not enough. Marks the change in `tracker`.
pub fn consume_stackable(
    inventory: &mut CompleteInventory,
    template: Uuid,
    count: u64,
    tracker: &mut InventoryChangeTracker,
) -> Result<(), EconomyError> {
    match inventory.backpack.stackable_items.remove(template, count) {
        Ok(_) => {
            tracker.modified_backpack.stackable_items.insert(template);
            Ok(())
        }
        Err(have) => Err(EconomyError::InsufficientStackable {
            template,
            needed: count,
            have,
        }),
    }
}

/// Remove an instanced item by id from the backpack (not the loadout), erroring if
/// absent. Marks it removed in `tracker`.
pub fn remove_backpack_item(
    inventory: &mut CompleteInventory,
    item_id: Uuid,
    tracker: &mut InventoryChangeTracker,
) -> Result<Item, EconomyError> {
    match inventory.backpack.items.0.remove(&item_id) {
        Some(item) => {
            tracker.modified_backpack.items.insert(item_id);
            Ok(item)
        }
        None => Err(EconomyError::ItemNotFound(item_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_data::{Backpack, ItemPropertiesAll, Loadout, Treasury};

    fn empty_inventory() -> CompleteInventory {
        CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        }
    }

    fn item(template: Uuid) -> Item {
        Item {
            item_template_id: template,
            tempering_level: 0,
            durability: 75.0,
            properties: ItemPropertiesAll::default(),
        }
    }

    #[test]
    fn credit_and_debit_track_balance() {
        let mut w = CompleteWallet::default();
        assert_eq!(w.balance(GOLD), 0);
        w.credit(GOLD, 100);
        assert_eq!(w.balance(GOLD), 100);
        w.debit(GOLD, 30).unwrap();
        assert_eq!(w.balance(GOLD), 70);
    }

    #[test]
    fn debit_below_zero_errors_without_mutating() {
        let mut w = CompleteWallet::default();
        w.credit(GOLD, 10);
        let err = w.debit(GOLD, 50).unwrap_err();
        assert_eq!(
            err,
            EconomyError::InsufficientFunds {
                currency: GOLD,
                needed: 50,
                have: 10
            }
        );
        assert_eq!(w.balance(GOLD), 10, "balance unchanged on failed debit");
    }

    #[test]
    fn try_pay_is_atomic_across_currencies() {
        let mut w = CompleteWallet::default();
        w.credit(GOLD, 100);
        w.credit(GEMS, 1);
        // Affordable gold but unaffordable gems → nothing is charged.
        let err = w
            .try_pay(&[Price::new(GOLD, 50), Price::new(GEMS, 5)])
            .unwrap_err();
        assert!(matches!(err, EconomyError::InsufficientFunds { .. }));
        assert_eq!(w.balance(GOLD), 100);
        assert_eq!(w.balance(GEMS), 1);
        // Affordable → both charged.
        w.try_pay(&[Price::new(GOLD, 50), Price::new(GEMS, 1)]).unwrap();
        assert_eq!(w.balance(GOLD), 50);
        assert_eq!(w.balance(GEMS), 0);
    }

    #[test]
    fn stackables_add_and_remove_to_zero_clears_entry() {
        let mut inv = empty_inventory();
        inv.backpack.stackable_items.add(GOLD, 3); // any uuid works as a template key
        assert_eq!(inv.backpack.stackable_items.count(GOLD), 3);
        assert_eq!(inv.backpack.stackable_items.remove(GOLD, 1).unwrap(), 2);
        assert_eq!(inv.backpack.stackable_items.remove(GOLD, 2).unwrap(), 0);
        assert!(inv.backpack.stackable_items.is_empty(), "zeroed stack removed");
        assert_eq!(inv.backpack.stackable_items.remove(GOLD, 1), Err(0));
    }

    #[test]
    fn apply_reward_credits_grants_and_adds_xp() {
        let mut w = CompleteWallet::default();
        let mut inv = empty_inventory();
        let mut ch = CompleteCharacter::default();
        ch.experience = 100;
        let mut tracker = InventoryChangeTracker::default();

        let material = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
        let instanced = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);
        let template = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);
        let reward = RewardGrant {
            currencies: HashMap::from([(GOLD, 250), (SIGIL, 12)]),
            stackable_items: HashMap::from([(material, 5)]),
            items: vec![RewardItem {
                id: instanced,
                item: item(template),
            }],
            character_xp: 700,
            town_xp: 10,
        };
        apply_reward(&reward, &mut w, &mut inv, &mut ch, &mut tracker);

        assert_eq!(w.balance(GOLD), 250);
        assert_eq!(w.balance(SIGIL), 12);
        assert_eq!(inv.backpack.stackable_items.count(material), 5);
        assert!(inv.backpack.items.0.contains_key(&instanced));
        assert_eq!(ch.experience, 800);
        assert!(tracker.modified_backpack.stackable_items.contains(&material));
        assert!(tracker.modified_backpack.items.contains(&instanced));
    }

    #[test]
    fn reward_grant_omits_empty_fields_on_the_wire() {
        let reward = RewardGrant {
            currencies: HashMap::from([(GOLD, 59)]),
            ..Default::default()
        };
        let json = serde_json::to_value(&reward).unwrap();
        assert!(json.get("currencies").is_some());
        assert!(json.get("stackableItems").is_none(), "empty omitted");
        assert!(json.get("items").is_none(), "empty omitted");
        assert!(json.get("characterXp").is_none(), "zero omitted");
    }

    #[test]
    fn reward_grant_deserializes_from_captured_quest_complete() {
        // Verbatim shape from a captured /quests/{id}/complete reward.
        let body = serde_json::json!({
            "currencies": {
                "c64bcb53-41f4-41ba-892a-fe2cca423caa": 12,
                "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2": 14000
            },
            "characterXp": 700,
            "townXp": 0
        });
        let reward: RewardGrant = serde_json::from_value(body).unwrap();
        assert_eq!(reward.currencies.get(&SIGIL), Some(&12));
        assert_eq!(reward.currencies.get(&GOLD), Some(&14000));
        assert_eq!(reward.character_xp, 700);
    }
}
