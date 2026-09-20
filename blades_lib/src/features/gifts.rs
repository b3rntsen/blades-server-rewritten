//! Global gifts — `GET /globalgifts`, `GET /globalgifts/{id}`, `POST /globalgifts/{id}`.
//!
//! Bethesda hands out time-windowed gifts (e.g. the captured "Sunset Gift" =
//! 50000 Gems + 1000 Sigil, claim limit 1). A gift's items are `{itemTemplateId,
//! quantity}`; a template that is a currency UUID credits the wallet, otherwise it
//! grants a stackable. Claiming is idempotent up to `claimCountLimit` and bounded by
//! the `[startTime, endTime]` window (0 = unbounded).
//!
//! Captured claim response:
//! ```jsonc
//! { "reward": { "currencies": { "c64bcb53-…": 1000, "470c8f58-…": 50000 } },
//!   "globalGift": { "globalGiftId": "…", "claimCount": 1 },
//!   "inventory": <CompleteInventoryUpdate>, "wallet": [ { currencyId, balance } ] }
//! ```

use thiserror::Error;

use std::collections::HashMap;

use uuid::Uuid;

use crate::economy::{self, RewardChest, RewardGrant, RewardItem};
use crate::game_data::GameDataItem;
use crate::features::repair::RepairData;
use crate::static_data::GiftDef;
use crate::user_data::{Item, ItemPropertiesAll};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GiftError {
    #[error("gift not found")]
    NotFound,
    #[error("gift not active at this time")]
    NotActive,
    #[error("gift claim limit reached")]
    LimitReached,
}

/// The item types retail keeps as INSTANCES, in `backpack.items`, rather than as
/// a counted line in `backpack.stackableItems`.
///
/// MEASURED, not assumed, across 606 captured retail inventories — the split is
/// total, with no template ever appearing on both sides:
///
/// | type | as instance | as stackable |
/// | --- | --- | --- |
/// | weapon (2) | 13,695 | 0 |
/// | armor (3) | 25,592 | 0 |
/// | shield (9) | 12,502 | 0 |
/// | ring (10) | 23,927 | 0 |
/// | jewelry (11) | 10,940 | 0 |
/// | consumable, material, decoration, emote, quest_item, special | 0 | 72,883 |
const INSTANCED_ITEM_TYPES: [u64; 5] = [2, 3, 9, 10, 11];

