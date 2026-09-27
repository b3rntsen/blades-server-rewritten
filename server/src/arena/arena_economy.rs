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
use diesel::{ExpressionMethods, OptionalExtension, QueryDsl, SelectableHelper};
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt,
};
use log::{error, info, warn};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use uuid::Uuid;

use crate::DbPool;
use crate::arena::arena_ladder;
use crate::arena::combat::messages::ARENA_GOLD_CURRENCY_UUID;
use crate::arena::ranking;
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
    /// One match-end timestamp shared with the synchronous op49 calculation, so a
    /// cooldown boundary cannot make the card and durable chest tier disagree.
    pub completed_at_secs: i64,
    /// The opponent's character id, for the audit row.
    pub opponent_character_id: Option<Uuid>,
    /// True only for paired human-vs-human matches. AI fights never touch the
    /// human-only public rating board.
    pub h2h: bool,
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

/// Wall clock in unix seconds, for the Arena Giveaway window check.
fn ffa_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Add the fixed portion of crossed promotion loot to the same `RewardGrant`
/// that is applied transactionally with match gold and XP.
fn add_promotion_stackables(
    reward: &mut RewardGrant,
    promo: &arena_ladder::PromotionRewards,
) -> u64 {
    for (template, quantity) in &promo.stackable_items {
        let template =
            Uuid::parse_str(template).expect("generated arena-promotion item id is a valid UUID");
        *reward.stackable_items.entry(template).or_insert(0) += quantity;
    }
    promo.stackable_items.iter().map(|(_, n)| n).sum()
}

