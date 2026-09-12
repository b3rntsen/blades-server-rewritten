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
//! The close handler performs all three steps in one transaction. Any failure
//! rolls back the standings, awards, season status, and character resets
//! together, leaving the active season safe to retry.

use std::collections::HashMap;

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::arena_season::{ScoringVariant, SeasonConfig, TrophyResetRule};

/// A row of `arena_seasons`.
#[derive(Debug, Clone, Queryable, QueryableByName, Selectable, Serialize)]
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

/// One row of the frozen ladder, including the last audit row's sticky Arena
/// high-water. The three high-water fields are used to record the participation
/// reward but are not columns in `arena_season_standings`; [`StandingInsertRow`]
/// is the deliberately narrower database shape.
#[derive(Debug, Clone, Serialize)]
pub struct StandingRow {
    pub season_id: Uuid,
    pub character_id: Uuid,
    pub rank: i32,
    pub trophies: i64,
    pub matches: i32,
    pub wins: i32,
    pub guild_id: Option<String>,
    pub high_water: i64,
    pub arena: i32,
    pub arena_level: i32,
}

/// Database projection of [`StandingRow`]. Keeping the season-reward inputs in
/// memory avoids an `ALTER TABLE` on the production standings table.
#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = crate::schema::arena_season_standings)]
pub struct StandingInsertRow {
    pub season_id: Uuid,
    pub character_id: Uuid,
    pub rank: i32,
    pub trophies: i64,
    pub matches: i32,
    pub wins: i32,
    pub guild_id: Option<String>,
}

impl From<&StandingRow> for StandingInsertRow {
    fn from(row: &StandingRow) -> Self {
        Self {
            season_id: row.season_id,
            character_id: row.character_id,
            rank: row.rank,
            trophies: row.trophies,
            matches: row.matches,
            wins: row.wins,
            guild_id: row.guild_id.clone(),
        }
    }
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

/// Ungranted award fields consumed by the bounded admin grant endpoint.
#[derive(Debug, Clone, Queryable, Selectable, QueryableByName)]
#[diesel(table_name = crate::schema::arena_season_awards)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct AwardGrantCandidate {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub character_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub kind: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub tier: String,
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub payload: Value,
}

/// Full award state used by the game-facing, one-shot season gift. The unique
/// `(season_id, character_id, kind)` index makes this at most three rows.
#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = crate::schema::arena_season_awards)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct AwardClaimRow {
    pub id: Uuid,
    pub kind: String,
    pub tier: String,
    pub payload: Value,
    pub granted_at: Option<i64>,
}

/// Which bracket a placing falls in.
///
/// Captured per-character season gifts establish three player brackets: gold
/// through rank 10, silver through 50, and bronze through 100. There is no
/// separate champion or top-three prize.
///
/// `None` beyond 100: the ladder the client shows is a top-100, so a placing
/// outside it is not a placing anyone saw.
pub fn rank_tier(rank: i32) -> Option<&'static str> {
    match rank {
        1..=10 => Some("top10"),
        11..=50 => Some("top50"),
        51..=100 => Some("top100"),
        _ => None,
    }
}

/// Later retail seasons deliberately paid every member of every top-100 guild
/// the same guild reward. Do not reuse the player ladder's 1/3/10/50 brackets:
/// that was the server's old, unsupported assumption.
pub fn guild_rank_tier(rank: i32) -> Option<&'static str> {
    (1..=100).contains(&rank).then_some("top100")
}

