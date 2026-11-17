//! Recent character renames for the website's `/arena/top100` (tracker #527,
//! HauDrauf): for a month after a rename the boards show the previous name next
//! to the new one ("Newname (Oldname)"), so everyone still knows who it is.
//!
//! Read-only. Two records of a rename exist, and both count:
//!
//! - `characters.server_state.currentCharacterRenames` — every website rename of
//!   the live character (`admin::rename_current_character`). A rename of one of
//!   a player's other alts from the website reaches the arena only when that alt
//!   is the live one, so this ledger is every rename the boards can show.
//! - `character_name_backfill_undo` — renames the server made: the unique-names
//!   backfill and the AI-name repair. Undone rows are skipped (the name is back).
//!
//! Not every rename carries an identity worth showing. A step is NOISE when it
//! only numbered a name ("Ravena" -> "Ravena 2", or back), only changed case or
//! spacing, or started from the starter placeholder ("Adventurer", "Imported" —
//! including the name the backfill generated from one). The previous name shown
//! is the one before the latest real rename, read back through noise steps:
//! "Aurora" -> "Aurora 2" (backfill) -> "Dawn" (player) shows "Dawn (Aurora)",
//! and "Markus Valerius" -> "Markus Valeriu 2" -> "Markus Valerius" shows
//! nothing. A previous name that is itself a placeholder is not shown.
//!
//! The answer is keyed by `(character_id, name)`: a character's alts share the
//! id, and the ledger does not say which alt it renamed, so the website matches a
//! board row only when its current name is the rename's new name.

use std::{collections::HashSet, sync::Arc};

use actix_web::{
    HttpRequest, get,
    web::{self, Json},
};
use diesel::{
    QueryableByName,
    sql_types::{BigInt, Text, Uuid as SqlUuid},
};
use diesel_async::RunQueryDsl;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    ServerGlobal,
    admin::check_import_token,
    arena::top100_boards::{cached, store},
    character_names::{is_default_name, norm, numbered},
    error::BladeApiError,
};

/// How long a previous name is shown after the rename.
pub const PREVIOUS_NAME_DAYS: i64 = 30;

/// One rename, from either record.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct RenameEvent {
    #[diesel(sql_type = SqlUuid)]
    pub character_id: Uuid,
    #[diesel(sql_type = Text)]
    pub old_name: String,
    #[diesel(sql_type = Text)]
    pub new_name: String,
    #[diesel(sql_type = BigInt)]
    pub renamed_at: i64,
    /// Order inside one second: ledger position, or the backfill row id.
    #[diesel(sql_type = BigInt)]
    pub seq: i64,
}

/// A board row named `name` on character `character_id` was `prev_name` until
/// `renamed_at` (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecentRename {
    pub character_id: Uuid,
    pub name: String,
    pub prev_name: String,
    pub renamed_at: i64,
}

#[derive(Serialize)]
pub struct RecentRenamesResponse {
    pub days: i64,
    pub rows: Vec<RecentRename>,
}

/// The starter placeholders: "Adventurer"/"Adventurer 2"… and "Imported"/"Imported 2"….
pub(crate) fn is_placeholder(name: &str) -> bool {
    if is_default_name(name) {
        return true;
    }
    let n = norm(name);
    match n.strip_prefix("imported") {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix(' ')
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit())),
        None => n.is_empty(),
    }
}

/// `to` is `from` with a disambiguation number, exactly as the server numbers it
/// (`character_names::numbered`, which may shorten a long name to fit).
fn is_numbered_from(from: &str, to: &str) -> bool {
    let to = to.trim();
    let Some((_, digits)) = to.rsplit_once(' ') else {
        return false;
    };
    let Ok(n) = digits.parse::<u64>() else {
        return false;
    };
    !from.trim().is_empty() && norm(&numbered(from, n)) == norm(to)
}

/// A step that changes nothing anyone would recognise a player by.
fn is_noise(e: &RenameEvent) -> bool {
    norm(&e.old_name) == norm(&e.new_name)
        || is_numbered_from(&e.old_name, &e.new_name)
        || is_numbered_from(&e.new_name, &e.old_name)
        || is_placeholder(&e.old_name)
}

