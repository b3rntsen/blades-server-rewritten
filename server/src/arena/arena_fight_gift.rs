//! The arena weapon gift — an **owner-approved deviation from retail** (tracker
//! #390/#391, approved by the owner 2026-10-16, proposed by HauDrauf to bring
//! players to the arena).
//!
//! After every [`FIGHTS_PER_WEAPON_GIFT`]th finished arena fight, won or lost, the
//! player is gifted one of the nine weapon types of the month's material, chosen
//! uniformly, as a BLANK Arcane II weapon ([`WEAPON_GIFT_ARCANE_TIER`], no
//! enchantments).
//!
//! The material follows the calendar month, UTC (owner-approved #391, decision
//! d23, 2026-10-25): Stalhrim through October 2026 (#539), then Amber, Ancient
//! Nordic, Chaurus, Dark Seducer, Forsworn, Golden Saint, Madness, Nordic Steel
//! and Stalhrim from November 2026, repeating every nine months
//! ([`MONTHLY_WEAPON_SETS`], [`material_index_for_month`]).
//!
//! HauDrauf's "zwei garantierte Verzauberungen" is the client's label for Arcane
//! tier 2 — "guaranteed exactly 2 bonus enchantments when enchanted", the phrase
//! he used for the Sigil Arcane02 necklaces in #386, which the shop sells blank.
//! So the weapon arrives exactly as retail's Sigil Arcane02 weapon grants did:
//! `{itemTemplateId, temperingLevel: 0, durability: <max>, arcaneTier: 2}` with no
//! ENCHANTING and no grade. The player enchants it, and the enchant
//! (`craft.rs::roll_enchant`) then adds the primary plus exactly two bonus
//! enchantments, because `arcaneTierCountOdds["2"]` is `[0, 0, 1]`.
//!
//! # The count
//!
//! Per ACCOUNT (`users.id`), never per character or alt: a user's alts share one
//! counter, so switching alts cannot farm it. AI fights count — the owner chose
//! that over a human-only count. A "finished" fight is the rule the Gems use
//! ([`super::arena_bonus_rewards::MatchFinish`]): the side that conceded and a
//! voided no-show do not count. The counter starts at zero at deploy.
//!
//! # The weapon
//!
//! "Divine Stalhrim" is the APK's Dragonbone-strength Stalhrim, and every Divine
//! material likewise: every one is material tier 10 at required level 45 with
//! exactly its Dragonbone twin's durability and weapon type ("as strong as those
//! made of dragon bones"). Amber and Madness have no Divine weapons; their own
//! weapons already are tier 10 at level 45 with the twin's stats, so those are
//! paid. None of the gift's templates is in the Legendary loot pool (their
//! `lootWeight` is 0).
//! Should the gift ever carry pre-rolled enchantments
//! ([`WEAPON_GIFT_ENCHANTMENTS`] > 0), they are rolled on the twin's enchanting
//! tables by the Legendary-gear generator
//! ([`blades_lib::features::legendary_gear`]): a recipe primary allowed on the
//! weapon, then distinct secondaries, each uniformly.
//!
//! # Delivery
//!
//! Behind [`weapon_on_result_card`] (`ARENA_WEAPON_ON_RESULT_CARD`, off until a client check). When OFF, every
//! milestone is a claimable `globalgifts` gift as described in the last paragraph
//! below. When ON:
//!
//! **On the match-end card, straight into the backpack** (HauDrauf, #401: "so
//! people see it's connected to the fights"). The engine knows the account's
//! pre-match count from the loadout ([`account_fights`], read at queue time), so
//! on the milestone fight it mints the weapon instance ([`weapon_instance`]) and
//! puts it in op49's `reward.items` plus the `inventory.backpack.items` change
//! set with a bumped `backpackVersion` — the same `reward` block the client parses
//! for every REST reward (`BaseRewardData`; chest `collect` answers carry
//! `reward.items` the same way). The persistence writer then grants exactly that
//! instance and records the milestone row already granted, so it is never
//! advertised as a gift.
//!
//! The claimable `globalgifts` path stays for two cases: rows minted before this
//! change (still pending, still claimable — `global_gift.rs`), and a milestone the
//! card could not show (the pre-match count was unknown, or a concurrent fight on
//! the same account moved it). Each such gift is its own row in
//! `arena_fight_gifts` whose id IS the `globalGiftId`, advertised in
//! `/announcements` under the edge's gift-bearing `gift/` namespace and claimed
//! once by the alt that fought the milestone fight.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::Result;
use blades_lib::economy::RewardItem;
use blades_lib::static_data::{GiftDef, GiftItem};
use blades_lib::user_data::{Item, ItemPropertiesAll};
use diesel::OptionalExtension;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use uuid::Uuid;

/// Whether the milestone weapon is shown on the fight's own op49 result card and
/// granted straight into the backpack at match end (#401), instead of arriving
/// as a claimable `globalgifts` gift (#539).
///
/// OFF until a client check: retail never put items on op49 — not one of 31
/// reassembled retail cards carries `reward.items` — so whether the result screen
/// renders (or survives) one is unproven. Enable after the 25th-fight card has
/// been seen working on a real client (#401). While off, the card never carries
/// the weapon and every milestone is minted as a claimable gift, exactly as #539.
///
/// Read once from `ARENA_WEAPON_ON_RESULT_CARD` (`1`/`true`/`on`/`yes`), like
/// `arena_ladder::promotion_items_on_card`, so the owner can turn it on with an
/// env line instead of a rebuild. Unset = off.
pub fn weapon_on_result_card() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("ARENA_WEAPON_ON_RESULT_CARD")
            .map(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(false)
    })
}

/// Finished arena fights per weapon gift, counted per account. Owner-approved
/// deviation (#390/#391, 2026-10-16).
pub const FIGHTS_PER_WEAPON_GIFT: i64 = 25;

/// The gifted weapon's Arcane tier. Owner-approved deviation (#390/#391,
/// 2026-10-16).
pub const WEAPON_GIFT_ARCANE_TIER: u64 = 2;

/// Pre-rolled enchantments on the gifted weapon: none. The weapon is gifted blank,
/// as the Sigil shop sells Arcane02 items; Arcane II guarantees two bonus
/// enchantments when the player enchants it. Owner-approved deviation (#390/#391,
/// 2026-10-16).
pub const WEAPON_GIFT_ENCHANTMENTS: usize = 0;

/// The tier any pre-rolled enchantment would take (unused while
/// [`WEAPON_GIFT_ENCHANTMENTS`] is 0): 10, matching the weapons' material tier.
pub const WEAPON_GIFT_ENCHANT_TIER: u64 = 10;

/// The gift's server-side description (never shown: the client renders the
/// announcement's generic gift copy).
pub const WEAPON_GIFT_DESCRIPTION: &str = "Arena: a weapon for every 25 fights";

