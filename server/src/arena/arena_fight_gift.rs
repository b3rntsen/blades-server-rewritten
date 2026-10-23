//! The arena weapon gift — an **owner-approved deviation from retail** (tracker
//! #390/#391, approved by the owner 2026-10-16, proposed by HauDrauf to bring
//! players to the arena).
//!
//! After every [`FIGHTS_PER_WEAPON_GIFT`]th finished arena fight, won or lost, the
//! player is gifted one of the nine Divine Stalhrim weapons, chosen uniformly, as a
//! BLANK Arcane II weapon ([`WEAPON_GIFT_ARCANE_TIER`], no enchantments).
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
//! "Divine Stalhrim" is the APK's Dragonbone-strength Stalhrim: every one is
//! material tier 10 at required level 45 with exactly its Dragonbone twin's
//! durability and weapon type ("as strong as those made of dragon bones"). The
//! Divine templates are not in the Legendary loot pool (their `lootWeight` is 0).
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

/// The nine Divine Stalhrim weapons, each with the Dragonbone twin whose
/// enchanting tables it shares (same type, weapon type, tier 10, level 45). Ids
/// from `reference/game-defs/items.json`, `editor_name` "Divine Stalhrim …".
pub const DIVINE_STALHRIM_WEAPONS: [(Uuid, Uuid, &str); 9] = [
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
];

/// Whether an account's running fight count just reached a gift.
pub fn milestone_reached(fights: i64) -> bool {
    fights > 0 && fights % FIGHTS_PER_WEAPON_GIFT == 0
}

