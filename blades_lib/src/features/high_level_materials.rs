//! HauDrauf's material rules for character levels 45-100 (tracker #438).
//!
//! OWNER-APPROVED DEVIATION FROM RETAIL, #438, 2026-10-27, under the owner's
//! standing rule that HauDrauf's reversible common-good economy rules ship without
//! a separate decision. HauDrauf (German, translated): "Level 45-100. The
//! ingredient stacks all get a size of 30: events, quests, Abyss, containers.
//! Wooden chests always hold 2 stacks of them. That means the 7 ingredients, salts
//! etc. The tempering materials go back to one, except in chests, where there are
//! 1-3, Opal etc. The ingots are fine; only Dragon Bone and Dragon Scale go back
//! to 1-7, as in the original." His answers (msg 3018): the 30 in chests is for
//! Wooden chests ("in Holztruhen ja"; read narrowly, every other chest keeps its
//! sizes), and Seeds, Dye and Pelt may make way for Ectoplasm, which retail paid
//! "about like Moon Sugar".
//!
//! Every number lives in this one module, so the whole rule set can be reverted by
//! setting [`FROM_LEVEL`] above the level cap (or removing the calls to
//! [`pay_rewards`], [`pay_chest`] and [`pay_dungeon`]).
//!
//! It is applied on top of #388's crafting-material stacks (5, 10 or 20,
//! [`crate::features::beginner_economy::MATERIAL_STACK_SIZES`]) wherever #388
//! sizes them -- treasury, daily, gift and store chests; story-quest and job
//! completion rewards; enemy corpses and containers in quest, job, event and Abyss
//! dungeons -- and on the Abyss `/end` package and rung rewards. An event's fixed
//! rewards (milestones, final reward) are untouched, as are daily rewards' own
//! stacks. Below [`FROM_LEVEL`] nothing moves.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use uuid::Uuid;

use crate::features::store_bundles::mix;
use crate::user_data::{DungeonGeneratedData, LootTableResult};

/// The first character level the #438 rules apply to; every level from here to
/// 100 is treated the same ("Stufe 45 - 100").
pub const FROM_LEVEL: i64 = 45;

/// Whether the #438 rules apply to a character of `level`.
pub fn applies(level: i64) -> bool {
    level >= FROM_LEVEL
}

// ---------------------------------------------------------------------------
// 1. The seven effect ingredients: stacks of 30; a Wooden chest holds two
// ---------------------------------------------------------------------------

/// The seven ingredients HauDrauf means ("die 7 Zutaten, Salze usw."): the APK's
/// "Alchemy & Enchanting Material" tag (`Item.Tag.IngredientEffect`), the Sigil
/// shop's `EffectIngredients` group -- Frost, Fire and Void Salts, Glow Dust, Imp
/// Stool, Honeycomb and Deathbell ([`crate::util::dungeon::EFFECT_INGREDIENTS`]).
pub const EFFECT_INGREDIENTS: [u128; 7] = crate::util::dungeon::EFFECT_INGREDIENTS;

/// The size of every effect-ingredient stack from [`FROM_LEVEL`] (#438), in
/// every source the rules cover, chests included.
pub const EFFECT_INGREDIENT_STACK: u64 = 30;

/// How many effect-ingredient stacks a Wooden chest (tier 1) holds from
/// [`FROM_LEVEL`] (#438): "In Holztruhen gibt es immer 2 Stapel davon".
pub const WOODEN_CHEST_EFFECT_INGREDIENT_STACKS: usize = 2;

// ---------------------------------------------------------------------------
// 2. Tempering materials: 1, or 1-3 in a chest
// ---------------------------------------------------------------------------

