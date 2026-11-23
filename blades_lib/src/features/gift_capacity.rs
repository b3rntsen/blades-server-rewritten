//! Gifted gear and a full backpack: grant what fits, remember the rest, pay it
//! later (tracker #499, HauDrauf's rule, approved by the owner 2026-11-23).
//!
//! # Why the server has to do this
//!
//! The backpack limit is the client's, not ours: no handler checks space, and
//! retail has no "inventory full" error. A player at the limit who is handed
//! gear cannot see it or loses what they do next (tracker #256), and a backpack
//! far over the limit stops the character loading at all (387 items loaded,
//! 397 did not). A gift of twelve armour pieces to every player is exactly the
//! case that hurts, so for a gift that carries a [`GearCapRule`] the claim
//! grants only the gear that keeps the backpack at or below the rule's share of
//! its capacity, and files the rest on the character as [`DeferredGiftGear`].
//!
//! Only gear is held back. Currencies and stackables (decorations, materials,
//! soul gems) take no backpack slot and are always paid in full.
//!
//! # What "capacity" is
//!
//! The client's own sum (`PlayerActor.GetPlayerInventoryCapacity`,
//! `InventoryData` in the `common` bundle):
//!
//! ```text
//! capacity = 40                              _initialPlayerInventorySize
//!          + 10 x inventoryLevel (0..=11)    _playerInventoryLevelList._capacity
//!          + Load Bearer perk bonus          10 at rank 1, +5 a rank, 50 at rank 9
//! ```
//!
//! Tiers 1-8 are retail's (40 + 80 = 120 at the top); tiers 9-11 are the three
//! +10 tiers of tracker #469, which the shipped asset pack adds. Checked against
//! a live character: inventory level 8 with Load Bearer rank 3 is 140, the
//! number that player's client shows.
//!
//! # What counts against it
//!
//! The instanced items in the backpack (`inventory.backpack.items`): weapons,
//! armour, shields, rings and jewellery that are not equipped. Equipped gear
//! lives in the loadout and stackables are counts, not slots; neither counts.
//!
//! Both numbers are deliberately the cautious reading. A bonus the server does
//! not know of can only make the real capacity larger, so the worst a wrong
//! guess does is hold a piece back until later, never push a backpack over.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::economy::{RewardGrant, RewardItem};
use crate::features::gifts;
use crate::features::repair::RepairData;
use crate::static_data::{GiftDef, GiftItem};
use crate::user_data::{CompleteCharacter, CompleteInventory, ItemSingleProperty};

/// Backpack slots every character starts with (`_initialPlayerInventorySize`).
pub const BASE_BACKPACK_SLOTS: u64 = 40;

/// Slots each bought backpack tier adds (`_capacity` is 10 x the tier).
pub const SLOTS_PER_BACKPACK_TIER: u64 = 10;

/// The Load Bearer perk ("Increases inventory size by {0}").
pub const LOAD_BEARER_PERK: Uuid = Uuid::from_u128(0x7f0c9202_2130_4376_aa17_890c25040e7b);

/// Highest Load Bearer rank the game data holds.
pub const LOAD_BEARER_MAX_RANK: u64 = 9;

/// Slots Load Bearer adds at `rank`: 10, 15, … 50 for ranks 1-9 (the perk's
/// `bonusValue` ladder), nothing unlearned.
pub fn load_bearer_slots(rank: u64) -> u64 {
    match rank.min(LOAD_BEARER_MAX_RANK) {
        0 => 0,
        rank => 5 + 5 * rank,
    }
}

