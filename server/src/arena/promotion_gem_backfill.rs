//! One-off repair for arena-promotion Gems that were not paid before #413.
//!
//! Retail's new-arena level-1 promotion tables grant fixed Gems at Arena 2-6.
//! The live match path pays them now; this module repairs characters whose
//! current-season high-water mark had already reached those arenas before that
//! deploy. The idempotency guard is the same server-only
//! `arenaPromotionLootGrants` threshold set that live promotion persistence uses.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use blades_lib::{
    economy::GEMS,
    server_state::ServerState,
    user_data::{CompleteCharacter, CompleteWallet},
};
use diesel::OptionalExtension;
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{DbPool, arena::arena_ladder};

// When fork #413 (live promotion gems) finished deploying: its publish run
// completed 2026-09-28 08:31:49 UTC. Crossings after this were paid live.
pub const DEFAULT_LIVE_GRANT_START_SECS: i64 = 1_790_584_309;

const GEM_BACKFILL_AUDIT_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS arena_promotion_gem_backfill_audit (
    id UUID PRIMARY KEY,
    character_id UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    arena INTEGER NOT NULL,
    threshold BIGINT NOT NULL,
    gems BIGINT NOT NULL,
    character_name TEXT NOT NULL,
    old_gems BIGINT NOT NULL,
    new_gems BIGINT NOT NULL,
    live_grant_start_secs BIGINT NOT NULL,
    reason TEXT NOT NULL DEFAULT 'arena promotion gem backfill',
    granted_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    UNIQUE (character_id, threshold)
);
"#;