/// Apply one match outcome durably: PvP counters + wallet + XP + any promotion
/// chests and fixed stackables. One transaction, row-locked, so two matches ending
/// at the same instant for the same character cannot interleave.
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
                    if ch.pvp_winning_streak > 0 {
                        ch.pvp_winning_streak + 1
                    } else {
                        1
                    }
                } else if ch.pvp_winning_streak < 0 {
                    ch.pvp_winning_streak - 1
                } else {
                    -1
                };
                // The chest meter counts ROUNDS won and wraps at capacity 8.
                let (meter, filled) =
                    arena_ladder::advance_chest_meter(ch.pvp_chest_meter, o.rounds_won);
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
                let promotion_stackables = add_promotion_stackables(&mut reward, &promo);

                // Arena Giveaway. Retail's banner promised Gems for turning up inside
                // a two-hour Saturday window — "Join them during that time and earn
                // free Gems!" — so the grant belongs HERE, on a finished match,
                // rather than on a claim button.
                //
                // It rides the same RewardGrant and therefore the same transaction as
                // the gold: `try_earn` writes the once-per-character ledger row, and a
                // rollback takes both or neither. Splitting them would let a crash
                // mark a player paid who never was.
                let giveaway = crate::free_for_all::try_earn(&mut conn, entry.id, ffa_now()).await;
                if let Some((_, gems)) = giveaway {
                    if gems > 0 {
                        *reward
                            .currencies
                            .entry(blades_lib::economy::GEMS)
                            .or_insert(0) += gems as u64;
                    }
                }

                let mut tracker = InventoryChangeTracker::default();
                apply_reward(
                    &reward,
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );
                if !promo.stackable_items.is_empty() {
                    entry.inventory.0.backpack_version += 1;
                    entry
                        .server_state
                        .0
                        .arena_promotion_loot_grants
                        .extend(promo.loot_thresholds.iter().copied());
                }

                // Ladder promotion chests (`rewards_once_reached`) plus any chest the
                // meter completed this match. Both land in the treasury exactly like a
                // quest/dungeon chest does.
                let mut granted = 0usize;
                for (rarity, level) in &promo.chests {
                    grant_chest(
                        &mut entry.inventory.0,
                        *rarity as u64,
                        *level as u64,
                        &mut tracker,
                    );
                    granted += 1;
                }
                for _ in 0..filled {
                    let award = arena_ladder::next_pvp_chest(
                        o.character_id,
                        entry.server_state.0.arena_chests_earned,
                        entry.server_state.0.arena_last_elder_one_chest_at_secs,
                        entry.server_state.0.arena_last_elder_two_chest_at_secs,
                        entry.server_state.0.arena_last_legendary_chest_at_secs,
                        o.completed_at_secs,
                    );
                    grant_chest(
                        &mut entry.inventory.0,
                        award.kind.tier() as u64,
                        entry.character.0.level as u64,
                        &mut tracker,
                    );
                    entry.server_state.0.arena_chests_earned =
                        entry.server_state.0.arena_chests_earned.saturating_add(1);
                    match award.rule {
                        Some(arena_ladder::PvpChestRule::ElderOne) => {
                            entry.server_state.0.arena_last_elder_one_chest_at_secs =
                                o.completed_at_secs;
                        }
                        Some(arena_ladder::PvpChestRule::ElderTwo) => {
                            entry.server_state.0.arena_last_elder_two_chest_at_secs =
                                o.completed_at_secs;
                        }
                        Some(arena_ladder::PvpChestRule::Legendary) => {
                            entry.server_state.0.arena_last_legendary_chest_at_secs =
                                o.completed_at_secs;
                        }
                        _ => {}
                    }
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

                // GUILD TROPHIES. `guilds.trophies` is the guild's running trophy
                // total — it is what `GET /guilds/leaderboard` orders on and what the
                // client shows on every guild card. It was written once, as 0, at guild
                // creation and never again, so every guild sat at 0 and the "top guilds"
                // ladder was really ordering by guild id.
                //
                // The aggregation rule is the one `season_store::guild_standings_from`
                // already encodes: a guild's trophies are the SUM of its members'
                // trophies. Applied here as the same delta the character just took, so
                // the total tracks play without a full re-scan on every match.
                //
                // Clamped at 0 for the same reason the character's own count is:
                // retail cards bottom out at zero and never go negative.
                if o.trophy_delta != 0 {
                    use crate::schema::{guild_members, guilds};
                    let guild_of: Option<String> = guild_members::table
                        .filter(guild_members::character_id.eq(character_id))
                        .select(guild_members::guild_id)
                        .first(&mut conn)
                        .await
                        .optional()?;
                    if let Some(gid) = guild_of {
                        let _ = guilds::table; // keep the import meaningful for the reader
                        diesel::sql_query(
                            "UPDATE guilds SET trophies = GREATEST(trophies + $1, 0) WHERE id = $2",
                        )
                        .bind::<diesel::sql_types::BigInt, _>(o.trophy_delta)
                        .bind::<diesel::sql_types::Text, _>(&gid)
                        .execute(&mut conn)
                        .await?;
                    }
                }

                Ok(Some(AppliedOutcome {
                    giveaway_gems: giveaway.map(|(_, g)| g).unwrap_or(0),
                    character_id,
                    pre_trophies,
                    post_trophies,
                    post_high_water,
                    arena: tier.arena as i32,
                    arena_level: tier.level as i32,
                    meter,
                    granted,
                    promotion_stackables,
                }))
            }
            .scope_boxed()
        })
        .await?;

    // No character row (bot / starter loadout) — nothing to audit either.
    let Some(a) = applied else { return Ok(()) };

    info!(
        "arena economy: persisted {} for L{} character {} — gold {:+}, xp {:+}, \
         trophies {} -> {} ({:+}), high-water {}, arena {}/{}, meter {}{}{}",
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
        if a.granted > 0 {
            format!(", {} chest(s)", a.granted)
        } else {
            String::new()
        },
        if a.promotion_stackables > 0 {
            format!(", {} promotion stackable(s)", a.promotion_stackables)
        } else {
            String::new()
        },
    );

    if a.giveaway_gems > 0 {
        info!(
            "arena giveaway: character {} earned {} Gems for fighting inside the window",
            a.character_id, a.giveaway_gems
        );
    }

    // Phase 2 — the audit row, OUTSIDE the reward transaction above (see the doc
    // comment: a missing table would otherwise roll the reward back). Failure
    // only logs. H2H rating writes are deliberately best-effort after the audit
    // commits; reconcile can repair from the durable row.
    if outcome.h2h
        && let Some(opponent_id) = outcome.opponent_character_id
    {
        let cfg = ranking::load(pool).await.config.h2h_rating;
        let audit = persist_h2h_audit_and_rating(&mut conn, &cfg, outcome, &a, opponent_id).await;
        if let Err(e) = audit {
            warn!(
                "arena h2h audit: insert failed for {} vs {} in {:?}; \
                 the REWARD IS SAFE (it committed in its own transaction): {e}",
                outcome.character_id, opponent_id, outcome.game_session_id
            );
        }
        return Ok(());
    }

    let audit = insert_match_audit(&mut conn, outcome, &a).await;
    if let Err(e) = audit {
        warn!(
            "arena economy: audit insert into arena_match_results failed — the REWARD \
             IS SAFE (it committed in its own transaction), only the audit row is \
             missing. Is the Phase-5.4 migration applied on this box? {e}"
        );
    }

    Ok(())
}

