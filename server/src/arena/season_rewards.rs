//! Retail Arena season-end rewards.
//!
//! The base APK contains only the schema.  The values below come from captured
//! retail `globalgifts/{seasonId}` responses, correlated with each character's
//! `pvpSeasonHistory`, plus the three surviving Arena Reset screenshots.
//! Keeping the table here makes the one unobserved cell (Arena 6 gems) explicit
//! and reviewable instead of hiding it in an operator-supplied JSON catalogue.

use blades_lib::{
    economy::{GEMS, RewardChest, RewardGrant, RewardItem},
    features::repair::RepairData,
    static_data::{GiftChest, GiftDef, GiftItem},
    user_data::{Item, ItemPropertiesAll},
};
use serde_json::Value;
use uuid::Uuid;

pub const TRANSCENDENT_SOUL_GEM: Uuid = Uuid::from_u128(0xd94b_ab85_53d5_4c9c_a637_acd9_4fc6_6c98);
pub const GLORIOUS_SOUL_GEM: Uuid = Uuid::from_u128(0xbafe_6ed5_6473_4a4c_aef5_421d_3af5_c8cb);
pub const GRAND_SOUL_GEM: Uuid = Uuid::from_u128(0x68d7_941e_8c8d_47bf_9f66_becb_058f_1817);

// [season rotation][gold, silver, bronze]. Retail seasons rotate sword,
// shield, helm, then repeat; the user's screenshots independently confirm all
// three bronze templates and the 4=sword, 5=shield continuation.
const TROPHIES: [[Uuid; 3]; 3] = [
    [
        Uuid::from_u128(0x3090_29c8_4c16_49f3_8947_ff64_3540_af4c),
        Uuid::from_u128(0x64b5_37a3_83c7_4bc7_bf9b_285b_5404_5e1f),
        Uuid::from_u128(0x7487_6a6b_9e51_41d3_b44b_7b36_381e_5339),
    ],
    [
        Uuid::from_u128(0x73c9_bef2_2c2d_4a49_843c_a973_fb7c_3ee6),
        Uuid::from_u128(0xdc2c_3bd9_fb5a_4203_ad29_30c9_d172_4b75),
        Uuid::from_u128(0xe301_a86f_ca52_4f21_bfcc_6810_a45a_0b6c),
    ],
    [
        Uuid::from_u128(0xa8bf_ebbd_e525_47d2_93d2_0f9c_978a_4185),
        Uuid::from_u128(0xb755_f9a5_8017_4224_8835_7b6c_8260_4b58),
        Uuid::from_u128(0x228f_2ef9_c63e_4f26_8616_274e_3e34_56c8),
    ],
];

fn rotation_index(season_number: i32) -> usize {
    season_number.saturating_sub(1).rem_euclid(3) as usize
}

fn rank_parts(season_number: i32, tier: &str) -> Option<(Uuid, u64, bool)> {
    let (medal, gems, gold) = match tier {
        "top10" => (0, 250, true),
        "top50" => (1, 200, false),
        "top100" => (2, 150, false),
        _ => return None,
    };
    Some((TROPHIES[rotation_index(season_number)][medal], gems, gold))
}

fn arena_number(tier: &str, payload: &Value) -> Option<i32> {
    payload
        .get("arena")
        .and_then(Value::as_i64)
        .map(|v| v as i32)
        .or_else(|| {
            tier.strip_prefix("arena")?
                .split('_')
                .next()?
                .parse::<i32>()
                .ok()
        })
        .filter(|arena| (1..=6).contains(arena))
}

/// `(gems, chest rarity)` for the highest Arena reached that season.
///
/// Arenas 1..=5 are directly capture-correlated. No Arena 6 gift survives;
/// 150 is the continuation of retail's exact +25 sequence, while the Elder
/// chest rarity remains the same as Arenas 4 and 5.
pub fn arena_parts(arena: i32) -> Option<(u64, u64)> {
    match arena {
        1 => Some((25, 3)),
        2 => Some((50, 3)),
        3 => Some((75, 3)),
        4 => Some((100, 4)),
        5 => Some((125, 4)),
        6 => Some((150, 4)),
        _ => None,
    }
}