/// One gift weapon: its template, the Dragonbone twin whose enchanting tables
/// it shares (same weapon type, tier 10, level 45, same durability), and its
/// `editor_name`.
pub type GiftWeapon = (Uuid, Uuid, &'static str);

/// The nine weapon sets the gift's material cycles through, one per calendar
/// month (UTC), in cycle order: November 2026 is the first, July 2027 the last,
/// and then it repeats ([`material_index_for_month`]). Owner-approved (#391,
/// decision d23, 2026-10-25). Each set is its material's nine weapon types in
/// the same order (Battleaxe, Dagger, Greatsword, Hand Axe, Light Hammer,
/// Longsword, Mace, War Axe, Warhammer), so a set's `n`th weapon shares the
/// `n`th Dragonbone twin. Ids from `reference/game-defs/items.json`.
///
/// Seven materials have "Divine" weapons (tag `Item.Tag.Divine`). Amber and
/// Madness have none in the game data: their own weapons are already tier 10 at
/// level 45 with their Dragonbone twin's damage, durability and value (tagged
/// `Dragon`, like Dragonbone), so those two sets are the plain Amber and Madness
/// weapons.
pub const MONTHLY_WEAPON_SETS: [(&str, [GiftWeapon; 9]); 9] = [
    (
        "Amber",
        [
            (
                Uuid::from_u128(0xc7ec_eb24_878e_49b9_b5a6_5ed2_a65a_4bd7),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Amber Battleaxe",
            ),
            (
                Uuid::from_u128(0x1a8b_d31f_3538_41c6_8db5_76e3_1b4c_e6d1),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Amber Dagger",
            ),
            (
                Uuid::from_u128(0xd971_2b22_a50b_4ee9_b59e_b99e_dd22_78ef),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Amber Greatsword",
            ),
            (
                Uuid::from_u128(0x3ae6_d46a_bbc7_497c_8c1b_10d1_f51a_4453),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Amber Hand Axe",
            ),
            (
                Uuid::from_u128(0xc647_65ec_85fb_4265_b77d_ad9d_8bb2_2298),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Amber Light Hammer",
            ),
            (
                Uuid::from_u128(0xcfa5_b02b_ae0c_486a_babb_d6dc_76fb_1a73),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Amber Longsword",
            ),
            (
                Uuid::from_u128(0xa7a6_660c_e46a_479b_bcfc_cdd0_9752_328e),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Amber Mace",
            ),
            (
                Uuid::from_u128(0x6ae6_66e4_875c_4c65_80d9_44ec_626d_0fda),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Amber War Axe",
            ),
            (
                Uuid::from_u128(0x747e_4b70_45ef_4081_ac7d_f688_fdfc_15e0),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Amber Warhammer",
            ),
        ],
    ),
    (
        "Ancient Nordic",
        [
            (
                Uuid::from_u128(0x3b3a_73cb_50c6_4ef4_8c0a_549a_d3b7_151a),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Divine Ancient Nordic Battleaxe",
            ),
            (
                Uuid::from_u128(0x9d13_d24d_f4df_4653_871a_1f04_0180_2025),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Divine Ancient Nordic Dagger",
            ),
            (
                Uuid::from_u128(0x537a_f13c_8038_4e00_aece_20df_3365_aa71),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Divine Ancient Nordic Greatsword",
            ),
            (
                Uuid::from_u128(0x9903_bcc8_4565_418c_9bfe_e189_8a3f_08ee),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Divine Ancient Nordic Hand Axe",
            ),
            (
                Uuid::from_u128(0xbb45_d4a9_e29d_43c2_9e0d_90c5_fd06_dc62),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Divine Ancient Nordic Light Hammer",
            ),
            (
                Uuid::from_u128(0x6aca_f2f5_3c5a_4529_9392_9a6d_fba0_9e3e),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Divine Ancient Nordic Longsword",
            ),
            (
                Uuid::from_u128(0x7643_bed4_801b_4b13_bbb1_4b27_6deb_5631),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Divine Ancient Nordic Mace",
            ),
            (
                Uuid::from_u128(0xf2d9_5dae_92b4_4f84_ab6d_bba4_4690_31da),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Divine Ancient Nordic War Axe",
            ),
            (
                Uuid::from_u128(0xe411_54c6_dcfd_48af_b808_24bb_2d25_9aee),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Divine Ancient Nordic Warhammer",
            ),
        ],
    ),
    (
        "Chaurus",
        [
            (
                Uuid::from_u128(0xc834_47e1_740b_4db8_accf_276e_3e35_623d),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Divine Chaurus Battleaxe",
            ),
            (
                Uuid::from_u128(0x8f5d_77e5_6db7_44ae_b7d4_a5b1_7d49_bc1f),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Divine Chaurus Dagger",
            ),
            (
                Uuid::from_u128(0xd557_00e0_16c5_4e92_9b6a_2d5f_38f0_ea75),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Divine Chaurus Greatsword",
            ),
            (
                Uuid::from_u128(0xc4d6_db09_6aec_4447_bec3_65ae_feb7_9823),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Divine Chaurus Hand Axe",
            ),
            (
                Uuid::from_u128(0xdc73_3e9f_d69e_4fb6_bcc3_4ba4_cbfa_f527),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Divine Chaurus Light Hammer",
            ),
            (
                Uuid::from_u128(0x5316_7599_9ccc_4cf9_b275_40e1_f2ba_bc70),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Divine Chaurus Longsword",
            ),
            (
                Uuid::from_u128(0x7231_0406_876a_415b_b50e_88d7_97ac_cfa8),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Divine Chaurus Mace",
            ),
            (
                Uuid::from_u128(0x785f_023c_f9b5_4ad2_a874_b164_58da_f4ef),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Divine Chaurus War Axe",
            ),
            (
                Uuid::from_u128(0xd9f8_bb82_0e42_4061_aa5a_fc4b_e083_693a),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Divine Chaurus Warhammer",
            ),
        ],
    ),
    (
        "Dark Seducer",
        [
            (
                Uuid::from_u128(0x1baa_a5a1_ed9d_4817_8c39_c47b_807a_ac0f),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Divine Dark Seducer Battleaxe",
            ),
            (
                Uuid::from_u128(0x4ca4_837e_dbff_423a_8372_8c0a_160c_2e02),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Divine Dark Seducer Dagger",
            ),
            (
                Uuid::from_u128(0x071f_4b42_5356_49ca_b2f4_6df2_056d_b241),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Divine Dark Seducer Greatsword",
            ),
            (
                Uuid::from_u128(0xf25c_d627_34ef_4b00_930f_e797_e1d8_93db),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Divine Dark Seducer Hand Axe",
            ),
            (
                Uuid::from_u128(0x6291_cd12_cda1_438a_ad40_3017_5e02_dd60),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Divine Dark Seducer Light Hammer",
            ),
            (
                Uuid::from_u128(0x9a80_8759_3e96_4fc1_be3f_c22d_ef38_e886),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Divine Dark Seducer Longsword",
            ),
            (
                Uuid::from_u128(0xea66_242a_82a2_42da_a3f0_ea37_f33b_c407),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Divine Dark Seducer Mace",
            ),
            (
                Uuid::from_u128(0xf40c_c035_f2d6_43b9_8be8_66ce_483c_e71e),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Divine Dark Seducer War Axe",
            ),
            (
                Uuid::from_u128(0xbe5b_7eed_c53f_4dca_8853_eeda_7978_04f8),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Divine Dark Seducer Warhammer",
            ),
        ],
    ),
    (
        "Forsworn",
        [
            (
                Uuid::from_u128(0x14b4_262e_3490_493e_8b24_6e87_0fe1_bfc3),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Divine Forsworn Battleaxe",
            ),
            (
                Uuid::from_u128(0xebcd_f42d_c7d1_4bdf_9f25_1520_0fa5_ddf2),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Divine Forsworn Dagger",
            ),
            (
                Uuid::from_u128(0x3882_5725_21f4_4ab5_be4e_7ad6_1547_2d63),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Divine Forsworn Greatsword",
            ),
            (
                Uuid::from_u128(0xaadb_81ec_bcca_422d_ac3e_0519_b83a_b022),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Divine Forsworn Hand Axe",
            ),
            (
                Uuid::from_u128(0x0014_d5ed_2980_4dad_8da6_8cad_7c2c_c17c),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Divine Forsworn Light Hammer",
            ),
            (
                Uuid::from_u128(0xb399_0bd6_4784_41ca_9e05_e53e_77cd_ef73),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Divine Forsworn Longsword",
            ),
            (
                Uuid::from_u128(0xfafd_68b5_1f3f_481d_a5b6_862e_640a_f6e8),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Divine Forsworn Mace",
            ),
            (
                Uuid::from_u128(0xfe45_99df_d7a4_4f7e_aaf5_9a9c_fa91_7303),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Divine Forsworn War Axe",
            ),
            (
                Uuid::from_u128(0xdae8_4e5b_6345_480d_af80_6d96_eb90_637c),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Divine Forsworn Warhammer",
            ),
        ],
    ),
    (
        "Golden Saint",
        [
            (
                Uuid::from_u128(0x6f54_3736_441b_4068_bb51_d082_9abc_0cac),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Divine Golden Saint Battleaxe",
            ),
            (
                Uuid::from_u128(0xc10f_8e7f_146d_432f_b8d3_5d7c_39cc_64b2),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Divine Golden Saint Dagger",
            ),
            (
                Uuid::from_u128(0x21e0_b402_b1be_4bdc_822b_033d_41bf_9e84),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Divine Golden Saint Greatsword",
            ),
            (
                Uuid::from_u128(0x5d64_e438_2af7_4554_adcf_f46d_2cb9_9930),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Divine Golden Saint Hand Axe",
            ),
            (
                Uuid::from_u128(0xd257_c893_cbaf_4abd_bf73_5a68_0fcf_e018),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Divine Golden Saint Light Hammer",
            ),
            (
                Uuid::from_u128(0x99c4_2ddf_815f_467a_9c1d_5b1d_bceb_61ba),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Divine Golden Saint Longsword",
            ),
            (
                Uuid::from_u128(0x7bd7_1afd_f831_4be2_a086_2898_ee17_c30b),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Divine Golden Saint Mace",
            ),
            (
                Uuid::from_u128(0x512e_8401_0d2a_4be6_b317_8793_e119_89f4),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Divine Golden Saint War Axe",
            ),
            (
                Uuid::from_u128(0x0e74_f0ae_d58a_41aa_976f_0144_26a7_d7f5),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Divine Golden Saint Warhammer",
            ),
        ],
    ),
    (
        "Madness",
        [
            (
                Uuid::from_u128(0x8034_4fa2_2c4a_4ef3_a5e3_7533_b712_a740),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Madness Battleaxe",
            ),
            (
                Uuid::from_u128(0x9af1_87a5_a7b9_4999_b4b4_1f07_c34c_0212),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Madness Dagger",
            ),
            (
                Uuid::from_u128(0xcbe1_b160_74c9_433b_aa7a_aa8c_339f_3087),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Madness Greatsword",
            ),
            (
                Uuid::from_u128(0xce7a_cc15_5ab3_4c33_b349_da03_d722_2c5f),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Madness Hand Axe",
            ),
            (
                Uuid::from_u128(0xc29a_bcdd_505d_4ffd_a6d7_b094_e842_d449),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Madness Light Hammer",
            ),
            (
                Uuid::from_u128(0x054d_7e97_0111_462c_8c3b_a546_1d7d_1c2a),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Madness Longsword",
            ),
            (
                Uuid::from_u128(0x768a_8194_cda8_4a1a_b802_64b9_b2e5_9901),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Madness Mace",
            ),
            (
                Uuid::from_u128(0x6e04_41a2_7577_4a9e_950e_e564_4963_7a3c),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Madness War Axe",
            ),
            (
                Uuid::from_u128(0xc453_9fa0_ef1f_43de_8372_c339_ab52_b063),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Madness Warhammer",
            ),
        ],
    ),
    (
        "Nordic Steel",
        [
            (
                Uuid::from_u128(0x0250_3c3a_52f4_4d1a_b05c_75a7_6272_966b),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Divine Nordic Steel Battleaxe",
            ),
            (
                Uuid::from_u128(0xaa13_c5c7_864d_4b77_9d1d_2909_b7cd_4d92),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Divine Nordic Steel Dagger",
            ),
            (
                Uuid::from_u128(0xef2d_3e37_5703_4091_ac44_f943_6709_8bef),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Divine Nordic Steel Greatsword",
            ),
            (
                Uuid::from_u128(0xf644_872a_3723_4fe5_b0a0_edfd_efdb_a28c),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Divine Nordic Steel Hand Axe",
            ),
            (
                Uuid::from_u128(0xfa1a_3a6e_6752_4157_b7a6_f8ec_19c1_2e7e),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Divine Nordic Steel Light Hammer",
            ),
            (
                Uuid::from_u128(0x9f62_e950_87ac_4b18_bfd1_8f95_8f0f_3f70),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Divine Nordic Steel Longsword",
            ),
            (
                Uuid::from_u128(0xd212_05fe_0009_4208_bffd_6c5c_a429_38ad),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Divine Nordic Steel Mace",
            ),
            (
                Uuid::from_u128(0x7452_44a6_0ac0_44e5_b728_5563_df61_e1e6),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Divine Nordic Steel War Axe",
            ),
            (
                Uuid::from_u128(0x9952_5bb7_84d4_4520_877d_2f21_f6b0_59ec),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Divine Nordic Steel Warhammer",
            ),
        ],
    ),
    (
        "Stalhrim",
        [
            (
                Uuid::from_u128(0xa669_8394_a03a_4ae8_96c9_a71f_26bf_b181),
                Uuid::from_u128(0x4edb_65be_0ebc_4698_bb97_2a01_2656_c238),
                "Divine Stalhrim Battleaxe",
            ),
            (
                Uuid::from_u128(0x19f7_c6e0_dabd_48bd_abe9_51d9_153f_b3a9),
                Uuid::from_u128(0x03b1_e366_3d2a_48b8_99f2_f06b_e005_e207),
                "Divine Stalhrim Dagger",
            ),
            (
                Uuid::from_u128(0x8c6b_e63c_65e7_4855_b7f5_eb4e_9fbd_cb8d),
                Uuid::from_u128(0xdad1_06ff_62e9_4d23_b7a7_e897_b554_bd5d),
                "Divine Stalhrim Greatsword",
            ),
            (
                Uuid::from_u128(0x9d95_e5d8_0534_4c80_8edc_6927_7f33_55c5),
                Uuid::from_u128(0x1d6c_0fb1_50e2_401f_939e_b886_0d9f_e026),
                "Divine Stalhrim Hand Axe",
            ),
            (
                Uuid::from_u128(0xab52_dd8d_9292_4055_be5e_368b_3c62_db5f),
                Uuid::from_u128(0xbacb_3089_2378_45d2_b717_2dd3_f01e_5939),
                "Divine Stalhrim Light Hammer",
            ),
            (
                Uuid::from_u128(0x21f9_f03f_250c_4f37_b725_b967_3318_3e4c),
                Uuid::from_u128(0xc4e0_77d7_2a55_4b4d_a5b8_bcec_9121_d3d6),
                "Divine Stalhrim Longsword",
            ),
            (
                Uuid::from_u128(0xa696_6f6a_bc99_44d2_a188_b158_6e12_132a),
                Uuid::from_u128(0x3567_538a_18ea_4101_9380_8997_b36c_b65e),
                "Divine Stalhrim Mace",
            ),
            (
                Uuid::from_u128(0x29a0_2cb7_048b_4dc7_96f3_f684_e370_7c99),
                Uuid::from_u128(0xefb6_d38f_ecc9_47bd_bde2_397c_d35d_888b),
                "Divine Stalhrim War Axe",
            ),
            (
                Uuid::from_u128(0xc4b0_634d_7f3a_4afd_9724_dfd4_adf4_27b3),
                Uuid::from_u128(0x912c_d6ac_d0ec_480e_8565_f721_baf4_1c95),
                "Divine Stalhrim Warhammer",
            ),
        ],
    ),
];

/// Index of the Stalhrim set in [`MONTHLY_WEAPON_SETS`]: the gift's material
/// through October 2026 (#539), and July of every cycle.
pub const STALHRIM_SET: usize = 8;

/// The nine Divine Stalhrim weapons (#539), the gift's material through October
/// 2026.
pub const DIVINE_STALHRIM_WEAPONS: [GiftWeapon; 9] = MONTHLY_WEAPON_SETS[STALHRIM_SET].1;

/// The first month of the monthly cycle: November 2026 (year, month 1-12).
/// Every earlier month pays Stalhrim.
pub const MONTHLY_CYCLE_START: (i32, u32) = (2026, 11);

/// The [`MONTHLY_WEAPON_SETS`] index for a calendar month (`month` 1-12):
/// Stalhrim up to October 2026, then Amber, Ancient Nordic, Chaurus, Dark Seducer,
/// Forsworn, Golden Saint, Madness, Nordic Steel, Stalhrim from November 2026,
/// repeating every nine months.
pub fn material_index_for_month(year: i32, month: u32) -> usize {
    let at = i64::from(year) * 12 + i64::from(month) - 1;
    let start = i64::from(MONTHLY_CYCLE_START.0) * 12 + i64::from(MONTHLY_CYCLE_START.1) - 1;
    if at < start {
        return STALHRIM_SET;
    }
    ((at - start) % MONTHLY_WEAPON_SETS.len() as i64) as usize
}

/// The weapon set of the calendar month (UTC) containing `unix_secs`.
pub fn weapon_set_at(unix_secs: i64) -> &'static (&'static str, [GiftWeapon; 9]) {
    use chrono::Datelike;
    let index = chrono::DateTime::from_timestamp(unix_secs, 0).map_or(STALHRIM_SET, |t| {
        material_index_for_month(t.year(), t.month())
    });
    &MONTHLY_WEAPON_SETS[index]
}

