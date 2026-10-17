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
use crate::arena::arena_fight_gift;
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
    /// Arena Gems for this match — identical to the value on the op49 card. An
    /// owner-approved deviation from retail (#390/#391, 2026-10-16); see
    /// [`crate::arena::arena_bonus_rewards`]. 0 for an abandoned or voided match.
    pub gems: i64,
    /// Whether this side FINISHED the fight (won or lost it; did not concede and
    /// was not voided as a no-show). Only a finished fight counts toward the
    /// per-account weapon gift ([`crate::arena::arena_fight_gift`]).
    pub finished_fight: bool,
    /// The every-25th-fight weapon the op49 card showed (#401), to be granted
    /// straight into the backpack when the account count confirms the milestone.
    /// `None` on every other fight.
    pub card_weapon: Option<blades_lib::economy::RewardItem>,
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

fn add_promotion_currencies(
    reward: &mut RewardGrant,
    promo: &arena_ladder::PromotionRewards,
) -> u64 {
    for (currency, quantity) in &promo.currencies {
        let currency = Uuid::parse_str(currency)
            .expect("generated arena-promotion currency id is a valid UUID");
        *reward.currencies.entry(currency).or_insert(0) += quantity;
    }
    promo.currencies.iter().map(|(_, n)| n).sum()
}

/// The match's own reward — gold, XP and the arena Gems — before promotion loot
/// and the Arena Giveaway are added to it.
fn match_reward_grant(o: &MatchEconomyOutcome) -> RewardGrant {
    let mut reward = RewardGrant::default();
    if o.gold > 0 {
        reward
            .currencies
            .insert(ARENA_GOLD_CURRENCY_UUID_PARSED.clone(), o.gold as u64);
    }
    // Owner-approved deviation (#390/#391, 2026-10-16): Gems for every finished
    // match, credited in the same transaction as the gold.
    if o.gems > 0 {
        *reward
            .currencies
            .entry(blades_lib::economy::GEMS)
            .or_insert(0) += o.gems as u64;
    }
    reward.character_xp = o.character_xp.max(0) as u64;
    reward
}

fn promotion_needs_loot_marker(promo: &arena_ladder::PromotionRewards) -> bool {
    !promo.loot_thresholds.is_empty()
}

fn promotion_changes_backpack(promo: &arena_ladder::PromotionRewards) -> bool {
    !promo.stackable_items.is_empty()
}

