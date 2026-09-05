//! Seasons as DATA, so a season can be opened and closed without a rebuild.
//!
//! `arena_season::SEASONS` is a compile-time `[SeasonConfig; 1]`. Opening a
//! season therefore meant editing Rust, rebuilding the image and redeploying —
//! which is exactly why the 2026-09-01 attempt never landed: nothing could
//! actually run it, and zero characters ever carried that season's id.
//!
//! This module keeps seasons in `arena_seasons` and converts a row into the
//! same `SeasonConfig` the existing rollover already understands, so the proven
//! `roll_character_into` path is reused rather than reimplemented.
//!
//! Ending a season is three things in one order, and the order matters:
//!
//! 1. **Freeze the ladder** into `arena_season_standings`. The live standings
//!    live on the character (`pvpTrophies`) and step 3 zeroes them, so a
//!    snapshot taken afterwards would record all zeros. This is also what makes
//!    "keep all the data" true: matches were already durable in
//!    `arena_match_results`, but the final placings existed nowhere.
//! 2. **Record awards** from that frozen ladder, ungranted.
//! 3. **Roll characters** into the next season (zeroing counters).
//!
//! Steps 1 and 2 are pure reads plus inserts; only step 3 mutates players. A
//! failure between 2 and 3 leaves a season with standings and awards recorded
//! and players untouched, which is re-runnable. The reverse order would not be.

use std::collections::HashMap;

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::arena_season::{ScoringVariant, SeasonConfig, TrophyResetRule};

/// A row of `arena_seasons`.
#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = crate::schema::arena_seasons)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct SeasonRow {
    pub id: Uuid,
    pub number: i32,
    pub name: String,
    pub starts_at: i64,
    pub ends_at: i64,
    pub status: String,
    pub scoring: String,
    pub reset_rule: String,
    pub created_at: i64,
    pub ended_at: Option<i64>,
}

#[derive(Insertable)]
#[diesel(table_name = crate::schema::arena_seasons)]
pub struct NewSeason {
    pub id: Uuid,
    pub number: i32,
    pub name: String,
    pub starts_at: i64,
    pub ends_at: i64,
    pub status: String,
    pub scoring: String,
    pub reset_rule: String,
}

impl SeasonRow {
    /// The `SeasonConfig` the existing rollover takes.
    ///
    /// Unknown `scoring` / `reset_rule` strings fall back to the shipped
    /// behaviour rather than erroring: a season row written by a future build
    /// must not make an older binary refuse to close it, which would strand a
    /// live ladder with no way out.
    pub fn config(&self) -> SeasonConfig {
        SeasonConfig {
            id: self.id,
            number: self.number.max(0) as u32,
            start_unix: self.starts_at,
            end_unix: self.ends_at,
            scoring: match self.scoring.as_str() {
                "shipped" => ScoringVariant::Shipped,
                _ => ScoringVariant::Shipped,
            },
            reset: match self.reset_rule.as_str() {
                "hard_reset" => TrophyResetRule::HardReset,
                _ => TrophyResetRule::HardReset,
            },
        }
    }
}

/// One row of the frozen ladder.
#[derive(Debug, Clone, Insertable, Serialize)]
#[diesel(table_name = crate::schema::arena_season_standings)]
pub struct StandingRow {
    pub season_id: Uuid,
    pub character_id: Uuid,
    pub rank: i32,
    pub trophies: i64,
    pub matches: i32,
    pub wins: i32,
    pub guild_id: Option<String>,
}

#[derive(Debug, Clone, Insertable, Serialize)]
#[diesel(table_name = crate::schema::arena_season_guild_standings)]
pub struct GuildStandingRow {
    pub season_id: Uuid,
    pub guild_id: String,
    pub rank: i32,
    pub trophies: i64,
    pub members: i32,
}

#[derive(Debug, Clone, Insertable, Serialize)]
#[diesel(table_name = crate::schema::arena_season_awards)]
pub struct AwardRow {
    pub id: Uuid,
    pub season_id: Uuid,
    pub character_id: Uuid,
    pub kind: String,
    pub rank: i32,
    pub tier: String,
    pub payload: Value,
}

/// Which bracket a placing falls in.
///
/// ASSUMPTION, and it is flagged rather than hidden: the capture set contains
/// **no** season-reward endpoint — the only `reward` traffic is the daily town
/// reward — so retail's exact prize table is not recoverable from what we hold.
/// These brackets are the conventional 1 / 2-3 / 4-10 / 11-50 / 51-100 shape,
/// and the payload is deliberately a description rather than an item grant, so
/// changing the prizes later is a data edit and not a migration.
///
/// `None` beyond 100: the ladder the client shows is a top-100, so a placing
/// outside it is not a placing anyone saw.
pub fn rank_tier(rank: i32) -> Option<&'static str> {
    match rank {
        1 => Some("champion"),
        2..=3 => Some("top3"),
        4..=10 => Some("top10"),
        11..=50 => Some("top50"),
        51..=100 => Some("top100"),
        _ => None,
    }
}