/// Exact gift lines for one frozen award, in retail display order.
fn gift_lines(
    season_number: i32,
    kind: &str,
    tier: &str,
    payload: &Value,
) -> Option<(Vec<GiftItem>, Vec<GiftChest>)> {
    match kind {
        "arena_reached" => {
            let (gems, rarity) = arena_parts(arena_number(tier, payload)?)?;
            Some((
                vec![GiftItem {
                    item_template_id: GEMS,
                    quantity: gems,
                }],
                vec![GiftChest { rarity }],
            ))
        }
        "rank" => {
            let (template, gems, _) = rank_parts(season_number, tier)?;
            Some((
                vec![
                    GiftItem {
                        item_template_id: template,
                        quantity: 1,
                    },
                    GiftItem {
                        item_template_id: GEMS,
                        quantity: gems,
                    },
                ],
                vec![],
            ))
        }
        "guild_rank" if tier == "top100" => Some((
            vec![
                GiftItem {
                    item_template_id: GEMS,
                    quantity: 250,
                },
                GiftItem {
                    item_template_id: TRANSCENDENT_SOUL_GEM,
                    quantity: 2,
                },
                GiftItem {
                    item_template_id: GLORIOUS_SOUL_GEM,
                    quantity: 4,
                },
                GiftItem {
                    item_template_id: GRAND_SOUL_GEM,
                    quantity: 6,
                },
            ],
            vec![],
        )),
        _ => None,
    }
}

pub fn reward_is_known(kind: &str, tier: &str, payload: &Value) -> bool {
    gift_lines(1, kind, tier, payload).is_some()
}

pub fn reward_for_award(
    season_number: i32,
    kind: &str,
    tier: &str,
    payload: &Value,
    repair_data: &RepairData,
) -> Option<RewardGrant> {
    let (lines, chests) = gift_lines(season_number, kind, tier, payload)?;
    let mut reward = RewardGrant::default();
    for line in lines {
        if line.item_template_id == GEMS {
            *reward.currencies.entry(GEMS).or_insert(0) += line.quantity;
        } else if TROPHIES
            .iter()
            .flatten()
            .any(|id| *id == line.item_template_id)
        {
            let gold = rank_parts(season_number, tier)
                .map(|(_, _, gold)| gold)
                .unwrap_or(false);
            for _ in 0..line.quantity {
                reward.items.push(RewardItem {
                    // All award handlers replace this before inserting it.
                    id: Uuid::nil(),
                    item: Item {
                        item_template_id: line.item_template_id,
                        grade: None,
                        tempering_level: 0,
                        durability: repair_data
                            .max_durability(line.item_template_id, 0)
                            .unwrap_or(100.0),
                        properties: ItemPropertiesAll::default(),
                        // The captured Gold Aegis gift carries the common
                        // Arcane02 enhancement; silver/bronze carry none.
                        arcane_tier: gold.then_some(2),
                    },
                });
            }
        } else {
            *reward
                .stackable_items
                .entry(line.item_template_id)
                .or_insert(0) += line.quantity;
        }
    }
    reward.chests = chests
        .into_iter()
        .map(|chest| RewardChest {
            id: None,
            tier: chest.rarity,
            level: 0,
        })
        .collect();
    Some(reward)
}

pub fn merge_reward(into: &mut RewardGrant, mut other: RewardGrant) {
    for (id, amount) in other.currencies.drain() {
        *into.currencies.entry(id).or_insert(0) += amount;
    }
    for (id, amount) in other.stackable_items.drain() {
        *into.stackable_items.entry(id).or_insert(0) += amount;
    }
    into.items.append(&mut other.items);
    into.chests.append(&mut other.chests);
    into.character_xp += other.character_xp;
    into.town_xp += other.town_xp;
}