/// Whether `template` is one of the gift's weapons, of any month.
pub fn is_gift_weapon(template: Uuid) -> bool {
    MONTHLY_WEAPON_SETS
        .iter()
        .any(|(_, set)| set.iter().any(|w| w.0 == template))
}

/// Seconds since the Unix epoch, now.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Whether an account's running fight count just reached a gift.
pub fn milestone_reached(fights: i64) -> bool {
    fights > 0 && fights % FIGHTS_PER_WEAPON_GIFT == 0
}

/// The weapon a gift minted at `unix_secs` pays: the material of that calendar
/// month (UTC, [`weapon_set_at`]), and a uniform pick of its nine weapon types
/// drawn from the gift's own id, blank at Arcane II.
pub fn roll_weapon(gift_id: Uuid, unix_secs: i64) -> GiftItem {
    let seed = crate::jewelry_roll::seed(&[gift_id.as_bytes(), b"arena-fight-gift"], 0);
    let (_, set) = weapon_set_at(unix_secs);
    let (template, twin, name) = set[(seed % set.len() as u64) as usize];
    let enchanting = blades_lib::features::legendary_gear::roll_enchanting_for(
        &twin,
        WEAPON_GIFT_ENCHANTMENTS,
        WEAPON_GIFT_ENCHANT_TIER,
        seed.rotate_left(17) ^ 0x9E37_79B9_7F4A_7C15,
    )
    .unwrap_or_else(|| {
        // Only reachable with WEAPON_GIFT_ENCHANTMENTS > 0; a table that cannot
        // supply the roll still pays the weapon rather than nothing.
        log::error!("arena fight gift: no enchanting roll for {name}; granting it plain");
        Vec::new()
    });
    GiftItem {
        item_template_id: template,
        quantity: 1,
        arcane_tier: Some(WEAPON_GIFT_ARCANE_TIER),
        enchanting,
    }
}