/// The reward payload recorded for a tier. Descriptive on purpose — see
/// `rank_tier`. Granting reads this; nothing here grants by itself.
pub fn award_payload(kind: &str, tier: &str, rank: i32) -> Value {
    json!({
        "kind": kind,
        "tier": tier,
        "rank": rank,
        // Nothing is handed out until a grant step runs, so an award being
        // recorded can never silently duplicate items.
        "granted": false,
        "source": "arena-season-end",
    })
}

/// Live standings for every character that scored, best first.
///
/// Reads `pvpTrophies` off the character JSON — the same number the client's
/// ladder shows — and joins match counts from `arena_match_results`, which is
/// already durable per match.
pub async fn freeze_standings(
    conn: &mut AsyncPgConnection,
    season_id: Uuid,
) -> QueryResult<Vec<StandingRow>> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        character_id: Uuid,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        trophies: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        matches: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        wins: i64,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        guild_id: Option<String>,
    }

    // COALESCE because a character that never queued has no rows in
    // arena_match_results, and an INNER JOIN would drop them from the ladder
    // entirely rather than placing them last.
    let rows: Vec<Row> = diesel::sql_query(
        "SELECT c.id AS character_id, \
                COALESCE((c.character->>'pvpTrophies')::bigint, 0) AS trophies, \
                COALESCE(m.matches, 0) AS matches, \
                COALESCE(m.wins, 0) AS wins, \
                gm.guild_id AS guild_id \
         FROM characters c \
         LEFT JOIN ( \
             SELECT character_id, COUNT(*) AS matches, \
                    COUNT(*) FILTER (WHERE win) AS wins \
             FROM arena_match_results GROUP BY character_id \
         ) m ON m.character_id = c.id \
         LEFT JOIN guild_members gm ON gm.character_id = c.id \
         WHERE COALESCE((c.character->>'pvpTrophies')::bigint, 0) > 0 \
         ORDER BY trophies DESC, matches DESC, c.id",
    )
    .get_results(conn)
    .await?;

    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| StandingRow {
            season_id,
            character_id: r.character_id,
            rank: (i as i32) + 1,
            trophies: r.trophies,
            matches: r.matches as i32,
            wins: r.wins as i32,
            guild_id: r.guild_id,
        })
        .collect())
}

/// Guild ladder for the season, derived from the character standings just
/// frozen rather than from `guilds.trophies` — the guild row is a running
/// total that the rollover does not reset, so using it would carry last
/// season's score into this one's result.
pub fn guild_standings_from(season_id: Uuid, standings: &[StandingRow]) -> Vec<GuildStandingRow> {
    let mut totals: HashMap<&str, (i64, i32)> = HashMap::new();
    for s in standings {
        if let Some(g) = s.guild_id.as_deref() {
            let e = totals.entry(g).or_insert((0, 0));
            e.0 += s.trophies;
            e.1 += 1;
        }
    }
    let mut rows: Vec<GuildStandingRow> = totals
        .into_iter()
        .map(|(g, (trophies, members))| GuildStandingRow {
            season_id,
            guild_id: g.to_string(),
            rank: 0,
            trophies,
            members,
        })
        .collect();
    // Ties broken by guild id so two runs of the same data produce the same
    // ladder; an unstable sort here would make awards non-reproducible.
    rows.sort_by(|a, b| {
        b.trophies
            .cmp(&a.trophies)
            .then_with(|| a.guild_id.cmp(&b.guild_id))
    });
    for (i, r) in rows.iter_mut().enumerate() {
        r.rank = (i as i32) + 1;
    }
    rows
}