/// The weapon a gift pays, drawn from its own id so the roll is reproducible
/// from the row alone: a uniform pick of the nine, blank at Arcane II.
pub fn roll_weapon(gift_id: Uuid) -> GiftItem {
    let seed = crate::jewelry_roll::seed(&[gift_id.as_bytes(), b"arena-fight-gift"], 0);
    let (template, twin, name) =
        DIVINE_STALHRIM_WEAPONS[(seed % DIVINE_STALHRIM_WEAPONS.len() as u64) as usize];
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

/// The nine weapons' new (temper 0) max durability, installed once at startup
/// from the server's durability table so the synchronous match-end card can
/// build the same instance the writer persists.
static WEAPON_DURABILITY: OnceLock<HashMap<Uuid, f64>> = OnceLock::new();

/// Record the nine weapons' max durability from the loaded durability table.
/// Idempotent: the first install wins.
pub fn install_durability(repair: &blades_lib::features::repair::RepairData) {
    let table = DIVINE_STALHRIM_WEAPONS
        .iter()
        .filter_map(|(divine, _, _)| Some((*divine, repair.max_durability(*divine, 0)?)))
        .collect::<HashMap<_, _>>();
    if table.len() != DIVINE_STALHRIM_WEAPONS.len() {
        log::warn!(
            "arena fight gift: durability known for {} of {} Divine Stalhrim weapons; the rest \
             arrive at the game's default {}",
            table.len(),
            DIVINE_STALHRIM_WEAPONS.len(),
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

/// The weapon instance a milestone fight pays, with `item_id` as both the
/// instance id and the roll seed: new (temper 0, full durability), blank, Arcane
/// II — the same instance [`FightGiftRow::gift_def`] + `build_gift_reward`
/// produce for a claimed gift.
pub fn weapon_instance(item_id: Uuid) -> RewardItem {
    let roll = roll_weapon(item_id);
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
        return Ok(FightTally { fights, gift: None, delivered: None });
    }
    // The card's weapon is delivered directly: its row is born granted, under the
    // instance's own id. Otherwise a fresh claimable gift, exactly as before.
    let (gift_id, item, granted_at) = match card_weapon {
        Some(w) => (w.id, gift_item_of(w), Some(now_secs)),
        None => {
            let id = Uuid::new_v4();
            (id, roll_weapon(id), None)
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
        return Ok(FightTally { fights, gift: None, delivered: None });
    }
    Ok(match card_weapon {
        Some(w) => FightTally { fights, gift: None, delivered: Some(w.clone()) },
        None => FightTally { fights, gift: Some((gift_id, item)), delivered: None },
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
    let row: Option<Alt> = diesel::sql_query("SELECT source_alt_uuid FROM characters WHERE id = $1")
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

    /// The nine ids are the APK's Divine Stalhrim weapons, and each twin is the
    /// Dragonbone weapon of the same weapon type, tier and durability. Checked
    /// against the capture repository's item table when it is present.
    #[test]
    fn the_nine_weapons_are_divine_stalhrim_with_dragonbone_twins() {
        let ids: std::collections::BTreeSet<Uuid> =
            DIVINE_STALHRIM_WEAPONS.iter().map(|w| w.0).collect();
        assert_eq!(ids.len(), 9, "nine distinct weapons");
        let items = items_json();
        if items.is_null() {
            eprintln!("SKIP: blades-capture items.json not found — ids not cross-checked");
            return;
        }
        for (divine, twin, name) in DIVINE_STALHRIM_WEAPONS {
            let d = &items[divine.to_string()];
            let t = &items[twin.to_string()];
            assert_eq!(d["editor_name"], name);
            assert_eq!(d["type_name"], "weapon");
            assert!(t["editor_name"].as_str().unwrap().starts_with("Dragonbone "), "{name} twin");
            for field in ["tier", "requiredLevel", "maxDurability"] {
                assert_eq!(d[field], t[field], "{name} {field} matches its Dragonbone twin");
            }
            assert_eq!(d["stats"]["weaponType"], t["stats"]["weaponType"], "{name} weapon type");
        }
    }

    /// Every gift is one of the nine, blank (no ENCHANTING) at Arcane II, and all
    /// nine come up.
    #[test]
    fn every_gift_is_a_blank_arcane_two_divine_stalhrim() {
        let mut seen = std::collections::BTreeSet::new();
        for n in 0..400u128 {
            let item = roll_weapon(Uuid::from_u128(n * 7919 + 1));
            assert!(DIVINE_STALHRIM_WEAPONS.iter().any(|w| w.0 == item.item_template_id));
            assert_eq!(item.quantity, 1);
            assert_eq!(item.arcane_tier, Some(2));
            assert!(item.enchanting.is_empty(), "gifted blank, enchanted by the player");
            seen.insert(item.item_template_id);
        }
        assert_eq!(seen.len(), 9, "the pick reaches all nine weapons");
        // The pick is a function of the gift id: GET and claim agree.
        let id = Uuid::from_u128(42);
        assert_eq!(roll_weapon(id).item_template_id, roll_weapon(id).item_template_id);
    }

    /// Arcane II is what guarantees the two bonus enchantments: the shipped
    /// enchanting tables give an arcane-2 item exactly 2 secondaries when it is
    /// enchanted (`craft.rs::roll_enchant` reads these odds).
    #[test]
    fn arcane_two_guarantees_exactly_two_bonus_enchantments_on_enchant() {
        let e: blades_lib::static_data::EnchantingData = serde_json::from_str(include_str!(
            "../../../deploy/static/enchanting.json"
        ))
        .unwrap();
        assert_eq!(e.arcane_tier_count_odds[&WEAPON_GIFT_ARCANE_TIER], vec![0.0, 0.0, 1.0]);
        for (divine, _, name) in DIVINE_STALHRIM_WEAPONS {
            assert!(e.table_for(&divine).is_some(), "{name} has a secondary table to draw from");
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
        let grants: serde_json::Value =
            serde_json::from_str(include_str!("../../../deploy/static/global_shop_grants.json"))
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
        assert!(retail.len() >= 3, "captured Arcane02 Dragonbone weapon grants exist");

        let durability: serde_json::Value =
            serde_json::from_str(include_str!("../../../deploy/static/item_durability.json"))
                .unwrap();
        let repair =
            blades_lib::features::repair::RepairData::from_json(&durability, &serde_json::json!({}));

        for captured in &retail {
            let twin: Uuid = captured["itemTemplateId"].as_str().unwrap().parse().unwrap();
            let (divine, _, name) =
                *DIVINE_STALHRIM_WEAPONS.iter().find(|w| w.1 == twin).unwrap();
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
                blades_lib::game_data::GameDataItem { name: name.into(), r#type: 2 },
            )]);
            let reward = blades_lib::features::gifts::build_gift_reward(&def, &types, &repair);
            assert_eq!(reward.items.len(), 1, "exactly one weapon");
            assert!(reward.stackable_items.is_empty() && reward.currencies.is_empty());
            let granted = &reward.items[0].item;
            assert_eq!(granted.item_template_id, divine);
            assert_eq!(granted.arcane_tier, Some(2));
            assert_eq!(granted.grade, None);
            assert_eq!(granted.tempering_level, 0);
            assert!(granted.properties.enchanting.is_empty() && granted.properties.grading.is_empty());

            let mut ours = serde_json::to_value(granted).unwrap();
            let mut theirs = captured.clone();
            ours.as_object_mut().unwrap().remove("id");
            theirs.as_object_mut().unwrap().remove("id");
            theirs["itemTemplateId"] = serde_json::json!(divine);
            // An empty `properties` object is how every server-built item
            // serializes; retail omitted it. The Sigil path emits the same.
            if ours.get("properties").is_some_and(|p| {
                p.as_object().is_some_and(|o| o.values().all(|v| v.as_array().is_some_and(|a| a.is_empty())))
            }) {
                ours.as_object_mut().unwrap().remove("properties");
            }
            assert_eq!(ours, theirs, "{name}: same shape and durability as retail's Arcane02 {twin}");

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
        conn.begin_test_transaction().await.expect("test transaction");
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
                    record_finished_fight(c, Uuid::from_u128(9), Uuid::from_u128(1), None, 1, None).await
                }
                .scope_boxed()
            })
            .await;
        assert!(counted.is_err(), "no arena_account_fight_counts table: the count fails");
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
                let t = record_finished_fight(&mut conn, user, cid, Some(alt_a), n, None).await.unwrap();
                assert_eq!((t.fights, t.gift.is_some()), (n, false));
            }
            let t = record_finished_fight(&mut conn, other, other_cid, None, n, None).await.unwrap();
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
            let t = record_finished_fight(&mut conn, user, cid, Some(alt_b), n, None).await.unwrap();
            assert_eq!((t.fights, t.gift.is_some()), (n, false), "fight {n} as alt B");
        }
        assert!(gift_rows(&mut conn).await.is_empty(), "nothing before the 25th fight");

        let t = record_finished_fight(&mut conn, user, cid, Some(alt_b), 25, None).await.unwrap();
        assert_eq!(t.fights, 25, "alts share the account's count");
        let (gift_id, item) = t.gift.expect("the 25th fight mints a gift");
        assert!(DIVINE_STALHRIM_WEAPONS.iter().any(|w| w.0 == item.item_template_id));
        assert!(item.enchanting.is_empty(), "blank: Arcane II, enchanted by the player");
        assert_eq!(item.arcane_tier, Some(2));

        let rows = gift_rows(&mut conn).await;
        assert_eq!(rows.len(), 1, "exactly one gift");
        assert_eq!((rows[0].id, rows[0].user_id, rows[0].milestone), (gift_id, user, 25));
        assert_eq!(rows[0].source_alt_uuid, Some(alt_b), "the alt that fought the 25th");
        assert_eq!(rows[0].gift_def().unwrap().items[0].item_template_id, item.item_template_id);

        // The 26th fight mints nothing more; the other account is still at 24.
        let t = record_finished_fight(&mut conn, user, cid, Some(alt_b), 26, None).await.unwrap();
        assert!(t.gift.is_none());
        assert_eq!(gift_rows(&mut conn).await.len(), 1);

        // Offered to alt B (live now), not to alt A.
        let pending = pending_for_live_alt(&mut conn, cid).await.unwrap();
        assert_eq!(pending.iter().map(|g| g.id).collect::<Vec<_>>(), vec![gift_id]);
        diesel::sql_query("UPDATE characters SET source_alt_uuid = $1 WHERE id = $2")
            .bind::<diesel::sql_types::Uuid, _>(alt_a)
            .bind::<diesel::sql_types::Uuid, _>(cid)
            .execute(&mut conn)
            .await
            .unwrap();
        assert!(pending_for_live_alt(&mut conn, cid).await.unwrap().is_empty());
        assert!(pending_for_live_alt(&mut conn, other_cid).await.unwrap().is_empty());

        // Claiming is once only.
        assert!(find(&mut conn, gift_id, cid, true).await.unwrap().is_some());
        assert!(find(&mut conn, gift_id, other_cid, false).await.unwrap().is_none());
        assert!(mark_granted(&mut conn, gift_id, 99).await.unwrap());
        assert!(!mark_granted(&mut conn, gift_id, 100).await.unwrap(), "no second claim");
        assert_eq!(find(&mut conn, gift_id, cid, false).await.unwrap().unwrap().claim_count(), 1);
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
        assert_eq!(account_fights(&mut conn, user).await.unwrap(), 0, "no row yet");
        for n in 1..=24 {
            record_finished_fight(&mut conn, user, cid, None, n, None).await.unwrap();
        }
        assert_eq!(account_fights(&mut conn, user).await.unwrap(), 24);
        assert!(!weapon_due_on_card(Some(23)), "fight 24 is not a milestone");
        assert!(weapon_due_on_card(Some(24)), "fight 25 is");
        assert!(!weapon_due_on_card(Some(25)), "fight 26 is not");
        assert!(!weapon_due_on_card(None), "an unknown count never promises one");
        let legacy = record_finished_fight(&mut conn, user, cid, None, 25, None).await.unwrap();
        let (legacy_id, _) = legacy.gift.expect("the legacy claimable gift");
        assert!(legacy.delivered.is_none());

        // Fights 26..=49 with a (stale) card weapon each time: none is a
        // milestone, so nothing is granted or recorded.
        for n in 26..=49 {
            let w = weapon_instance(Uuid::from_u128(1000 + n as u128));
            let t = record_finished_fight(&mut conn, user, cid, None, n, Some(&w)).await.unwrap();
            assert!(t.delivered.is_none() && t.gift.is_none(), "fight {n}: nothing");
        }

        // Fight 50 with the card's weapon: delivered, recorded as granted.
        let shown = weapon_instance(Uuid::from_u128(50_000));
        let t = record_finished_fight(&mut conn, user, cid, None, 50, Some(&shown)).await.unwrap();
        assert_eq!(t.fights, 50);
        assert_eq!(t.delivered.as_ref(), Some(&shown), "the card's very instance");
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
        assert_eq!(pending.iter().map(|g| g.id).collect::<Vec<_>>(), vec![legacy_id]);
        assert!(find(&mut conn, legacy_id, cid, true).await.unwrap().is_some());
        assert!(mark_granted(&mut conn, legacy_id, 99).await.unwrap(), "legacy gift claims");
        assert!(!mark_granted(&mut conn, legacy_id, 100).await.unwrap(), "once");
        assert!(
            !mark_granted(&mut conn, shown.id, 101).await.unwrap(),
            "delivered row is not claimable"
        );
        assert!(pending_for_live_alt(&mut conn, cid).await.unwrap().is_empty());
    }
}
