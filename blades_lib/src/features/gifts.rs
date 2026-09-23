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
use crate::user_data::CompleteInventory;
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

/// Does this template need an item instance of its own?
///
/// A template the game data does not know is left stackable: that is the old
/// behaviour, and inventing an instance for an id we cannot classify would put
/// an unopenable object in someone's backpack — which is the very bug this
/// function exists to stop. The type table it reads is in
/// [`crate::economy::bucket_for_template`], shared with the shop grant.
pub fn needs_instance(template: Uuid, items: &HashMap<Uuid, GameDataItem>) -> bool {
    economy::bucket_for_template(template, items) == Some(economy::ItemBucket::Instance)
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
                        // An authored enchant (e.g. a dual-enchant arcane weapon)
                        // is stamped on every instance; plain gear stays plain.
                        properties: ItemPropertiesAll {
                            enchanting: item.enchanting.clone(),
                            ..ItemPropertiesAll::default()
                        },
                        arcane_tier: item.arcane_tier,
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

/// The most instances one gift line may expand into.
///
/// A stackable line is a count; an instanced one is that many separate objects
/// in a backpack with a capacity. 100 is far above anything retail authored (the
/// largest gift gear line is 1) and far below a number that would fill an
/// inventory from a typo in the admin form.
pub const MAX_INSTANCED_GIFT_ITEMS: u64 = 100;

/// Move gear that a pre-#194 gift payout filed as a stackable into real item
/// instances, in place.
///
/// Returns what it moved, as `(template, count)`; an empty vector means there
/// was nothing to do and the caller must not write.
///
/// WHY IT IS NEEDED AT ALL. [`build_gift_reward`] now grants gear correctly, but
/// four characters had already been handed an Ebony Mail as
/// `{"count": 1, "itemTemplateId": "def810af-…"}` — an armour with no instance
/// id, no durability and no properties, which renders as locked and broken and
/// which repair, sell and salvage all refuse because each addresses an instance
/// that does not exist.
///
/// The classification is [`economy::bucket_for_template`], the same table the
/// payout and the shop grant read, so a row repaired here cannot disagree with
/// a row granted today. A template the game data cannot name is left alone: it
/// is not ours to reinterpret.
///
/// `new_item_id` is injected so a test can assert the ids rather than watch
/// random ones go by.
pub fn promote_legacy_gift_gear(
    items: &HashMap<Uuid, GameDataItem>,
    repair_data: &RepairData,
    inventory: &mut CompleteInventory,
    mut new_item_id: impl FnMut() -> Uuid,
) -> Vec<(Uuid, u64)> {
    // Snapshot first: the loop below mutates the same map this reads.
    let candidates: Vec<(Uuid, u64, f64)> = inventory
        .backpack
        .stackable_items
        .counts()
        .filter_map(|(template, count)| {
            if !needs_instance(template, items) || count == 0 || count > MAX_INSTANCED_GIFT_ITEMS {
                return None;
            }
            // Rings and jewellery never carry durability — absent on all 34,867
            // captured instances — so 0.0 is their faithful value, not a
            // fallback. Everything else takes the table's, or its default.
            let durability = if economy::template_skips_durability(template, items) {
                0.0
            } else {
                repair_data
                    .max_durability(template, 0)
                    .unwrap_or(crate::features::repair::DEFAULT_DURABILITY)
            };
            Some((template, count, durability))
        })
        .collect();

    let mut promoted = Vec::with_capacity(candidates.len());
    for (template, count, durability) in candidates {
        // The count came from the snapshot above and nothing else has touched
        // the map, so this cannot fail; if it ever does, leaving the stack in
        // place is the safe outcome rather than minting items for free.
        if inventory
            .backpack
            .stackable_items
            .remove(template, count)
            .is_err()
        {
            continue;
        }
        for _ in 0..count {
            inventory.backpack.items.0.insert(
                new_item_id(),
                Item {
                    item_template_id: template,
                    grade: None,
                    tempering_level: 0,
                    durability,
                    properties: ItemPropertiesAll::default(),
                    arcane_tier: None,
                },
            );
        }
        promoted.push((template, count));
    }
    promoted
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
                arcane_tier: None,
                enchanting: vec![],
            },
                GiftItem {
                    item_template_id: SIGIL,
                    quantity: 1000,
                arcane_tier: None,
                enchanting: vec![],
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
                arcane_tier: None,
                enchanting: vec![],
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
    use crate::user_data::ItemSingleProperty;

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

    const FIRE: Uuid = Uuid::from_u128(0xc40ed851_8777_4d09_b169_0223dae8f67d);
    const FORTIFY: Uuid = Uuid::from_u128(0xbf107d7d_8777_4411_b07a_56819a58a709);
    const RAVAGE: Uuid = Uuid::from_u128(0x7cdb7179_4cd4_466e_8b77_4caf2fddb268);

    fn dual_enchant_line(quantity: u64) -> GiftItem {
        GiftItem {
            item_template_id: EBONY_MAIL,
            quantity,
            arcane_tier: Some(2),
            enchanting: vec![
                ItemSingleProperty { id: FIRE, tier: 10 },
                ItemSingleProperty { id: FORTIFY, tier: 10 },
                ItemSingleProperty { id: RAVAGE, tier: 10 },
            ],
        }
    }

    /// An authored enchant reaches every granted instance: primary first, the
    /// arcane tier with it. Without this a gifted "dual enchant" weapon arrived
    /// plain.
    #[test]
    fn an_authored_enchant_is_stamped_on_every_instance() {
        let reward = build_gift_reward(&gift(vec![dual_enchant_line(2)]), &types(), &repair());
        assert_eq!(reward.items.len(), 2);
        for r in &reward.items {
            assert_eq!(r.item.arcane_tier, Some(2));
            let ids: Vec<Uuid> = r.item.properties.enchanting.iter().map(|p| p.id).collect();
            assert_eq!(ids, vec![FIRE, FORTIFY, RAVAGE]);
        }
    }

    /// CONTROL: a line with no authored enchant still grants plain gear.
    #[test]
    fn plain_gear_stays_plain() {
        let reward = build_gift_reward(
            &gift(vec![GiftItem { item_template_id: EBONY_MAIL, quantity: 1, arcane_tier: None, enchanting: vec![] }]),
            &types(),
            &repair(),
        );
        assert_eq!(reward.items[0].item.arcane_tier, None);
        assert!(reward.items[0].item.properties.enchanting.is_empty());
    }

    /// The client is shown retail's line shape only; the database keeps the rest.
    #[test]
    fn the_client_never_sees_the_enchant_but_storage_keeps_it() {
        let def = gift(vec![dual_enchant_line(1)]);
        let shown = serde_json::to_value(def.for_client()).unwrap();
        assert_eq!(
            shown["items"][0],
            serde_json::json!({"itemTemplateId": EBONY_MAIL, "quantity": 1}),
            "extra keys on the wire could break the gift screen"
        );
        let stored = serde_json::to_value(&def.items).unwrap();
        let back: Vec<GiftItem> = serde_json::from_value(stored).unwrap();
        assert_eq!(back[0].arcane_tier, Some(2));
        assert_eq!(back[0].enchanting.len(), 3);
    }

    /// Rows written before these fields existed still load.
    #[test]
    fn an_old_row_without_the_fields_still_loads() {
        let back: Vec<GiftItem> =
            serde_json::from_value(serde_json::json!([{"itemTemplateId": EBONY_MAIL, "quantity": 1}])).unwrap();
        assert_eq!(back[0].arcane_tier, None);
        assert!(back[0].enchanting.is_empty());
    }

    /// THE BUG (#194). An authored Ebony Mail gift granted a STACKABLE — a chest
    /// armour with no instance id, no durability and no properties. In game it
    /// read as locked and broken, and repair, sell and salvage each failed,
    /// because all three address an item instance that does not exist.
    #[test]
    fn gear_is_granted_as_an_instance_not_a_stackable() {
        let reward = build_gift_reward(
            &gift(vec![GiftItem { item_template_id: EBONY_MAIL, quantity: 1, arcane_tier: None, enchanting: vec![] }]),
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
            &gift(vec![GiftItem { item_template_id: POTION, quantity: 5, arcane_tier: None, enchanting: vec![] }]),
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
            &gift(vec![GiftItem { item_template_id: GEMS, quantity: 50_000, arcane_tier: None, enchanting: vec![] }]),
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
            &gift(vec![GiftItem { item_template_id: UNKNOWN, quantity: 2, arcane_tier: None, enchanting: vec![] }]),
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
            &gift(vec![GiftItem { item_template_id: EBONY_MAIL, quantity: 3, arcane_tier: None, enchanting: vec![] }]),
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

#[cfg(test)]
mod report194_legacy_promotion_tests {
    //! The repair for rows the old payout already wrote. Ported from the
    //! parallel fix (fork #329), with one assertion changed — see
    //! `a_ring_is_promoted_without_durability`.
    use super::*;
    use crate::game_data::GameDataItem;
    use crate::user_data::{Backpack, CompleteInventory, Loadout, Treasury};
    use serde_json::json;

    const EBONY_MAIL: Uuid = Uuid::from_u128(0xdef810af_e9f5_4e23_9247_1edf391d82e1);
    const RING: Uuid = Uuid::from_u128(0x11111111_2222_4333_8444_555555555555);
    const MATERIAL: Uuid = Uuid::from_u128(0x42d91529_c88b_4c5b_815b_b55508b4e7ef);

    fn game_items() -> HashMap<Uuid, GameDataItem> {
        HashMap::from([
            (EBONY_MAIL, GameDataItem { name: "Ebony Mail".into(), r#type: 3 }),
            (RING, GameDataItem { name: "Ring".into(), r#type: 10 }),
            (MATERIAL, GameDataItem { name: "Material".into(), r#type: 8 }),
        ])
    }

    fn repair_data() -> RepairData {
        let durability = json!({
            EBONY_MAIL.to_string(): {
                "0": 453.0, "1": 453.0, "2": 453.0, "3": 453.0, "4": 453.0,
                "5": 453.0, "6": 453.0, "7": 453.0, "8": 453.0, "9": 453.0, "10": 453.0
            }
        });
        RepairData::from_json(&durability, &json!({}))
    }

    /// `uuid` is built without the `v4` feature here, so mint distinct ids by
    /// counting rather than randomly — which a test wants anyway.
    fn fresh_id() -> Uuid {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0x1000);
        Uuid::from_u128(N.fetch_add(1, Ordering::Relaxed) as u128)
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

    /// THE REPAIR. Four characters hold an Ebony Mail filed as a stackable; this
    /// is what turns it back into something they can wear, repair and sell.
    #[test]
    fn legacy_gift_gear_is_promoted_to_full_item_instances() {
        let mut inv = inventory();
        inv.backpack.stackable_items.add(EBONY_MAIL, 2);
        inv.backpack.stackable_items.add(RING, 1);
        inv.backpack.stackable_items.add(MATERIAL, 7);

        let mut next = 1u128;
        let promoted = promote_legacy_gift_gear(&game_items(), &repair_data(), &mut inv, || {
            let id = Uuid::from_u128(next);
            next += 1;
            id
        });

        assert_eq!(promoted.len(), 2, "the armour and the ring, not the material");
        assert_eq!(inv.backpack.stackable_items.count(EBONY_MAIL), 0);
        assert_eq!(inv.backpack.stackable_items.count(RING), 0);
        // CONTROL: a material is a stackable and must stay one.
        assert_eq!(inv.backpack.stackable_items.count(MATERIAL), 7);
        assert_eq!(inv.backpack.items.0.len(), 3, "two armours and one ring");
        assert!(inv.backpack.items.0.values().any(|i| {
            i.item_template_id == EBONY_MAIL && i.durability == 453.0
        }));
    }

    /// The one place this differs from fork #329, which gave a promoted ring
    /// `DEFAULT_DURABILITY` (100).
    ///
    /// Rings and jewellery carry NO durability in retail — absent on all 34,867
    /// captured instances, and absent from `item_durability.json` because they
    /// do not wear out. Handing a gift ring 100 would make it the only ring in
    /// the game with a durability bar, so the faithful value is 0.
    #[test]
    fn a_ring_is_promoted_without_durability() {
        let mut inv = inventory();
        inv.backpack.stackable_items.add(RING, 1);
        promote_legacy_gift_gear(&game_items(), &repair_data(), &mut inv, fresh_id);
        let ring = inv
            .backpack
            .items
            .0
            .values()
            .find(|i| i.item_template_id == RING)
            .expect("the ring was promoted");
        assert_eq!(ring.durability, 0.0);
    }

    /// An absurd stack is left alone rather than expanded into a backpack full
    /// of objects — a typo in the admin form must not cost someone their
    /// inventory.
    #[test]
    fn an_absurd_stack_is_not_expanded() {
        let mut inv = inventory();
        inv.backpack
            .stackable_items
            .add(EBONY_MAIL, MAX_INSTANCED_GIFT_ITEMS + 1);
        assert!(
            promote_legacy_gift_gear(&game_items(), &repair_data(), &mut inv, Uuid::nil).is_empty()
        );
        assert_eq!(
            inv.backpack.stackable_items.count(EBONY_MAIL),
            MAX_INSTANCED_GIFT_ITEMS + 1
        );
        assert!(inv.backpack.items.is_empty());
    }

    /// CONTROL, and the one that keeps the caller honest: a healthy inventory
    /// reports nothing moved, so the inventory handler does not write on every
    /// single fetch for every single player.
    #[test]
    fn a_healthy_inventory_is_not_touched() {
        let mut inv = inventory();
        inv.backpack.stackable_items.add(MATERIAL, 3);
        assert!(
            promote_legacy_gift_gear(&game_items(), &repair_data(), &mut inv, fresh_id)
                .is_empty()
        );
        assert_eq!(inv.backpack.stackable_items.count(MATERIAL), 3);
        assert!(inv.backpack.items.is_empty());
    }

    /// A template the game data cannot name is not ours to reinterpret.
    #[test]
    fn an_unknown_template_is_left_stacked() {
        let mut inv = inventory();
        let unknown = Uuid::from_u128(0x99999999_8888_4777_8666_555555555555);
        inv.backpack.stackable_items.add(unknown, 1);
        assert!(
            promote_legacy_gift_gear(&game_items(), &repair_data(), &mut inv, fresh_id)
                .is_empty()
        );
        assert_eq!(inv.backpack.stackable_items.count(unknown), 1);
    }
}
