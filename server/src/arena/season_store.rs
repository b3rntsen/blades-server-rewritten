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
    /// The alt this placing belongs to. A user's alts share `character_id` (one
    /// live row per user), so the alt is what tells two placings apart.
    pub source_alt_uuid: Option<Uuid>,
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
    pub source_alt_uuid: Option<Uuid>,
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
            source_alt_uuid: row.source_alt_uuid,
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
    /// The alt that earned it; only that alt can claim it (owner decision
    /// 2026-10-10, tracker #348).
    pub source_alt_uuid: Option<Uuid>,
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
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    pub source_alt_uuid: Option<Uuid>,
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
    pub source_alt_uuid: Option<Uuid>,
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

/// Does an award (or standing) recorded for `award_alt` belong to the alt that
/// currently occupies the live row?
///
/// A user's alts share one `characters.id`, so the id alone cannot say whose a
/// season reward is — that is how spacemunk's claim screen paid out a placing
/// recorded while another alt was live (#348). NULL on the award means it was
/// recorded before alts were tracked: it stays claimable by whoever is live, as
/// every award was before.
pub fn award_belongs_to_alt(award_alt: Option<Uuid>, live_alt: Option<Uuid>) -> bool {
    award_alt.is_none() || award_alt == live_alt
}

/// The same rule as [`award_belongs_to_alt`], as SQL over an award row `a`.
pub const AWARD_BELONGS_TO_LIVE_ALT_SQL: &str = "(a.source_alt_uuid IS NULL \
     OR a.source_alt_uuid IS NOT DISTINCT FROM \
        (SELECT live.source_alt_uuid FROM characters live WHERE live.id = a.character_id))";

/// The season ladder, one row per ALT (owner decision 2026-10-10).
///
/// Partitioned by `(character_id, alt)`: a user's alts share the character id,
/// so partitioning by the id alone summed every alt's matches into one placing —
/// NBS-1's rank 2 was three alts' 4018 cups added together. A match recorded
/// before alts were tracked (`source_alt_uuid` NULL) counts for the alt live now,
/// which is what this query did for every match before.
///
/// The guild is the one the alt itself is in: the live alt's membership row, or,
/// for an alt switched out, the membership parked in its newest version — and
/// only while that guild still exists.
const FREEZE_STANDINGS_SQL: &str = "\
    WITH season_results AS ( \
         SELECT r.character_id, r.recorded_at, r.id, r.trophy_delta, r.win, \
                COALESCE(r.source_alt_uuid, c.source_alt_uuid) AS alt \
         FROM arena_match_results r \
         JOIN characters c ON c.id = r.character_id \
         WHERE r.recorded_at >= to_timestamp($1) \
           AND r.recorded_at < to_timestamp($2) \
     ), season_deltas AS ( \
         SELECT character_id, alt, recorded_at, id, \
                (SUM(trophy_delta) OVER ( \
                    PARTITION BY character_id, alt ORDER BY recorded_at, id \
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW \
                ))::bigint AS raw_score, \
                COUNT(*) OVER (PARTITION BY character_id, alt) AS matches, \
                COUNT(*) FILTER (WHERE win) OVER (PARTITION BY character_id, alt) AS wins, \
                ROW_NUMBER() OVER ( \
                    PARTITION BY character_id, alt ORDER BY recorded_at DESC, id DESC \
                ) AS latest \
         FROM season_results \
     ), season_scores AS ( \
         SELECT character_id, alt, recorded_at, id, matches, wins, latest, \
                raw_score - LEAST( \
                    0, \
                    MIN(raw_score) OVER ( \
                        PARTITION BY character_id, alt ORDER BY recorded_at, id \
                        ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW \
                    ) \
                ) AS trophies \
         FROM season_deltas \
     ), season_matches AS ( \
         SELECT character_id, alt, trophies, matches, wins, latest, \
                MAX(trophies) OVER (PARTITION BY character_id, alt) AS high_water \
         FROM season_scores \
     ) \
     SELECT c.id AS character_id, m.alt AS source_alt_uuid, m.trophies, m.matches, m.wins, \
            m.high_water, \
            CASE WHEN m.alt IS NOT DISTINCT FROM c.source_alt_uuid THEN gm.guild_id \
                 ELSE pg.id END AS guild_id \
     FROM season_matches m \
     JOIN characters c ON c.id = m.character_id \
     LEFT JOIN guild_members gm ON gm.character_id = c.id \
     LEFT JOIN LATERAL ( \
         SELECT v.server_state -> 'parkedGuildMembers' -> 0 ->> 'guild_id' AS guild_id \
         FROM character_versions v \
         WHERE v.character_id = m.character_id AND v.source_alt_uuid = m.alt \
         ORDER BY v.saved_at DESC \
         LIMIT 1 \
     ) parked ON true \
     LEFT JOIN guilds pg ON pg.id = parked.guild_id \
     WHERE m.latest = 1 \
     ORDER BY m.trophies DESC, m.matches DESC, c.id, m.alt";

