//! The arena trophy track **above 2500, up to 10000** — a HauDrauf rule for the
//! common good (tracker #625, 2026-11-25), shipped under the owner's standing
//! pre-approval for his reversible rules. NOT RETAIL: retail's ladder ends at
//! arena 6 level 1 (2500 cups) and paid nothing beyond it.
//!
//! His request: "Übernehme dabei die Belohnungen von 2000 bis 2500. Einmal alle 50
//! Punkte die gleichen Belohnungen und alle 500 Punkte die legendäre Truhe.
//! Edelsteine und Rüstungsteile nicht mehr bei den 500er Stufen, sondern als
//! Geschenk wenn man in die Stadt geht 3x das höchste Donnerstagspaket mit
//! Zutaten. Das ganze bitte bis 10000 einrichten."
//!
//! # The table
//!
//! For every 50-cup step `t` in `2550..=10000`:
//!
//! * `t` a multiple of 500 (3000, 3500, ... 10000): one **Legendary chest**
//!   (rarity 5, what 2500 pays), and a **town gift** of 3x the Heroic Enchanter
//!   pack — the highest of the Enchanter's Thursday packs, ingredients only
//!   ([`HEROIC_ENCHANTER_PACK`], contents from `global_shop_grants.json`). No
//!   Gems and no Blades armour piece at these steps.
//! * otherwise: exactly what the rung `2000 + t % 500` pays — its
//!   `rewards_once_reached` chest and its fixed loot-table stackables at the
//!   character's level ([`template_rung`]). Retail has no rung at 2450, so the
//!   copy of it (2950, 3450, ...) pays nothing, as 2450 does.
//!
//! The steps ride the existing promotion path (`arena_ladder::promotion_rewards_in_season`):
//! a step pays once, when the season's high-water mark first crosses it, on the
//! match-end card and durably in the same transaction. The arena/level the card
//! reports stays arena 6 level 1 — the client's own ladder (`loot.json` in the
//! APK) has no rungs past 2500, so no new tier is invented for it.
//!
//! # From which season
//!
//! "Für den nächsten Monat": from the season after the one live when this was
//! written. The seasons it does NOT apply to are named ([`SEASONS_BEFORE_TRACK`]),
//! so a match is judged by the season its character is stamped with — never by
//! the wall clock, which would switch it on mid-season.
//!
//! # Revert
//!
//! `ARENA_TROPHY_TRACK_10K=0` in the arena env turns the whole track off without
//! a rebuild (gifts already earned stay claimable).

use std::collections::HashMap;
use std::sync::OnceLock;

use blades_lib::economy::RewardGrant;
use blades_lib::server_state::{ArenaTrophyGift, ServerState};
use blades_lib::static_data::{GiftDef, GiftItem};
use uuid::{Uuid, uuid};

use super::arena_ladder::{ARENA_LADDER, ArenaTier};

/// The track starts above this many cups (exclusive): retail's last rung.
pub const TRACK_FLOOR: i64 = 2500;
/// The last step paid (inclusive).
pub const TRACK_CEILING: i64 = 10_000;
/// A step every this many cups.
pub const TRACK_STEP: i64 = 50;
/// Every this many cups, the Legendary chest and the town gift.
pub const GIFT_STEP: i64 = 500;
/// The chest a 500-cup step pays: Legendary, as at 2500.
pub const GIFT_STEP_CHEST_RARITY: u8 = 5;
/// The 500-cup band whose rungs the other steps copy (2000..2500).
pub const TEMPLATE_BAND_START: i64 = 2000;
/// `IAP_GemPack05_d_HeroicEnchanter` — the highest of the Enchanter's Thursday
/// window packs (Apprentice 350 / Journeyman 350 / Expert 480 / Heroic 674 items).
pub const HEROIC_ENCHANTER_PACK: Uuid = uuid!("6ec8f67f-2cef-41aa-a7fc-f46237ae809c");
/// Packs per town gift.
pub const PACKS_PER_GIFT: u64 = 3;
/// Server-side description of the gift (the client shows the generic gift copy).
pub const GIFT_DESCRIPTION: &str = "Arena: 3x Heroic Enchanter pack for a 500-cup step";

/// Seasons the track does not apply to: everything up to and including the one
/// live on 2026-11-25 (season 3, ends 2026-11-28). From season 4 on, it applies.
pub const SEASONS_BEFORE_TRACK: [Uuid; 4] = [
    // The compile-time season `arena_season::SEASON_2026_09`.
    uuid!("9b3f1c74-5a20-4f8e-9d61-2c07ab54e319"),
    // Production `arena_seasons` 1, 2 and 3.
    uuid!("6d26822b-ef37-4c54-9572-29344096464a"),
    uuid!("caee78e3-1bca-4a5c-9f77-d91761f15155"),
    uuid!("f1f9ee77-023d-4abf-ba1a-8873980e9900"),
];