/// The character's learned Load Bearer rank (`character.abilities` is
/// `{abilityId: rank}`), 0 when it is not learned or the map is missing.
fn load_bearer_rank(abilities: &Value) -> u64 {
    abilities
        .get(LOAD_BEARER_PERK.to_string())
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// How many gear items this character's backpack holds, as their client counts it.
pub fn backpack_capacity(character: &CompleteCharacter) -> u64 {
    let tier = u64::from(character.inventory_level).min(u64::from(
        crate::features::character_ops::MAX_INVENTORY_LEVEL,
    ));
    BASE_BACKPACK_SLOTS
        + SLOTS_PER_BACKPACK_TIER * tier
        + load_bearer_slots(load_bearer_rank(&character.abilities))
}

/// How many slots the backpack uses now: its instanced items.
pub fn backpack_used(inventory: &CompleteInventory) -> u64 {
    inventory.backpack.items.0.len() as u64
}

/// A gift whose gear must leave room in the backpack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GearCapRule {
    /// The share of the backpack gifted gear may fill, in percent.
    pub fill_percent: u64,
    /// Unix seconds from which gear that did not fit is paid out.
    pub payable_from_secs: i64,
}

/// The rule a gift carries, if it carries one.
///
/// No gift does yet: this is the mechanism only, and an authored, static or
/// season gift is paid exactly as before. The December 2026 calendar registers
/// its armour gifts here.
pub fn rule_for(_gift_id: Uuid) -> Option<GearCapRule> {
    None
}

/// How many more gear items fit while the backpack stays at or below
/// `fill_percent` of its capacity. 0 when it is already there or beyond.
pub fn gear_room(
    character: &CompleteCharacter,
    inventory: &CompleteInventory,
    fill_percent: u64,
) -> u64 {
    let limit = backpack_capacity(character) * fill_percent.min(100) / 100;
    limit.saturating_sub(backpack_used(inventory))
}

/// Gear a gift owed a character and could not hand over without overfilling
/// the backpack. One entry is `quantity` identical pieces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeferredGiftGear {
    /// The gift that owed it, for the audit trail.
    pub gift_id: Uuid,
    pub item_template_id: Uuid,
    pub quantity: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arcane_tier: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enchanting: Vec<ItemSingleProperty>,
    /// Unix seconds from which this may be paid.
    pub payable_from_secs: i64,
}

/// Hold back the gear of `reward` that does not fit, and file it in `deferred`.
///
/// The first `room` items stay in the reward, in the gift's own order, so a
/// gift authored set by set hands over whole sets before a partial one.
/// Returns how many pieces were held back.
pub fn withhold_gear(
    reward: &mut RewardGrant,
    room: u64,
    gift_id: Uuid,
    payable_from_secs: i64,
    deferred: &mut Vec<DeferredGiftGear>,
) -> u64 {
    let keep = usize::try_from(room).unwrap_or(usize::MAX);
    if reward.items.len() <= keep {
        return 0;
    }
    let held: Vec<RewardItem> = reward.items.split_off(keep);
    let count = held.len() as u64;
    for piece in held {
        let same = deferred.iter_mut().find(|d| {
            d.gift_id == gift_id
                && d.item_template_id == piece.item.item_template_id
                && d.arcane_tier == piece.item.arcane_tier
                && d.enchanting == piece.item.properties.enchanting
                && d.payable_from_secs == payable_from_secs
        });
        match same {
            Some(entry) => entry.quantity += 1,
            None => deferred.push(DeferredGiftGear {
                gift_id,
                item_template_id: piece.item.item_template_id,
                quantity: 1,
                arcane_tier: piece.item.arcane_tier,
                enchanting: piece.item.properties.enchanting,
                payable_from_secs,
            }),
        }
    }
    count
}

/// Order gear set by set: the first copy of every template in the gift's own
/// order, then the second copies, and so on. A gift line of "3 x armour,
/// 3 x boots, …" is then handed over as whole sets, so a backpack with room
/// for five of twelve pieces gets one complete set and one piece, not three
/// armours and two pairs of boots.
fn order_set_by_set(items: &mut Vec<RewardItem>) {
    let mut copies: Vec<(Uuid, usize)> = Vec::new();
    let mut keyed: Vec<(usize, RewardItem)> = std::mem::take(items)
        .into_iter()
        .map(|piece| {
            let template = piece.item.item_template_id;
            let copy = match copies.iter_mut().find(|(t, _)| *t == template) {
                Some((_, n)) => {
                    *n += 1;
                    *n
                }
                None => {
                    copies.push((template, 0));
                    0
                }
            };
            (copy, piece)
        })
        .collect();
    // Stable: within one copy number the gift's own order is kept.
    keyed.sort_by_key(|(copy, _)| *copy);
    items.extend(keyed.into_iter().map(|(_, piece)| piece));
}