/// The tempering materials ("Härtungsmaterialien ... Opal usw."): every item the
/// APK tags "Tempering & Jewelry Material" (`Item.Tag.IngredientJewelryAndTempering`:
/// Topaz to Faerite) or "Tempering Material" only (Pyrine, Arentine, Firebreath
/// Opal). Pearl is a jewellery-only material and Brass an ingot (smithing tag), so
/// neither is here.
pub const TEMPERING_MATERIALS: [Uuid; 12] = [
    Uuid::from_u128(0x3ca59113_a093_4cc8_8389_0f578ad94851), // Topaz
    Uuid::from_u128(0x9df9233f_e7b9_47e8_bc75_f37707917759), // Garnet
    Uuid::from_u128(0x3ec6cf6f_d90e_4b76_bb7f_82da251ab5e5), // Amethyst
    Uuid::from_u128(0xcf4b1b42_a736_4aa1_99b8_baca9f9c2276), // Ruby
    Uuid::from_u128(0x014606cd_8898_4c1d_8029_63220a179c47), // Sapphire
    Uuid::from_u128(0x4d1231be_d3fa_4282_81b2_0f7bed4aabff), // Emerald
    Uuid::from_u128(0x16e102fb_b1c0_42de_8106_0aa27e77f7f0), // Diamond
    Uuid::from_u128(0xab97efe1_bae9_4d16_8fd9_e05bad9ecb95), // Atronite
    Uuid::from_u128(0x70f2c013_813e_4f63_8b29_7a54a13b5e58), // Faerite
    Uuid::from_u128(0x32ef7be4_88aa_4a0b_a198_fa88d484a41d), // Pyrine
    Uuid::from_u128(0xc1abc963_c03c_4cd9_84d7_433ed8ab3877), // Arentine
    Uuid::from_u128(0x91c40a5d_36b8_4ed9_9c07_209b52ebc732), // Firebreath Opal
];

/// A tempering-material stack outside a chest from [`FROM_LEVEL`] (#438).
pub const TEMPERING_STACK: u64 = 1;

/// A tempering-material stack in a chest (any tier, treasury or store) from
/// [`FROM_LEVEL`] (#438), uniformly.
pub const CHEST_TEMPERING_STACK: RangeInclusive<u64> = 1..=3;

// ---------------------------------------------------------------------------
// 3. Dragon Bone and Dragon Scale: 1-7, as in retail
// ---------------------------------------------------------------------------

/// Dragon Bones and Dragon Scales: the two ingots that go back to retail's sizes.
pub const DRAGON_MATERIALS: [Uuid; 2] = [
    Uuid::from_u128(0xd523932f_8c7f_4192_9112_5dbd60883c2b), // Dragon Bones
    Uuid::from_u128(0x34ddfe0e_5119_4e52_8eef_a77b6bc810a7), // Dragon Scales
];

/// A Dragon Bone or Dragon Scale stack from [`FROM_LEVEL`] (#438), uniformly, in
/// every source the rules cover.
pub const DRAGON_STACK: RangeInclusive<u64> = 1..=7;

// ---------------------------------------------------------------------------
// 4. Seeds, Dye and Pelt make way for Ectoplasm
// ---------------------------------------------------------------------------

/// Seeds, Dye and Pelt: from [`FROM_LEVEL`] they are paid nowhere the rules
/// cover. HauDrauf (#438, msg 3018): none of the three matters at the top levels,
/// "wenn du kein Ektoplasma findest, kannst du es z. B. mit diesen Sachen ersetzen.
/// Ektoplasma gab es ungefähr wie Mondzucker."
pub const SEEDS_DYE_PELT: [Uuid; 3] = [
    Uuid::from_u128(0xb94c2028_dba7_44bd_b6f9_6f85ab0195b6), // Seeds
    Uuid::from_u128(0x6bd960ca_9af3_4b33_9ec0_5c06d50ad43e), // Dye
    Uuid::from_u128(0x51f7612f_a797_4417_8a92_493b0aae7f45), // Pelt
];

/// Ectoplasm (an Alchemy Material, not one of the seven effect ingredients, so it
/// is paid at Moon Sugar's size: #388's 5, 10 or 20).
pub const ECTOPLASM: Uuid = Uuid::from_u128(0xdba6adde_b1f3_4146_8155_91426652495a);