/// The name before `events[i]`, read back through noise steps. `events` is one
/// character's renames in time order.
fn previous_identity(events: &[RenameEvent], i: usize) -> Option<String> {
    let latest = &events[i];
    if is_noise(latest) {
        return None;
    }
    let mut candidate = latest.old_name.clone();
    let mut before = i;
    while let Some(k) = events[..before]
        .iter()
        .rposition(|e| e.new_name == candidate)
    {
        if !is_noise(&events[k]) {
            break;
        }
        candidate = events[k].old_name.clone();
        before = k;
    }
    if is_placeholder(&candidate) || norm(&candidate) == norm(&latest.new_name) {
        return None;
    }
    Some(candidate.trim().to_string())
}

/// Every name renamed to since `cutoff` (unix seconds) that has a previous
/// name worth showing, one row per `(character_id, name)` — its latest rename.
pub fn recent_renames(mut events: Vec<RenameEvent>, cutoff: i64) -> Vec<RecentRename> {
    events.sort_by(|a, b| {
        (a.character_id, a.renamed_at, a.seq).cmp(&(b.character_id, b.renamed_at, b.seq))
    });
    let mut out = Vec::new();
    for chunk in events.chunk_by(|a, b| a.character_id == b.character_id) {
        let mut seen = HashSet::new();
        for i in (0..chunk.len()).rev() {
            let e = &chunk[i];
            if e.renamed_at < cutoff {
                break;
            }
            if !seen.insert(e.new_name.trim().to_string()) {
                continue;
            }
            if let Some(prev_name) = previous_identity(chunk, i) {
                out.push(RecentRename {
                    character_id: e.character_id,
                    name: e.new_name.trim().to_string(),
                    prev_name,
                    renamed_at: e.renamed_at,
                });
            }
        }
    }
    out.sort_by(|a, b| b.renamed_at.cmp(&a.renamed_at).then(a.name.cmp(&b.name)));
    out
}

/// Both rename records since `since` (unix seconds). The window reaches back
/// twice as far as the cutoff, so the noise steps before a recent rename are
/// still there to read back through.
pub const RENAME_EVENTS_SQL: &str = "\
SELECT character_id, old_name, new_name, renamed_at, seq FROM ( \
  SELECT c.id AS character_id, e.v->>'previousName' AS old_name, e.v->>'newName' AS new_name, \
         CASE WHEN jsonb_typeof(e.v->'renamedAtSecs') = 'number' \
              THEN floor((e.v->>'renamedAtSecs')::numeric)::bigint END AS renamed_at, \
         e.ord AS seq \
    FROM characters c \
    CROSS JOIN LATERAL jsonb_array_elements( \
         CASE WHEN jsonb_typeof(c.server_state->'currentCharacterRenames') = 'array' \
              THEN c.server_state->'currentCharacterRenames' ELSE '[]'::jsonb END) \
         WITH ORDINALITY AS e(v, ord) \
   WHERE c.server_state ? 'currentCharacterRenames' \
) ledger \
 WHERE renamed_at >= $1 AND old_name IS NOT NULL AND new_name IS NOT NULL \
UNION ALL \
SELECT c.id, u.old_name, u.new_name, u.renamed_at, u.id \
  FROM character_name_backfill_undo u \
  JOIN characters c ON c.user_id = u.user_id \
 WHERE u.undone_at IS NULL AND u.renamed_at >= $1";