/// Apply a gift's [`GearCapRule`] to the reward a claim is about to grant:
/// whole sets first, then whatever still fits. Returns how many pieces were
/// held back.
pub fn cap_gift_gear(
    reward: &mut RewardGrant,
    rule: GearCapRule,
    gift_id: Uuid,
    character: &CompleteCharacter,
    inventory: &CompleteInventory,
    deferred: &mut Vec<DeferredGiftGear>,
) -> u64 {
    order_set_by_set(&mut reward.items);
    let room = gear_room(character, inventory, rule.fill_percent);
    withhold_gear(reward, room, gift_id, rule.payable_from_secs, deferred)
}

/// The fixed id of the gift that pays deferred gear out.
///
/// One id for every character: what it holds is computed per character from
/// their own [`DeferredGiftGear`], as retail computed a `globalGiftOverride`
/// per player.
pub const DEFERRED_GEAR_GIFT_ID: Uuid = Uuid::from_u128(0xdefe44ed_6ea4_4d12_9a95_499000000001);

/// The share of the backpack a deferred payout may fill: the same 95 % the
/// rule that deferred it used.
pub const DEFERRED_PAYOUT_FILL_PERCENT: u64 = 95;

/// What a deferred payout would hand over right now: the pieces that are due
/// and fit, oldest entry first. Empty when nothing is due or nothing fits.
pub fn payable_now(deferred: &[DeferredGiftGear], now: i64, room: u64) -> Vec<GiftItem> {
    let mut left = room;
    let mut out = Vec::new();
    for entry in deferred {
        if left == 0 {
            break;
        }
        if entry.payable_from_secs > now || entry.quantity == 0 {
            continue;
        }
        let quantity = entry.quantity.min(left);
        left -= quantity;
        out.push(GiftItem {
            item_template_id: entry.item_template_id,
            quantity,
            arcane_tier: entry.arcane_tier,
            enchanting: entry.enchanting.clone(),
        });
    }
    out
}

/// How many pieces are still owed, due or not.
pub fn owed(deferred: &[DeferredGiftGear]) -> u64 {
    deferred.iter().map(|d| d.quantity).sum()
}

/// The deferred-payout gift as this character would see it now, or `None`
/// when there is nothing to hand over.
pub fn payout_gift_def(
    deferred: &[DeferredGiftGear],
    character: &CompleteCharacter,
    inventory: &CompleteInventory,
    now: i64,
) -> Option<GiftDef> {
    let room = gear_room(character, inventory, DEFERRED_PAYOUT_FILL_PERCENT);
    let items = payable_now(deferred, now, room);
    if items.is_empty() {
        return None;
    }
    Some(GiftDef {
        global_gift_id: DEFERRED_GEAR_GIFT_ID,
        items,
        chests: Vec::new(),
        start_time: 0,
        end_time: 0,
        claim_count_limit: 1,
        description: Some("Gifted gear that did not fit in your backpack".to_string()),
    })
}

