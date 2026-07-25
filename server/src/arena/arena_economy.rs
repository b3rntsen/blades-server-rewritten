//! **Match-end economy persistence** (Phase 5.4).
//!
//! Before this module the victory card was wire-only theatre: `engine.rs` built a
//! beautiful op49 `ResultsJSON` with gold, XP and a new trophy count, sent it, and
//! then threw it all away. Nothing in the arena path ever wrote
//! `pvp_trophies` / `pvp_winning_streak` / the wallet, so the moment the player
//! returned to the menu the client re-synced from REST and every reward vanished.
//!
//! # Why it is a queue and not a direct write
//!
//! The combat engine is synchronous and lives inside the ENet host's tick loop;
//! the database handle is an **async** diesel pool owned by the actix runtime.
//! Blocking the tick on a round-trip to Postgres would stall every other live
//! match. So the engine calls [`record`] — a non-blocking push onto an unbounded
//! channel — and a single background task drains it and applies each outcome in
//! its own transaction.
//!
//! This also keeps the write off the critical path of the card itself: the client
//! gets its op49 immediately, and the durable state catches up a few milliseconds
//! later, well before the player has walked back through
//! `BackendMatchEnd -> PostMatch -> DisconnectingPlayersAfterMatch` (15 s of
//! MatchState walk) and re-read `/characters/{id}`.
//!
//! # What gets written
//!
//! Everything lives in JSONB columns that already exist (`characters.character`,
//! `characters.wallet`, `characters.inventory`), so **no `ALTER TABLE` on
//! `characters` is required**. The only new object is the audit table
//! [`arena_match_results`](../../../migrations) — see the migration for the
//! by-hand DDL, since the migrate one-shot skips once `users` exists.

use std::sync::OnceLock;

use blades_lib::economy::{RewardGrant, apply_reward, grant_chest};
use blades_lib::user_data::InventoryChangeTracker;
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use log::{error, info, warn};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use uuid::Uuid;

use crate::DbPool;
use crate::arena::arena_ladder;
use crate::arena::combat::messages::ARENA_GOLD_CURRENCY_UUID;
use crate::models::CharacterDbEntryEconomy;

/// One finished match, from ONE player's point of view, queued for persistence.
///
/// Built by `engine.rs` at match end from the same numbers that go on the wire, so
/// the card and the database can never disagree about what was awarded.
#[derive(Debug, Clone)]
pub struct MatchEconomyOutcome {
    /// The character this reward belongs to (`characters.id`).
    pub character_id: Uuid,
    /// The match's `gameSessionId`, for the audit row. `None` for a dev/bot match.
    pub game_session_id: Option<Uuid>,
    /// Character level at match time (the reward base is a function of it).
    pub level: u16,
    /// Gold granted — identical to the value on the op49 card.
    pub gold: i64,
    /// Character XP granted — identical to the value on the op49 card.
    pub character_xp: i64,
    /// Signed trophy swing (already Elo-weighted by the engine).
    pub trophy_delta: i64,
    /// Rounds this player won (drives the chest meter).
    pub rounds_won: u8,
    /// Rounds the opponent won.
    pub rounds_lost: u8,
    /// Whether this player won the match.
    pub win: bool,
    /// The opponent's character id, for the audit row.
    pub opponent_character_id: Option<Uuid>,
}

/// The queue endpoint. `None` until [`install`] runs — which is the normal state
/// in unit tests and the offline round-trip harness, where [`record`] must be a
/// no-op rather than a panic.
static SINK: OnceLock<UnboundedSender<MatchEconomyOutcome>> = OnceLock::new();

/// Wire the persistence queue to the database and start its drain task.
///
/// Called once from `main.rs` right after the pool is built. Idempotent: a second
/// call is ignored (the `OnceLock` keeps the first sender), so a stray call cannot
/// spawn two writers racing on the same rows.
pub fn install(pool: DbPool) {
    let (tx, mut rx) = unbounded_channel::<MatchEconomyOutcome>();
    if SINK.set(tx).is_err() {
        warn!("arena economy: persistence queue already installed; ignoring second install()");
        return;
    }
    actix_web::rt::spawn(async move {
        info!("arena economy: match-end persistence writer started");
        while let Some(outcome) = rx.recv().await {
            if let Err(e) = persist(&pool, &outcome).await {
                error!(
                    "arena economy: FAILED to persist match result for character {} \
                     (gold {}, xp {}, trophies {:+}): {e}",
                    outcome.character_id, outcome.gold, outcome.character_xp, outcome.trophy_delta,
                );
            }
        }
        warn!("arena economy: persistence queue closed; match rewards are no longer durable");
    });
}

/// Queue a finished match for persistence. Non-blocking and infallible from the
/// caller's point of view: if the queue was never installed (tests, offline
/// harness) the outcome is dropped after a debug log.
pub fn record(outcome: MatchEconomyOutcome) {
    match SINK.get() {
        Some(tx) => {
            if tx.send(outcome).is_err() {
                error!("arena economy: persistence writer is gone; a match reward was lost");
            }
        }
        None => {
            log::debug!(
                "arena economy: no persistence queue installed (test/offline); \
                 dropping match result for {}",
                outcome.character_id
            );
        }
    }
}