/// Every gift weapon's new (temper 0) max durability, installed once at startup
/// from the server's durability table so the synchronous match-end card can
/// build the same instance the writer persists.
static WEAPON_DURABILITY: OnceLock<HashMap<Uuid, f64>> = OnceLock::new();

/// Record every gift weapon's max durability from the loaded durability table.
/// Idempotent: the first install wins.
pub fn install_durability(repair: &blades_lib::features::repair::RepairData) {
    let all = || MONTHLY_WEAPON_SETS.iter().flat_map(|(_, set)| set.iter());
    let table = all()
        .filter_map(|(weapon, _, _)| Some((*weapon, repair.max_durability(*weapon, 0)?)))
        .collect::<HashMap<_, _>>();
    if table.len() != all().count() {
        log::warn!(
            "arena fight gift: durability known for {} of {} gift weapons; the rest \
             arrive at the game's default {}",
            table.len(),
            all().count(),
            blades_lib::features::repair::DEFAULT_DURABILITY
        );
    }
    let _ = WEAPON_DURABILITY.set(table);
}

fn weapon_durability(template: Uuid) -> f64 {
    WEAPON_DURABILITY
        .get()
        .and_then(|t| t.get(&template).copied())
        .unwrap_or(blades_lib::features::repair::DEFAULT_DURABILITY)
}

/// Whether the fight about to finish is the account's milestone fight, given the
/// account's finished fights BEFORE it. `None` (count unknown) never is: the
/// card must not promise a weapon the writer may not grant.
pub fn weapon_due_on_card(fights_before: Option<i64>) -> bool {
    fights_before.is_some_and(|n| n >= 0 && milestone_reached(n + 1))
}

/// The weapon instance a milestone fight finished at `unix_secs` pays, with
/// `item_id` as both the instance id and the roll seed: new (temper 0, full
/// durability), blank, Arcane II — the same instance [`FightGiftRow::gift_def`] +
/// `build_gift_reward` produce for a claimed gift.
pub fn weapon_instance(item_id: Uuid, unix_secs: i64) -> RewardItem {
    let roll = roll_weapon(item_id, unix_secs);
    RewardItem {
        id: item_id,
        item: Item {
            item_template_id: roll.item_template_id,
            grade: None,
            tempering_level: 0,
            durability: weapon_durability(roll.item_template_id),
            properties: ItemPropertiesAll {
                enchanting: roll.enchanting,
                ..ItemPropertiesAll::default()
            },
            arcane_tier: roll.arcane_tier,
        },
    }
}

fn gift_item_of(weapon: &RewardItem) -> GiftItem {
    GiftItem {
        item_template_id: weapon.item.item_template_id,
        quantity: 1,
        arcane_tier: weapon.item.arcane_tier,
        enchanting: weapon.item.properties.enchanting.clone(),
    }
}

/// The account's finished arena fights so far (0 before its first). Read at
/// queue time into the loadout so the match-end card knows whether this fight
/// is a milestone.
pub async fn account_fights(conn: &mut AsyncPgConnection, user_id: Uuid) -> Result<i64> {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        fights: i64,
    }
    let row: Option<Count> =
        diesel::sql_query("SELECT fights FROM arena_account_fight_counts WHERE user_id = $1")
            .bind::<diesel::sql_types::Uuid, _>(user_id)
            .get_result(conn)
            .await
            .optional()?;
    Ok(row.map(|r| r.fights).unwrap_or(0))
}

/// One `arena_fight_gifts` row.
#[derive(Debug, Clone, diesel::QueryableByName)]
pub struct FightGiftRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub user_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub character_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    pub source_alt_uuid: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub milestone: i64,
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub item: serde_json::Value,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub created_at: i64,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    pub granted_at: Option<i64>,
}

impl FightGiftRow {
    /// The gift as a `globalGiftOverride`: one weapon, claimable once, no window.
    pub fn gift_def(&self) -> Option<GiftDef> {
        let item: GiftItem = serde_json::from_value(self.item.clone()).ok()?;
        Some(GiftDef {
            global_gift_id: self.id,
            items: vec![item],
            chests: Vec::new(),
            start_time: 0,
            end_time: 0,
            claim_count_limit: 1,
            description: Some(WEAPON_GIFT_DESCRIPTION.into()),
        })
    }

    pub fn claim_count(&self) -> u64 {
        u64::from(self.granted_at.is_some())
    }
}

/// What one finished fight did to its account's tally.
#[derive(Debug, Clone)]
pub struct FightTally {
    /// The account's fights after this one.
    pub fights: i64,
    /// A CLAIMABLE gift minted by this fight: a milestone the match-end card did
    /// not show, delivered through `globalgifts` instead.
    pub gift: Option<(Uuid, GiftItem)>,
    /// The weapon the match-end card showed, to be put straight into the
    /// backpack by the caller (its milestone row is recorded already granted).
    pub delivered: Option<RewardItem>,
}

