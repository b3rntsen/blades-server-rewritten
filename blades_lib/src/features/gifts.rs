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

use crate::economy::{self, RewardGrant};
use crate::static_data::GiftDef;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GiftError {
    #[error("gift not found")]
    NotFound,
    #[error("gift not active at this time")]
    NotActive,
    #[error("gift claim limit reached")]
    LimitReached,
}

/// Build the reward a gift grants: currency-template lines credit the wallet
/// (`reward.currencies`), everything else grants a stackable (`reward.stackableItems`).
pub fn build_gift_reward(def: &GiftDef) -> RewardGrant {
    let mut reward = RewardGrant::default();
    for item in &def.items {
        if economy::is_currency(item.item_template_id) {
            *reward.currencies.entry(item.item_template_id).or_insert(0) += item.quantity;
        } else {
            *reward
                .stackable_items
                .entry(item.item_template_id)
                .or_insert(0) += item.quantity;
        }
    }
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
    use crate::static_data::GiftItem;
    use uuid::Uuid;

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
            start_time: 1774584000,
            end_time: 1782878400,
            claim_count_limit: 1,
            description: Some("Sunset Gift".to_string()),
        }
    }

    #[test]
    fn currency_items_become_reward_currencies() {
        let reward = build_gift_reward(&sunset_gift());
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
            start_time: 0,
            end_time: 0,
            claim_count_limit: 3,
            description: None,
        };
        let reward = build_gift_reward(&def);
        assert_eq!(reward.stackable_items.get(&material), Some(&5));
        assert!(reward.currencies.is_empty());
    }

    #[test]
    fn claim_respects_window_and_limit() {
        let gift = sunset_gift();
        // Before the window opens.
        assert_eq!(can_claim(&gift, 0, 1774583999), Err(GiftError::NotActive));
        // Inside the window, never claimed → ok.
        assert_eq!(can_claim(&gift, 0, 1777000000), Ok(()));
        // Inside the window but already at the limit.
        assert_eq!(can_claim(&gift, 1, 1777000000), Err(GiftError::LimitReached));
        // After the window closes.
        assert_eq!(can_claim(&gift, 0, 1782878401), Err(GiftError::NotActive));
    }

    #[test]
    fn zero_window_is_unbounded() {
        let def = GiftDef {
            global_gift_id: Uuid::from_u128(2),
            items: vec![],
            start_time: 0,
            end_time: 0,
            claim_count_limit: 1,
            description: None,
        };
        assert_eq!(can_claim(&def, 0, 0), Ok(()));
        assert_eq!(can_claim(&def, 0, 9_999_999_999), Ok(()));
    }
}