async fn insert_match_audit(
    conn: &mut AsyncPgConnection,
    outcome: &MatchEconomyOutcome,
    a: &AppliedOutcome,
) -> Result<(), anyhow::Error> {
    diesel::sql_query(
        "INSERT INTO arena_match_results \
         (id, character_id, opponent_character_id, game_session_id, win, \
          rounds_won, rounds_lost, gold, character_xp, trophy_delta, \
          trophies_after, matchmaking_trophies_after, arena, arena_level, chest_meter, \
          recorded_at, is_h2h) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                 to_timestamp($16), $17)",
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
    .bind::<diesel::sql_types::BigInt, _>(outcome.completed_at_secs)
    .bind::<diesel::sql_types::Bool, _>(outcome.h2h)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(diesel::QueryableByName)]
struct H2hRatingRow {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    rating: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    wins: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    losses: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    ties: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    matches: i32,
}

impl H2hRatingRow {
    fn state(self) -> ranking::H2hRatingState {
        ranking::H2hRatingState {
            rating: self.rating as i64,
            wins: self.wins as i64,
            losses: self.losses as i64,
            ties: self.ties as i64,
            matches: self.matches as i64,
        }
    }
}

async fn h2h_state(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    start: i64,
) -> Result<ranking::H2hRatingState, anyhow::Error> {
    let row: Option<H2hRatingRow> = diesel::sql_query(
        "SELECT rating, wins, losses, ties, matches \
         FROM arena_h2h_ratings WHERE character_id = $1",
    )
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .get_result(conn)
    .await
    .optional()?;
    Ok(row
        .map(H2hRatingRow::state)
        .unwrap_or_else(|| ranking::H2hRatingState::new(start)))
}

async fn h2h_season_state(
    conn: &mut AsyncPgConnection,
    season_id: Uuid,
    character_id: Uuid,
    start: i64,
) -> Result<ranking::H2hRatingState, anyhow::Error> {
    let row: Option<H2hRatingRow> = diesel::sql_query(
        "SELECT rating, wins, losses, ties, matches \
         FROM arena_h2h_season_ratings WHERE season_id = $1 AND character_id = $2",
    )
    .bind::<diesel::sql_types::Uuid, _>(season_id)
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .get_result(conn)
    .await
    .optional()?;
    Ok(row
        .map(H2hRatingRow::state)
        .unwrap_or_else(|| ranking::H2hRatingState::new(start)))
}

#[derive(diesel::QueryableByName)]
struct H2hSeasonAtRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    id: Uuid,
}

async fn h2h_season_at(
    conn: &mut AsyncPgConnection,
    completed_at_secs: i64,
) -> Result<Option<Uuid>, anyhow::Error> {
    let row: Option<H2hSeasonAtRow> = diesel::sql_query(
        "SELECT id FROM arena_seasons \
         WHERE starts_at <= $1 AND ends_at > $1 \
         ORDER BY starts_at DESC LIMIT 1",
    )
    .bind::<diesel::sql_types::BigInt, _>(completed_at_secs)
    .get_result(conn)
    .await
    .optional()?;
    Ok(row.map(|r| r.id))
}