/// Count one finished fight for `user_id` and, on a milestone, mint its weapon
/// for the character and alt that fought it.
///
/// `card_weapon` is the instance the op49 card showed, if it showed one. On a
/// milestone it is recorded as granted and returned in
/// [`FightTally::delivered`] for the caller to put in the backpack; without one,
/// the milestone becomes a claimable gift as before. A card weapon on a fight
/// that is NOT a milestone (a concurrent fight moved the count) is not granted.
///
/// Called inside the match-reward transaction, as a nested transaction
/// (savepoint) so a missing table cannot roll the reward back.
pub async fn record_finished_fight(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    character_id: Uuid,
    alt: Option<Uuid>,
    now_secs: i64,
    card_weapon: Option<&RewardItem>,
) -> Result<FightTally> {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        fights: i64,
    }
    let Count { fights } = diesel::sql_query(
        "INSERT INTO arena_account_fight_counts (user_id, fights, updated_at) \
         VALUES ($1, 1, $2) \
         ON CONFLICT (user_id) DO UPDATE SET \
             fights = arena_account_fight_counts.fights + 1, \
             updated_at = EXCLUDED.updated_at \
         RETURNING fights",
    )
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .bind::<diesel::sql_types::BigInt, _>(now_secs)
    .get_result(conn)
    .await?;

    if !milestone_reached(fights) {
        return Ok(FightTally {
            fights,
            gift: None,
            delivered: None,
        });
    }
    // The card's weapon is delivered directly: its row is born granted, under the
    // instance's own id. Otherwise a fresh claimable gift, exactly as before.
    let (gift_id, item, granted_at) = match card_weapon {
        Some(w) => (w.id, gift_item_of(w), Some(now_secs)),
        None => {
            let id = Uuid::new_v4();
            (id, roll_weapon(id, now_secs), None)
        }
    };
    let inserted = diesel::sql_query(
        "INSERT INTO arena_fight_gifts \
         (id, user_id, character_id, source_alt_uuid, milestone, item, created_at, granted_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (user_id, milestone) DO NOTHING",
    )
    .bind::<diesel::sql_types::Uuid, _>(gift_id)
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(alt)
    .bind::<diesel::sql_types::BigInt, _>(fights)
    .bind::<diesel::sql_types::Jsonb, _>(serde_json::to_value(&item)?)
    .bind::<diesel::sql_types::BigInt, _>(now_secs)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(granted_at)
    .execute(conn)
    .await?;
    if inserted != 1 {
        return Ok(FightTally {
            fights,
            gift: None,
            delivered: None,
        });
    }
    Ok(match card_weapon {
        Some(w) => FightTally {
            fights,
            gift: None,
            delivered: Some(w.clone()),
        },
        None => FightTally {
            fights,
            gift: Some((gift_id, item)),
            delivered: None,
        },
    })
}

/// The gift `gift_id` if it belongs to `character_id`, whatever its alt or
/// claim state. `FOR UPDATE` when `lock` (the claim path).
pub async fn find(
    conn: &mut AsyncPgConnection,
    gift_id: Uuid,
    character_id: Uuid,
    lock: bool,
) -> Result<Option<FightGiftRow>> {
    Ok(diesel::sql_query(format!(
        "SELECT id, user_id, character_id, source_alt_uuid, milestone, item, created_at, granted_at \
         FROM arena_fight_gifts WHERE id = $1 AND character_id = $2{}",
        if lock { " FOR UPDATE" } else { "" }
    ))
    .bind::<diesel::sql_types::Uuid, _>(gift_id)
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .get_result(conn)
    .await
    .optional()?)
}

/// The alt occupying a live character row (`None` for a row from before alts
/// were tracked).
pub async fn live_alt(conn: &mut AsyncPgConnection, character_id: Uuid) -> Result<Option<Uuid>> {
    #[derive(diesel::QueryableByName)]
    struct Alt {
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
        source_alt_uuid: Option<Uuid>,
    }
    let row: Option<Alt> =
        diesel::sql_query("SELECT source_alt_uuid FROM characters WHERE id = $1")
            .bind::<diesel::sql_types::Uuid, _>(character_id)
            .get_result(conn)
            .await
            .optional()?;
    Ok(row.and_then(|r| r.source_alt_uuid))
}

/// Unclaimed gifts for the alt playing `character_id` now, oldest first — what
/// `/announcements` advertises.
pub async fn pending_for_live_alt(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
) -> Result<Vec<FightGiftRow>> {
    Ok(diesel::sql_query(
        "SELECT g.id, g.user_id, g.character_id, g.source_alt_uuid, g.milestone, g.item, \
                g.created_at, g.granted_at \
         FROM arena_fight_gifts g \
         WHERE g.character_id = $1 AND g.granted_at IS NULL \
           AND (g.source_alt_uuid IS NULL OR g.source_alt_uuid IS NOT DISTINCT FROM \
                (SELECT live.source_alt_uuid FROM characters live WHERE live.id = g.character_id)) \
         ORDER BY g.milestone",
    )
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .load(conn)
    .await?)
}