/// Live standings for every character that scored, best first.
///
/// Replays the in-window trophy deltas from zero (including the zero floor), and
/// reads the match count from `arena_match_results`, which is already durable per
/// match. Current character JSON and imported `trophies_after` baselines are
/// deliberately not season boundaries.
pub async fn freeze_standings(
    conn: &mut AsyncPgConnection,
    season: &SeasonRow,
) -> QueryResult<Vec<StandingRow>> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        character_id: Uuid,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
        source_alt_uuid: Option<Uuid>,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        trophies: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        matches: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        wins: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        high_water: i64,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        guild_id: Option<String>,
    }

    // `arena_match_results` is the season boundary: current character JSON may
    // already belong to the next season when a delayed/retried close runs, and
    // lifetime aggregation made every old match appear in every new season.
    //
    // Replay deltas from the season's required zero baseline. `trophies_after`
    // cannot be trusted here because a character imported after season start used
    // to carry retail cups into its first result; counting only local wins beside
    // that inherited total produced impossible standings (including cups with zero
    // wins). The running-prefix minimum implements the per-result `max(0)` clamp:
    // final = raw_sum - min(0, lowest_prefix_sum).
    let cutoff = season.ends_at.min(super::arena_season::now_unix());
    let rows: Vec<Row> = diesel::sql_query(FREEZE_STANDINGS_SQL)
    .bind::<diesel::sql_types::BigInt, _>(season.starts_at)
    .bind::<diesel::sql_types::BigInt, _>(cutoff)
    .get_results(conn)
    .await?;

    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let tier = super::arena_ladder::tier_for_trophies(r.high_water);
            StandingRow {
                season_id: season.id,
                character_id: r.character_id,
                source_alt_uuid: r.source_alt_uuid,
                rank: (i as i32) + 1,
                trophies: r.trophies,
                matches: r.matches as i32,
                wins: r.wins as i32,
                guild_id: r.guild_id,
                high_water: r.high_water,
                arena: tier.arena as i32,
                arena_level: tier.level as i32,
            }
        })
        .collect())
}