/// Put the weapon the op49 card showed (#401) into the backpack, as the very
/// instance the card listed — same id, same fields — so the client's inventory
/// and the server's agree without a re-sync.
fn deliver_card_weapon(
    inventory: &mut blades_lib::user_data::CompleteInventory,
    weapon: &blades_lib::economy::RewardItem,
    tracker: &mut InventoryChangeTracker,
) {
    // `apply_reward`'s item branch, without a wallet or character to touch.
    inventory.backpack.items.0.insert(weapon.id, weapon.item.clone());
    tracker.modified_backpack.items.insert(weapon.id);
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
                let mut reward = match_reward_grant(&o);
                let promotion_stackables = add_promotion_stackables(&mut reward, &promo);
                let promotion_currencies = add_promotion_currencies(&mut reward, &promo);

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
                let character_id = entry.id;
                let user_id = entry.user_id;
                // THE WEAPON GIFT (owner-approved #390/#391): count this finished
                // fight on the ACCOUNT and, on every 25th, grant the weapon. In a
                // nested transaction — a savepoint — so a failure (the table not yet
                // migrated, say) rolls back only the count and never the reward
                // above, which a failed statement in this transaction would.
                //
                // Done BEFORE the character row is written: the weapon the op49 card
                // showed (#401) goes straight into this same backpack, under the same
                // single `backpackVersion` bump the card advertised.
                let fight_tally = if o.finished_fight {
                    let now = o.completed_at_secs;
                    let card_weapon = o.card_weapon.clone();
                    let counted = conn
                        .transaction::<_, anyhow::Error, _>(|conn| {
                            async move {
                                let alt = arena_fight_gift::live_alt(conn, character_id).await?;
                                arena_fight_gift::record_finished_fight(
                                    conn,
                                    user_id,
                                    character_id,
                                    alt,
                                    now,
                                    card_weapon.as_ref(),
                                )
                                .await
                            }
                            .scope_boxed()
                        })
                        .await;
                    match counted {
                        Ok(tally) => Some(tally),
                        Err(e) => {
                            warn!(
                                "arena fight gift: could not count the fight for account {user_id} \
                                 (character {character_id}); the match REWARD IS SAFE: {e}"
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                let delivered = fight_tally.as_ref().and_then(|t| t.delivered.clone());
                if let Some(weapon) = &delivered {
                    deliver_card_weapon(&mut entry.inventory.0, weapon, &mut tracker);
                } else if o.card_weapon.is_some() {
                    warn!(
                        "arena fight gift: the op49 card showed a weapon to character {character_id} \
                         but the account count ({:?}) did not confirm the milestone; not granted",
                        fight_tally.as_ref().map(|t| t.fights)
                    );
                }
                // One backpack mutation, one bump — as the op49 card advertised it.
                if promotion_changes_backpack(&promo) || delivered.is_some() {
                    entry.inventory.0.backpack_version += 1;
                }
                if promotion_needs_loot_marker(&promo) {
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
                //
                // An operator test alt adds nothing (`crate::testchar`): its cups
                // were not earned, and this running total is the guild board until
                // the next recompute from `FREEZE_STANDINGS_SQL`, which skips it too.
                if o.trophy_delta != 0
                    && !crate::testchar::live_alt_is_test(&mut conn, character_id).await?
                {
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
                    fight_tally,
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
                    promotion_currencies,
                }))
            }
            .scope_boxed()
        })
        .await?;

    // No character row (bot / starter loadout) — nothing to audit either.
    let Some(a) = applied else { return Ok(()) };

    info!(
        "arena economy: persisted {} for L{} character {} — gold {:+}, xp {:+}, gems {:+}, \
         trophies {} -> {} ({:+}), high-water {}, arena {}/{}, meter {}{}{}{}",
        if outcome.win { "WIN" } else { "LOSS" },
        outcome.level,
        a.character_id,
        outcome.gold,
        outcome.character_xp,
        outcome.gems,
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
        if a.promotion_currencies > 0 {
            format!(", {} promotion currency", a.promotion_currencies)
        } else {
            String::new()
        },
    );

    if let Some(tally) = &a.fight_tally {
        if let Some(weapon) = &tally.delivered {
            info!(
                "arena fight gift: character {} reached account fight #{} — weapon {} ({}) \
                 delivered into the backpack from the match-end card",
                a.character_id, tally.fights, weapon.id, weapon.item.item_template_id,
            );
        }
        match &tally.gift {
            Some((gift_id, item)) => info!(
                "arena fight gift: character {} reached account fight #{} — gift {gift_id} \
                 ({} at arcane tier {:?}, {} enchantment(s))",
                a.character_id,
                tally.fights,
                item.item_template_id,
                item.arcane_tier,
                item.enchanting.len(),
            ),
            None if tally.delivered.is_some() => {}
            None => log::debug!(
                "arena fight gift: character {} at account fight #{}",
                a.character_id,
                tally.fights
            ),
        }
    }

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
          recorded_at, is_h2h, source_alt_uuid) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                 to_timestamp($16), $17, \
                 (SELECT source_alt_uuid FROM characters WHERE id = $2))",
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

/// The alt a live character is playing as right now — the same key the replay
/// gives a legacy row ([`ranking::H2H_REPLAY_ROWS_CTE`]): its alt id, or its own
/// id when it has none (or no row at all).
async fn h2h_live_key(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
) -> Result<ranking::H2hKey, anyhow::Error> {
    #[derive(diesel::QueryableByName)]
    struct AltRow {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        alt_key: Uuid,
    }
    let row: Option<AltRow> = diesel::sql_query(
        "SELECT COALESCE(source_alt_uuid, id) AS alt_key FROM characters WHERE id = $1",
    )
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .get_result(conn)
    .await
    .optional()?;
    Ok(ranking::H2hKey::new(
        character_id,
        row.map(|r| r.alt_key).unwrap_or(character_id),
    ))
}