/// Apply one match outcome durably: PvP counters + wallet + XP + any promotion
/// chests. One transaction, row-locked, so two matches ending at the same instant
/// for the same character cannot interleave.
///
/// # Why the audit row is written AFTER the transaction
///
/// `arena_match_results` is created by a migration that has to be applied by hand
/// on the box (the migrate one-shot skips once `users` exists), so there is a real
/// window where the table does not exist. In Postgres a failed statement poisons
/// the whole transaction — an `INSERT` into a missing table inside the same
/// transaction would silently roll back the reward as well, which is precisely the
/// bug this module exists to fix. Verified against a live Postgres:
///
/// ```text
/// BEGIN; UPDATE characters SET ...;  -- UPDATE 1
/// INSERT INTO table_that_does_not_exist ...;  -- ERROR
/// COMMIT;                                     -- ROLLBACK: the UPDATE is GONE
/// ```
///
/// So the reward commits on its own, and the audit row is a separate statement
/// afterwards whose failure only logs.
async fn persist(pool: &DbPool, outcome: &MatchEconomyOutcome) -> Result<(), anyhow::Error> {
    let mut conn = pool.get().await?;
    let o = outcome.clone();

    // Phase 1 — the reward itself, transactionally. Returns what the audit row
    // needs, or `None` when there was no character row to reward (bot).
    let applied: Option<AppliedOutcome> = conn
        .transaction(move |mut conn| {
        async move {
            let mut entry = {
                use crate::schema::characters;
                characters::table
                    .filter(characters::id.eq(o.character_id))
                    .select(CharacterDbEntryEconomy::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
            };
            let Some(entry) = entry.take() else {
                // A bot / starter loadout has no character row. Not an error.
                log::debug!(
                    "arena economy: no character row for {} (bot?); nothing to persist",
                    o.character_id
                );
                return Ok::<_, anyhow::Error>(None);
            };
            let mut entry = entry;

            let ch = &mut entry.character.0;
            let pre_trophies = ch.pvp_trophies;
            let pre_high_water = ch.matchmaking_pvp_trophies;

            // --- PvP counters -------------------------------------------------
            // Trophies never go below zero (retail cards bottom out at 0, never
            // negative — flapdroid sat at 0 through a 20-loss streak).
            ch.pvp_trophies = (pre_trophies + o.trophy_delta).max(0);
            // `matchmakingPvpTrophies` is the season HIGH-WATER mark: monotone
            // non-decreasing, and what the ladder promotes on. Capture-proven
            // across all 108 op49 cards.
            ch.matchmaking_pvp_trophies = pre_high_water.max(ch.pvp_trophies);
            // Streak: positive counts consecutive wins, negative consecutive
            // losses; a result of the other sign resets it to +/-1.
            ch.pvp_winning_streak = if o.win {
                if ch.pvp_winning_streak > 0 { ch.pvp_winning_streak + 1 } else { 1 }
            } else if ch.pvp_winning_streak < 0 {
                ch.pvp_winning_streak - 1
            } else {
                -1
            };
            // The chest meter counts ROUNDS won and wraps at capacity 8.
            let (meter, filled) = arena_ladder::advance_chest_meter(ch.pvp_chest_meter, o.rounds_won);
            ch.pvp_chest_meter = meter;
            ch.number_pvp_match_played += 1;

            // --- Ladder position ---------------------------------------------
            let tier = arena_ladder::tier_for_trophies(ch.matchmaking_pvp_trophies);
            ch.highest_arena_reached = tier.arena as u64;
            ch.highest_level_arena_reached = tier.level as u64;

            // --- Currency, XP and promotion chests ----------------------------
            let promo = arena_ladder::promotion_rewards(
                pre_high_water,
                ch.matchmaking_pvp_trophies,
                ch.level,
            );
            let mut reward = RewardGrant::default();
            if o.gold > 0 {
                reward
                    .currencies
                    .insert(ARENA_GOLD_CURRENCY_UUID_PARSED.clone(), o.gold as u64);
            }
            reward.character_xp = o.character_xp.max(0) as u64;

            let mut tracker = InventoryChangeTracker::default();
            apply_reward(
                &reward,
                &mut entry.wallet.0,
                &mut entry.inventory.0,
                &mut entry.character.0,
                &mut tracker,
            );

            // Ladder promotion chests (`rewards_once_reached`) plus any chest the
            // meter completed this match. Both land in the treasury exactly like a
            // quest/dungeon chest does.
            let mut granted = 0usize;
            for (rarity, level) in &promo.chests {
                grant_chest(&mut entry.inventory.0, *rarity as u64, *level as u64, &mut tracker);
                granted += 1;
            }
            for _ in 0..filled {
                // A completed chest meter pays out at the CURRENT ladder rung's
                // rarity (the tier the player is standing on).
                let rarity = tier.chests_once_reached.first().copied().unwrap_or(2);
                grant_chest(
                    &mut entry.inventory.0,
                    rarity as u64,
                    entry.character.0.level as u64,
                    &mut tracker,
                );
                granted += 1;
            }
            if granted > 0 {
                entry.inventory.0.treasury_version += 1;
            }

            let post_trophies = entry.character.0.pvp_trophies;
            let post_high_water = entry.character.0.matchmaking_pvp_trophies;
            let character_id = entry.id;

            {
                use crate::schema::characters;
                diesel::update(characters::table)
                    .filter(characters::id.eq(character_id))
                    .set(entry)
                    .execute(&mut conn)
                    .await?;
            }

            Ok(Some(AppliedOutcome {
                character_id,
                pre_trophies,
                post_trophies,
                post_high_water,
                arena: tier.arena as i32,
                arena_level: tier.level as i32,
                meter,
                granted,
            }))
        }
        .scope_boxed()
        })
        .await?;

    // No character row (bot / starter loadout) — nothing to audit either.
    let Some(a) = applied else { return Ok(()) };

    info!(
        "arena economy: persisted {} for L{} character {} — gold {:+}, xp {:+}, \
         trophies {} -> {} ({:+}), high-water {}, arena {}/{}, meter {}{}",
        if outcome.win { "WIN" } else { "LOSS" },
        outcome.level,
        a.character_id,
        outcome.gold,
        outcome.character_xp,
        a.pre_trophies,
        a.post_trophies,
        outcome.trophy_delta,
        a.post_high_water,
        a.arena,
        a.arena_level,
        a.meter,
        if a.granted > 0 { format!(", {} chest(s)", a.granted) } else { String::new() },
    );

    // Phase 2 — the audit row, OUTSIDE the transaction above (see the doc comment:
    // a missing table would otherwise roll the reward back). Failure only logs.
    let audit = diesel::sql_query(
        "INSERT INTO arena_match_results \
         (id, character_id, opponent_character_id, game_session_id, win, \
          rounds_won, rounds_lost, gold, character_xp, trophy_delta, \
          trophies_after, matchmaking_trophies_after, arena, arena_level, chest_meter) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
    .bind::<diesel::sql_types::Uuid, _>(a.character_id)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(outcome.opponent_character_id)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(outcome.game_session_id)
    .bind::<diesel::sql_types::Bool, _>(outcome.win)
    .bind::<diesel::sql_types::Integer, _>(outcome.rounds_won as i32)
    .bind::<diesel::sql_types::Integer, _>(outcome.rounds_lost as i32)
    .bind::<diesel::sql_types::BigInt, _>(outcome.gold)
    .bind::<diesel::sql_types::BigInt, _>(outcome.character_xp)
    .bind::<diesel::sql_types::BigInt, _>(outcome.trophy_delta)
    .bind::<diesel::sql_types::BigInt, _>(a.post_trophies)
    .bind::<diesel::sql_types::BigInt, _>(a.post_high_water)
    .bind::<diesel::sql_types::Integer, _>(a.arena)
    .bind::<diesel::sql_types::Integer, _>(a.arena_level)
    .bind::<diesel::sql_types::BigInt, _>(a.meter)
    .execute(&mut conn)
    .await;
    if let Err(e) = audit {
        warn!(
            "arena economy: audit insert into arena_match_results failed — the REWARD \
             IS SAFE (it committed in its own transaction), only the audit row is \
             missing. Is the Phase-5.4 migration applied on this box? {e}"
        );
    }

    Ok(())
}

/// What phase 1 committed — carried out of the transaction so the audit row can be
/// written separately without risking a rollback of the reward.
struct AppliedOutcome {
    character_id: Uuid,
    pre_trophies: i64,
    post_trophies: i64,
    post_high_water: i64,
    arena: i32,
    arena_level: i32,
    meter: i64,
    granted: usize,
}

/// The arena gold currency uuid, parsed once. Same constant the op49 card uses, so
/// the wallet we credit and the wallet the card shows can never drift apart.
static ARENA_GOLD_CURRENCY_UUID_PARSED: std::sync::LazyLock<Uuid> = std::sync::LazyLock::new(|| {
    Uuid::parse_str(ARENA_GOLD_CURRENCY_UUID).expect("ARENA_GOLD_CURRENCY_UUID is a valid uuid")
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_without_install_is_a_no_op() {
        // The offline harness and every unit test run without a database; the
        // engine must be able to call record() unconditionally.
        record(MatchEconomyOutcome {
            character_id: Uuid::nil(),
            game_session_id: None,
            level: 86,
            gold: 14961,
            character_xp: 691,
            trophy_delta: 30,
            rounds_won: 2,
            rounds_lost: 0,
            win: true,
            opponent_character_id: None,
        });
    }

    #[test]
    fn gold_currency_uuid_is_the_captured_one() {
        assert_eq!(
            ARENA_GOLD_CURRENCY_UUID_PARSED.to_string(),
            "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2",
            "the currency id every retail op49 wallet/reward block uses"
        );
    }
}