/// In how many Wooden-chest openings per 10,000 Ectoplasm is paid from
/// [`FROM_LEVEL`]: Moon Sugar's own rate there ("ungefähr wie Mondzucker").
///
/// Measured on our generators for a level-100 opener
/// (`report_438::print_moon_sugar_and_ectoplasm_rates`, 10,000 openings each):
/// Moon Sugar in 1,645 / 1,656 / 1,639 Wooden chests minted at level 84 / 90 / 100
/// (894 at 78). Among the sources the rules cover, Wooden chests are the only one
/// paying Moon Sugar at 45-100: corpses (0 of 3,108 stacks), containers (0 of
/// 12,165), the Abyss (0 of 13,599: corpses, containers, `/end`, rungs) and Silver
/// to Legendary and store chests (0 of 14,014) pay none, so no Ectoplasm is added
/// there and Seeds, Dye and Pelt simply stop. Story-quest rewards already pay
/// Ectoplasm in two quests to Moon Sugar's one.
pub const WOODEN_CHEST_ECTOPLASM_PER_10K: u64 = 1_650;

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

/// Whether `id` is a stack the #438 rules size, remove or add: an effect
/// ingredient, a tempering material, Dragon Bone or Dragon Scale, Seeds, Dye, Pelt
/// or Ectoplasm.
pub fn names(id: &Uuid) -> bool {
    is_effect_ingredient(id)
        || TEMPERING_MATERIALS.contains(id)
        || DRAGON_MATERIALS.contains(id)
        || SEEDS_DYE_PELT.contains(id)
        || *id == ECTOPLASM
}

fn is_effect_ingredient(id: &Uuid) -> bool {
    EFFECT_INGREDIENTS.contains(&id.as_u128())
}

fn draw(range: &RangeInclusive<u64>, seed: u64) -> u64 {
    range.start() + seed % (range.end() - range.start() + 1)
}

/// Resize every stack the rules name and take out Seeds, Dye and Pelt; `chest`
/// picks the tempering size, `effect_30` whether effect ingredients become 30.
fn resize(stacks: &mut HashMap<Uuid, u64>, chest: bool, effect_30: bool, seed: u64) {
    stacks.retain(|id, _| !SEEDS_DYE_PELT.contains(id));
    for (id, qty) in stacks.iter_mut() {
        if *qty == 0 {
            continue;
        }
        let item =
            mix(seed ^ (id.as_u128() as u64) ^ ((id.as_u128() >> 64) as u64).rotate_left(23));
        if is_effect_ingredient(id) {
            if effect_30 {
                *qty = EFFECT_INGREDIENT_STACK;
            }
        } else if TEMPERING_MATERIALS.contains(id) {
            *qty = if chest {
                draw(&CHEST_TEMPERING_STACK, item)
            } else {
                TEMPERING_STACK
            };
        } else if DRAGON_MATERIALS.contains(id) {
            *qty = draw(&DRAGON_STACK, mix(item ^ 0x0438_D2A6));
        }
    }
}

/// A non-chest reward's stacks (a story-quest or job completion reward, the Abyss
/// `/end` package or a rung) for a character of `level`.
pub fn pay_rewards(stacks: &mut HashMap<Uuid, u64>, level: i64, seed: u64) {
    if applies(level) {
        resize(stacks, false, true, seed);
    }
}

/// A chest's stacks (treasury, daily, gift or store chest of `tier`) for an opener
/// of `level`: tempering materials 1-3, dragon 1-7, no Seeds, Dye or Pelt. A
/// Wooden chest also holds exactly [`WOODEN_CHEST_EFFECT_INGREDIENT_STACKS`]
/// effect-ingredient stacks of [`EFFECT_INGREDIENT_STACK`] -- the ones it rolled
/// (lowest ids first), topped up with others drawn uniformly from the seven -- and
/// Ectoplasm in [`WOODEN_CHEST_ECTOPLASM_PER_10K`] openings. Every other chest keeps
/// its effect ingredients' sizes (HauDrauf, msg 3018: 30 "in Holztruhen ja").
pub fn pay_chest(stacks: &mut HashMap<Uuid, u64>, tier: u64, level: i64, seed: u64) {
    if !applies(level) {
        return;
    }
    if tier == 1 {
        let mut held: Vec<Uuid> = stacks
            .iter()
            .filter(|(id, q)| **q > 0 && is_effect_ingredient(id))
            .map(|(id, _)| *id)
            .collect();
        held.sort_unstable();
        for extra in held.iter().skip(WOODEN_CHEST_EFFECT_INGREDIENT_STACKS) {
            stacks.remove(extra);
        }
        held.truncate(WOODEN_CHEST_EFFECT_INGREDIENT_STACKS);
        let mut k = 0u64;
        while held.len() < WOODEN_CHEST_EFFECT_INGREDIENT_STACKS {
            let missing: Vec<u128> = EFFECT_INGREDIENTS
                .iter()
                .copied()
                .filter(|id| !held.contains(&Uuid::from_u128(*id)))
                .collect();
            let pick = Uuid::from_u128(
                missing
                    [(mix(seed ^ 0x0438_3E5E ^ k.rotate_left(40)) % missing.len() as u64) as usize],
            );
            stacks.insert(pick, EFFECT_INGREDIENT_STACK);
            held.push(pick);
            k += 1;
        }
        if mix(seed ^ 0x0438_EC70_91A5) % 10_000 < WOODEN_CHEST_ECTOPLASM_PER_10K {
            stacks.entry(ECTOPLASM).or_insert_with(|| {
                crate::features::beginner_economy::material_stack(seed, &ECTOPLASM)
            });
        }
    }
    resize(stacks, true, tier == 1, seed);
}