/// Awards implied by a frozen ladder. Pure: it grants nothing and writes
/// nothing, so a dry run can show exactly what a real run would record.
pub fn awards_from(
    season_id: Uuid,
    standings: &[StandingRow],
    guilds: &[GuildStandingRow],
) -> Vec<AwardRow> {
    let mut out = Vec::new();

    for s in standings {
        if let Some(tier) = rank_tier(s.rank) {
            out.push(AwardRow {
                id: Uuid::new_v4(),
                season_id,
                character_id: s.character_id,
                kind: "rank".into(),
                rank: s.rank,
                tier: tier.into(),
                payload: award_payload("rank", tier, s.rank),
            });
        }
    }

    // Guild awards go to the MEMBERS, because a guild cannot hold an item.
    let guild_rank: HashMap<&str, i32> = guilds
        .iter()
        .map(|g| (g.guild_id.as_str(), g.rank))
        .collect();
    for s in standings {
        let Some(g) = s.guild_id.as_deref() else {
            continue;
        };
        let Some(&rank) = guild_rank.get(g) else {
            continue;
        };
        if let Some(tier) = rank_tier(rank) {
            out.push(AwardRow {
                id: Uuid::new_v4(),
                season_id,
                character_id: s.character_id,
                kind: "guild_rank".into(),
                rank,
                tier: tier.into(),
                payload: award_payload("guild_rank", tier, rank),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(id: Uuid, rank: i32, trophies: i64, guild: Option<&str>) -> StandingRow {
        StandingRow {
            season_id: Uuid::nil(),
            character_id: id,
            rank,
            trophies,
            matches: 0,
            wins: 0,
            guild_id: guild.map(|g| g.to_string()),
        }
    }

    #[test]
    fn tiers_cover_the_top_hundred_and_stop_there() {
        assert_eq!(rank_tier(1), Some("champion"));
        assert_eq!(rank_tier(3), Some("top3"));
        assert_eq!(rank_tier(10), Some("top10"));
        assert_eq!(rank_tier(50), Some("top50"));
        assert_eq!(rank_tier(100), Some("top100"));
        // The client's ladder is a top-100; 101st is not a placing anyone saw,
        // so it must not silently earn the bottom bracket.
        assert_eq!(rank_tier(101), None);
        assert_eq!(rank_tier(0), None, "rank is 1-based");
    }

    #[test]
    fn guild_ladder_sums_members_not_the_guild_row() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let st = vec![
            standing(a, 1, 100, Some("g1")),
            standing(b, 2, 60, Some("g2")),
            standing(c, 3, 50, Some("g1")),
        ];
        let g = guild_standings_from(Uuid::nil(), &st);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].guild_id, "g1");
        assert_eq!(g[0].trophies, 150, "must sum its members");
        assert_eq!(g[0].members, 2);
        assert_eq!(g[1].guild_id, "g2");
    }

    /// Two runs over the same ladder must produce the same guild order, or the
    /// awards a player gets would depend on hash iteration order.
    #[test]
    fn guild_ladder_is_deterministic_on_ties() {
        let st = vec![
            standing(Uuid::new_v4(), 1, 10, Some("bbb")),
            standing(Uuid::new_v4(), 2, 10, Some("aaa")),
            standing(Uuid::new_v4(), 3, 10, Some("ccc")),
        ];
        let first = guild_standings_from(Uuid::nil(), &st);
        for _ in 0..20 {
            let again = guild_standings_from(Uuid::nil(), &st);
            let a: Vec<_> = first.iter().map(|g| (&g.guild_id, g.rank)).collect();
            let b: Vec<_> = again.iter().map(|g| (&g.guild_id, g.rank)).collect();
            assert_eq!(a, b, "tie order must not depend on hash iteration");
        }
        assert_eq!(first[0].guild_id, "aaa", "ties break by id, ascending");
    }

    #[test]
    fn awards_cover_both_kinds_and_never_double_up_per_kind() {
        let a = Uuid::new_v4();
        let st = vec![standing(a, 1, 100, Some("g1"))];
        let g = guild_standings_from(Uuid::nil(), &st);
        let aw = awards_from(Uuid::nil(), &st, &g);
        let kinds: Vec<&str> = aw.iter().map(|x| x.kind.as_str()).collect();
        assert!(kinds.contains(&"rank"));
        assert!(kinds.contains(&"guild_rank"));
        // The unique index is (season, character, kind); producing two of one
        // kind would make the insert fail at season end, in production.
        assert_eq!(kinds.iter().filter(|k| **k == "rank").count(), 1);
        assert_eq!(kinds.iter().filter(|k| **k == "guild_rank").count(), 1);
    }

    #[test]
    fn a_guildless_player_gets_only_a_rank_award() {
        let st = vec![standing(Uuid::new_v4(), 1, 100, None)];
        let g = guild_standings_from(Uuid::nil(), &st);
        assert!(g.is_empty());
        let aw = awards_from(Uuid::nil(), &st, &g);
        assert_eq!(aw.len(), 1);
        assert_eq!(aw[0].kind, "rank");
    }

    #[test]
    fn nothing_is_granted_at_record_time() {
        let st = vec![standing(Uuid::new_v4(), 1, 5, None)];
        let aw = awards_from(Uuid::nil(), &st, &[]);
        assert_eq!(aw[0].payload["granted"], json!(false));
    }
}