/// The kill switch: `ARENA_TROPHY_TRACK_10K=0|false|off|no` turns the track off.
/// Unset = on (the season gate still applies).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("ARENA_TROPHY_TRACK_10K")
            .map(|v| !matches!(v.trim(), "0" | "false" | "off" | "no"))
            .unwrap_or(true)
    })
}

/// Whether the track applies in the season a character is stamped with. An
/// unstamped character (nil) is not in a season and gets nothing past 2500.
pub fn applies_to_season(season_id: Uuid) -> bool {
    !season_id.is_nil() && !SEASONS_BEFORE_TRACK.contains(&season_id)
}

/// [`enabled`] and [`applies_to_season`].
pub fn active_for(season_id: Uuid) -> bool {
    enabled() && applies_to_season(season_id)
}

/// What one step above 2500 pays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepReward {
    /// A copy of a rung in `2050..=2400`.
    Copy(&'static ArenaTier),
    /// A 500-cup step: Legendary chest + town gift.
    GiftStep,
}

/// The 2000-2500 rung a step copies: `2000 + t % 500`. `None` at a 500-cup
/// step and at offset 450, where retail has no rung.
pub fn template_rung(threshold: i64) -> Option<&'static ArenaTier> {
    let offset = threshold.rem_euclid(GIFT_STEP);
    if offset == 0 {
        return None;
    }
    ARENA_LADDER
        .iter()
        .find(|t| t.required_trophies == TEMPLATE_BAND_START + offset)
}

/// What the step at `threshold` pays, or `None` when it is not a step of the
/// track or pays nothing (the 2450 copies).
pub fn step_reward(threshold: i64) -> Option<StepReward> {
    if threshold <= TRACK_FLOOR || threshold > TRACK_CEILING || threshold % TRACK_STEP != 0 {
        return None;
    }
    if threshold % GIFT_STEP == 0 {
        return Some(StepReward::GiftStep);
    }
    template_rung(threshold).map(StepReward::Copy)
}

/// Every paying step crossed when the high-water mark moves `old -> new`
/// (exclusive of `old`, inclusive of `new`), ascending.
pub fn steps_crossed(old_high_water: i64, new_high_water: i64) -> Vec<(i64, StepReward)> {
    let lo = old_high_water.max(TRACK_FLOOR);
    let hi = new_high_water.min(TRACK_CEILING);
    if hi <= lo {
        return Vec::new();
    }
    let first = (lo / TRACK_STEP + 1) * TRACK_STEP;
    (first..=hi)
        .step_by(TRACK_STEP as usize)
        .filter_map(|t| step_reward(t).map(|r| (t, r)))
        .collect()
}

/// The town gift's `globalGiftId`: deterministic per (character, season, step),
/// so a step can only ever mint one gift however often it is recorded.
pub fn gift_id(character_id: Uuid, season_id: Uuid, threshold: i64) -> Uuid {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"arena-trophy-track-gift\0");
    h.update(character_id.as_bytes());
    h.update(season_id.as_bytes());
    h.update(threshold.to_be_bytes());
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::new_v8(bytes)
}

/// Record the town gift for a 500-cup step. `false` when it already exists.
pub fn record_gift(
    state: &mut ServerState,
    character_id: Uuid,
    season_id: Uuid,
    threshold: i64,
    now_secs: i64,
) -> bool {
    let id = gift_id(character_id, season_id, threshold);
    if state.arena_trophy_gifts.contains_key(&id) {
        return false;
    }
    state.arena_trophy_gifts.insert(
        id,
        ArenaTrophyGift {
            threshold,
            season_id,
            created_at: now_secs,
        },
    );
    true
}

/// Whether `gift_id` is one of this state's trophy gifts, and claimed.
pub fn is_claimed(state: &ServerState, gift_id: &Uuid) -> bool {
    state.gift_claims.get(gift_id).copied().unwrap_or(0) > 0
}

/// Unclaimed trophy gifts, oldest first.
pub fn pending(state: &ServerState) -> Vec<(Uuid, ArenaTrophyGift)> {
    let mut out: Vec<_> = state
        .arena_trophy_gifts
        .iter()
        .filter(|(id, _)| !is_claimed(state, id))
        .map(|(id, g)| (*id, g.clone()))
        .collect();
    out.sort_by_key(|(id, g)| (g.created_at, g.threshold, *id));
    out
}