/// The reward payload recorded for a tier. Granting resolves it through the
/// capture-derived table in `season_rewards`; nothing here grants by itself.
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
/// Reads the final in-window cup total, match count, and sticky Arena high-water
/// from `arena_match_results`, which is already durable per match. Current
/// character JSON is deliberately not a season boundary.
pub async fn freeze_standings(
    conn: &mut AsyncPgConnection,
    season: &SeasonRow,
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
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        high_water: i64,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        arena: i32,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        arena_level: i32,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        guild_id: Option<String>,
    }

    // `arena_match_results` is the season boundary: current character JSON may
    // already belong to the next season when a delayed/retried close runs, and
    // lifetime aggregation made every old match appear in every new season.
    //
    // The last audit row supplies trophies AND sticky Arena high-water as they
    // stood at the cutoff. This also admits a participant who finished at zero
    // cups; retail still paid the highest-Arena participation reward.
    let cutoff = season.ends_at.min(super::arena_season::now_unix());
    let rows: Vec<Row> = diesel::sql_query(
        "WITH season_matches AS ( \
             SELECT character_id, trophies_after AS trophies, \
                    matchmaking_trophies_after AS high_water, arena, arena_level, \
                    COUNT(*) OVER (PARTITION BY character_id) AS matches, \
                    COUNT(*) FILTER (WHERE win) OVER (PARTITION BY character_id) AS wins, \
                    ROW_NUMBER() OVER ( \
                        PARTITION BY character_id ORDER BY recorded_at DESC, id DESC \
                    ) AS latest \
             FROM arena_match_results \
             WHERE recorded_at >= to_timestamp($1) \
               AND recorded_at < to_timestamp($2) \
         ) \
         SELECT c.id AS character_id, m.trophies, m.matches, m.wins, \
                m.high_water, m.arena, m.arena_level, \
                gm.guild_id AS guild_id \
         FROM season_matches m \
         JOIN characters c ON c.id = m.character_id \
         LEFT JOIN guild_members gm ON gm.character_id = c.id \
         WHERE m.latest = 1 \
         ORDER BY m.trophies DESC, m.matches DESC, c.id",
    )
    .bind::<diesel::sql_types::BigInt, _>(season.starts_at)
    .bind::<diesel::sql_types::BigInt, _>(cutoff)
    .get_results(conn)
    .await?;

    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| StandingRow {
            season_id: season.id,
            character_id: r.character_id,
            rank: (i as i32) + 1,
            trophies: r.trophies,
            matches: r.matches as i32,
            wins: r.wins as i32,
            guild_id: r.guild_id,
            high_water: r.high_water,
            arena: r.arena,
            arena_level: r.arena_level,
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
        let tier = format!("arena{}_level{}", s.arena, s.arena_level);
        let mut payload = award_payload("arena_reached", &tier, s.rank);
        payload["arena"] = json!(s.arena);
        payload["arenaLevel"] = json!(s.arena_level);
        payload["highWaterTrophies"] = json!(s.high_water);
        out.push(AwardRow {
            id: Uuid::new_v4(),
            season_id,
            character_id: s.character_id,
            kind: "arena_reached".into(),
            rank: s.rank,
            tier,
            payload,
        });

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
        if let Some(tier) = guild_rank_tier(rank) {
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
            high_water: trophies,
            arena: 1,
            arena_level: 1,
        }
    }

    #[test]
    fn tiers_cover_the_top_hundred_and_stop_there() {
        assert_eq!(rank_tier(1), Some("top10"));
        assert_eq!(rank_tier(3), Some("top10"));
        assert_eq!(rank_tier(10), Some("top10"));
        assert_eq!(rank_tier(50), Some("top50"));
        assert_eq!(rank_tier(100), Some("top100"));
        // The client's ladder is a top-100; 101st is not a placing anyone saw,
        // so it must not silently earn the bottom bracket.
        assert_eq!(rank_tier(101), None);
        assert_eq!(rank_tier(0), None, "rank is 1-based");
        assert_eq!(guild_rank_tier(1), Some("top100"));
        assert_eq!(guild_rank_tier(57), Some("top100"));
        assert_eq!(guild_rank_tier(100), Some("top100"));
        assert_eq!(guild_rank_tier(101), None);
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
        assert_eq!(kinds.iter().filter(|k| **k == "arena_reached").count(), 1);
    }

    #[test]
    fn a_guildless_player_gets_only_a_rank_award() {
        let st = vec![standing(Uuid::new_v4(), 1, 100, None)];
        let g = guild_standings_from(Uuid::nil(), &st);
        assert!(g.is_empty());
        let aw = awards_from(Uuid::nil(), &st, &g);
        assert_eq!(aw.len(), 2);
        assert!(aw.iter().any(|a| a.kind == "rank"));
        assert!(aw.iter().any(|a| a.kind == "arena_reached"));
    }

    #[test]
    fn nothing_is_granted_at_record_time() {
        let st = vec![standing(Uuid::new_v4(), 1, 5, None)];
        let aw = awards_from(Uuid::nil(), &st, &[]);
        assert!(aw.iter().all(|a| a.payload["granted"] == json!(false)));
    }

    #[test]
    fn arena_participation_award_uses_the_sticky_high_water() {
        let mut s = standing(Uuid::new_v4(), 101, 0, None);
        s.high_water = 1_275;
        s.arena = 3;
        s.arena_level = 6;
        let awards = awards_from(Uuid::nil(), &[s], &[]);
        assert_eq!(awards.len(), 1, "outside top 100 still earns participation");
        assert_eq!(awards[0].kind, "arena_reached");
        assert_eq!(awards[0].tier, "arena3_level6");
        assert_eq!(awards[0].payload["highWaterTrophies"], json!(1_275));
    }
}