async fn persist_h2h_audit_and_rating(
    conn: &mut AsyncPgConnection,
    cfg: &ranking::H2hRatingConfig,
    outcome: &MatchEconomyOutcome,
    applied: &AppliedOutcome,
    opponent_id: Uuid,
) -> Result<(), anyhow::Error> {
    let cfg = cfg.clone();
    let outcome = outcome.clone();

    insert_match_audit(conn, &outcome, applied).await?;
    if outcome.character_id.as_u128() >= opponent_id.as_u128() {
        return Ok(());
    }

    let character_id = outcome.character_id;
    let game_session_id = outcome.game_session_id;
    let rating = conn
        .transaction::<_, anyhow::Error, _>(|mut conn| {
            async move {
                ranking::lock_h2h_rating_transaction(&mut conn).await?;
                persist_h2h_rating_pair_locked(&mut conn, &cfg, &outcome, opponent_id).await?;
                Ok(())
            }
            .scope_boxed()
        })
        .await;
    if let Err(e) = rating {
        warn!(
            "arena h2h rating: best-effort update failed for {} vs {} in {:?}; \
             audit row is durable and reconcile will repair it: {e}",
            character_id, opponent_id, game_session_id
        );
    }
    Ok(())
}

async fn persist_h2h_rating_pair_locked(
    conn: &mut AsyncPgConnection,
    cfg: &ranking::H2hRatingConfig,
    outcome: &MatchEconomyOutcome,
    opponent_id: Uuid,
) -> Result<(), anyhow::Error> {
    let character_id = outcome.character_id;
    let completed_at_secs = outcome.completed_at_secs;
    let a_outcome = arena_ladder::MatchOutcome {
        rounds_won: outcome.rounds_won,
        rounds_lost: outcome.rounds_lost,
        win: outcome.win,
    };

    let a = h2h_state(conn, character_id, cfg.start).await?;
    let b = h2h_state(conn, opponent_id, cfg.start).await?;
    if h2h_needs_increment(conn, None, [(&a, character_id), (&b, opponent_id)]).await? {
        let (a, b) = ranking::apply_h2h_rating(cfg, a, b, a_outcome);
        upsert_h2h_state(conn, character_id, a, completed_at_secs).await?;
        upsert_h2h_state(conn, opponent_id, b, completed_at_secs).await?;
    } else {
        log::debug!(
            "arena h2h rating: {:?} already covered by reconcile; skipping all-time increment",
            outcome.game_session_id
        );
    }

    if let Some(season_id) = h2h_season_at(conn, completed_at_secs).await? {
        let a = h2h_season_state(conn, season_id, character_id, cfg.start).await?;
        let b = h2h_season_state(conn, season_id, opponent_id, cfg.start).await?;
        if h2h_needs_increment(
            conn,
            Some(season_id),
            [(&a, character_id), (&b, opponent_id)],
        )
        .await?
        {
            let (a, b) = ranking::apply_h2h_rating(cfg, a, b, a_outcome);
            upsert_h2h_season_state(conn, season_id, character_id, a, completed_at_secs).await?;
            upsert_h2h_season_state(conn, season_id, opponent_id, b, completed_at_secs).await?;
        } else {
            log::debug!(
                "arena h2h rating: {:?} already covered by reconcile; skipping season increment",
                outcome.game_session_id
            );
        }
    }

    Ok(())
}