/// The per-character `globalGiftOverride` shown by the retail Arena Reset UI.
/// Rows are deliberately emitted arena, player rank, guild rank; captures show
/// duplicate Gem lines rather than a pre-summed line, and the client animates
/// them independently.
pub fn gift_for_awards<'a>(
    season_id: Uuid,
    season_number: i32,
    awards: impl IntoIterator<Item = (&'a str, &'a str, &'a Value)>,
) -> Option<GiftDef> {
    let rows: Vec<_> = awards.into_iter().collect();
    let mut items = Vec::new();
    let mut chests = Vec::new();
    for wanted in ["arena_reached", "rank", "guild_rank"] {
        if let Some((kind, tier, payload)) = rows.iter().copied().find(|r| r.0 == wanted) {
            let (mut row_items, mut row_chests) = gift_lines(season_number, kind, tier, payload)?;
            items.append(&mut row_items);
            chests.append(&mut row_chests);
        }
    }
    (!items.is_empty() || !chests.is_empty()).then_some(GiftDef {
        global_gift_id: season_id,
        items,
        chests,
        start_time: 0,
        end_time: 0,
        claim_count_limit: 1,
        description: Some("Arena season rewards".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repair_data() -> RepairData {
        let durability: Value =
            serde_json::from_str(include_str!("../../../deploy/static/item_durability.json"))
                .unwrap();
        RepairData::from_json(&durability, &json!({}))
    }

    #[test]
    fn captured_arena_table_and_isolated_arena_six_inference() {
        assert_eq!(arena_parts(1), Some((25, 3)));
        assert_eq!(arena_parts(2), Some((50, 3)));
        assert_eq!(arena_parts(3), Some((75, 3)));
        assert_eq!(arena_parts(4), Some((100, 4)));
        assert_eq!(arena_parts(5), Some((125, 4)));
        assert_eq!(arena_parts(6), Some((150, 4)));
        assert_eq!(arena_parts(7), None);
    }

    #[test]
    fn seasons_rotate_sword_shield_helm() {
        let rd = repair_data();
        let ids: Vec<_> = (1..=4)
            .map(|season| {
                reward_for_award(season, "rank", "top100", &json!({}), &rd)
                    .unwrap()
                    .items[0]
                    .item
                    .item_template_id
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                TROPHIES[0][2],
                TROPHIES[1][2],
                TROPHIES[2][2],
                TROPHIES[0][2]
            ]
        );
    }

    #[test]
    fn rank_brackets_match_captured_gold_silver_bronze() {
        let rd = repair_data();
        for (tier, gems, medal) in [("top10", 250, 0), ("top50", 200, 1), ("top100", 150, 2)] {
            let r = reward_for_award(2, "rank", tier, &json!({}), &rd).unwrap();
            assert_eq!(r.currencies[&GEMS], gems);
            assert_eq!(r.items[0].item.item_template_id, TROPHIES[1][medal]);
            assert_eq!(r.items[0].item.arcane_tier, (tier == "top10").then_some(2));
            assert!(r.items[0].item.durability > 0.0);
        }
    }

    #[test]
    fn gift_keeps_additive_gem_rows_in_retail_order() {
        let arena = json!({"arena": 3});
        let empty = json!({});
        let gift = gift_for_awards(
            Uuid::nil(),
            2,
            [
                ("guild_rank", "top100", &empty),
                ("rank", "top50", &empty),
                ("arena_reached", "arena3_level7", &arena),
            ],
        )
        .unwrap();
        assert_eq!(gift.items[0].quantity, 75);
        assert_eq!(gift.items[1].item_template_id, TROPHIES[1][1]);
        assert_eq!(gift.items[2].quantity, 200);
        assert_eq!(gift.items[3].quantity, 250);
        assert_eq!(gift.chests[0].rarity, 3);
    }
}