/// Guild ladder for the season, derived from the character standings just
/// frozen rather than from `guilds.trophies` — the guild row is a running
/// total that the rollover does not reset, so using it would carry last
/// season's score into this one's result.
/// A guild member's share of their own trophies, by their rank INSIDE the guild.
///
/// Retail does not sum a guild's members flat — it weights them by standing, so a
/// guild is carried by its strongest four and a tail of low-trophy members adds
/// almost nothing. Guilds cap at 20 players (`UI.Help.Guilds.Description`), which is
/// exactly the five brackets below.
///
/// **Measured**, not assumed. Pairing captured `guilds/leaderboard` scores against
/// the arena leaderboard snapshot taken at the same moment (`arena_leaderboard_
/// snapshots`, which carries each ranked player's `cups` and `guild_name`), over the
/// 12 observations where at most 3 of a guild's members sat below the top-100 board:
///
/// | | exact | within 2% |
/// | --- | --- | --- |
/// | this table | 1 | **10 / 12** |
/// | flat sum | 0 | **0 / 12** |
///
/// Shadowblades with 18 of 20 members on the board reported **2833** against 2833.2
/// from this table, and 12,507 from a flat sum. The residuals on the rest are tens of
/// trophies against totals of 3–5k, and they track how many members were missing from
/// the board — which is what an undercount of the 10% tail looks like.
const GUILD_MEMBER_WEIGHT: [f64; 20] = [
    0.30, 0.30, 0.30, 0.30, // ranks 1-4
    0.25, 0.25, 0.25, 0.25, // ranks 5-8
    0.20, 0.20, 0.20, 0.20, // 9-12
    0.15, 0.15, 0.15, 0.15, // 13-16
    0.10, 0.10, 0.10, 0.10, // 17-20
];

/// A guild's score from its members' individual scores: sorted best first, each
/// weighted by its standing INSIDE the guild ([`GUILD_MEMBER_WEIGHT`]); members past
/// the 20th add nothing. The one implementation of the guild rule — the season
/// ladder uses it on trophies, and the website's human-vs-human guild board
/// (`arena::top100_boards::get_arena_h2h_guilds`) uses it on h2h ratings, so the two cannot drift.
pub fn guild_weighted_score(mut member_scores: Vec<i64>) -> i64 {
    // Highest first: the weight is for the member's standing in the guild.
    member_scores.sort_unstable_by(|a, b| b.cmp(a));
    let score: f64 = member_scores
        .iter()
        .zip(GUILD_MEMBER_WEIGHT.iter())
        .map(|(t, w)| *t as f64 * w)
        .sum();
    score.round() as i64
}