/// The gift as a `globalGiftOverride`: the Heroic Enchanter pack's stackables
/// times [`PACKS_PER_GIFT`], claimable once, no window. `None` when the pack is
/// missing from the shop grants (the static data failed to load).
pub fn gift_def(gift_id: Uuid, shop_grants: &HashMap<Uuid, RewardGrant>) -> Option<GiftDef> {
    let pack = shop_grants.get(&HEROIC_ENCHANTER_PACK)?;
    if pack.stackable_items.is_empty() && pack.currencies.is_empty() {
        return None;
    }
    let mut items: Vec<GiftItem> = pack
        .stackable_items
        .iter()
        .chain(pack.currencies.iter())
        .map(|(template, quantity)| GiftItem {
            item_template_id: *template,
            quantity: quantity.saturating_mul(PACKS_PER_GIFT),
            arcane_tier: None,
            enchanting: Vec::new(),
        })
        .collect();
    items.sort_by_key(|i| i.item_template_id);
    Some(GiftDef {
        global_gift_id: gift_id,
        items,
        chests: Vec::new(),
        start_time: 0,
        end_time: 0,
        claim_count_limit: 1,
        description: Some(GIFT_DESCRIPTION.into()),
    })
}

/// The live alt's unclaimed trophy gifts, read from its `server_state`. What
/// `/announcements` advertises.
pub async fn pending_for_character(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
) -> anyhow::Result<Vec<(Uuid, ArenaTrophyGift)>> {
    use diesel::{ExpressionMethods, OptionalExtension, QueryDsl};
    use diesel_async::RunQueryDsl;
    let state: Option<serde_json::Value> = {
        use crate::schema::characters::dsl as c;
        c::characters
            .filter(c::id.eq(character_id))
            .select(c::server_state)
            .first(conn)
            .await
            .optional()?
    };
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let state: ServerState = serde_json::from_value(state)?;
    Ok(pending(&state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::arena_ladder;

    const SEASON_4: Uuid = uuid!("3ee6c4e6-8f46-4a17-bba6-9c30b36bd179");
    const SEASON_3: Uuid = uuid!("f1f9ee77-023d-4abf-ba1a-8873980e9900");

    fn shop_grants() -> HashMap<Uuid, RewardGrant> {
        serde_json::from_str(include_str!(
            "../../../deploy/static/global_shop_grants.json"
        ))
        .expect("global_shop_grants.json parses")
    }

    /// 2550..=10000 in steps of 50 is 150 steps; 15 are 500-cup steps
    /// (3000..=10000) and 15 are copies of the rung-less 2450, which pay nothing.
    #[test]
    fn the_track_has_a_step_every_50_cups_to_10000() {
        let steps = steps_crossed(0, 1_000_000);
        assert_eq!(steps.first().map(|s| s.0), Some(2550));
        assert_eq!(steps.last().map(|s| s.0), Some(10_000));
        let gifts: Vec<i64> = steps
            .iter()
            .filter(|(_, r)| *r == StepReward::GiftStep)
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(gifts, (3000..=10_000).step_by(500).collect::<Vec<_>>());
        assert_eq!(steps.len(), 150 - 15);
        for t in (2950..=9950).step_by(500) {
            assert_eq!(step_reward(t), None, "{t} copies 2450, which has no rung");
        }
        assert_eq!(step_reward(10_050), None, "nothing past 10000");
        assert_eq!(step_reward(2500), None, "2500 stays retail's own rung");
        assert_eq!(step_reward(2525), None, "not a 50-cup step");
    }

    /// Every non-500 step pays exactly what its 2000-2500 rung pays.
    #[test]
    fn a_50_cup_step_copies_its_2000_to_2500_rung() {
        for (t, want) in [
            (2550, 2050),
            (2600, 2100),
            (3200, 2200),
            (5400, 2400),
            (9850, 2350),
        ] {
            let Some(StepReward::Copy(rung)) = step_reward(t) else {
                panic!("{t} should copy a rung");
            };
            assert_eq!(rung.required_trophies, want);
        }
        // Copied chests: 2050 Gold, 2200 Silver.
        assert_eq!(template_rung(2550).unwrap().chests_once_reached, &[3]);
        assert_eq!(template_rung(3200).unwrap().chests_once_reached, &[2]);
    }

    #[test]
    fn crossing_is_exclusive_of_old_and_inclusive_of_new() {
        let at = |o, n| {
            steps_crossed(o, n)
                .into_iter()
                .map(|s| s.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(2549, 2550), vec![2550]);
        assert_eq!(at(2550, 2599), Vec::<i64>::new());
        assert_eq!(at(2990, 3010), vec![3000]);
        assert_eq!(at(2400, 2600), vec![2550, 2600], "2500 is the ladder's");
        assert_eq!(at(9990, 12_000), vec![10_000]);
        assert_eq!(at(3000, 2000), Vec::<i64>::new(), "a drop crosses nothing");
    }

    #[test]
    fn the_track_starts_with_the_season_after_season_3() {
        assert!(applies_to_season(SEASON_4));
        assert!(applies_to_season(Uuid::new_v4()), "any later season");
        assert!(!applies_to_season(SEASON_3));
        assert!(!applies_to_season(arena_season_2026_09()));
        assert!(!applies_to_season(Uuid::nil()));
    }

    fn arena_season_2026_09() -> Uuid {
        crate::arena::arena_season::SEASON_2026_09.id
    }

    /// The gift is 3x the Heroic Enchanter pack: 674 items a pack, 2022 a gift,
    /// ingredients only.
    #[test]
    fn the_town_gift_is_three_heroic_enchanter_packs() {
        let grants = shop_grants();
        let def = gift_def(Uuid::nil(), &grants).expect("pack present");
        let total: u64 = def.items.iter().map(|i| i.quantity).sum();
        assert_eq!(total, 3 * 674);
        assert_eq!(def.items.len(), 11);
        assert_eq!(def.claim_count_limit, 1);
        assert!(
            def.items
                .iter()
                .all(|i| !blades_lib::economy::is_currency(i.item_template_id)),
            "no Gems in the gift"
        );
        // The pack is bigger than the Journeyman and Expert ones (Apprentice is
        // not in the recorded grants).
        for tier in [
            uuid!("ef234f68-8ee2-4fdd-8fbe-a8f9bf70fc21"),
            uuid!("69ab6e05-8968-41a2-b45e-997c8534e166"),
        ] {
            let n: u64 = grants[&tier].stackable_items.values().sum();
            assert!(n < 674, "{tier} has {n}");
        }
        assert!(gift_def(Uuid::nil(), &HashMap::new()).is_none());
    }

    /// One gift per (character, season, step), however often it is recorded; a
    /// claimed gift is no longer pending; the next season mints a fresh one.
    #[test]
    fn a_step_mints_its_town_gift_once() {
        let ch = Uuid::from_u128(7);
        let mut state = ServerState::default();
        assert!(record_gift(&mut state, ch, SEASON_4, 3000, 10));
        assert!(!record_gift(&mut state, ch, SEASON_4, 3000, 20));
        assert!(record_gift(&mut state, ch, SEASON_4, 3500, 30));
        assert_eq!(state.arena_trophy_gifts.len(), 2);
        let pending_ids: Vec<i64> = pending(&state).iter().map(|(_, g)| g.threshold).collect();
        assert_eq!(pending_ids, vec![3000, 3500]);

        let first = gift_id(ch, SEASON_4, 3000);
        state.gift_claims.insert(first, 1);
        assert_eq!(pending(&state).len(), 1);
        assert!(is_claimed(&state, &first));

        let next_season = Uuid::from_u128(99);
        assert_ne!(gift_id(ch, next_season, 3000), first);
        assert_ne!(gift_id(Uuid::from_u128(8), SEASON_4, 3000), first);
    }

    /// The new field round-trips and an old row (without it) still loads.
    #[test]
    fn server_state_keeps_trophy_gifts() {
        let mut state = ServerState::default();
        record_gift(&mut state, Uuid::from_u128(1), SEASON_4, 4000, 5);
        let json = serde_json::to_value(&state).unwrap();
        // The exact path the claim route probes (`global_gift::trophy_gift_probe`).
        let id = gift_id(Uuid::from_u128(1), SEASON_4, 4000);
        assert!(json["arenaTrophyGifts"].get(id.to_string()).is_some());
        let back: ServerState = serde_json::from_value(json).unwrap();
        assert_eq!(back.arena_trophy_gifts, state.arena_trophy_gifts);
        let old: ServerState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(old.arena_trophy_gifts.is_empty());
        assert!(
            serde_json::to_value(&ServerState::default())
                .unwrap()
                .get("arenaTrophyGifts")
                .is_none()
        );
    }

    /// The match path's promotion: a 500-cup step pays the Legendary chest and
    /// a town gift — no Gems, no Blades piece; a 50-cup step pays its copied
    /// rung's chest and fixed loot.
    #[test]
    fn promotion_rewards_pay_the_track_above_2500() {
        let gift = arena_ladder::promotion_rewards_with_track(2990, 3010, 86, true);
        assert_eq!(gift.chests, vec![(5, 86)]);
        assert!(gift.currencies.is_empty(), "no Gems at a 500-cup step");
        assert!(gift.items.is_empty(), "no Blades piece at a 500-cup step");
        assert!(gift.stackable_items.is_empty());
        assert_eq!(gift.town_gift_thresholds, vec![3000]);
        assert_eq!(gift.new_tier, None, "the ladder tier does not move");

        // 2600 copies 2100: Arena5 level 3's fixed loot at level 86, no chest.
        let copy = arena_ladder::promotion_rewards_with_track(2560, 2610, 86, true);
        let rung_2100 = arena_ladder::promotion_rewards(2090, 2110, 86);
        assert_eq!(copy.chests, rung_2100.chests);
        assert_eq!(copy.stackable_items, rung_2100.stackable_items);
        assert!(!copy.stackable_items.is_empty());
        assert_eq!(copy.loot_thresholds, vec![2600]);
        assert!(copy.town_gift_thresholds.is_empty());

        // Across 2500: the retail rung untouched (Legendary, 600 Gems, Sword +
        // Helmet), then 2550 (Gold, as 2050) and 2600.
        let across = arena_ladder::promotion_rewards_with_track(2490, 2600, 50, true);
        let retail = arena_ladder::promotion_rewards(2490, 2510, 50);
        assert_eq!(across.chests, vec![(5, 50), (3, 50)]);
        assert_eq!(across.currencies, retail.currencies);
        assert_eq!(across.items, retail.items);
        assert_eq!(across.loot_thresholds, vec![2500, 2600]);

        // A long climb pays every step once: 2501 -> 10000 is 15 Legendary
        // chests and gifts, plus the copied Gold/Silver chests.
        let all = arena_ladder::promotion_rewards_with_track(2501, 20_000, 86, true);
        assert_eq!(all.town_gift_thresholds.len(), 15);
        assert_eq!(all.chests.iter().filter(|c| c.0 == 5).count(), 15);
        assert_eq!(all.chests.iter().filter(|c| c.0 == 3).count(), 15 * 4);
        assert_eq!(all.chests.iter().filter(|c| c.0 == 2).count(), 15 * 2);
    }

    /// Off for the current season, on for the next; and nothing changes below
    /// 2500 either way.
    #[test]
    fn promotion_rewards_are_gated_on_the_season() {
        let off = arena_ladder::promotion_rewards_in_season(2990, 3010, 86, SEASON_3);
        assert!(off.is_empty());
        assert!(off.town_gift_thresholds.is_empty());
        let on = arena_ladder::promotion_rewards_in_season(2990, 3010, 86, SEASON_4);
        assert_eq!(on.town_gift_thresholds, vec![3000]);
        assert_eq!(
            arena_ladder::promotion_rewards_in_season(180, 2400, 86, SEASON_4),
            arena_ladder::promotion_rewards(180, 2400, 86)
        );
        assert!(
            arena_ladder::promotion_rewards_with_track(2990, 3010, 86, false).is_empty(),
            "without the track nothing is paid past 2500"
        );
    }

    /// `pending_for_character` reads the live row's `server_state` as written.
    #[tokio::test]
    async fn pending_gifts_are_read_from_the_character_row() {
        use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
        let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — trophy gift read not verified");
            return;
        };
        let mut conn = AsyncPgConnection::establish(&url).await.unwrap();
        conn.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        let ch = Uuid::from_u128(0x625);
        let mut state = ServerState::default();
        record_gift(&mut state, ch, SEASON_4, 3000, 1);
        record_gift(&mut state, ch, SEASON_4, 3500, 2);
        state.gift_claims.insert(gift_id(ch, SEASON_4, 3000), 1);
        for stmt in [
            format!("CREATE SCHEMA {schema}"),
            format!("SET LOCAL search_path TO {schema}"),
            "CREATE TABLE characters (id UUID PRIMARY KEY, server_state JSONB NOT NULL)".into(),
        ] {
            diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
        }
        diesel::sql_query("INSERT INTO characters VALUES ($1, $2)")
            .bind::<diesel::sql_types::Uuid, _>(ch)
            .bind::<diesel::sql_types::Jsonb, _>(serde_json::to_value(&state).unwrap())
            .execute(&mut conn)
            .await
            .unwrap();
        let pending = pending_for_character(&mut conn, ch).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.threshold, 3500);
        assert!(
            pending_for_character(&mut conn, Uuid::from_u128(1))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The card's arena/level never moves past arena 6 level 1.
    #[test]
    fn the_ladder_tier_stays_arena_6_level_1() {
        let t = arena_ladder::tier_for_trophies(9_000);
        assert_eq!((t.arena, t.level), (6, 1));
    }
}