async fn h2h_needs_increment(
    conn: &mut AsyncPgConnection,
    season_id: Option<Uuid>,
    states: [(&ranking::H2hRatingState, Uuid); 2],
) -> Result<bool, anyhow::Error> {
    for (state, character_id) in states {
        let replay_matches = h2h_replay_match_count(conn, character_id, season_id).await?;
        if state.matches < replay_matches {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(diesel::QueryableByName)]
struct H2hReplayCountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    matches: i64,
}

async fn h2h_replay_match_count(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    season_id: Option<Uuid>,
) -> Result<i64, anyhow::Error> {
    let row: H2hReplayCountRow = diesel::sql_query(
        "WITH candidates AS ( \
             SELECT s.id AS season_id, \
                    LEAST(r.character_id, r.opponent_character_id) AS character_id, \
                    GREATEST(r.character_id, r.opponent_character_id) AS opponent_character_id, \
                    r.recorded_at, r.id, r.game_session_id \
             FROM arena_match_results r \
             LEFT JOIN LATERAL ( \
                 SELECT id FROM arena_seasons s \
                 WHERE r.recorded_at >= to_timestamp(s.starts_at) \
                   AND r.recorded_at < to_timestamp(s.ends_at) \
                 ORDER BY s.starts_at DESC LIMIT 1 \
             ) s ON true \
             WHERE r.opponent_character_id IS NOT NULL \
               AND r.is_h2h = true \
         ), replay_rows AS ( \
             SELECT DISTINCT ON (game_session_id, character_id, opponent_character_id) \
                    season_id, character_id, opponent_character_id, recorded_at, id \
             FROM candidates \
             ORDER BY game_session_id, character_id, opponent_character_id, recorded_at, id \
         ) \
         SELECT COUNT(*) AS matches \
         FROM replay_rows \
         WHERE ($2 IS NULL OR season_id = $2) \
           AND (character_id = $1 OR opponent_character_id = $1)",
    )
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(season_id)
    .get_result(conn)
    .await?;
    Ok(row.matches)
}

async fn upsert_h2h_state(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    state: ranking::H2hRatingState,
    completed_at_secs: i64,
) -> Result<(), anyhow::Error> {
    diesel::sql_query(
        "INSERT INTO arena_h2h_ratings \
         (character_id, rating, wins, losses, ties, matches, last_match_at) \
         VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7)) \
         ON CONFLICT (character_id) DO UPDATE SET \
             rating = EXCLUDED.rating, wins = EXCLUDED.wins, losses = EXCLUDED.losses, \
             ties = EXCLUDED.ties, matches = EXCLUDED.matches, \
             last_match_at = EXCLUDED.last_match_at",
    )
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .bind::<diesel::sql_types::Integer, _>(state.rating as i32)
    .bind::<diesel::sql_types::Integer, _>(state.wins as i32)
    .bind::<diesel::sql_types::Integer, _>(state.losses as i32)
    .bind::<diesel::sql_types::Integer, _>(state.ties as i32)
    .bind::<diesel::sql_types::Integer, _>(state.matches as i32)
    .bind::<diesel::sql_types::BigInt, _>(completed_at_secs)
    .execute(conn)
    .await?;
    Ok(())
}

async fn upsert_h2h_season_state(
    conn: &mut AsyncPgConnection,
    season_id: Uuid,
    character_id: Uuid,
    state: ranking::H2hRatingState,
    completed_at_secs: i64,
) -> Result<(), anyhow::Error> {
    diesel::sql_query(
        "INSERT INTO arena_h2h_season_ratings \
         (season_id, character_id, rating, wins, losses, ties, matches, last_match_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, to_timestamp($8)) \
         ON CONFLICT (season_id, character_id) DO UPDATE SET \
             rating = EXCLUDED.rating, wins = EXCLUDED.wins, losses = EXCLUDED.losses, \
             ties = EXCLUDED.ties, matches = EXCLUDED.matches, \
             last_match_at = EXCLUDED.last_match_at",
    )
    .bind::<diesel::sql_types::Uuid, _>(season_id)
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .bind::<diesel::sql_types::Integer, _>(state.rating as i32)
    .bind::<diesel::sql_types::Integer, _>(state.wins as i32)
    .bind::<diesel::sql_types::Integer, _>(state.losses as i32)
    .bind::<diesel::sql_types::Integer, _>(state.ties as i32)
    .bind::<diesel::sql_types::Integer, _>(state.matches as i32)
    .bind::<diesel::sql_types::BigInt, _>(completed_at_secs)
    .execute(conn)
    .await?;
    Ok(())
}

/// What phase 1 committed — carried out of the transaction so the audit row can be
/// written separately without risking a rollback of the reward.
#[derive(Clone)]
struct AppliedOutcome {
    character_id: Uuid,
    pre_trophies: i64,
    post_trophies: i64,
    post_high_water: i64,
    arena: i32,
    arena_level: i32,
    meter: i64,
    granted: usize,
    promotion_stackables: u64,
    /// Gems paid by an open Arena Giveaway window, 0 when none was running.
    giveaway_gems: i64,
}

/// The arena gold currency uuid, parsed once. Same constant the op49 card uses, so
/// the wallet we credit and the wallet the card shows can never drift apart.
static ARENA_GOLD_CURRENCY_UUID_PARSED: std::sync::LazyLock<Uuid> =
    std::sync::LazyLock::new(|| {
        Uuid::parse_str(ARENA_GOLD_CURRENCY_UUID).expect("ARENA_GOLD_CURRENCY_UUID is a valid uuid")
    });

#[cfg(test)]
mod tests {
    use super::*;
    use diesel_async::{AsyncConnection, RunQueryDsl};

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
            completed_at_secs: 1_700_000_000,
            opponent_character_id: None,
            h2h: false,
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

    #[test]
    fn crossing_200_stages_transcendent_gems_for_durable_application() {
        let promo = arena_ladder::promotion_rewards(180, 206, 86);
        let mut reward = RewardGrant::default();
        assert_eq!(add_promotion_stackables(&mut reward, &promo), 3);
        assert_eq!(
            reward
                .stackable_items
                .get(&Uuid::parse_str("d94bab85-53d5-4c9c-a637-acd94fc66c98").unwrap()),
            Some(&3)
        );
    }

    async fn h2h_fixture() -> Option<AsyncPgConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let mut conn = AsyncPgConnection::establish(&url)
            .await
            .expect("TEST_DATABASE_URL is set but unreachable");
        conn.begin_test_transaction()
            .await
            .expect("test transaction");
        let schema = format!("t{}", Uuid::new_v4().simple());
        diesel::sql_query(format!("CREATE SCHEMA {schema}"))
            .execute(&mut conn)
            .await
            .unwrap();
        diesel::sql_query(format!("SET LOCAL search_path TO {schema}"))
            .execute(&mut conn)
            .await
            .unwrap();
        for stmt in [
            "CREATE TABLE arena_seasons ( \
                 id UUID PRIMARY KEY, number INTEGER NOT NULL, name TEXT NOT NULL, \
                 starts_at BIGINT NOT NULL, ends_at BIGINT NOT NULL, status TEXT NOT NULL DEFAULT 'scheduled', \
                 scoring TEXT NOT NULL DEFAULT 'shipped', reset_rule TEXT NOT NULL DEFAULT 'hard_reset', \
                 created_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint, ended_at BIGINT)",
            "CREATE TABLE arena_matches ( \
                 ticket_id UUID PRIMARY KEY, user_id UUID NOT NULL, status TEXT NOT NULL, \
                 game_session_id UUID, paired BOOLEAN NOT NULL DEFAULT FALSE, \
                 recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), resolved_at TIMESTAMPTZ)",
            "CREATE TABLE arena_match_results ( \
                 id UUID PRIMARY KEY, character_id UUID NOT NULL, opponent_character_id UUID, \
                 game_session_id UUID, win BOOLEAN NOT NULL, rounds_won INTEGER NOT NULL DEFAULT 0, \
                 rounds_lost INTEGER NOT NULL DEFAULT 0, gold BIGINT NOT NULL DEFAULT 0, \
                 character_xp BIGINT NOT NULL DEFAULT 0, trophy_delta BIGINT NOT NULL DEFAULT 0, \
                 trophies_after BIGINT NOT NULL DEFAULT 0, matchmaking_trophies_after BIGINT NOT NULL DEFAULT 0, \
                 arena INTEGER NOT NULL DEFAULT 1, arena_level INTEGER NOT NULL DEFAULT 1, \
                 chest_meter BIGINT NOT NULL DEFAULT 0, recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 is_h2h BOOLEAN NOT NULL DEFAULT false)",
            "CREATE TABLE arena_h2h_ratings ( \
                 character_id UUID PRIMARY KEY, rating INTEGER NOT NULL, wins INTEGER NOT NULL DEFAULT 0, \
                 losses INTEGER NOT NULL DEFAULT 0, ties INTEGER NOT NULL DEFAULT 0, \
                 matches INTEGER NOT NULL DEFAULT 0, last_match_at TIMESTAMPTZ)",
            "CREATE TABLE arena_h2h_season_ratings ( \
                 season_id UUID NOT NULL, character_id UUID NOT NULL, rating INTEGER NOT NULL, \
                 wins INTEGER NOT NULL DEFAULT 0, losses INTEGER NOT NULL DEFAULT 0, \
                 ties INTEGER NOT NULL DEFAULT 0, matches INTEGER NOT NULL DEFAULT 0, \
                 last_match_at TIMESTAMPTZ, PRIMARY KEY (season_id, character_id))",
        ] {
            diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
        }
        Some(conn)
    }

    fn applied(character_id: Uuid) -> AppliedOutcome {
        AppliedOutcome {
            character_id,
            pre_trophies: 100,
            post_trophies: 124,
            post_high_water: 124,
            arena: 1,
            arena_level: 1,
            meter: 2,
            granted: 0,
            promotion_stackables: 0,
            giveaway_gems: 0,
        }
    }

    fn h2h_outcome(
        character_id: Uuid,
        opponent_id: Uuid,
        game_session_id: Uuid,
        win: bool,
    ) -> MatchEconomyOutcome {
        MatchEconomyOutcome {
            character_id,
            game_session_id: Some(game_session_id),
            level: 1,
            gold: 0,
            character_xp: 0,
            trophy_delta: if win { 24 } else { -24 },
            rounds_won: if win { 2 } else { 0 },
            rounds_lost: if win { 0 } else { 2 },
            win,
            completed_at_secs: 1_700_000_100,
            opponent_character_id: Some(opponent_id),
            h2h: true,
        }
    }

    #[derive(Debug, PartialEq, Eq, diesel::QueryableByName)]
    struct TestRatingRow {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        character_id: Uuid,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        rating: i32,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        wins: i32,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        losses: i32,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        matches: i32,
    }

    #[derive(Debug, PartialEq, Eq, diesel::QueryableByName)]
    struct TestAuditRow {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        character_id: Uuid,
        #[diesel(sql_type = diesel::sql_types::Bool)]
        is_h2h: bool,
    }

    async fn test_ratings(conn: &mut AsyncPgConnection) -> Vec<TestRatingRow> {
        diesel::sql_query(
            "SELECT character_id, rating, wins, losses, matches \
             FROM arena_h2h_ratings ORDER BY character_id",
        )
        .get_results(conn)
        .await
        .unwrap()
    }

    async fn test_audit_rows(conn: &mut AsyncPgConnection) -> Vec<TestAuditRow> {
        diesel::sql_query(
            "SELECT character_id, is_h2h FROM arena_match_results ORDER BY character_id",
        )
        .get_results(conn)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn audit_insert_persists_the_live_h2h_flag() {
        let Some(mut conn) = h2h_fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — h2h audit SQL not verified");
            return;
        };
        let human = Uuid::from_u128(1);
        let opponent = Uuid::from_u128(2);
        let bot_fight = Uuid::from_u128(3);

        insert_match_audit(
            &mut conn,
            &h2h_outcome(human, opponent, Uuid::from_u128(90), true),
            &applied(human),
        )
        .await
        .unwrap();
        let mut ai_outcome = h2h_outcome(bot_fight, opponent, Uuid::from_u128(91), true);
        ai_outcome.h2h = ranking::is_h2h_match(2, 2, true);
        insert_match_audit(&mut conn, &ai_outcome, &applied(bot_fight))
            .await
            .unwrap();

        assert_eq!(
            test_audit_rows(&mut conn).await,
            vec![
                TestAuditRow {
                    character_id: human,
                    is_h2h: true,
                },
                TestAuditRow {
                    character_id: bot_fight,
                    is_h2h: false,
                },
            ],
            "replay consumes the same durable flag that live match end stores"
        );
    }

    #[tokio::test]
    async fn h2h_audit_survives_rating_update_failure_and_reconcile_repairs_board() {
        let Some(mut conn) = h2h_fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — h2h audit failure SQL not verified");
            return;
        };
        let cfg = ranking::H2hRatingConfig::default();
        let season = Uuid::from_u128(10);
        let lower = Uuid::from_u128(1);
        let higher = Uuid::from_u128(2);
        let game_session_id = Uuid::from_u128(99);

        diesel::sql_query(
            "INSERT INTO arena_seasons (id, number, name, starts_at, ends_at) \
             VALUES ($1, 1, 'test', 1700000000, 1700002000)",
        )
        .bind::<diesel::sql_types::Uuid, _>(season)
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query("DROP TABLE arena_h2h_ratings")
            .execute(&mut conn)
            .await
            .unwrap();

        let lower_win = h2h_outcome(lower, higher, game_session_id, true);
        persist_h2h_audit_and_rating(&mut conn, &cfg, &lower_win, &applied(lower), higher)
            .await
            .unwrap();
        assert_eq!(
            test_audit_rows(&mut conn).await,
            vec![TestAuditRow { character_id: lower, is_h2h: true }],
            "rating table failure must not roll back the audit row"
        );

        diesel::sql_query(
            "CREATE TABLE arena_h2h_ratings ( \
                 character_id UUID PRIMARY KEY, rating INTEGER NOT NULL, wins INTEGER NOT NULL DEFAULT 0, \
                 losses INTEGER NOT NULL DEFAULT 0, ties INTEGER NOT NULL DEFAULT 0, \
                 matches INTEGER NOT NULL DEFAULT 0, last_match_at TIMESTAMPTZ)",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        ranking::reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
            .await
            .unwrap();

        assert_eq!(
            test_ratings(&mut conn).await,
            vec![
                TestRatingRow {
                    character_id: lower,
                    rating: 1024,
                    wins: 1,
                    losses: 0,
                    matches: 1,
                },
                TestRatingRow {
                    character_id: higher,
                    rating: 976,
                    wins: 0,
                    losses: 1,
                    matches: 1,
                },
            ],
            "reconcile must rebuild the board from the surviving audit row"
        );
    }

    #[tokio::test]
    async fn canonical_h2h_increment_skips_match_already_absorbed_by_reconcile() {
        let Some(mut conn) = h2h_fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — h2h audit race SQL not verified");
            return;
        };
        let cfg = ranking::H2hRatingConfig::default();
        let season = Uuid::from_u128(10);
        let lower = Uuid::from_u128(1);
        let higher = Uuid::from_u128(2);
        let game_session_id = Uuid::from_u128(99);

        diesel::sql_query(
            "INSERT INTO arena_seasons (id, number, name, starts_at, ends_at) \
             VALUES ($1, 1, 'test', 1700000000, 1700002000)",
        )
        .bind::<diesel::sql_types::Uuid, _>(season)
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(
            "INSERT INTO arena_matches (ticket_id, user_id, status, game_session_id, paired) \
             VALUES ($1, $2, 'matched', $3, true)",
        )
        .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
        .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
        .bind::<diesel::sql_types::Uuid, _>(game_session_id)
        .execute(&mut conn)
        .await
        .unwrap();

        let higher_loss = h2h_outcome(higher, lower, game_session_id, false);
        persist_h2h_audit_and_rating(&mut conn, &cfg, &higher_loss, &applied(higher), lower)
            .await
            .unwrap();
        ranking::reconcile_h2h_ratings_with_config(&mut conn, &cfg, false)
            .await
            .unwrap();

        let lower_win = h2h_outcome(lower, higher, game_session_id, true);
        persist_h2h_audit_and_rating(&mut conn, &cfg, &lower_win, &applied(lower), higher)
            .await
            .unwrap();

        assert_eq!(
            test_ratings(&mut conn).await,
            vec![
                TestRatingRow {
                    character_id: lower,
                    rating: 1024,
                    wins: 1,
                    losses: 0,
                    matches: 1,
                },
                TestRatingRow {
                    character_id: higher,
                    rating: 976,
                    wins: 0,
                    losses: 1,
                    matches: 1,
                },
            ],
            "the canonical incremental path must not double-apply a match that reconcile already absorbed"
        );
    }
}
