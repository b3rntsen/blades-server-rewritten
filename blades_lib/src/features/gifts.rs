//! Global gifts — `GET /globalgifts`, `GET /globalgifts/{id}`, `POST /globalgifts/{id}`.
//!
//! Bethesda hands out time-windowed gifts (e.g. the captured "Sunset Gift" =
//! 50000 Gems + 1000 Sigil, claim limit 1). A gift's items are `{itemTemplateId,
//! quantity}`; a currency credits the wallet, APK-known breakable gear mints item
//! instances, and the remaining templates grant stackables. Claiming is idempotent
//! up to `claimCountLimit` and bounded by the `[startTime, endTime]` window
//! (0 = unbounded).
//!
//! Captured claim response:
//! ```jsonc
//! { "reward": { "currencies": { "c64bcb53-…": 1000, "470c8f58-…": 50000 } },
//!   "globalGift": { "globalGiftId": "…", "claimCount": 1 },
//!   "inventory": <CompleteInventoryUpdate>, "wallet": [ { currencyId, balance } ] }
//! ```

use thiserror::Error;

use crate::economy::{self, RewardChest, RewardGrant, RewardItem};
use crate::features::repair::{MAX_INSTANCED_GIFT_ITEMS, RepairData};
use crate::static_data::GiftDef;
use crate::user_data::{Item, ItemPropertiesAll};
use uuid::Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GiftError {
    #[error("gift not found")]
    NotFound,
    #[error("gift not active at this time")]
    NotActive,
    #[error("gift claim limit reached")]
    LimitReached,
    #[error("gift contains too many instanced items")]
    TooManyInstancedItems,
}

/// Build the reward a gift grants.
///
/// The gift wire names only a template and quantity, but the APK still tells us
/// which templates are breakable gear and their full untempered durability. Those
/// lines must mint instances; treating Ebony Mail as a stackable is the report #194
/// corruption. Unknown/non-breakable templates remain stackables, matching retail's
/// captured material gifts.
pub fn build_gift_reward(
    def: &GiftDef,
    repair_data: &RepairData,
    mut new_item_id: impl FnMut() -> Uuid,
) -> Result<RewardGrant, GiftError> {
    let mut reward = RewardGrant::default();
    for item in &def.items {
        if economy::is_currency(item.item_template_id) {
            *reward.currencies.entry(item.item_template_id).or_insert(0) += item.quantity;
        } else if let Some(durability) = repair_data.max_durability(item.item_template_id, 0) {
            if item.quantity > MAX_INSTANCED_GIFT_ITEMS {
                return Err(GiftError::TooManyInstancedItems);
            }
            reward.items.extend((0..item.quantity).map(|_| RewardItem {
                id: new_item_id(),
                item: Item {
                    item_template_id: item.item_template_id,
                    grade: None,
                    tempering_level: 0,
                    durability,
                    properties: ItemPropertiesAll::default(),
                    arcane_tier: None,
                },
            }));
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
    Ok(reward)
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
    use serde_json::json;
    use uuid::Uuid;

    const EBONY_MAIL: Uuid = Uuid::from_u128(0xdef810af_e9f5_4e23_9247_1edf391d82e1);

    fn repair_data() -> RepairData {
        let durability = json!({
            EBONY_MAIL.to_string(): {
                "0": 453.0, "1": 453.0, "2": 453.0, "3": 453.0,
                "4": 453.0, "5": 453.0, "6": 453.0, "7": 453.0,
                "8": 453.0, "9": 453.0, "10": 453.0
            }
        });
        RepairData::from_json(&durability, &json!({}))
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
        let reward = build_gift_reward(&sunset_gift(), &RepairData::default(), Uuid::nil).unwrap();
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
        let reward = build_gift_reward(&def, &RepairData::default(), Uuid::nil).unwrap();
        assert_eq!(reward.stackable_items.get(&material), Some(&5));
        assert!(reward.currencies.is_empty());
    }

    /// Report #194: the owner authored a one-Ebony-Mail gift. The old generic
    /// non-currency branch stored it as `{itemTemplateId,count}`, so the client
    /// had no item id to send to repair/sell/salvage and every request was 400.
    #[test]
    fn apk_breakable_gift_becomes_a_full_item_instance() {
        let mut def = sunset_gift();
        def.items = vec![GiftItem {
            item_template_id: EBONY_MAIL,
            quantity: 1,
        }];

        let reward = build_gift_reward(&def, &repair_data(), || Uuid::from_u128(0x194)).unwrap();

        assert!(reward.stackable_items.is_empty());
        assert_eq!(reward.items.len(), 1);
        assert_eq!(reward.items[0].id, Uuid::from_u128(0x194));
        assert_eq!(reward.items[0].item.item_template_id, EBONY_MAIL);
        assert_eq!(reward.items[0].item.tempering_level, 0);
        assert_eq!(reward.items[0].item.durability, 453.0);
    }

    #[test]
    fn implausibly_large_gear_gift_is_rejected_before_allocating_instances() {
        let mut def = sunset_gift();
        def.items = vec![GiftItem {
            item_template_id: EBONY_MAIL,
            quantity: MAX_INSTANCED_GIFT_ITEMS + 1,
        }];

        assert!(matches!(
            build_gift_reward(&def, &repair_data(), Uuid::nil),
            Err(GiftError::TooManyInstancedItems)
        ));
    }

    #[test]
    fn gift_chest_rarity_becomes_treasury_tier() {
        let mut def = sunset_gift();
        def.chests.push(GiftChest { rarity: 4 });
        let reward = build_gift_reward(&def, &RepairData::default(), Uuid::nil).unwrap();
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