/// Pay out the deferred gear that is due and fits, removing it from `deferred`.
///
/// The reward's item ids are nil, as [`gifts::build_gift_reward`] leaves them:
/// the handler mints them. An empty reward means nothing was due or nothing fit,
/// and `deferred` is untouched.
pub fn pay_deferred(
    deferred: &mut Vec<DeferredGiftGear>,
    character: &CompleteCharacter,
    inventory: &CompleteInventory,
    now: i64,
    repair_data: &RepairData,
) -> RewardGrant {
    let Some(def) = payout_gift_def(deferred, character, inventory, now) else {
        return RewardGrant::default();
    };
    let mut reward = RewardGrant::default();
    for line in &def.items {
        // Built by the same function a gift claim uses, so a piece paid late is
        // the piece the gift would have granted.
        for _ in 0..line.quantity {
            reward.items.push(gifts::gear_instance(line, repair_data));
        }
        let mut left = line.quantity;
        for entry in deferred.iter_mut() {
            if left == 0 {
                break;
            }
            if entry.payable_from_secs > now
                || entry.item_template_id != line.item_template_id
                || entry.arcane_tier != line.arcane_tier
                || entry.enchanting != line.enchanting
            {
                continue;
            }
            let taken = entry.quantity.min(left);
            entry.quantity -= taken;
            left -= taken;
        }
    }
    deferred.retain(|entry| entry.quantity > 0);
    reward
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_data::{Backpack, Item, ItemPropertiesAll, Loadout, Treasury};
    use serde_json::json;

    const ARMOR: Uuid = Uuid::from_u128(0x1063c3f9_1bbf_4962_b80d_fb9832075c1c);
    const BOOTS: Uuid = Uuid::from_u128(0x2c9ac3da_b5ff_40e9_8ced_3306c8036a93);
    const GIFT: Uuid = Uuid::from_u128(0xa);
    const DEC_25: i64 = 1_798_156_800;

    fn repair() -> RepairData {
        let ladder: Vec<f64> = vec![325.0; 11];
        let levels: serde_json::Map<String, Value> = ladder
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), json!(v)))
            .collect();
        RepairData::from_json(
            &json!({ ARMOR.to_string(): levels.clone(), BOOTS.to_string(): levels }),
            &json!({}),
        )
    }

    fn character(inventory_level: u16, load_bearer: u64) -> CompleteCharacter {
        CompleteCharacter {
            inventory_level,
            abilities: if load_bearer == 0 {
                Value::Null
            } else {
                json!({ LOAD_BEARER_PERK.to_string(): load_bearer })
            },
            ..CompleteCharacter::default()
        }
    }

    fn inventory(gear: u64) -> CompleteInventory {
        let mut inv = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        };
        for n in 0..gear {
            inv.backpack.items.0.insert(
                Uuid::from_u128(0x1000 + u128::from(n)),
                Item {
                    item_template_id: BOOTS,
                    grade: None,
                    tempering_level: 0,
                    durability: 325.0,
                    properties: ItemPropertiesAll::default(),
                    arcane_tier: None,
                },
            );
        }
        // Stackables and equipped gear take no slot.
        inv.backpack.stackable_items.add(Uuid::from_u128(7), 5_000);
        inv
    }

    fn gear_reward(pieces: &[Uuid]) -> RewardGrant {
        let mut reward = RewardGrant::default();
        for (n, template) in pieces.iter().enumerate() {
            let mut piece = gifts::gear_instance(
                &GiftItem {
                    item_template_id: *template,
                    quantity: 1,
                    arcane_tier: None,
                    enchanting: vec![],
                },
                &repair(),
            );
            piece.id = Uuid::from_u128(0x9000 + n as u128);
            reward.items.push(piece);
        }
        reward
    }

    /// The client's own sum, tier by tier, and the live character that pins it:
    /// tier 8 with Load Bearer rank 3 shows 140 in game.
    #[test]
    fn capacity_is_base_plus_tiers_plus_load_bearer() {
        assert_eq!(backpack_capacity(&character(0, 0)), 40);
        assert_eq!(backpack_capacity(&character(8, 0)), 120, "retail's top");
        assert_eq!(backpack_capacity(&character(11, 0)), 150, "tiers 9-11");
        assert_eq!(backpack_capacity(&character(8, 3)), 140);
        assert_eq!(backpack_capacity(&character(11, 9)), 200);
        // A level or rank past the table clamps to the top row, as the client's
        // `GetInventoryCapacity` does.
        assert_eq!(backpack_capacity(&character(40, 0)), 150);
        assert_eq!(backpack_capacity(&character(0, 99)), 90);
        assert_eq!(
            (1..=9).map(load_bearer_slots).collect::<Vec<_>>(),
            vec![10, 15, 20, 25, 30, 35, 40, 45, 50]
        );
    }

    /// Only instanced backpack items count: not stackables, not equipped gear.
    #[test]
    fn only_backpack_gear_takes_a_slot() {
        assert_eq!(backpack_used(&inventory(0)), 0);
        assert_eq!(backpack_used(&inventory(17)), 17);
    }

    /// 95 % of 120 is 114: a backpack at 110 takes four pieces, at 111 three,
    /// at 114 or beyond none.
    #[test]
    fn room_is_what_keeps_the_backpack_at_95_percent() {
        let ch = character(8, 0);
        assert_eq!(gear_room(&ch, &inventory(0), 95), 114);
        assert_eq!(gear_room(&ch, &inventory(110), 95), 4);
        assert_eq!(gear_room(&ch, &inventory(111), 95), 3);
        assert_eq!(gear_room(&ch, &inventory(114), 95), 0);
        assert_eq!(gear_room(&ch, &inventory(400), 95), 0, "never negative");
        // The limit rounds DOWN: 95 % of 150 is 142.5, so 142.
        assert_eq!(gear_room(&character(11, 0), &inventory(0), 95), 142);
    }

    /// With room for everything the reward is untouched and nothing is filed.
    #[test]
    fn a_gift_that_fits_is_paid_whole() {
        let mut reward = gear_reward(&[ARMOR, BOOTS, ARMOR, BOOTS]);
        let before = reward.clone();
        let mut deferred = Vec::new();
        let held = cap_gift_gear(
            &mut reward,
            GearCapRule {
                fill_percent: 95,
                payable_from_secs: DEC_25,
            },
            GIFT,
            &character(8, 0),
            &inventory(110),
            &mut deferred,
        );
        assert_eq!(held, 0);
        assert_eq!(reward, before);
        assert!(deferred.is_empty());
    }

    /// THE RULE. A backpack with room for three of four pieces is handed the
    /// first three and owed the fourth; the backpack ends at exactly 95 %.
    #[test]
    fn gear_beyond_95_percent_is_held_back_and_remembered() {
        let mut reward = gear_reward(&[ARMOR, BOOTS, ARMOR, BOOTS]);
        reward.currencies.insert(crate::economy::GEMS, 600);
        reward.stackable_items.insert(Uuid::from_u128(7), 3);
        let mut deferred = Vec::new();
        let held = cap_gift_gear(
            &mut reward,
            GearCapRule {
                fill_percent: 95,
                payable_from_secs: DEC_25,
            },
            GIFT,
            &character(8, 0),
            &inventory(111),
            &mut deferred,
        );
        assert_eq!(held, 1);
        assert_eq!(
            reward
                .items
                .iter()
                .map(|p| p.item.item_template_id)
                .collect::<Vec<_>>(),
            vec![ARMOR, BOOTS, ARMOR],
            "the gift's own order"
        );
        assert_eq!(111 + reward.items.len() as u64, 114);
        // Gems and decorations are never held back.
        assert_eq!(reward.currencies[&crate::economy::GEMS], 600);
        assert_eq!(reward.stackable_items[&Uuid::from_u128(7)], 3);
        assert_eq!(
            deferred,
            vec![DeferredGiftGear {
                gift_id: GIFT,
                item_template_id: BOOTS,
                quantity: 1,
                arcane_tier: None,
                enchanting: vec![],
                payable_from_secs: DEC_25,
            }]
        );
    }

    /// Three sets authored as "3 x armour, 3 x boots" reach a backpack with room
    /// for three pieces as one whole set and one more piece — never three of
    /// the same.
    #[test]
    fn whole_sets_are_handed_over_before_a_partial_one() {
        let mut reward = gear_reward(&[ARMOR, ARMOR, ARMOR, BOOTS, BOOTS, BOOTS]);
        let mut deferred = Vec::new();
        let held = cap_gift_gear(
            &mut reward,
            GearCapRule {
                fill_percent: 95,
                payable_from_secs: DEC_25,
            },
            GIFT,
            &character(8, 0),
            &inventory(111),
            &mut deferred,
        );
        assert_eq!(held, 3);
        assert_eq!(
            reward
                .items
                .iter()
                .map(|p| p.item.item_template_id)
                .collect::<Vec<_>>(),
            vec![ARMOR, BOOTS, ARMOR]
        );
        let ids: std::collections::BTreeSet<Uuid> = reward.items.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), 3, "every piece keeps its own id");
        assert_eq!(
            deferred
                .iter()
                .map(|d| (d.item_template_id, d.quantity))
                .collect::<Vec<_>>(),
            vec![(BOOTS, 2), (ARMOR, 1)]
        );
    }

    /// A full backpack is handed no gear at all and owed all of it; identical
    /// pieces share one entry.
    #[test]
    fn a_full_backpack_is_owed_everything() {
        let mut reward = gear_reward(&[ARMOR, BOOTS, ARMOR, BOOTS, ARMOR, BOOTS]);
        let mut deferred = Vec::new();
        let held = withhold_gear(&mut reward, 0, GIFT, DEC_25, &mut deferred);
        assert_eq!(held, 6);
        assert!(reward.items.is_empty());
        assert_eq!(owed(&deferred), 6);
        assert_eq!(deferred.len(), 2);
        assert!(deferred.iter().all(|d| d.quantity == 3));
    }

    fn owing(armor: u64, boots: u64) -> Vec<DeferredGiftGear> {
        [(ARMOR, armor), (BOOTS, boots)]
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .map(|(item_template_id, quantity)| DeferredGiftGear {
                gift_id: GIFT,
                item_template_id,
                quantity,
                arcane_tier: None,
                enchanting: vec![],
                payable_from_secs: DEC_25,
            })
            .collect()
    }

    /// Nothing is paid before the day it becomes payable, however much room
    /// there is — and the first second of that day pays.
    #[test]
    fn nothing_is_paid_before_it_is_due() {
        let ch = character(8, 0);
        let inv = inventory(0);
        let mut deferred = owing(3, 3);
        let early = pay_deferred(&mut deferred, &ch, &inv, DEC_25 - 1, &repair());
        assert!(early.is_empty());
        assert_eq!(owed(&deferred), 6, "still owed");
        assert!(payout_gift_def(&deferred, &ch, &inv, DEC_25 - 1).is_none());

        let due = pay_deferred(&mut deferred, &ch, &inv, DEC_25, &repair());
        assert_eq!(due.items.len(), 6);
        assert!(deferred.is_empty(), "paid in full, nothing left on file");
    }

    /// A payout obeys the same 95 %: it pays what fits and keeps the rest for
    /// the next time there is room.
    #[test]
    fn a_payout_pays_what_fits_and_keeps_the_rest() {
        let ch = character(8, 0);
        let mut deferred = owing(3, 3);

        // Room for four: three armours and one pair of boots.
        let first = pay_deferred(&mut deferred, &ch, &inventory(110), DEC_25, &repair());
        assert_eq!(first.items.len(), 4);
        assert_eq!(owed(&deferred), 2);
        assert_eq!(deferred, owing(0, 2));

        // Still full: nothing to hand over, nothing lost.
        let none = pay_deferred(&mut deferred, &ch, &inventory(114), DEC_25 + 60, &repair());
        assert!(none.is_empty());
        assert_eq!(owed(&deferred), 2);

        // Room again: the rest, and the file is closed.
        let last = pay_deferred(&mut deferred, &ch, &inventory(50), DEC_25 + 120, &repair());
        assert_eq!(last.items.len(), 2);
        assert!(deferred.is_empty());
    }

    /// A piece paid late is a real instance: full durability, plain properties,
    /// and never a stackable.
    #[test]
    fn a_deferred_piece_is_minted_like_a_gifted_one() {
        let mut deferred = owing(1, 0);
        let reward = pay_deferred(
            &mut deferred,
            &character(8, 0),
            &inventory(0),
            DEC_25,
            &repair(),
        );
        assert!(reward.stackable_items.is_empty() && reward.currencies.is_empty());
        let piece = &reward.items[0];
        assert_eq!(piece.item.item_template_id, ARMOR);
        assert_eq!(piece.item.durability, 325.0);
        assert_eq!(piece.item.tempering_level, 0);
        assert_eq!(piece.item.properties, ItemPropertiesAll::default());
        assert_eq!(piece.id, Uuid::nil(), "the handler mints the id");
    }

    /// The wire shape of a piece paid late, against an armour instance a player
    /// already holds in production (the same template, tempered and enchanted):
    /// the same keys, nothing more and nothing missing.
    #[test]
    fn a_deferred_piece_has_the_shape_of_a_held_one() {
        let held: Value = serde_json::from_str(
            r#"{"id": "fec565d0-13d7-49c1-9bf6-6f412ad2cec6", "durability": 675.0,
                "properties": {"GRADING": [], "ENCHANTING": [
                    {"id": "b4de8d1b-d8a1-4575-8517-c1e5800a7525", "tier": 10},
                    {"id": "334f4ed9-5402-4a5a-981c-44772c799b88", "tier": 10}]},
                "itemTemplateId": "1063c3f9-1bbf-4962-b80d-fb9832075c1c",
                "temperingLevel": 10}"#,
        )
        .unwrap();
        let mut deferred = owing(1, 0);
        let reward = pay_deferred(
            &mut deferred,
            &character(8, 0),
            &inventory(0),
            DEC_25,
            &repair(),
        );
        let paid = serde_json::to_value(&reward.items[0]).unwrap();
        let keys = |v: &Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&paid), keys(&held));
        assert_eq!(keys(&paid["properties"]), keys(&held["properties"]));
        assert_eq!(paid["itemTemplateId"], held["itemTemplateId"]);
        assert_eq!(paid["properties"]["ENCHANTING"], json!([]));
        assert_eq!(paid["temperingLevel"], json!(0));
        assert_eq!(paid["durability"], json!(325.0));
    }

    /// The payout gift shows exactly what a claim would hand over.
    #[test]
    fn the_payout_gift_lists_what_a_claim_would_pay() {
        let def = payout_gift_def(&owing(3, 3), &character(8, 0), &inventory(110), DEC_25)
            .expect("something is due and fits");
        assert_eq!(def.global_gift_id, DEFERRED_GEAR_GIFT_ID);
        assert_eq!(
            def.items
                .iter()
                .map(|i| (i.item_template_id, i.quantity))
                .collect::<Vec<_>>(),
            vec![(ARMOR, 3), (BOOTS, 1)]
        );
        assert!(payout_gift_def(&owing(3, 3), &character(8, 0), &inventory(114), DEC_25).is_none());
        assert!(payout_gift_def(&[], &character(8, 0), &inventory(0), DEC_25).is_none());
    }

    /// Rows written before the field existed load with nothing owed, and a row
    /// that owes nothing does not carry the key.
    #[test]
    fn server_state_round_trips_the_file() {
        use crate::server_state::ServerState;
        let old: ServerState = serde_json::from_str("{}").unwrap();
        assert!(old.deferred_gift_gear.is_empty());
        assert!(
            !serde_json::to_string(&old)
                .unwrap()
                .contains("deferredGiftGear")
        );
        let mut st = ServerState::default();
        st.deferred_gift_gear = owing(2, 0);
        let back: ServerState = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back.deferred_gift_gear, owing(2, 0));
    }

    /// No gift carries the rule yet: merging this changes no gift.
    #[test]
    fn no_gift_is_capped_until_one_registers() {
        assert_eq!(rule_for(DEFERRED_GEAR_GIFT_ID), None);
        assert_eq!(rule_for(Uuid::nil()), None);
    }
}