pub fn guild_standings_from(season_id: Uuid, standings: &[StandingRow]) -> Vec<GuildStandingRow> {
    // Collect each guild's member trophies, then weight them by rank WITHIN the guild.
    let mut members: HashMap<&str, Vec<i64>> = HashMap::new();
    for s in standings {
        if let Some(g) = s.guild_id.as_deref() {
            members.entry(g).or_default().push(s.trophies);
        }
    }
    let mut rows: Vec<GuildStandingRow> = members
        .into_iter()
        .map(|(g, trophies)| {
            let count = trophies.len() as i32;
            GuildStandingRow {
                season_id,
                guild_id: g.to_string(),
                rank: 0,
                trophies: guild_weighted_score(trophies),
                members: count,
            }
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
            source_alt_uuid: s.source_alt_uuid,
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
                source_alt_uuid: s.source_alt_uuid,
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
                source_alt_uuid: s.source_alt_uuid,
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
            source_alt_uuid: None,
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

    /// Owner decision 2026-10-10: a season reward belongs to the alt that
    /// earned it. Legacy (unstamped) awards stay claimable by whoever is live.
    #[test]
    fn an_award_belongs_only_to_the_alt_that_earned_it() {
        let (a, b) = (Uuid::from_u128(0xa), Uuid::from_u128(0xb));
        assert!(award_belongs_to_alt(Some(a), Some(a)));
        assert!(!award_belongs_to_alt(Some(a), Some(b)), "another alt must not claim it");
        assert!(!award_belongs_to_alt(Some(a), None), "nor a row that lost its alt id");
        assert!(award_belongs_to_alt(None, Some(b)), "a legacy award stays claimable");
        assert!(award_belongs_to_alt(None, None));
    }

    /// Two alts of one user share the character id; each placing carries its
    /// own alt onto every award it implies, guild awards included.
    #[test]
    fn awards_carry_the_alt_of_their_standing() {
        let cid = Uuid::from_u128(7);
        let (a, b) = (Uuid::from_u128(0xa), Uuid::from_u128(0xb));
        let mut first = standing(cid, 1, 900, Some("g1"));
        first.source_alt_uuid = Some(a);
        let mut second = standing(cid, 2, 500, Some("g2"));
        second.source_alt_uuid = Some(b);
        let standings = vec![first, second];
        let guilds = guild_standings_from(Uuid::nil(), &standings);
        let awards = awards_from(Uuid::nil(), &standings, &guilds);
        for alt in [a, b] {
            let kinds: Vec<&str> = awards
                .iter()
                .filter(|w| w.source_alt_uuid == Some(alt))
                .map(|w| w.kind.as_str())
                .collect();
            assert!(kinds.contains(&"rank") && kinds.contains(&"arena_reached"));
            assert!(kinds.contains(&"guild_rank"), "guild award of {alt} missing: {kinds:?}");
        }
        assert!(awards.iter().all(|w| w.source_alt_uuid.is_some()));
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
    fn guild_ladder_weights_members_by_rank_not_a_flat_sum() {
        let g1 = Uuid::nil();
        let st = vec![
            standing(Uuid::from_u128(1), 1, 100, Some("g1")),
            standing(Uuid::from_u128(2), 2, 50, Some("g1")),
            standing(Uuid::from_u128(3), 3, 10, None),
        ];
        let g = guild_standings_from(g1, &st);
        assert_eq!(g.len(), 1, "the guildless character forms no guild");
        assert_eq!(g[0].guild_id, "g1");
        assert_eq!(g[0].members, 2);
        // Both sit in the top-4 bracket: 30% each. A flat sum would say 150.
        assert_eq!(g[0].trophies, 45, "0.30*100 + 0.30*50 = 45, not the flat 150");
    }

    /// The bracket boundaries, walked one member at a time.
    ///
    /// 20 members of 100 trophies each: 4x30 + 4x25 + 4x20 + 4x15 + 4x10 = 400.
    /// A flat sum would be 2000 — the 5x that made the flat rule obvious in the
    /// captures.
    #[test]
    fn the_five_brackets_cover_exactly_twenty_members() {
        let st: Vec<StandingRow> = (0..20)
            .map(|i| standing(Uuid::from_u128(i + 1), i as i32 + 1, 100, Some("g1")))
            .collect();
        let g = guild_standings_from(Uuid::nil(), &st);
        assert_eq!(g[0].members, 20);
        assert_eq!(g[0].trophies, 400, "4x(30+25+20+15+10) of 100 = 400");
    }

    /// Rank is by TROPHIES inside the guild, so the order rows arrive in cannot
    /// change the score — otherwise a guild's total would depend on the ladder's
    /// tie-breaking.
    #[test]
    fn the_weighting_follows_trophies_not_arrival_order() {
        let asc = vec![
            standing(Uuid::from_u128(1), 9, 10, Some("g1")),
            standing(Uuid::from_u128(2), 8, 900, Some("g1")),
        ];
        let desc = vec![
            standing(Uuid::from_u128(2), 8, 900, Some("g1")),
            standing(Uuid::from_u128(1), 9, 10, Some("g1")),
        ];
        let a = guild_standings_from(Uuid::nil(), &asc);
        let b = guild_standings_from(Uuid::nil(), &desc);
        assert_eq!(a[0].trophies, b[0].trophies);
        assert_eq!(a[0].trophies, 273, "0.30*900 + 0.30*10");
    }

    /// A 21st member would fall off the table; guilds cap at 20, but the score must
    /// not panic or start reading past the array if a stale row ever says otherwise.
    #[test]
    fn a_guild_over_twenty_members_ignores_the_overflow() {
        let st: Vec<StandingRow> = (0..25)
            .map(|i| standing(Uuid::from_u128(i + 1), i as i32 + 1, 100, Some("g1")))
            .collect();
        let g = guild_standings_from(Uuid::nil(), &st);
        assert_eq!(g[0].members, 25, "the count still reports what was there");
        assert_eq!(g[0].trophies, 400, "but only 20 members are weighted");
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