/// A seed for one loot result: its place in the dungeon and its currencies (which
/// the rules never move), so the pass is idempotent -- a re-run, or a regenerated
/// dungeon merged over a stored one, pays the same -- and still varies run to run.
fn result_seed(place: u64, loot: &LootTableResult) -> u64 {
    let mut currencies: Vec<(&Uuid, &u64)> = loot.currencies.iter().collect();
    currencies.sort_unstable();
    currencies
        .into_iter()
        .fold(mix(place ^ 0x0438_0000_D00D), |h, (id, n)| {
            mix(h ^ (id.as_u128() as u64) ^ n.rotate_left(17))
        })
}

/// Every enemy corpse's and container's loot in a generated dungeon, for a
/// character of `level` (quest, job, event or Abyss dungeon). Chests are rolled when
/// opened and are paid by [`pay_chest`].
pub fn pay_dungeon(data: &mut DungeonGeneratedData, level: i64) {
    if !applies(level) {
        return;
    }
    for (group, spawners) in &mut data.enemy_generated_data {
        for (a, enemies) in spawners.iter_mut().enumerate() {
            for (b, enemy) in enemies.iter_mut().enumerate() {
                let corpse = mix(group.as_u128() as u64 ^ ((a as u64) << 32) ^ b as u64);
                let seed = result_seed(corpse, &enemy.spawn_group_loot);
                resize(
                    &mut enemy.spawn_group_loot.stackable_items,
                    false,
                    true,
                    seed,
                );
                for (table, loot) in &mut enemy.loot_table_loot {
                    let seed = result_seed(corpse ^ table.as_u128() as u64, loot);
                    resize(&mut loot.stackable_items, false, true, seed);
                }
            }
        }
    }
    for (group, pile) in &mut data.item_generated_data {
        for (a, result) in pile.iter_mut().enumerate() {
            for (table, loot) in &mut result.loot_table_loot {
                let place =
                    group.as_u128() as u64 ^ (a as u64).rotate_left(48) ^ table.as_u128() as u64;
                let seed = result_seed(place, loot);
                resize(&mut loot.stackable_items, false, true, seed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROST_SALTS: Uuid = Uuid::from_u128(0x05b4dd6b_796f_4088_a772_0e33ea3db976);
    const EBONY: Uuid = Uuid::from_u128(0x75112030_b248_49b0_9c70_0da8dea150d1);
    const OPAL: Uuid = TEMPERING_MATERIALS[11];
    const DRAGON_BONES: Uuid = DRAGON_MATERIALS[0];
    const LUMBER: Uuid = Uuid::from_u128(0xe7193116_d761_479b_8a20_5633737977f5);

    fn sample() -> HashMap<Uuid, u64> {
        HashMap::from([
            (FROST_SALTS, 10),
            (EBONY, 20),
            (OPAL, 5),
            (DRAGON_BONES, 20),
            (LUMBER, 9),
        ])
    }

    #[test]
    fn seeds_dye_and_pelt_make_way_for_ectoplasm_in_wooden_chests() {
        let mut ecto = 0u64;
        for seed in 0..20_000u64 {
            for tier in 1..=5u64 {
                let mut s = sample();
                s.extend(SEEDS_DYE_PELT.map(|id| (id, 10)));
                if tier == 1 {
                    pay_chest(&mut s, tier, 45, seed);
                    if let Some(q) = s.get(&ECTOPLASM) {
                        assert!([5, 10, 20].contains(q), "Moon Sugar's size, not {q}");
                        ecto += 1;
                    }
                } else if tier == 2 {
                    pay_rewards(&mut s, 100, seed);
                    assert!(!s.contains_key(&ECTOPLASM));
                } else {
                    pay_chest(&mut s, tier, 100, seed);
                    assert!(!s.contains_key(&ECTOPLASM), "tier {tier}");
                }
                assert!(SEEDS_DYE_PELT.iter().all(|id| !s.contains_key(id)));
            }
            let mut low = HashMap::from([(SEEDS_DYE_PELT[0], 10)]);
            pay_chest(&mut low, 1, 44, seed);
            assert_eq!(
                low,
                HashMap::from([(SEEDS_DYE_PELT[0], 10)]),
                "44 untouched"
            );
        }
        let want = WOODEN_CHEST_ECTOPLASM_PER_10K * 2;
        assert!(
            ecto.abs_diff(want) * 100 <= want * 5,
            "{ecto} of 20,000 vs {want}"
        );
    }

    #[test]
    fn the_sets_are_the_apk_tags() {
        assert_eq!(EFFECT_INGREDIENTS.len(), 7);
        for id in TEMPERING_MATERIALS.iter().chain(&DRAGON_MATERIALS) {
            assert!(!EFFECT_INGREDIENTS.contains(&id.as_u128()));
            assert!(
                crate::features::beginner_economy::is_crafting_material(id),
                "{id}"
            );
        }
    }

    #[test]
    fn level_44_is_untouched() {
        let mut s = sample();
        pay_rewards(&mut s, 44, 1);
        assert_eq!(s, sample());
        pay_chest(&mut s, 1, 44, 1);
        assert_eq!(s, sample());
    }

    #[test]
    fn rewards_pay_30_1_and_1_to_7_from_45_to_100() {
        for level in [45, 60, 100] {
            let mut seen = std::collections::BTreeSet::new();
            for seed in 0..2_000u64 {
                let mut s = sample();
                pay_rewards(&mut s, level, seed);
                assert_eq!(
                    (s[&FROST_SALTS], s[&EBONY], s[&OPAL], s[&LUMBER]),
                    (30, 20, 1, 9)
                );
                assert!(DRAGON_STACK.contains(&s[&DRAGON_BONES]));
                seen.insert(s[&DRAGON_BONES]);
            }
            assert_eq!(seen.len(), 7, "L{level}: every size 1-7 occurs");
        }
    }

    #[test]
    fn chests_pay_tempering_1_to_3_and_wooden_holds_two_ingredient_stacks() {
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..2_000u64 {
            for tier in 1..=5u64 {
                let mut s = sample();
                pay_chest(&mut s, tier, 70, seed);
                assert!(CHEST_TEMPERING_STACK.contains(&s[&OPAL]));
                seen.insert(s[&OPAL]);
                let effect: Vec<u64> = s
                    .iter()
                    .filter(|(id, _)| is_effect_ingredient(id))
                    .map(|(_, q)| *q)
                    .collect();
                if tier == 1 {
                    assert!(effect.iter().all(|q| *q == 30));
                    assert_eq!(effect.len(), 2, "seed {seed}");
                    assert!(s.contains_key(&FROST_SALTS), "the rolled stack is kept");
                } else {
                    // Only a Wooden chest's are 30 (msg 3018); the rest keep theirs.
                    assert_eq!(effect, vec![10]);
                }
            }
            // A Wooden chest that rolled none still holds two.
            let mut none = HashMap::from([(EBONY, 5)]);
            pay_chest(&mut none, 1, 45, seed);
            assert_eq!(
                none.iter()
                    .filter(|(id, q)| is_effect_ingredient(id) && **q == 30)
                    .count(),
                2
            );
        }
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), vec![1, 2, 3]);
    }
}