/// Does this template need an item instance of its own?
///
/// A template the game data does not know is left stackable: that is the old
/// behaviour, and inventing an instance for an id we cannot classify would put
/// an unopenable object in someone's backpack — which is the very bug this
/// function exists to stop.
pub fn needs_instance(template: Uuid, items: &HashMap<Uuid, GameDataItem>) -> bool {
    items
        .get(&template)
        .is_some_and(|i| INSTANCED_ITEM_TYPES.contains(&i.r#type))
}

/// Build the reward a gift grants.
///
/// Currency templates credit the wallet. Gear — weapons, armour, shields, rings,
/// jewellery — becomes one item INSTANCE per unit granted. Everything else is a
/// counted stackable.
///
/// THE BUG THIS FIXES (#194). Every non-currency line used to become a
/// stackable, so an authored Ebony Mail gift landed in `backpack.stackableItems`
/// as `{"count": 1, "itemTemplateId": "def810af-…"}` — a chest armour with no
/// instance id, no durability and no properties. It showed in game as locked and
/// broken, and repair, sell and salvage all failed, because each of those
/// addresses an item instance that does not exist. The reporter saw
/// "Communication Error - Network Unreachable" three different ways.
///
/// `id` is left nil for the caller to replace, matching the season-award path:
/// every grant mints fresh uuid4s so two claims cannot share an instance id.
pub fn build_gift_reward(
    def: &GiftDef,
    items: &HashMap<Uuid, GameDataItem>,
    repair_data: &RepairData,
) -> RewardGrant {
    let mut reward = RewardGrant::default();
    for item in &def.items {
        if economy::is_currency(item.item_template_id) {
            *reward.currencies.entry(item.item_template_id).or_insert(0) += item.quantity;
        } else if needs_instance(item.item_template_id, items) {
            for _ in 0..item.quantity {
                reward.items.push(RewardItem {
                    // Replaced by the handler before it reaches the inventory.
                    id: Uuid::nil(),
                    item: Item {
                        item_template_id: item.item_template_id,
                        grade: None,
                        tempering_level: 0,
                        // A gift arrives new. Without the table we still hand over
                        // something usable rather than an item at zero durability,
                        // which is what "broken on health status" looked like.
                        durability: repair_data
                            .max_durability(item.item_template_id, 0)
                            .unwrap_or(100.0),
                        properties: ItemPropertiesAll::default(),
                        arcane_tier: None,
                    },
                });
            }
        } else {
            *reward
                .stackable_items
                .entry(item.item_template_id)
                .or_insert(0) += item.quantity;
        }
    }
    reward.chests = def
        .chests
        .iter()
        .map(|chest| RewardChest {
            id: None,
            tier: chest.rarity,
            // Replaced with the claiming character's level by the handler.
            level: 0,
        })
        .collect();
    reward
}

/// Whether the gift can be claimed now, given how many times this character has
/// already claimed it. `now` is unix seconds.
pub fn can_claim(def: &GiftDef, current_count: u64, now: i64) -> Result<(), GiftError> {
    if def.start_time != 0 && now < def.start_time {
        return Err(GiftError::NotActive);
    }
    if def.end_time != 0 && now > def.end_time {
        return Err(GiftError::NotActive);
    }
    if current_count >= def.claim_count_limit {
        return Err(GiftError::LimitReached);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::{GEMS, SIGIL};
    use crate::static_data::{GiftChest, GiftItem};
    use uuid::Uuid;

    /// The real Ebony Mail — chest armour, and the item of report #194.
    const EBONY_MAIL: Uuid = Uuid::from_u128(0xdef810af_e9f5_4e23_9247_1edf391d82e1);
    /// A Minor Healing potion: a consumable, and therefore a stackable.
    const POTION: Uuid = Uuid::from_u128(0x11111111_2222_4333_8444_555555555555);

    fn item_types() -> HashMap<Uuid, GameDataItem> {
        HashMap::from([
            (EBONY_MAIL, GameDataItem { name: "Ebony Mail".into(), r#type: 3 }),
            (POTION, GameDataItem { name: "Potion".into(), r#type: 1 }),
        ])
    }

    /// Empty on purpose in most tests: a missing durability table must still
    /// produce a usable item rather than one at zero.
    fn repair() -> RepairData {
        RepairData::from_json(&serde_json::json!({}), &serde_json::json!({}))
    }

    fn reward_for(def: &GiftDef) -> RewardGrant {
        build_gift_reward(def, &item_types(), &repair())
    }

    fn sunset_gift() -> GiftDef {
        // Verbatim from the captured "Sunset Gift".
        GiftDef {
            global_gift_id: Uuid::from_u128(0x32d4f977_5438_457d_9b48_69e5eaf70eb0),
            items: vec![
                GiftItem {
                    item_template_id: GEMS,
                    quantity: 50000,
                },
                GiftItem {
                    item_template_id: SIGIL,
                    quantity: 1000,
                },
            ],
            chests: vec![],
            start_time: 1774584000,
            end_time: 1782878400,
            claim_count_limit: 1,
            description: Some("Sunset Gift".to_string()),
        }
    }

    #[test]
    fn currency_items_become_reward_currencies() {
        let reward = reward_for(&sunset_gift());
        assert_eq!(reward.currencies.get(&GEMS), Some(&50000));
        assert_eq!(reward.currencies.get(&SIGIL), Some(&1000));
        assert!(reward.stackable_items.is_empty(), "no non-currency items");
    }

    #[test]
    fn non_currency_items_become_stackables() {
        let material = Uuid::from_u128(0x42d91529_c88b_4c5b_815b_b55508b4e7ef);
        let def = GiftDef {
            global_gift_id: Uuid::from_u128(1),
            items: vec![GiftItem {
                item_template_id: material,
                quantity: 5,
            }],
            chests: vec![],
            start_time: 0,
            end_time: 0,
            claim_count_limit: 3,
            description: None,
        };
        let reward = reward_for(&def);
        assert_eq!(reward.stackable_items.get(&material), Some(&5));
        assert!(reward.currencies.is_empty());
    }

    #[test]
    fn gift_chest_rarity_becomes_treasury_tier() {
        let mut def = sunset_gift();
        def.chests.push(GiftChest { rarity: 4 });
        let reward = reward_for(&def);
        assert_eq!(reward.chests.len(), 1);
        assert_eq!(reward.chests[0].tier, 4);
        assert_eq!(reward.chests[0].level, 0);
    }

    #[test]
    fn claim_respects_window_and_limit() {
        let gift = sunset_gift();
        // Before the window opens.
        assert_eq!(can_claim(&gift, 0, 1774583999), Err(GiftError::NotActive));
        // Inside the window, never claimed → ok.
        assert_eq!(can_claim(&gift, 0, 1777000000), Ok(()));
        // Inside the window but already at the limit.
        assert_eq!(
            can_claim(&gift, 1, 1777000000),
            Err(GiftError::LimitReached)
        );
        // After the window closes.
        assert_eq!(can_claim(&gift, 0, 1782878401), Err(GiftError::NotActive));
    }

    #[test]
    fn zero_window_is_unbounded() {
        let def = GiftDef {
            global_gift_id: Uuid::from_u128(2),
            items: vec![],
            chests: vec![],
            start_time: 0,
            end_time: 0,
            claim_count_limit: 1,
            description: None,
        };
        assert_eq!(can_claim(&def, 0, 0), Ok(()));
        assert_eq!(can_claim(&def, 0, 9_999_999_999), Ok(()));
    }
}

#[cfg(test)]
mod report194_gear_instance_tests {
    use super::*;
    use crate::economy::GEMS;
    use crate::static_data::GiftItem;

    const EBONY_MAIL: Uuid = Uuid::from_u128(0xdef810af_e9f5_4e23_9247_1edf391d82e1);
    const POTION: Uuid = Uuid::from_u128(0x11111111_2222_4333_8444_555555555555);
    const UNKNOWN: Uuid = Uuid::from_u128(0x99999999_8888_4777_8666_555555555555);

    fn types() -> HashMap<Uuid, GameDataItem> {
        HashMap::from([
            (EBONY_MAIL, GameDataItem { name: "Ebony Mail".into(), r#type: 3 }),
            (POTION, GameDataItem { name: "Potion".into(), r#type: 1 }),
        ])
    }

    fn repair() -> RepairData {
        RepairData::from_json(&serde_json::json!({}), &serde_json::json!({}))
    }

    fn gift(items: Vec<GiftItem>) -> GiftDef {
        GiftDef {
            global_gift_id: Uuid::from_u128(0x6f7c99c5_e403_4928_bc68_a82f6be18ded),
            items,
            chests: vec![],
            start_time: 0,
            end_time: 0,
            claim_count_limit: 1,
            description: None,
        }
    }

    /// THE BUG (#194). An authored Ebony Mail gift granted a STACKABLE — a chest
    /// armour with no instance id, no durability and no properties. In game it
    /// read as locked and broken, and repair, sell and salvage each failed,
    /// because all three address an item instance that does not exist.
    #[test]
    fn gear_is_granted_as_an_instance_not_a_stackable() {
        let reward = build_gift_reward(
            &gift(vec![GiftItem { item_template_id: EBONY_MAIL, quantity: 1 }]),
            &types(),
            &repair(),
        );
        assert!(
            reward.stackable_items.is_empty(),
            "armour must never land in stackableItems: {:?}",
            reward.stackable_items
        );
        assert_eq!(reward.items.len(), 1);
        assert_eq!(reward.items[0].item.item_template_id, EBONY_MAIL);
        // Broken on arrival is what the reporter actually saw, so this is not a
        // detail: a gift arrives new.
        assert!(reward.items[0].item.durability > 0.0);
    }

    /// CONTROL: consumables and materials must STAY stackable. A rule that
    /// instanced everything would put 20 separate potions in a backpack and
    /// break stacking for the whole game.
    #[test]
    fn a_consumable_stays_stackable() {
        let reward = build_gift_reward(
            &gift(vec![GiftItem { item_template_id: POTION, quantity: 5 }]),
            &types(),
            &repair(),
        );
        assert!(reward.items.is_empty());
        assert_eq!(reward.stackable_items.get(&POTION), Some(&5));
    }

    /// CONTROL: a currency is neither. This is the captured Sunset Gift's whole
    /// payload, so getting it wrong would break the only gift retail ever sent.
    #[test]
    fn a_currency_is_still_a_currency() {
        let reward = build_gift_reward(
            &gift(vec![GiftItem { item_template_id: GEMS, quantity: 50_000 }]),
            &types(),
            &repair(),
        );
        assert_eq!(reward.currencies.get(&GEMS), Some(&50_000));
        assert!(reward.items.is_empty() && reward.stackable_items.is_empty());
    }

    /// A template the game data cannot classify stays stackable — the old
    /// behaviour. Inventing an instance for an id we do not know would put
    /// another unopenable object in a backpack, which is this ticket.
    #[test]
    fn an_unknown_template_is_left_alone() {
        let reward = build_gift_reward(
            &gift(vec![GiftItem { item_template_id: UNKNOWN, quantity: 2 }]),
            &types(),
            &repair(),
        );
        assert_eq!(reward.stackable_items.get(&UNKNOWN), Some(&2));
        assert!(reward.items.is_empty());
    }

    /// Quantity means COUNT for a stackable and COPIES for gear: three shields
    /// are three separate things to equip, repair and sell.
    #[test]
    fn a_quantity_of_gear_is_that_many_instances() {
        let reward = build_gift_reward(
            &gift(vec![GiftItem { item_template_id: EBONY_MAIL, quantity: 3 }]),
            &types(),
            &repair(),
        );
        assert_eq!(reward.items.len(), 3);
    }

    /// The five types retail keeps as instances, and a sample of those it does
    /// not — measured across 606 captured inventories, where the split is total.
    #[test]
    fn the_instanced_types_are_the_measured_ones() {
        for (t, instanced) in [
            (2, true),   // weapon
            (3, true),   // armor
            (9, true),   // shield
            (10, true),  // ring
            (11, true),  // jewelry
            (1, false),  // consumable
            (4, false),  // decoration
            (5, false),  // special
            (8, false),  // material
            (12, false), // quest_item
            (14, false), // emote
        ] {
            let id = Uuid::from_u128(t as u128);
            let table = HashMap::from([(id, GameDataItem { name: "x".into(), r#type: t })]);
            assert_eq!(needs_instance(id, &table), instanced, "type {t}");
        }
    }
}