#[derive(Debug, Clone)]
pub struct BackfillOptions {
    pub apply: bool,
    pub live_grant_start_secs: i64,
    pub bot_user_ids: BTreeSet<Uuid>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillReport {
    pub applied: bool,
    pub live_grant_start_secs: i64,
    pub characters_seen: usize,
    pub characters_skipped_bots: usize,
    pub characters_unreadable: usize,
    pub characters_with_grants: usize,
    pub total_gems: u64,
    pub grants: Vec<BackfillGrantReport>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackfillGrantReport {
    pub character_id: Uuid,
    pub user_id: Uuid,
    pub character_name: String,
    pub arena: u8,
    pub threshold: i64,
    pub gems: u64,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PromotionGemDue {
    arena: u8,
    threshold: i64,
    gems: u64,
    reason: &'static str,
}

#[derive(diesel::QueryableByName)]
struct CharacterBackfillRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    user_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    character: Value,
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    wallet: Value,
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    server_state: Value,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    is_ai_mimic: bool,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    arena2_first_reached_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    arena3_first_reached_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    arena4_first_reached_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    arena5_first_reached_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    arena6_first_reached_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    arena2_audited: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    arena3_audited: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    arena4_audited: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    arena5_audited: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    arena6_audited: bool,
}

const CHARACTER_BACKFILL_SQL: &str = r#"
SELECT
    c.id,
    c.user_id,
    c.character,
    c.wallet,
    c.server_state,
    (m.character_id IS NOT NULL) AS is_ai_mimic,
    (
        SELECT EXTRACT(EPOCH FROM MIN(r.recorded_at))::bigint
        FROM arena_match_results r
        WHERE r.character_id = c.id AND r.matchmaking_trophies_after >= 500
    ) AS arena2_first_reached_at,
    (
        SELECT EXTRACT(EPOCH FROM MIN(r.recorded_at))::bigint
        FROM arena_match_results r
        WHERE r.character_id = c.id AND r.matchmaking_trophies_after >= 1000
    ) AS arena3_first_reached_at,
    (
        SELECT EXTRACT(EPOCH FROM MIN(r.recorded_at))::bigint
        FROM arena_match_results r
        WHERE r.character_id = c.id AND r.matchmaking_trophies_after >= 1500
    ) AS arena4_first_reached_at,
    (
        SELECT EXTRACT(EPOCH FROM MIN(r.recorded_at))::bigint
        FROM arena_match_results r
        WHERE r.character_id = c.id AND r.matchmaking_trophies_after >= 2000
    ) AS arena5_first_reached_at,
    (
        SELECT EXTRACT(EPOCH FROM MIN(r.recorded_at))::bigint
        FROM arena_match_results r
        WHERE r.character_id = c.id AND r.matchmaking_trophies_after >= 2500
    ) AS arena6_first_reached_at,
    EXISTS (
        SELECT 1 FROM arena_promotion_gem_backfill_audit a
        WHERE a.character_id = c.id AND a.threshold = 500
    ) AS arena2_audited,
    EXISTS (
        SELECT 1 FROM arena_promotion_gem_backfill_audit a
        WHERE a.character_id = c.id AND a.threshold = 1000
    ) AS arena3_audited,
    EXISTS (
        SELECT 1 FROM arena_promotion_gem_backfill_audit a
        WHERE a.character_id = c.id AND a.threshold = 1500
    ) AS arena4_audited,
    EXISTS (
        SELECT 1 FROM arena_promotion_gem_backfill_audit a
        WHERE a.character_id = c.id AND a.threshold = 2000
    ) AS arena5_audited,
    EXISTS (
        SELECT 1 FROM arena_promotion_gem_backfill_audit a
        WHERE a.character_id = c.id AND a.threshold = 2500
    ) AS arena6_audited
FROM characters c
LEFT JOIN arena_ai_mimics m ON m.character_id = c.id
"#;

const ORDER_BY_CHARACTER: &str = " ORDER BY COALESCE(c.character ->> 'name', ''), c.id";

pub async fn run(pool: &DbPool, options: BackfillOptions) -> Result<BackfillReport> {
    let mut conn = pool.get().await?;
    ensure_audit_table(&mut conn).await?;
    let rows: Vec<CharacterBackfillRow> =
        diesel::sql_query(format!("{CHARACTER_BACKFILL_SQL}{ORDER_BY_CHARACTER}"))
            .get_results(&mut conn)
            .await
            .context("reading arena promotion gem backfill candidates")?;

    let mut report = BackfillReport {
        applied: options.apply,
        live_grant_start_secs: options.live_grant_start_secs,
        characters_seen: rows.len(),
        characters_skipped_bots: 0,
        characters_unreadable: 0,
        characters_with_grants: 0,
        total_gems: 0,
        grants: Vec::new(),
    };

    for row in rows {
        let planned = plan_row(&row, &options);
        match planned {
            RowPlan::SkippedBot => report.characters_skipped_bots += 1,
            RowPlan::Unreadable => report.characters_unreadable += 1,
            RowPlan::Noop => {}
            RowPlan::Grant(grants) => {
                let grants = if options.apply {
                    apply_character(pool, row.id, &options).await?
                } else {
                    grants
                        .into_iter()
                        .map(|g| report_grant(&row, character_name(&row.character), g))
                        .collect()
                };
                if grants.is_empty() {
                    continue;
                }
                report.characters_with_grants += 1;
                report.total_gems += grants.iter().map(|g| g.gems).sum::<u64>();
                report.grants.extend(grants);
            }
        }
    }

    Ok(report)
}

async fn ensure_audit_table(conn: &mut diesel_async::AsyncPgConnection) -> Result<()> {
    diesel::sql_query(GEM_BACKFILL_AUDIT_DDL)
        .execute(conn)
        .await
        .context("creating arena promotion gem backfill audit table")?;
    Ok(())
}

async fn apply_character(
    pool: &DbPool,
    character_id: Uuid,
    options: &BackfillOptions,
) -> Result<Vec<BackfillGrantReport>> {
    let options = options.clone();
    let mut conn = pool.get().await?;
    conn.transaction::<_, anyhow::Error, _>(|mut conn| {
        async move {
            ensure_audit_table(&mut conn).await?;
            let row: Option<CharacterBackfillRow> = diesel::sql_query(format!(
                "{CHARACTER_BACKFILL_SQL} WHERE c.id = $1 FOR UPDATE OF c"
            ))
            .bind::<diesel::sql_types::Uuid, _>(character_id)
            .get_result(&mut conn)
            .await
            .optional()?;
            let Some(row) = row else {
                return Ok(Vec::new());
            };
            let RowPlan::Grant(due) = plan_row(&row, &options) else {
                return Ok(Vec::new());
            };

            let mut wallet: CompleteWallet = serde_json::from_value(row.wallet.clone())
                .context("stored wallet did not deserialize")?;
            let mut server_state: ServerState = serde_json::from_value(row.server_state.clone())
                .context("stored server_state did not deserialize")?;
            let character: CompleteCharacter = serde_json::from_value(row.character.clone())
                .context("stored character did not deserialize")?;
            let old_gems = wallet.balance(GEMS);

            let total_gems = due.iter().map(|grant| grant.gems).sum::<u64>();
            for grant in &due {
                server_state
                    .arena_promotion_loot_grants
                    .insert(grant.threshold);
            }
            wallet.credit(GEMS, total_gems);

            let new_gems = wallet.balance(GEMS);
            let wallet_value = serde_json::to_value(&wallet)?;
            let server_state_value = serde_json::to_value(&server_state)?;
            diesel::sql_query("UPDATE characters SET wallet = $1, server_state = $2 WHERE id = $3")
                .bind::<diesel::sql_types::Jsonb, _>(wallet_value)
                .bind::<diesel::sql_types::Jsonb, _>(server_state_value)
                .bind::<diesel::sql_types::Uuid, _>(row.id)
                .execute(&mut conn)
                .await?;

            let mut reports = Vec::new();
            for grant in due {
                diesel::sql_query(
                    "INSERT INTO arena_promotion_gem_backfill_audit \
                     (id, character_id, arena, threshold, gems, character_name, old_gems, \
                      new_gems, live_grant_start_secs) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                )
                .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
                .bind::<diesel::sql_types::Uuid, _>(row.id)
                .bind::<diesel::sql_types::Integer, _>(grant.arena as i32)
                .bind::<diesel::sql_types::BigInt, _>(grant.threshold)
                .bind::<diesel::sql_types::BigInt, _>(grant.gems as i64)
                .bind::<diesel::sql_types::Text, _>(&character.name)
                .bind::<diesel::sql_types::BigInt, _>(old_gems as i64)
                .bind::<diesel::sql_types::BigInt, _>(new_gems as i64)
                .bind::<diesel::sql_types::BigInt, _>(options.live_grant_start_secs)
                .execute(&mut conn)
                .await?;
                reports.push(report_grant(&row, character.name.clone(), grant));
            }
            Ok(reports)
        }
        .scope_boxed()
    })
    .await
}

#[derive(Debug, PartialEq, Eq)]
enum RowPlan {
    SkippedBot,
    Unreadable,
    Noop,
    Grant(Vec<PromotionGemDue>),
}

fn plan_row(row: &CharacterBackfillRow, options: &BackfillOptions) -> RowPlan {
    let character: CompleteCharacter = match serde_json::from_value(row.character.clone()) {
        Ok(c) => c,
        Err(_) => return RowPlan::Unreadable,
    };
    let state: ServerState = match serde_json::from_value(row.server_state.clone()) {
        Ok(s) => s,
        Err(_) => return RowPlan::Unreadable,
    };
    if is_bot_or_mimic(
        &character.name,
        row.is_ai_mimic,
        row.user_id,
        &options.bot_user_ids,
    ) {
        return RowPlan::SkippedBot;
    }
    let audited = |arena| match arena {
        2 => row.arena2_audited,
        3 => row.arena3_audited,
        4 => row.arena4_audited,
        5 => row.arena5_audited,
        6 => row.arena6_audited,
        _ => false,
    };
    let first_reached_at = |arena| match arena {
        2 => row.arena2_first_reached_at,
        3 => row.arena3_first_reached_at,
        4 => row.arena4_first_reached_at,
        5 => row.arena5_first_reached_at,
        6 => row.arena6_first_reached_at,
        _ => None,
    };
    let due = promotion_gems_due(
        character.highest_arena_reached as u8,
        character.level,
        &state.arena_promotion_loot_grants,
        &audited,
        &first_reached_at,
        options.live_grant_start_secs,
    );
    if due.is_empty() {
        RowPlan::Noop
    } else {
        RowPlan::Grant(due)
    }
}

fn promotion_gems_due(
    highest_arena_reached: u8,
    character_level: u16,
    granted_thresholds: &BTreeSet<i64>,
    audited: &dyn Fn(u8) -> bool,
    first_reached_at: &dyn Fn(u8) -> Option<i64>,
    live_grant_start_secs: i64,
) -> Vec<PromotionGemDue> {
    arena_ladder::ARENA_LADDER
        .iter()
        .filter(|tier| tier.arena >= 2 && tier.level == 1 && tier.arena <= highest_arena_reached)
        .filter(|tier| !granted_thresholds.contains(&tier.required_trophies))
        .filter(|tier| !audited(tier.arena))
        .filter(|tier| {
            first_reached_at(tier.arena)
                .map(|first| first < live_grant_start_secs)
                // No match of ours crossed the threshold: the arena was reached
                // on retail (an import) and retail already paid its promotion.
                .unwrap_or(false)
        })
        .filter_map(|tier| {
            let gems =
                super::arena_promotion_loot::currencies_for(tier.loot_table, character_level)
                    .iter()
                    .filter(|c| c.currency_uuid == GEMS.to_string())
                    .map(|c| c.quantity)
                    .sum::<u64>();
            (gems > 0).then_some(PromotionGemDue {
                arena: tier.arena,
                threshold: tier.required_trophies,
                gems,
                reason: "reached before live grant deploy and no promotion marker",
            })
        })
        .collect()
}

fn is_bot_or_mimic(
    character_name: &str,
    is_ai_mimic: bool,
    user_id: Uuid,
    bot_user_ids: &BTreeSet<Uuid>,
) -> bool {
    is_ai_mimic || character_name.trim() == "(AI)" || bot_user_ids.contains(&user_id)
}

fn report_grant(
    row: &CharacterBackfillRow,
    character_name: String,
    grant: PromotionGemDue,
) -> BackfillGrantReport {
    BackfillGrantReport {
        character_id: row.id,
        user_id: row.user_id,
        character_name,
        arena: grant.arena,
        threshold: grant.threshold,
        gems: grant.gems,
        reason: grant.reason,
    }
}

fn character_name(value: &Value) -> String {
    value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_audit(_: u8) -> bool {
        false
    }

    fn pre_live_first_reach(arena: u8) -> Option<i64> {
        match arena {
            2 => Some(DEFAULT_LIVE_GRANT_START_SECS - 180),
            3 => Some(DEFAULT_LIVE_GRANT_START_SECS - 120),
            4 => Some(DEFAULT_LIVE_GRANT_START_SECS - 60),
            _ => None,
        }
    }

    #[test]
    fn backfill_is_idempotent_after_marking_the_same_thresholds() {
        let mut markers = BTreeSet::new();
        let first = promotion_gems_due(
            4,
            86,
            &markers,
            &no_audit,
            &pre_live_first_reach,
            DEFAULT_LIVE_GRANT_START_SECS,
        );
        assert_eq!(
            first.iter().map(|g| (g.arena, g.gems)).collect::<Vec<_>>(),
            vec![(2, 50), (3, 100), (4, 150)]
        );
        markers.extend(first.iter().map(|g| g.threshold));

        let second = promotion_gems_due(
            4,
            86,
            &markers,
            &no_audit,
            &pre_live_first_reach,
            DEFAULT_LIVE_GRANT_START_SECS,
        );
        assert!(second.is_empty(), "a second run must pay zero");
    }

    #[test]
    fn live_path_and_cutoff_prevent_double_pay() {
        let mut markers = BTreeSet::new();
        markers.insert(500);
        let first_reached_after_live = |arena| match arena {
            2 => Some(DEFAULT_LIVE_GRANT_START_SECS + 60),
            3 => Some(DEFAULT_LIVE_GRANT_START_SECS + 120),
            _ => None,
        };
        let due = promotion_gems_due(
            3,
            86,
            &markers,
            &no_audit,
            &first_reached_after_live,
            DEFAULT_LIVE_GRANT_START_SECS,
        );
        assert!(due.is_empty());
    }

    #[test]
    fn bots_and_mimics_are_excluded() {
        let user_id = Uuid::new_v4();
        let mut bots = BTreeSet::new();
        bots.insert(user_id);

        assert!(is_bot_or_mimic(
            "Real Name",
            true,
            Uuid::new_v4(),
            &BTreeSet::new()
        ));
        assert!(is_bot_or_mimic(
            "(AI)",
            false,
            Uuid::new_v4(),
            &BTreeSet::new()
        ));
        assert!(is_bot_or_mimic("Legacy", false, user_id, &bots));
        assert!(!is_bot_or_mimic("Human", false, Uuid::new_v4(), &bots));
    }

    #[test]
    fn ordinary_arena_rungs_are_not_backfilled_as_gems() {
        let due = promotion_gems_due(
            2,
            86,
            &BTreeSet::new(),
            &no_audit,
            &pre_live_first_reach,
            DEFAULT_LIVE_GRANT_START_SECS,
        );
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].threshold, 500);
        assert_eq!(due[0].gems, 50);
    }

    #[test]
    fn an_arena_reached_without_any_match_of_ours_is_not_paid() {
        // An imported retail character already holds its arena; retail paid it.
        let due = promotion_gems_due(
            4,
            86,
            &BTreeSet::new(),
            &no_audit,
            &|_| None,
            DEFAULT_LIVE_GRANT_START_SECS,
        );
        assert!(due.is_empty());
    }
}