/// Mark a gift claimed. `false` when it was already claimed (a racing claim).
pub async fn mark_granted(
    conn: &mut AsyncPgConnection,
    gift_id: Uuid,
    now_secs: i64,
) -> Result<bool> {
    let changed = diesel::sql_query(
        "UPDATE arena_fight_gifts SET granted_at = $2 WHERE id = $1 AND granted_at IS NULL",
    )
    .bind::<diesel::sql_types::Uuid, _>(gift_id)
    .bind::<diesel::sql_types::BigInt, _>(now_secs)
    .execute(conn)
    .await?;
    Ok(changed == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel_async::AsyncConnection;
    use diesel_async::scoped_futures::ScopedFutureExt;

    fn items_json() -> serde_json::Value {
        let path = std::env::var("BLADES_CAPTURE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../blades-capture")
            })
            .join("reference/game-defs/items.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null)
    }

    /// 2026-10-15 12:00 UTC: inside the Stalhrim-only era (#539).
    const OCT_2026: i64 = 1_792_065_600;

    /// Unix seconds at 00:00 UTC on the first of `month` (1-12) of `year`.
    fn month_start(year: i32, month: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(year, month, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp()
    }

    /// Each monthly set is nine distinct weapons of its material, one per weapon
    /// type, and each twin is the Dragonbone weapon of the same weapon type, tier,
    /// level and durability. Seven sets are the APK's Divine weapons (tag
    /// `Item.Tag.Divine`); Amber and Madness have no Divine weapons, so theirs are
    /// the plain tier-10 sets. Checked against the capture repository's item table
    /// when it is present.
    #[test]
    fn every_monthly_set_is_nine_dragonbone_strength_weapons_of_its_material() {
        const DIVINE_TAG: &str = "7f13becf-9fb9-41ba-bd1c-ed4669005399";
        let all: std::collections::BTreeSet<Uuid> = MONTHLY_WEAPON_SETS
            .iter()
            .flat_map(|(_, set)| set.iter().map(|w| w.0))
            .collect();
        assert_eq!(all.len(), 81, "81 distinct weapons");
        let twins: Vec<Uuid> = DIVINE_STALHRIM_WEAPONS.iter().map(|w| w.1).collect();
        for (material, set) in MONTHLY_WEAPON_SETS {
            assert_eq!(
                set.iter().map(|w| w.1).collect::<Vec<_>>(),
                twins,
                "{material}: twins in type order"
            );
        }
        let items = items_json();
        if items.is_null() {
            eprintln!("SKIP: blades-capture items.json not found — ids not cross-checked");
            return;
        }
        for (material, set) in MONTHLY_WEAPON_SETS {
            let divine = !matches!(material, "Amber" | "Madness");
            for (weapon, twin, name) in set {
                let d = &items[weapon.to_string()];
                let t = &items[twin.to_string()];
                assert_eq!(d["editor_name"], name);
                let expected_prefix = if divine {
                    format!("Divine {material} ")
                } else {
                    format!("{material} ")
                };
                assert!(name.starts_with(&expected_prefix), "{name} is {material}");
                assert_eq!(d["type_name"], "weapon");
                let tagged_divine = d["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x == DIVINE_TAG);
                assert_eq!(tagged_divine, divine, "{name}: Divine tag");
                assert!(
                    t["editor_name"]
                        .as_str()
                        .unwrap()
                        .starts_with("Dragonbone "),
                    "{name} twin"
                );
                for field in ["tier", "requiredLevel", "maxDurability", "value"] {
                    assert_eq!(
                        d[field], t[field],
                        "{name} {field} matches its Dragonbone twin"
                    );
                }
                assert_eq!(d["tier"], 10, "{name} tier");
                assert_eq!(d["lootWeight"], 0.0, "{name} is not in the loot pool");
                for stat in ["weaponType", "baseDamage"] {
                    assert_eq!(d["stats"][stat], t["stats"][stat], "{name} {stat}");
                }
            }
        }
    }

    /// #391 (decision d23): the material follows the UTC calendar month.
    /// October 2026 and before is Stalhrim; November 2026 starts the cycle at
    /// Amber, July 2027 ends it at Stalhrim, and August 2027 wraps to Amber.
    #[test]
    fn the_material_follows_the_calendar_month() {
        let name = |y: i32, m: u32| MONTHLY_WEAPON_SETS[material_index_for_month(y, m)].0;
        assert_eq!(name(2026, 10), "Stalhrim");
        assert_eq!(name(2026, 1), "Stalhrim");
        assert_eq!(name(2025, 12), "Stalhrim");
        let cycle = [
            ((2026, 11), "Amber"),
            ((2026, 12), "Ancient Nordic"),
            ((2027, 1), "Chaurus"),
            ((2027, 2), "Dark Seducer"),
            ((2027, 3), "Forsworn"),
            ((2027, 4), "Golden Saint"),
            ((2027, 5), "Madness"),
            ((2027, 6), "Nordic Steel"),
            ((2027, 7), "Stalhrim"),
            ((2027, 8), "Amber"),
            ((2027, 9), "Ancient Nordic"),
            ((2028, 4), "Stalhrim"),
            ((2028, 5), "Amber"),
            ((2028, 12), "Nordic Steel"),
        ];
        for ((y, m), material) in cycle {
            assert_eq!(name(y, m), material, "{y}-{m:02}");
        }
        // The month turns at midnight UTC.
        assert_eq!(weapon_set_at(month_start(2026, 11) - 1).0, "Stalhrim");
        assert_eq!(weapon_set_at(month_start(2026, 11)).0, "Amber");
        assert_eq!(weapon_set_at(month_start(2027, 1) - 1).0, "Ancient Nordic");
        assert_eq!(weapon_set_at(month_start(2027, 1)).0, "Chaurus");
        assert_eq!(weapon_set_at(OCT_2026).0, "Stalhrim");
        assert_eq!(weapon_set_at(0).0, "Stalhrim");
    }

    /// Every gift is one of the month's nine, blank (no ENCHANTING) at Arcane II,
    /// and all nine come up, in every month of the cycle and in October 2026.
    #[test]
    fn every_gift_is_a_blank_arcane_two_weapon_of_the_months_material() {
        let mut months = vec![OCT_2026];
        months.extend(
            (0..10).map(|k| {
                month_start(2026 + (10 + k) / 12, ((10 + k) % 12 + 1) as u32) + 86_400 * 9
            }),
        );
        for at in months {
            let (material, set) = weapon_set_at(at);
            let mut seen = std::collections::BTreeSet::new();
            for n in 0..400u128 {
                let item = roll_weapon(Uuid::from_u128(n * 7919 + 1), at);
                assert!(
                    set.iter().any(|w| w.0 == item.item_template_id),
                    "{material}"
                );
                assert_eq!(item.quantity, 1);
                assert_eq!(item.arcane_tier, Some(2));
                assert!(
                    item.enchanting.is_empty(),
                    "gifted blank, enchanted by the player"
                );
                seen.insert(item.item_template_id);
            }
            assert_eq!(
                seen.len(),
                9,
                "{material}: the pick reaches all nine weapons"
            );
        }
        // October 2026 pays Divine Stalhrim, exactly as #539.
        let item = roll_weapon(Uuid::from_u128(5), OCT_2026);
        assert!(
            DIVINE_STALHRIM_WEAPONS
                .iter()
                .any(|w| w.0 == item.item_template_id)
        );
        // The pick is a function of the gift id and month: GET and claim agree.
        let id = Uuid::from_u128(42);
        assert_eq!(
            roll_weapon(id, OCT_2026).item_template_id,
            roll_weapon(id, OCT_2026).item_template_id
        );
    }

    /// Every material's nine ids resolve in the shipped game data: a durability
    /// (equal to the Dragonbone twin's) and an enchanting table to draw from.
    #[test]
    fn every_material_resolves_to_shipped_item_ids() {
        let durability: serde_json::Value =
            serde_json::from_str(include_str!("../../../deploy/static/item_durability.json"))
                .unwrap();
        let repair = blades_lib::features::repair::RepairData::from_json(
            &durability,
            &serde_json::json!({}),
        );
        let e: blades_lib::static_data::EnchantingData =
            serde_json::from_str(include_str!("../../../deploy/static/enchanting.json")).unwrap();
        for (material, set) in MONTHLY_WEAPON_SETS {
            for (weapon, twin, name) in set {
                let own = repair.max_durability(weapon, 0);
                assert!(own.is_some(), "{material}: {name} has no durability");
                assert_eq!(
                    own,
                    repair.max_durability(twin, 0),
                    "{name}: durability matches its twin"
                );
                assert!(
                    e.table_for(&weapon).is_some(),
                    "{material}: {name} has no enchanting table"
                );
                let id = weapon.to_string();
                assert!(
                    crate::arena::combat::block_temper::BLOCK_TEMPER
                        .iter()
                        .any(|(t, _)| *t == id),
                    "{material}: {name} has no arena block values"
                );
            }
        }
    }

    /// Arcane II is what guarantees the two bonus enchantments: the shipped
    /// enchanting tables give an arcane-2 item exactly 2 secondaries when it is
    /// enchanted (`craft.rs::roll_enchant` reads these odds).
    #[test]
    fn arcane_two_guarantees_exactly_two_bonus_enchantments_on_enchant() {
        let e: blades_lib::static_data::EnchantingData =
            serde_json::from_str(include_str!("../../../deploy/static/enchanting.json")).unwrap();
        assert_eq!(
            e.arcane_tier_count_odds[&WEAPON_GIFT_ARCANE_TIER],
            vec![0.0, 0.0, 1.0]
        );
        for (_, set) in MONTHLY_WEAPON_SETS {
            for (weapon, _, name) in set {
                assert!(
                    e.table_for(&weapon).is_some(),
                    "{name} has a secondary table to draw from"
                );
            }
        }
    }

    #[test]
    fn every_twenty_fifth_fight_is_a_milestone() {
        let hits: Vec<i64> = (0..=100).filter(|n| milestone_reached(*n)).collect();
        assert_eq!(hits, vec![25, 50, 75, 100]);
    }

    /// The claimed gift grants exactly one weapon instance shaped exactly like
    /// retail's Sigil Arcane02 weapon grants, while the client is shown only
    /// retail's `{itemTemplateId, quantity}` gift line.
    ///
    /// The control is retail itself: `global_shop_grants.json` holds captured
    /// Arcane02 grants of the Dragonbone twins — `{arcaneTier: 2, durability,
    /// id, itemTemplateId, temperingLevel: 0}`, nothing else. The gifted Divine
    /// Stalhrim must serialize with the same keys and its twin's durability.
    #[test]
    fn the_gift_grants_one_blank_arcane_two_weapon_like_the_sigil_shop() {
        let grants: serde_json::Value = serde_json::from_str(include_str!(
            "../../../deploy/static/global_shop_grants.json"
        ))
        .unwrap();
        let mut retail = Vec::new();
        fn walk(v: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
            match v {
                serde_json::Value::Object(m) => {
                    if m.get("arcaneTier") == Some(&serde_json::json!(2))
                        && let Some(t) = m.get("itemTemplateId").and_then(|t| t.as_str())
                        && DIVINE_STALHRIM_WEAPONS.iter().any(|w| w.1.to_string() == t)
                    {
                        out.push(v.clone());
                    }
                    m.values().for_each(|x| walk(x, out));
                }
                serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                _ => {}
            }
        }
        walk(&grants, &mut retail);
        assert!(
            retail.len() >= 3,
            "captured Arcane02 Dragonbone weapon grants exist"
        );

        let durability: serde_json::Value =
            serde_json::from_str(include_str!("../../../deploy/static/item_durability.json"))
                .unwrap();
        let repair = blades_lib::features::repair::RepairData::from_json(
            &durability,
            &serde_json::json!({}),
        );

        for captured in &retail {
            let twin: Uuid = captured["itemTemplateId"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            let (divine, _, name) = *DIVINE_STALHRIM_WEAPONS
                .iter()
                .find(|w| w.1 == twin)
                .unwrap();
            let item = GiftItem {
                item_template_id: divine,
                quantity: 1,
                arcane_tier: Some(WEAPON_GIFT_ARCANE_TIER),
                enchanting: Vec::new(),
            };
            let row = FightGiftRow {
                id: Uuid::from_u128(5),
                user_id: Uuid::nil(),
                character_id: Uuid::nil(),
                source_alt_uuid: None,
                milestone: 25,
                item: serde_json::to_value(&item).unwrap(),
                created_at: 0,
                granted_at: None,
            };
            let def = row.gift_def().unwrap();
            let types = std::collections::HashMap::from([(
                divine,
                blades_lib::game_data::GameDataItem {
                    name: name.into(),
                    r#type: 2,
                },
            )]);
            let reward = blades_lib::features::gifts::build_gift_reward(&def, &types, &repair);
            assert_eq!(reward.items.len(), 1, "exactly one weapon");
            assert!(reward.stackable_items.is_empty() && reward.currencies.is_empty());
            let granted = &reward.items[0].item;
            assert_eq!(granted.item_template_id, divine);
            assert_eq!(granted.arcane_tier, Some(2));
            assert_eq!(granted.grade, None);
            assert_eq!(granted.tempering_level, 0);
            assert!(
                granted.properties.enchanting.is_empty() && granted.properties.grading.is_empty()
            );

            let mut ours = serde_json::to_value(granted).unwrap();
            let mut theirs = captured.clone();
            ours.as_object_mut().unwrap().remove("id");
            theirs.as_object_mut().unwrap().remove("id");
            theirs["itemTemplateId"] = serde_json::json!(divine);
            // An empty `properties` object is how every server-built item
            // serializes; retail omitted it. The Sigil path emits the same.
            if ours.get("properties").is_some_and(|p| {
                p.as_object().is_some_and(|o| {
                    o.values()
                        .all(|v| v.as_array().is_some_and(|a| a.is_empty()))
                })
            }) {
                ours.as_object_mut().unwrap().remove("properties");
            }
            assert_eq!(
                ours, theirs,
                "{name}: same shape and durability as retail's Arcane02 {twin}"
            );

            let shown = serde_json::to_value(def.for_client()).unwrap();
            assert_eq!(
                shown["items"][0],
                serde_json::json!({"itemTemplateId": divine, "quantity": 1})
            );
        }
    }

    async fn fixture() -> Option<AsyncPgConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let mut conn = AsyncPgConnection::establish(&url)
            .await
            .expect("TEST_DATABASE_URL is set but unreachable");
        conn.begin_test_transaction()
            .await
            .expect("test transaction");
        let schema = format!("t{}", Uuid::new_v4().simple());
        for stmt in [
            format!("CREATE SCHEMA {schema}"),
            format!("SET LOCAL search_path TO {schema}"),
            "CREATE TABLE users (id UUID PRIMARY KEY)".into(),
            "CREATE TABLE characters (id UUID PRIMARY KEY, user_id UUID NOT NULL, source_alt_uuid UUID)"
                .into(),
        ] {
            diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
        }
        // The real migration, as production applies it.
        const UP: &str =
            include_str!("../../../migrations/2026-10-16-000000-0000_add_arena_fight_gifts/up.sql");
        let sql: String = UP
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        for stmt in sql.split(';').filter(|s| !s.trim().is_empty()) {
            diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
        }
        Some(conn)
    }

    async fn gift_rows(conn: &mut AsyncPgConnection) -> Vec<FightGiftRow> {
        diesel::sql_query(
            "SELECT id, user_id, character_id, source_alt_uuid, milestone, item, created_at, granted_at \
             FROM arena_fight_gifts ORDER BY user_id, milestone",
        )
        .load(conn)
        .await
        .unwrap()
    }

    /// The match reward and the fight count share a transaction, so the count
    /// runs in a savepoint: a failure in it (here, the table not yet migrated)
    /// must leave the enclosing transaction usable, or the reward would be lost.
    #[tokio::test]
    async fn a_failed_count_does_not_poison_the_reward_transaction() {
        let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — savepoint isolation not verified");
            return;
        };
        let mut conn = AsyncPgConnection::establish(&url).await.unwrap();
        conn.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        for stmt in [
            format!("CREATE SCHEMA {schema}"),
            format!("SET LOCAL search_path TO {schema}"),
            "CREATE TABLE characters (id UUID PRIMARY KEY, source_alt_uuid UUID)".into(),
            "INSERT INTO characters VALUES ('00000000-0000-0000-0000-000000000001', NULL)".into(),
        ] {
            diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
        }
        let counted = conn
            .transaction::<_, anyhow::Error, _>(|c| {
                async move {
                    record_finished_fight(c, Uuid::from_u128(9), Uuid::from_u128(1), None, 1, None)
                        .await
                }
                .scope_boxed()
            })
            .await;
        assert!(
            counted.is_err(),
            "no arena_account_fight_counts table: the count fails"
        );
        // The enclosing transaction still works — the reward write would commit.
        let alt = live_alt(&mut conn, Uuid::from_u128(1)).await;
        assert!(alt.is_ok(), "outer transaction poisoned: {alt:?}");
    }

    /// The 25th fight on an account mints exactly one blank Arcane II Divine
    /// Stalhrim weapon; the count runs across the account's alts; another
    /// account's fights do not move it; and only the alt that fought the 25th is
    /// offered the gift.
    #[tokio::test]
    async fn the_25th_fight_per_account_across_alts_mints_one_weapon() {
        let Some(mut conn) = fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — fight-gift SQL not verified");
            return;
        };
        let (user, other) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let (cid, other_cid) = (Uuid::from_u128(10), Uuid::from_u128(20));
        let (alt_a, alt_b) = (Uuid::from_u128(0xa), Uuid::from_u128(0xb));
        diesel::sql_query("INSERT INTO users VALUES ($1), ($2)")
            .bind::<diesel::sql_types::Uuid, _>(user)
            .bind::<diesel::sql_types::Uuid, _>(other)
            .execute(&mut conn)
            .await
            .unwrap();
        diesel::sql_query("INSERT INTO characters VALUES ($1, $2, $3), ($4, $5, NULL)")
            .bind::<diesel::sql_types::Uuid, _>(cid)
            .bind::<diesel::sql_types::Uuid, _>(user)
            .bind::<diesel::sql_types::Uuid, _>(alt_a)
            .bind::<diesel::sql_types::Uuid, _>(other_cid)
            .bind::<diesel::sql_types::Uuid, _>(other)
            .execute(&mut conn)
            .await
            .unwrap();

        // 20 fights as alt A, interleaved with another account's 24.
        for n in 1..=24 {
            if n <= 20 {
                let t = record_finished_fight(&mut conn, user, cid, Some(alt_a), n, None)
                    .await
                    .unwrap();
                assert_eq!((t.fights, t.gift.is_some()), (n, false));
            }
            let t = record_finished_fight(&mut conn, other, other_cid, None, n, None)
                .await
                .unwrap();
            assert!(t.gift.is_none());
        }
        // Switch alt (same character row, same account) and keep fighting.
        diesel::sql_query("UPDATE characters SET source_alt_uuid = $1 WHERE id = $2")
            .bind::<diesel::sql_types::Uuid, _>(alt_b)
            .bind::<diesel::sql_types::Uuid, _>(cid)
            .execute(&mut conn)
            .await
            .unwrap();
        for n in 21..=24 {
            let t = record_finished_fight(&mut conn, user, cid, Some(alt_b), n, None)
                .await
                .unwrap();
            assert_eq!(
                (t.fights, t.gift.is_some()),
                (n, false),
                "fight {n} as alt B"
            );
        }
        assert!(
            gift_rows(&mut conn).await.is_empty(),
            "nothing before the 25th fight"
        );

        let t = record_finished_fight(&mut conn, user, cid, Some(alt_b), 25, None)
            .await
            .unwrap();
        assert_eq!(t.fights, 25, "alts share the account's count");
        let (gift_id, item) = t.gift.expect("the 25th fight mints a gift");
        assert!(
            DIVINE_STALHRIM_WEAPONS
                .iter()
                .any(|w| w.0 == item.item_template_id)
        );
        assert!(
            item.enchanting.is_empty(),
            "blank: Arcane II, enchanted by the player"
        );
        assert_eq!(item.arcane_tier, Some(2));

        let rows = gift_rows(&mut conn).await;
        assert_eq!(rows.len(), 1, "exactly one gift");
        assert_eq!(
            (rows[0].id, rows[0].user_id, rows[0].milestone),
            (gift_id, user, 25)
        );
        assert_eq!(
            rows[0].source_alt_uuid,
            Some(alt_b),
            "the alt that fought the 25th"
        );
        assert_eq!(
            rows[0].gift_def().unwrap().items[0].item_template_id,
            item.item_template_id
        );

        // The 26th fight mints nothing more; the other account is still at 24.
        let t = record_finished_fight(&mut conn, user, cid, Some(alt_b), 26, None)
            .await
            .unwrap();
        assert!(t.gift.is_none());
        assert_eq!(gift_rows(&mut conn).await.len(), 1);

        // Offered to alt B (live now), not to alt A.
        let pending = pending_for_live_alt(&mut conn, cid).await.unwrap();
        assert_eq!(
            pending.iter().map(|g| g.id).collect::<Vec<_>>(),
            vec![gift_id]
        );
        diesel::sql_query("UPDATE characters SET source_alt_uuid = $1 WHERE id = $2")
            .bind::<diesel::sql_types::Uuid, _>(alt_a)
            .bind::<diesel::sql_types::Uuid, _>(cid)
            .execute(&mut conn)
            .await
            .unwrap();
        assert!(
            pending_for_live_alt(&mut conn, cid)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            pending_for_live_alt(&mut conn, other_cid)
                .await
                .unwrap()
                .is_empty()
        );

        // Claiming is once only.
        assert!(find(&mut conn, gift_id, cid, true).await.unwrap().is_some());
        assert!(
            find(&mut conn, gift_id, other_cid, false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(mark_granted(&mut conn, gift_id, 99).await.unwrap());
        assert!(
            !mark_granted(&mut conn, gift_id, 100).await.unwrap(),
            "no second claim"
        );
        assert_eq!(
            find(&mut conn, gift_id, cid, false)
                .await
                .unwrap()
                .unwrap()
                .claim_count(),
            1
        );
    }

    /// #401: the weapon the op49 card showed is DELIVERED on the milestone — its
    /// row is born granted under the instance's id, so it is never advertised or
    /// claimable — while fights 24 and 26 grant nothing even if a card claimed
    /// one. A gift minted before this change (granted_at NULL) stays pending and
    /// claims exactly once, untouched by later deliveries.
    #[tokio::test]
    async fn the_card_weapon_is_delivered_and_a_legacy_gift_still_claims() {
        let Some(mut conn) = fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — fight-gift delivery SQL not verified");
            return;
        };
        let user = Uuid::from_u128(1);
        let cid = Uuid::from_u128(10);
        diesel::sql_query("INSERT INTO users VALUES ($1)")
            .bind::<diesel::sql_types::Uuid, _>(user)
            .execute(&mut conn)
            .await
            .unwrap();
        diesel::sql_query("INSERT INTO characters VALUES ($1, $2, NULL)")
            .bind::<diesel::sql_types::Uuid, _>(cid)
            .bind::<diesel::sql_types::Uuid, _>(user)
            .execute(&mut conn)
            .await
            .unwrap();

        // Fights 1..=25 under the OLD flow (no card weapon): fight 25 mints the
        // legacy claimable gift, as production did before this change.
        assert_eq!(
            account_fights(&mut conn, user).await.unwrap(),
            0,
            "no row yet"
        );
        for n in 1..=24 {
            record_finished_fight(&mut conn, user, cid, None, n, None)
                .await
                .unwrap();
        }
        assert_eq!(account_fights(&mut conn, user).await.unwrap(), 24);
        assert!(!weapon_due_on_card(Some(23)), "fight 24 is not a milestone");
        assert!(weapon_due_on_card(Some(24)), "fight 25 is");
        assert!(!weapon_due_on_card(Some(25)), "fight 26 is not");
        assert!(
            !weapon_due_on_card(None),
            "an unknown count never promises one"
        );
        let legacy = record_finished_fight(&mut conn, user, cid, None, 25, None)
            .await
            .unwrap();
        let (legacy_id, _) = legacy.gift.expect("the legacy claimable gift");
        assert!(legacy.delivered.is_none());

        // Fights 26..=49 with a (stale) card weapon each time: none is a
        // milestone, so nothing is granted or recorded.
        for n in 26..=49 {
            let w = weapon_instance(Uuid::from_u128(1000 + n as u128), OCT_2026);
            let t = record_finished_fight(&mut conn, user, cid, None, n, Some(&w))
                .await
                .unwrap();
            assert!(
                t.delivered.is_none() && t.gift.is_none(),
                "fight {n}: nothing"
            );
        }

        // Fight 50 with the card's weapon: delivered, recorded as granted.
        let shown = weapon_instance(Uuid::from_u128(50_000), OCT_2026);
        let t = record_finished_fight(&mut conn, user, cid, None, 50, Some(&shown))
            .await
            .unwrap();
        assert_eq!(t.fights, 50);
        assert_eq!(
            t.delivered.as_ref(),
            Some(&shown),
            "the card's very instance"
        );
        assert!(t.gift.is_none(), "not a claimable gift");
        let rows = gift_rows(&mut conn).await;
        assert_eq!(rows.len(), 2);
        let delivered_row = rows.iter().find(|r| r.milestone == 50).unwrap();
        assert_eq!(delivered_row.id, shown.id, "recorded under the instance id");
        assert_eq!(delivered_row.granted_at, Some(50), "born granted");
        assert_eq!(
            delivered_row.gift_def().unwrap().items[0].item_template_id,
            shown.item.item_template_id
        );

        // Only the legacy gift is advertised, and it still claims exactly once.
        let pending = pending_for_live_alt(&mut conn, cid).await.unwrap();
        assert_eq!(
            pending.iter().map(|g| g.id).collect::<Vec<_>>(),
            vec![legacy_id]
        );
        assert!(
            find(&mut conn, legacy_id, cid, true)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            mark_granted(&mut conn, legacy_id, 99).await.unwrap(),
            "legacy gift claims"
        );
        assert!(
            !mark_granted(&mut conn, legacy_id, 100).await.unwrap(),
            "once"
        );
        assert!(
            !mark_granted(&mut conn, shown.id, 101).await.unwrap(),
            "delivered row is not claimable"
        );
        assert!(
            pending_for_live_alt(&mut conn, cid)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