async fn h2h_state(
    conn: &mut AsyncPgConnection,
    key: ranking::H2hKey,
    start: i64,
) -> Result<ranking::H2hRatingState, anyhow::Error> {
    let row: Option<H2hRatingRow> = diesel::sql_query(
        "SELECT rating, wins, losses, ties, matches \
         FROM arena_h2h_ratings WHERE character_id = $1 AND alt_key = $2",
    )
    .bind::<diesel::sql_types::Uuid, _>(key.character_id)
    .bind::<diesel::sql_types::Uuid, _>(key.alt_key)
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
    key: ranking::H2hKey,
    start: i64,
) -> Result<ranking::H2hRatingState, anyhow::Error> {
    let row: Option<H2hRatingRow> = diesel::sql_query(
        "SELECT rating, wins, losses, ties, matches \
         FROM arena_h2h_season_ratings \
         WHERE season_id = $1 AND character_id = $2 AND alt_key = $3",
    )
    .bind::<diesel::sql_types::Uuid, _>(season_id)
    .bind::<diesel::sql_types::Uuid, _>(key.character_id)
    .bind::<diesel::sql_types::Uuid, _>(key.alt_key)
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

    // Both humans are live at match end, so each plays as its live alt.
    let a_key = h2h_live_key(conn, character_id).await?;
    let b_key = h2h_live_key(conn, opponent_id).await?;
    // Same rule as the replay (`H2H_REPLAY_ROWS_CTE`): a fight with an operator
    // test alt on either side is not rated.
    if crate::testchar::any_test_alt(conn, &[a_key.alt_key, b_key.alt_key]).await? {
        log::info!(
            "arena h2h rating: {:?} involves a test character; not rated",
            outcome.game_session_id
        );
        return Ok(());
    }

    let a = h2h_state(conn, a_key, cfg.start).await?;
    let b = h2h_state(conn, b_key, cfg.start).await?;
    if h2h_needs_increment(conn, None, [(&a, a_key), (&b, b_key)]).await? {
        let (a, b) = ranking::apply_h2h_rating(cfg, a, b, a_outcome);
        upsert_h2h_state(conn, a_key, a, completed_at_secs).await?;
        upsert_h2h_state(conn, b_key, b, completed_at_secs).await?;
    } else {
        log::debug!(
            "arena h2h rating: {:?} already covered by reconcile; skipping all-time increment",
            outcome.game_session_id
        );
    }

    if let Some(season_id) = h2h_season_at(conn, completed_at_secs).await? {
        let a = h2h_season_state(conn, season_id, a_key, cfg.start).await?;
        let b = h2h_season_state(conn, season_id, b_key, cfg.start).await?;
        if h2h_needs_increment(conn, Some(season_id), [(&a, a_key), (&b, b_key)]).await? {
            let (a, b) = ranking::apply_h2h_rating(cfg, a, b, a_outcome);
            upsert_h2h_season_state(conn, season_id, a_key, a, completed_at_secs).await?;
            upsert_h2h_season_state(conn, season_id, b_key, b, completed_at_secs).await?;
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
    states: [(&ranking::H2hRatingState, ranking::H2hKey); 2],
) -> Result<bool, anyhow::Error> {
    for (state, key) in states {
        let replay_matches = h2h_replay_match_count(conn, key, season_id).await?;
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
    key: ranking::H2hKey,
    season_id: Option<Uuid>,
) -> Result<i64, anyhow::Error> {
    let row: H2hReplayCountRow = diesel::sql_query(format!(
        "{} \
         SELECT COUNT(*) AS matches \
         FROM replay_rows \
         WHERE ($3 IS NULL OR season_id = $3) \
           AND ((character_id = $1 AND alt_key = $2) \
             OR (opponent_character_id = $1 AND opponent_alt_key = $2))",
        ranking::H2H_REPLAY_ROWS_CTE
    ))
    .bind::<diesel::sql_types::Uuid, _>(key.character_id)
    .bind::<diesel::sql_types::Uuid, _>(key.alt_key)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(season_id)
    .get_result(conn)
    .await?;
    Ok(row.matches)
}

async fn upsert_h2h_state(
    conn: &mut AsyncPgConnection,
    key: ranking::H2hKey,
    state: ranking::H2hRatingState,
    completed_at_secs: i64,
) -> Result<(), anyhow::Error> {
    diesel::sql_query(
        "INSERT INTO arena_h2h_ratings \
         (character_id, alt_key, rating, wins, losses, ties, matches, last_match_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, to_timestamp($8)) \
         ON CONFLICT (character_id, alt_key) DO UPDATE SET \
             rating = EXCLUDED.rating, wins = EXCLUDED.wins, losses = EXCLUDED.losses, \
             ties = EXCLUDED.ties, matches = EXCLUDED.matches, \
             last_match_at = EXCLUDED.last_match_at",
    )
    .bind::<diesel::sql_types::Uuid, _>(key.character_id)
    .bind::<diesel::sql_types::Uuid, _>(key.alt_key)
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
    key: ranking::H2hKey,
    state: ranking::H2hRatingState,
    completed_at_secs: i64,
) -> Result<(), anyhow::Error> {
    diesel::sql_query(
        "INSERT INTO arena_h2h_season_ratings \
         (season_id, character_id, alt_key, rating, wins, losses, ties, matches, last_match_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, to_timestamp($9)) \
         ON CONFLICT (season_id, character_id, alt_key) DO UPDATE SET \
             rating = EXCLUDED.rating, wins = EXCLUDED.wins, losses = EXCLUDED.losses, \
             ties = EXCLUDED.ties, matches = EXCLUDED.matches, \
             last_match_at = EXCLUDED.last_match_at",
    )
    .bind::<diesel::sql_types::Uuid, _>(season_id)
    .bind::<diesel::sql_types::Uuid, _>(key.character_id)
    .bind::<diesel::sql_types::Uuid, _>(key.alt_key)
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
    promotion_currencies: u64,
    /// Gems paid by an open Arena Giveaway window, 0 when none was running.
    giveaway_gems: i64,
    /// The account's fight count after this match and any weapon gift it minted;
    /// `None` when the fight did not count or could not be counted.
    fight_tally: Option<arena_fight_gift::FightTally>,
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
            gems: 6,
            finished_fight: true,
            card_weapon: None,
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

    /// #401: the weapon the card showed lands in the backpack as that exact
    /// instance — same id, and the backpack item serializes to the card's
    /// `inventory.backpack.items` entry — and is reported in the change set.
    #[test]
    fn the_card_weapon_lands_in_the_backpack_as_the_card_showed_it() {
        use blades_lib::user_data as ud;
        let mut inv = ud::CompleteInventory {
            backpack: ud::Backpack::default(),
            loadout: ud::Loadout::default(),
            treasury: ud::Treasury::default(),
            overflow_treasury: ud::Treasury::default(),
            backpack_version: 7,
            treasury_version: 3,
        };
        let weapon = crate::arena::arena_fight_gift::weapon_instance(Uuid::from_u128(25));
        let mut tracker = InventoryChangeTracker::default();
        deliver_card_weapon(&mut inv, &weapon, &mut tracker);

        let held = inv.backpack.items.0.get(&weapon.id).expect("the weapon is in the backpack");
        let mut as_card = serde_json::to_value(&weapon).unwrap();
        as_card.as_object_mut().unwrap().remove("id");
        assert_eq!(serde_json::to_value(held).unwrap(), as_card, "the card's instance, verbatim");
        assert!(tracker.modified_backpack.items.contains(&weapon.id));
        assert_eq!(inv.backpack_version, 7, "the writer bumps once, alongside promotions");
    }

    #[test]
    fn crossing_500_stages_premium_gems_for_durable_application() {
        let promo = arena_ladder::promotion_rewards(499, 500, 86);
        let mut reward = RewardGrant::default();
        // Retail's 50, doubled by the owner-approved #390/#391 policy.
        assert_eq!(add_promotion_currencies(&mut reward, &promo), 100);
        assert_eq!(reward.currencies.get(&blades_lib::economy::GEMS), Some(&100));
        assert!(
            promotion_needs_loot_marker(&promo),
            "gem-only arena promotions still need the durable per-threshold marker"
        );
        assert!(
            !promotion_changes_backpack(&promo),
            "wallet-only promotion loot must not bump the backpack version"
        );

        // Negative control: a disabled Gem row in loot.json does not become a grant.
        let disabled = arena_ladder::promotion_rewards(549, 550, 86);
        let mut reward = RewardGrant::default();
        assert_eq!(add_promotion_currencies(&mut reward, &disabled), 0);
        assert!(!reward.currencies.contains_key(&blades_lib::economy::GEMS));
        assert!(!promotion_needs_loot_marker(&disabled));
    }

    fn plain_outcome(win: bool, gems: i64) -> MatchEconomyOutcome {
        MatchEconomyOutcome {
            character_id: Uuid::from_u128(7),
            game_session_id: None,
            level: 86,
            gold: 1_000,
            character_xp: 50,
            gems,
            finished_fight: gems > 0,
            card_weapon: None,
            trophy_delta: 0,
            rounds_won: if win { 2 } else { 0 },
            rounds_lost: if win { 0 } else { 2 },
            win,
            completed_at_secs: 1_700_000_000,
            opponent_character_id: None,
            h2h: false,
        }
    }

    /// Owner-approved deviation (#390/#391): the Gems the engine priced reach the
    /// wallet through the same grant as the gold, and only the Gems.
    #[test]
    fn a_win_credits_six_gems_and_a_loss_three() {
        use crate::arena::arena_bonus_rewards::{MatchFinish, match_gems};
        use blades_lib::economy::GEMS;
        use blades_lib::user_data::CompleteWallet;
        for (win, expected) in [(true, 6u64), (false, 3u64)] {
            let gems = match_gems(MatchFinish::of(win, false)) as i64;
            let reward = match_reward_grant(&plain_outcome(win, gems));
            assert_eq!(reward.currencies.get(&GEMS), Some(&expected));
            assert_eq!(reward.currencies.get(&ARENA_GOLD_CURRENCY_UUID_PARSED), Some(&1_000));

            // `apply_reward` credits every `currencies` entry with exactly this call.
            let mut wallet = CompleteWallet::default();
            for (currency, amount) in &reward.currencies {
                wallet.credit(*currency, *amount);
            }
            assert_eq!(wallet.balance(GEMS), expected, "the wallet holds the match Gems");
        }
        // CONTROL: an abandoned match grants no Gems at all, not a zero entry.
        let reward = match_reward_grant(&plain_outcome(false, 0));
        assert!(!reward.currencies.contains_key(&GEMS));
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
                 is_h2h BOOLEAN NOT NULL DEFAULT false, source_alt_uuid UUID)",
            // Only what the audit insert reads: the alt the match is stamped with.
            "CREATE TABLE characters (id UUID PRIMARY KEY, source_alt_uuid UUID)",
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
        apply_h2h_per_alt_migration(&mut conn).await;
        crate::testchar::apply_migration_for_tests(&mut conn).await;
        Some(conn)
    }

    /// The rating tables as production has them after #373: the real migration.
    async fn apply_h2h_per_alt_migration(conn: &mut AsyncPgConnection) {
        const UP: &str = include_str!(
            "../../../migrations/2026-10-11-000000-0000_h2h_ratings_per_alt/up.sql"
        );
        let sql: String = UP
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        for stmt in sql.split(';') {
            if !stmt.trim().is_empty() {
                diesel::sql_query(stmt).execute(&mut *conn).await.unwrap();
            }
        }
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
            promotion_currencies: 0,
            giveaway_gems: 0,
            fight_tally: None,
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
            gems: 0,
            finished_fight: true,
            card_weapon: None,
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

    /// Every alt is its own character (owner decision 2026-10-10): a match is
    /// stamped with the alt occupying the live row when it was recorded, so a
    /// switch afterwards cannot move it to another alt's season standing.
    #[tokio::test]
    async fn audit_insert_stamps_the_alt_that_played() {
        let Some(mut conn) = h2h_fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — alt stamp on match audit not verified");
            return;
        };
        let cid = Uuid::from_u128(1);
        let (alt_a, alt_b) = (Uuid::from_u128(0xa), Uuid::from_u128(0xb));
        let legacy = Uuid::from_u128(2);
        diesel::sql_query("INSERT INTO characters VALUES ($1, $2), ($3, NULL)")
            .bind::<diesel::sql_types::Uuid, _>(cid)
            .bind::<diesel::sql_types::Uuid, _>(alt_a)
            .bind::<diesel::sql_types::Uuid, _>(legacy)
            .execute(&mut conn)
            .await
            .unwrap();
        let session = |n: u128| Uuid::from_u128(100 + n);
        insert_match_audit(&mut conn, &h2h_outcome(cid, legacy, session(1), true), &applied(cid))
            .await
            .unwrap();
        diesel::sql_query("UPDATE characters SET source_alt_uuid = $1 WHERE id = $2")
            .bind::<diesel::sql_types::Uuid, _>(alt_b)
            .bind::<diesel::sql_types::Uuid, _>(cid)
            .execute(&mut conn)
            .await
            .unwrap();
        insert_match_audit(&mut conn, &h2h_outcome(cid, legacy, session(2), false), &applied(cid))
            .await
            .unwrap();
        insert_match_audit(&mut conn, &h2h_outcome(legacy, cid, session(3), true), &applied(legacy))
            .await
            .unwrap();

        #[derive(diesel::QueryableByName, Debug, PartialEq)]
        struct Stamp {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            game_session_id: Uuid,
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
            source_alt_uuid: Option<Uuid>,
        }
        let stamps: Vec<Stamp> = diesel::sql_query(
            "SELECT game_session_id, source_alt_uuid FROM arena_match_results ORDER BY game_session_id",
        )
        .get_results(&mut conn)
        .await
        .unwrap();
        assert_eq!(
            stamps,
            vec![
                Stamp { game_session_id: session(1), source_alt_uuid: Some(alt_a) },
                Stamp { game_session_id: session(2), source_alt_uuid: Some(alt_b) },
                // A character with no alt id stays unstamped: NULL reads as "live".
                Stamp { game_session_id: session(3), source_alt_uuid: None },
            ]
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
            vec![TestAuditRow {
                character_id: lower,
                is_h2h: true
            }],
            "rating table failure must not roll back the audit row"
        );

        diesel::sql_query(
            "CREATE TABLE arena_h2h_ratings ( \
                 character_id UUID NOT NULL, alt_key UUID NOT NULL, rating INTEGER NOT NULL, \
                 wins INTEGER NOT NULL DEFAULT 0, \
                 losses INTEGER NOT NULL DEFAULT 0, ties INTEGER NOT NULL DEFAULT 0, \
                 matches INTEGER NOT NULL DEFAULT 0, last_match_at TIMESTAMPTZ, \
                 PRIMARY KEY (character_id, alt_key))",
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

    /// The live increment keys by alt as the reconcile does (tracker #373): a
    /// user who fights as one alt, switches, and fights as another gets two
    /// board rows, and the reconcile afterwards changes nothing.
    #[tokio::test]
    async fn live_increment_rates_each_alt_and_agrees_with_reconcile() {
        let Some(mut conn) = h2h_fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — per-alt h2h increment not verified");
            return;
        };
        let cfg = ranking::H2hRatingConfig::default();
        diesel::sql_query(
            "INSERT INTO arena_seasons (id, number, name, starts_at, ends_at) \
             VALUES ($1, 1, 'test', 1700000000, 1700002000)",
        )
        .bind::<diesel::sql_types::Uuid, _>(Uuid::from_u128(10))
        .execute(&mut conn)
        .await
        .unwrap();
        let (me, opp) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let (alt_a, alt_b) = (Uuid::from_u128(0xa), Uuid::from_u128(0xb));
        diesel::sql_query("INSERT INTO characters VALUES ($1, $2), ($3, NULL)")
            .bind::<diesel::sql_types::Uuid, _>(me)
            .bind::<diesel::sql_types::Uuid, _>(alt_a)
            .bind::<diesel::sql_types::Uuid, _>(opp)
            .execute(&mut conn)
            .await
            .unwrap();

        async fn fight(conn: &mut AsyncPgConnection, cfg: &ranking::H2hRatingConfig, gsid: u128) {
            let (me, opp) = (Uuid::from_u128(1), Uuid::from_u128(2));
            let gs = Uuid::from_u128(gsid);
            // Distinct times: Elo is order-dependent and the replay orders by time.
            let mut mine = h2h_outcome(me, opp, gs, true);
            mine.completed_at_secs += gsid as i64;
            persist_h2h_audit_and_rating(conn, cfg, &mine, &applied(me), opp).await.unwrap();
            let mut theirs = h2h_outcome(opp, me, gs, false);
            theirs.completed_at_secs = mine.completed_at_secs;
            persist_h2h_audit_and_rating(conn, cfg, &theirs, &applied(opp), me).await.unwrap();
        }
        fight(&mut conn, &cfg, 100).await;
        diesel::sql_query("UPDATE characters SET source_alt_uuid = $1 WHERE id = $2")
            .bind::<diesel::sql_types::Uuid, _>(alt_b)
            .bind::<diesel::sql_types::Uuid, _>(me)
            .execute(&mut conn)
            .await
            .unwrap();
        fight(&mut conn, &cfg, 101).await;

        #[derive(Debug, PartialEq, Eq, diesel::QueryableByName)]
        struct Row {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            character_id: Uuid,
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            alt_key: Uuid,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            rating: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            matches: i32,
        }
        async fn rows(conn: &mut AsyncPgConnection, table: &str) -> Vec<Row> {
            diesel::sql_query(format!(
                "SELECT character_id, alt_key, rating, matches FROM {table} ORDER BY character_id, alt_key"
            ))
            .get_results(conn)
            .await
            .unwrap()
        }
        let live_all = rows(&mut conn, "arena_h2h_ratings").await;
        let live_season = rows(&mut conn, "arena_h2h_season_ratings").await;
        assert_eq!(
            live_all.iter().map(|r| (r.character_id, r.alt_key, r.matches)).collect::<Vec<_>>(),
            vec![(me, alt_a, 1), (me, alt_b, 1), (opp, opp, 2)],
            "two alts, two rows; the opponent without an alt id is keyed by its own id"
        );
        assert_eq!(live_season.len(), 3);

        ranking::reconcile_h2h_ratings_with_config(&mut conn, &cfg, false).await.unwrap();
        assert_eq!(rows(&mut conn, "arena_h2h_ratings").await, live_all, "reconcile agrees");
        assert_eq!(rows(&mut conn, "arena_h2h_season_ratings").await, live_season);
    }

    /// The live increment skips a fight with an operator test alt
    /// (`crate::testchar`), exactly as the replay does, so the two still
    /// agree and the opponent's rating is not moved.
    #[tokio::test]
    async fn live_increment_does_not_rate_a_test_alt() {
        let Some(mut conn) = h2h_fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — test-alt h2h increment not verified");
            return;
        };
        let cfg = ranking::H2hRatingConfig::default();
        let (me, opp) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let (real_alt, test_alt) = (Uuid::from_u128(0xa), Uuid::from_u128(0xb));
        diesel::sql_query("INSERT INTO characters VALUES ($1, $2), ($3, NULL)")
        .bind::<diesel::sql_types::Uuid, _>(me)
        .bind::<diesel::sql_types::Uuid, _>(real_alt)
        .bind::<diesel::sql_types::Uuid, _>(opp)
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query("INSERT INTO test_characters (alt_uuid, user_id, marked_by) VALUES ($1, $1, 't')")
            .bind::<diesel::sql_types::Uuid, _>(test_alt)
            .execute(&mut conn)
            .await
            .unwrap();

        async fn fight(conn: &mut AsyncPgConnection, cfg: &ranking::H2hRatingConfig, gsid: u128) {
            let (me, opp) = (Uuid::from_u128(1), Uuid::from_u128(2));
            let gs = Uuid::from_u128(gsid);
            let mut mine = h2h_outcome(me, opp, gs, true);
            mine.completed_at_secs += gsid as i64;
            persist_h2h_audit_and_rating(conn, cfg, &mine, &applied(me), opp).await.unwrap();
            let mut theirs = h2h_outcome(opp, me, gs, false);
            theirs.completed_at_secs = mine.completed_at_secs;
            persist_h2h_audit_and_rating(conn, cfg, &theirs, &applied(opp), me).await.unwrap();
        }
        #[derive(Debug, PartialEq, Eq, diesel::QueryableByName)]
        struct Row {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            alt_key: Uuid,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            rating: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            matches: i32,
        }
        async fn all_time(conn: &mut AsyncPgConnection) -> Vec<Row> {
            diesel::sql_query("SELECT alt_key, rating, matches FROM arena_h2h_ratings ORDER BY alt_key")
                .get_results(conn)
                .await
                .unwrap()
        }

        // The control: the real alt's fight is rated.
        fight(&mut conn, &cfg, 100).await;
        let after_real = all_time(&mut conn).await;
        assert_eq!(
            after_real.iter().map(|r| (r.alt_key, r.matches)).collect::<Vec<_>>(),
            vec![(opp, 1), (Uuid::from_u128(0xa), 1)]
        );

        // Switch the test alt in and fight again: nothing moves.
        diesel::sql_query("UPDATE characters SET source_alt_uuid = $1 WHERE id = $2")
            .bind::<diesel::sql_types::Uuid, _>(test_alt)
            .bind::<diesel::sql_types::Uuid, _>(me)
            .execute(&mut conn)
            .await
            .unwrap();
        fight(&mut conn, &cfg, 101).await;
        assert_eq!(all_time(&mut conn).await, after_real, "no row for the test alt, opponent unmoved");

        // The fight is still in the audit (it happened), and the rebuild agrees.
        ranking::reconcile_h2h_ratings_with_config(&mut conn, &cfg, false).await.unwrap();
        assert_eq!(all_time(&mut conn).await, after_real, "reconcile agrees");
    }
}