/// `GET /api/dev/v1/arena-ranking/recent-renames` — names renamed to in the last
/// [`PREVIOUS_NAME_DAYS`] days with the previous name worth showing beside them.
#[get("/blades.bgs.services/api/dev/v1/arena-ranking/recent-renames")]
pub async fn get_arena_recent_renames(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<Value>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    const KEY: &str = "recent-renames";
    if let Some(v) = cached(KEY) {
        return Ok(Json(v));
    }
    let now = crate::arena::arena_season::now_unix();
    let cutoff = now - PREVIOUS_NAME_DAYS * 86_400;
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(BladeApiError::generic_internal_error)?;
    let events: Vec<RenameEvent> = diesel::sql_query(RENAME_EVENTS_SQL)
        .bind::<BigInt, _>(cutoff - PREVIOUS_NAME_DAYS * 86_400)
        .get_results(&mut conn)
        .await?;
    let value = serde_json::to_value(RecentRenamesResponse {
        days: PREVIOUS_NAME_DAYS,
        rows: recent_renames(events, cutoff),
    })
    .map_err(BladeApiError::generic_internal_error)?;
    store(KEY.to_string(), &value);
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_800_000_000;
    const CUTOFF: i64 = NOW - 30 * DAY;

    fn ev(c: u128, old: &str, new: &str, at: i64) -> RenameEvent {
        RenameEvent {
            character_id: Uuid::from_u128(c),
            old_name: old.into(),
            new_name: new.into(),
            renamed_at: at,
            seq: 0,
        }
    }

    fn shown(events: Vec<RenameEvent>) -> Vec<(String, String)> {
        let mut v: Vec<_> = recent_renames(events, CUTOFF)
            .into_iter()
            .map(|r| (r.name, r.prev_name))
            .collect();
        v.sort();
        v
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.into(), b.into())
    }

    #[test]
    fn a_player_rename_shows_the_old_name_with_its_date() {
        let rows = recent_renames(vec![ev(1, "WolfWalker", "Wuuthrad", NOW - DAY)], CUTOFF);
        assert_eq!(
            rows,
            vec![RecentRename {
                character_id: Uuid::from_u128(1),
                name: "Wuuthrad".into(),
                prev_name: "WolfWalker".into(),
                renamed_at: NOW - DAY,
            }]
        );
    }

    #[test]
    fn a_rename_older_than_the_window_is_not_shown() {
        assert!(shown(vec![ev(1, "WolfWalker", "Wuuthrad", CUTOFF - 1)]).is_empty());
        assert_eq!(
            shown(vec![ev(1, "WolfWalker", "Wuuthrad", CUTOFF)]),
            vec![pair("Wuuthrad", "WolfWalker")]
        );
    }

    #[test]
    fn placeholder_and_generated_names_carry_no_identity() {
        assert!(
            shown(vec![
                ev(1, "Adventurer", "Molag Sug", NOW - DAY),
                ev(2, "Adventurer 4", "Lil Dag", NOW - DAY),
                ev(3, "Imported", "Skagastol", NOW - DAY),
                ev(4, "Imported", "Imported 7", NOW - DAY),
                // The backfill's generated name, then the player's own choice.
                ev(5, "Adventurer", "Ice Lofty Acorn", NOW - 2 * DAY),
                ev(5, "Ice Lofty Acorn", "Bob", NOW - DAY),
            ])
            .is_empty()
        );
    }

    #[test]
    fn only_numbering_or_case_is_not_a_rename_anyone_needs_explained() {
        assert!(
            shown(vec![
                ev(1, "Aurora", "Aurora 2", NOW - DAY),
                ev(2, "Markus Valerius", "Markus Valeriu 2", NOW - DAY),
                ev(3, "Ravena 2", "Ravena", NOW - DAY),
                ev(4, "NewMunk", "newmunk", NOW - DAY),
                ev(5, "Tureduus Callidus", "Tureduus Callidus", NOW - DAY),
            ])
            .is_empty()
        );
    }

    #[test]
    fn the_previous_name_reads_back_through_numbering() {
        assert_eq!(
            shown(vec![
                ev(1, "Aurora", "Aurora 2", NOW - 2 * DAY),
                ev(1, "Aurora 2", "Dawn", NOW - DAY),
            ]),
            vec![pair("Dawn", "Aurora")]
        );
        // Numbered by the backfill, then renamed back by hand: nothing changed.
        assert!(
            shown(vec![
                ev(2, "Adventurer", "Markus Valerius", NOW - 10 * DAY),
                ev(2, "Markus Valerius", "Markus Valeriu 2", NOW - 2 * DAY),
                ev(2, "Markus Valeriu 2", "Markus Valerius", NOW - DAY),
            ])
            .is_empty()
        );
    }

    #[test]
    fn the_ai_marker_repair_shows_the_name_it_took_away() {
        // Brief example (#527): a player called "Yaskrava AI" set back by the
        // reserved-AI repair is listed as "Yaskrava (Yaskrava AI)".
        assert_eq!(
            shown(vec![ev(1, "Yaskrava AI", "Yaskrava", NOW - DAY)]),
            vec![pair("Yaskrava", "Yaskrava AI")]
        );
        // "Ravena" numbered by the backfill, then renamed by the player.
        assert_eq!(
            shown(vec![
                ev(2, "Ravena", "Ravena 3", NOW - 2 * DAY),
                ev(2, "Ravena 3", "Ravena AI", NOW - DAY),
            ]),
            vec![pair("Ravena AI", "Ravena")]
        );
    }

    #[test]
    fn each_name_a_character_holds_is_answered_from_its_latest_rename() {
        // Two alts of one character renamed: both names listed (the website
        // matches the row by name). A name renamed AWAY from is listed too, and
        // simply matches no row.
        assert_eq!(
            shown(vec![
                ev(1, "Ravena", "Pogagoga", NOW - 3 * DAY),
                ev(1, "Hexe", "Hexa Prime", NOW - 2 * DAY),
                ev(1, "Hexa Prime", "Hexa", NOW - DAY),
            ]),
            vec![
                pair("Hexa", "Hexa Prime"),
                pair("Hexa Prime", "Hexe"),
                pair("Pogagoga", "Ravena"),
            ]
        );
    }

    #[test]
    fn placeholders_are_recognised() {
        for p in [
            "Adventurer",
            "adventurer 12",
            "Imported",
            "Imported 3",
            "  ",
            "",
        ] {
            assert!(is_placeholder(p), "{p:?}");
        }
        for p in ["Importer", "Imported Hero", "Adventurers", "Ravena"] {
            assert!(!is_placeholder(p), "{p:?}");
        }
    }

    #[test]
    fn numbering_is_recognised_only_as_the_server_numbers() {
        assert!(is_numbered_from("Ravena", "Ravena 3"));
        assert!(is_numbered_from("Markus Valerius", "Markus Valeriu 2"));
        assert!(!is_numbered_from("Bobby", "Bob 2"));
        assert!(!is_numbered_from("Ravena", "Ravena"));
        assert!(!is_numbered_from("Agent", "Agent 47x"));
    }

    /// The query against a real database: both records are read, an undone
    /// backfill row and a ledger entry outside the read window are not, and a
    /// malformed ledger does not fail the board. Skips without TEST_DATABASE_URL.
    #[tokio::test]
    async fn the_query_reads_both_records_and_skips_undone_rows() {
        use diesel_async::{AsyncConnection, AsyncPgConnection, SimpleAsyncConnection};
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            return eprintln!("SKIP: TEST_DATABASE_URL unset — recent-renames query NOT verified");
        };
        let mut c = AsyncPgConnection::establish(&url)
            .await
            .expect("test db reachable");
        c.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        let player = Uuid::from_u128(1);
        let numbered = Uuid::from_u128(2);
        let odd = Uuid::from_u128(3);
        c.batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema};
             CREATE TABLE characters (id UUID PRIMARY KEY, user_id UUID NOT NULL,
                                      server_state JSONB NOT NULL);
             CREATE TABLE character_name_backfill_undo (
                 id BIGSERIAL PRIMARY KEY, run_id UUID NOT NULL, user_id UUID NOT NULL,
                 alt_key UUID NOT NULL, old_name TEXT NOT NULL, new_name TEXT NOT NULL,
                 reason TEXT NOT NULL, undone_at BIGINT,
                 renamed_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint);
             INSERT INTO characters VALUES
               ('{player}', '{player}', '{{\"currentCharacterRenames\": [
                  {{\"previousName\": \"Ancient\", \"newName\": \"Older\", \"renamedAtSecs\": {old}}},
                  {{\"previousName\": \"WolfWalker\", \"newName\": \"Wuuthrad\", \"renamedAtSecs\": {recent}}},
                  {{\"previousName\": \"Broken\", \"newName\": \"Entry\", \"renamedAtSecs\": \"soon\"}},
                  \"not an object\"]}}'),
               ('{numbered}', '{numbered}', '{{}}'),
               ('{odd}', '{odd}', '{{\"currentCharacterRenames\": {{\"not\": \"an array\"}}}}');
             INSERT INTO character_name_backfill_undo
               (run_id, user_id, alt_key, old_name, new_name, reason, undone_at, renamed_at) VALUES
               ('{player}', '{numbered}', '{numbered}', 'Aurora', 'Aurora 2', 'duplicate', NULL, {b1}),
               ('{player}', '{numbered}', '{numbered}', 'Yaskrava AI', 'Yaskrava', 'reserved-ai', NULL, {b2}),
               ('{player}', '{odd}', '{odd}', 'Zed AI', 'Zed', 'reserved-ai', {b2}, {b2});",
            old = NOW - 61 * DAY,
            recent = NOW - DAY,
            b1 = NOW - 2 * DAY,
            b2 = NOW - DAY,
        ))
        .await
        .unwrap();
        let events: Vec<RenameEvent> = diesel::sql_query(RENAME_EVENTS_SQL)
            .bind::<BigInt, _>(CUTOFF - 30 * DAY)
            .get_results(&mut c)
            .await
            .unwrap();
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(
            shown(events),
            vec![
                pair("Wuuthrad", "WolfWalker"),
                pair("Yaskrava", "Yaskrava AI")
            ]
        );
    }
}
