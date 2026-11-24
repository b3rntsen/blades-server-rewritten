//! The XP bank: levels past 100 are earned in Newblades, not carried in (#425).
//!
//! The owner, 2026-11-24: "turn on level 100+, but don't let anyone gain what's
//! in their bank. It's a newblades feature, so newblades play earn them levels.
//! Remember their entry xp levels and make progress past 100 relative to that."
//!
//! `character.experience` is the UNSPENT pool — a level-up spends from it (see
//! `level_up::apply_level_up`). Retail kept accruing it at level 100, so on
//! production level-100 characters hold 11-76 million of it: raised to 150 as is,
//! Swanne would jump to 124 the moment the cap went up.
//!
//! THE RULE. At the release, for every character row and every parked version:
//! the part of `experience` that would carry the character past level 100 —
//! `max(0, experience - xp still needed to reach 100 from its level)`, all of it
//! at level 100 — is moved out of `experience` into this bank. Nothing is
//! destroyed: the amount, the level and experience it entered with, when and why
//! are recorded per alt. After the release XP earned here counts normally, so
//! progress past 100 is relative to the entry point.
//!
//! WHY `experience` ITSELF IS LOWERED and the server does not merely refuse: the
//! client decides on its own whether to offer a level-up, from `experience`
//! against its own level table. A server that refuses a level-up the client
//! offered drops the player to the title screen and reloads, every tap (found
//! 2026-11-23). The number the client sees has to be the banked one.
//!
//! LATER WRITES. A retail transfer, or restoring a version kept from before the
//! release, would put pre-release XP back on a live row. Those paths call
//! [`bank_on_write`], which banks the same excess at write time. It is inert
//! until the release has happened (cap still 100 → nothing is banked anywhere),
//! and idempotent: a row that holds no excess banks nothing, and the per-alt
//! amount is the GREATEST excess ever seen for that alt, never a sum — a second
//! import of the same capture brings the same pre-release XP, not new XP.
//!
//! WHEN IT RUNS. At server startup, before the HTTP server binds and before the
//! arena match writer starts, and only when `ARENA_MAX_PLAYER_LEVEL` is above
//! 100 and no release has been recorded. So no request can read a row before it
//! is banked and write it back after, and the cap and the bank go live in the
//! same restart: there is no window with the cap raised and the XP unbanked. If
//! banking fails, the caller keeps the cap at 100 (fail closed). The tables are
//! created here with `CREATE TABLE IF NOT EXISTS`, so no hand-applied migration
//! has to precede the deploy. `blades-server bank-xp-past-100` is the read-only
//! dry run.

use anyhow::{Context, Result, bail};
use blades_lib::features::level_up::{LevelUpData, RETAIL_MAX_LEVEL};
use diesel::sql_types::{BigInt, Integer, Jsonb, Nullable, Text, Uuid as SqlUuid};
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection,
    scoped_futures::ScopedFutureExt,
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::DbPool;

/// Both tables, forward-only and idempotent. Times are BIGINT unix seconds, this
/// schema's convention.
pub const XP_BANK_DDL: &str = r#"
-- One row per alt: what was banked and where it entered. Keyed like the
-- rankings key alts, COALESCE(source_alt_uuid, characters.id), plus the user:
-- an AI copy carries its original's alt id under its own user (prod has one).
CREATE TABLE IF NOT EXISTS character_xp_bank (
    user_id              UUID NOT NULL,
    alt_key              UUID NOT NULL,
    character_id         UUID NOT NULL,
    character_name       TEXT NOT NULL DEFAULT '',
    -- The greatest pre-release excess seen for this alt. Never summed: every
    -- copy of an alt (live row, versions, a re-import) carries the same history.
    banked_xp            BIGINT NOT NULL,
    entry_level          INTEGER NOT NULL,
    entry_experience     BIGINT NOT NULL,
    resulting_experience BIGINT NOT NULL,
    reason               TEXT NOT NULL,
    banked_at            BIGINT NOT NULL,
    updated_at           BIGINT NOT NULL,
    events               INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (user_id, alt_key)
);
-- The release itself: present = banked, and the XP threshold table it used, so
-- write-time banking applies exactly the release's rule without static data.
CREATE TABLE IF NOT EXISTS xp_bank_release (
    id            SMALLINT PRIMARY KEY CHECK (id = 1),
    released_at   BIGINT NOT NULL,
    max_level     INTEGER NOT NULL,
    -- needs[L] = experience still needed to reach level 100 from level L.
    needs         JSONB NOT NULL,
    rows_banked   INTEGER NOT NULL DEFAULT 0,
    total_banked  BIGINT NOT NULL DEFAULT 0
);
"#;

/// `needs[L]`: the experience a level-`L` character still has to spend to reach
/// level 100, for `L` in `0..=100`. `None` when the table does not describe every
/// level up to 100 — banking against a guessed threshold is refused.
pub fn needs_to_retail_max(data: &LevelUpData) -> Option<Vec<u64>> {
    let max = u32::from(RETAIL_MAX_LEVEL);
    let mut needs = vec![0u64; max as usize + 1];
    for level in (0..max).rev() {
        let cost = u64::from(data.get_reward(level + 1)?.xp_to_reach);
        needs[level as usize] = needs[level as usize + 1] + cost;
    }
    Some(needs)
}

/// How much of `experience` a level-`level` character banks. Above 100 nothing:
/// a level past retail's top can only have been earned here.
pub fn excess(level: u64, experience: u64, needs: &[u64]) -> u64 {
    if level > u64::from(RETAIL_MAX_LEVEL) {
        return 0;
    }
    let need = needs.get(level as usize).copied().unwrap_or(0);
    experience.saturating_sub(need)
}

fn json_u64(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f > 0.0).map(|f| f as u64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

/// `(level, experience, name)` out of a `characters.character` blob.
pub fn read_character(ch: &Value) -> (u64, u64, String) {
    (
        json_u64(&ch["level"]),
        json_u64(&ch["experience"]),
        ch["name"].as_str().unwrap_or("").to_string(),
    )
}

/// Who a banked amount belongs to.
#[derive(Debug, Clone, Copy)]
pub struct BankOwner {
    pub alt_key: Uuid,
    pub user_id: Uuid,
    pub character_id: Uuid,
}

impl BankOwner {
    pub fn new(character_id: Uuid, user_id: Uuid, source_alt_uuid: Option<Uuid>) -> Self {
        BankOwner {
            alt_key: source_alt_uuid.unwrap_or(character_id),
            user_id,
            character_id,
        }
    }
}

#[derive(diesel::QueryableByName)]
struct Present {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    present: bool,
}

#[derive(diesel::QueryableByName)]
struct ReleaseRow {
    #[diesel(sql_type = BigInt)]
    released_at: i64,
    #[diesel(sql_type = Jsonb)]
    needs: Value,
}

/// The recorded release, if any: `(released_at, needs)`.
///
/// Checks the table exists first rather than letting the SELECT fail: a failed
/// statement aborts the caller's Postgres transaction, and the callers are an
/// import, a restore and a switch, none of which should fail because the
/// feature has not been released.
pub async fn release(
    conn: &mut AsyncPgConnection,
) -> Result<Option<(i64, Vec<u64>)>, diesel::result::Error> {
    let present: Present =
        diesel::sql_query("SELECT to_regclass('xp_bank_release') IS NOT NULL AS present")
            .get_result(conn)
            .await?;
    if !present.present {
        return Ok(None);
    }
    let row: Option<ReleaseRow> =
        diesel::sql_query("SELECT released_at, needs FROM xp_bank_release WHERE id = 1")
            .get_result(conn)
            .await
            .optional_row()?;
    Ok(row.map(|r| {
        let needs = r
            .needs
            .as_array()
            .map(|a| a.iter().map(json_u64).collect())
            .unwrap_or_default();
        (r.released_at, needs)
    }))
}

trait OptionalRow<T> {
    fn optional_row(self) -> Result<Option<T>, diesel::result::Error>;
}
impl<T> OptionalRow<T> for Result<T, diesel::result::Error> {
    fn optional_row(self) -> Result<Option<T>, diesel::result::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(diesel::result::Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

const UPSERT_BANK_SQL: &str = r#"
INSERT INTO character_xp_bank
    (alt_key, user_id, character_id, character_name, banked_xp, entry_level,
     entry_experience, resulting_experience, reason, banked_at, updated_at)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)
ON CONFLICT (user_id, alt_key) DO UPDATE SET
    -- When a copy holds MORE than the bank remembers, its entry point is the
    -- one recorded; a smaller or equal one changes nothing but the counters.
    entry_level          = CASE WHEN EXCLUDED.banked_xp > character_xp_bank.banked_xp
                                THEN EXCLUDED.entry_level ELSE character_xp_bank.entry_level END,
    entry_experience     = CASE WHEN EXCLUDED.banked_xp > character_xp_bank.banked_xp
                                THEN EXCLUDED.entry_experience ELSE character_xp_bank.entry_experience END,
    resulting_experience = CASE WHEN EXCLUDED.banked_xp > character_xp_bank.banked_xp
                                THEN EXCLUDED.resulting_experience ELSE character_xp_bank.resulting_experience END,
    character_name       = CASE WHEN EXCLUDED.banked_xp > character_xp_bank.banked_xp
                                THEN EXCLUDED.character_name ELSE character_xp_bank.character_name END,
    reason               = CASE WHEN EXCLUDED.banked_xp > character_xp_bank.banked_xp
                                THEN EXCLUDED.reason ELSE character_xp_bank.reason END,
    banked_xp    = GREATEST(character_xp_bank.banked_xp, EXCLUDED.banked_xp),
    updated_at   = EXCLUDED.updated_at,
    events       = character_xp_bank.events + 1
"#;

#[allow(clippy::too_many_arguments)]
async fn record(
    conn: &mut AsyncPgConnection,
    owner: BankOwner,
    name: &str,
    banked: u64,
    level: u64,
    experience: u64,
    reason: &str,
    now: i64,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(UPSERT_BANK_SQL)
        .bind::<SqlUuid, _>(owner.alt_key)
        .bind::<SqlUuid, _>(owner.user_id)
        .bind::<SqlUuid, _>(owner.character_id)
        .bind::<Text, _>(name)
        .bind::<BigInt, _>(banked as i64)
        .bind::<Integer, _>(level as i32)
        .bind::<BigInt, _>(experience as i64)
        .bind::<BigInt, _>(experience.saturating_sub(banked) as i64)
        .bind::<Text, _>(reason)
        .bind::<BigInt, _>(now)
        .execute(conn)
        .await?;
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bank a character blob that is about to be written onto a live row.
///
/// Call it from every path that writes a character whose XP was not earned
/// here since the release: a retail import, a restore, an alt switch.
/// `kept_at` is the version's `saved_at` for a restore/switch: a version kept
/// AFTER the release holds XP earned here and is left alone. Returns the amount
/// banked (0 when nothing was, and always 0 before the release).
pub async fn bank_on_write(
    conn: &mut AsyncPgConnection,
    owner: BankOwner,
    character: &mut Value,
    reason: &str,
    kept_at: Option<i64>,
) -> Result<u64, diesel::result::Error> {
    let (level, mut experience, name) = read_character(character);
    let banked =
        bank_experience(conn, owner, &name, level, &mut experience, reason, kept_at).await?;
    if banked > 0 {
        character["experience"] = Value::from(experience);
    }
    Ok(banked)
}

/// [`bank_on_write`] for a typed character: lowers `experience` in place.
pub async fn bank_experience(
    conn: &mut AsyncPgConnection,
    owner: BankOwner,
    name: &str,
    level: u64,
    experience: &mut u64,
    reason: &str,
    kept_at: Option<i64>,
) -> Result<u64, diesel::result::Error> {
    let Some((released_at, needs)) = release(conn).await? else {
        return Ok(0);
    };
    if kept_at.is_some_and(|t| t >= released_at) {
        return Ok(0);
    }
    let before = *experience;
    let banked = excess(level, before, &needs);
    if banked == 0 {
        return Ok(0);
    }
    *experience = before - banked;
    record(conn, owner, name, banked, level, before, reason, now_secs()).await?;
    log::info!(
        "[xp-bank] {reason}: banked {banked} of {name:?}'s {before} xp at level {level} \
         (alt {}, user {})",
        owner.alt_key,
        owner.user_id
    );
    Ok(banked)
}

/// The import's call: bank a transferred character before it is written.
/// The alt key is the alt the row will hold — the incoming one, else the live
/// row's own (an import that names no alt keeps the live stamp).
pub async fn bank_import(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    user_id: Uuid,
    incoming_alt: Option<Uuid>,
    created: bool,
    character: &mut blades_lib::user_data::CompleteCharacter,
    ai_copy: bool,
) -> Result<u64, diesel::result::Error> {
    use crate::schema::characters::dsl as ch;
    use diesel::{ExpressionMethods, QueryDsl};
    let row_alt = match (incoming_alt, created) {
        (Some(alt), _) => Some(alt),
        (None, true) => None,
        (None, false) => ch::characters
            .filter(ch::id.eq(character_id))
            .select(ch::source_alt_uuid)
            .first::<Option<Uuid>>(conn)
            .await
            .optional_row()?
            .flatten(),
    };
    let mut experience = character.experience;
    let banked = bank_experience(
        conn,
        BankOwner::new(character_id, user_id, row_alt),
        &character.name.clone(),
        u64::from(character.level),
        &mut experience,
        if ai_copy {
            "import (ai copy)"
        } else {
            "import"
        },
        None,
    )
    .await?;
    character.experience = experience;
    Ok(banked)
}

// ---------------------------------------------------------------------------
// The release: every row, once.
// ---------------------------------------------------------------------------

#[derive(diesel::QueryableByName)]
struct ScanRow {
    #[diesel(sql_type = Text)]
    kind: String,
    /// `characters.id` for a live row, `character_versions.id` for a version.
    #[diesel(sql_type = SqlUuid)]
    row_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    character_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    source_alt_uuid: Option<Uuid>,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = BigInt)]
    level: i64,
    #[diesel(sql_type = BigInt)]
    experience: i64,
    #[diesel(sql_type = BigInt)]
    banked: i64,
}

/// The rows that bank, computed IN POSTGRES: only ids, name, level and
/// experience ever reach this process — never a `character` blob. Production
/// holds ~1,600 rows (characters + versions); parsing every blob in a 512 MiB
/// container at startup risked an OOM, a rollback and a restart loop.
///
/// `$1` is `needs` as a JSON array (`needs[L]` = XP still needed to reach 100).
/// Non-numeric level/experience text is skipped, never cast (the CASE keeps
/// the cast from being evaluated on it). Live rows first, so an alt's entry
/// point is recorded from the row it is played on.
const SCAN_SQL: &str = r#"
WITH src AS (
    SELECT 'live' AS kind, c.id AS row_id, c.id AS character_id, c.user_id,
           c.source_alt_uuid, COALESCE(c.character->>'name', '') AS name,
           c.character->>'level' AS lv, c.character->>'experience' AS xp
      FROM characters c
    UNION ALL
    SELECT 'version', v.id, v.character_id, v.user_id, v.source_alt_uuid,
           COALESCE(v.character->>'name', ''),
           v.character->>'level', v.character->>'experience'
      FROM character_versions v
), num AS (
    SELECT kind, row_id, character_id, user_id, source_alt_uuid, name,
           CASE WHEN lv ~ '^[0-9]{1,6}(\.[0-9]+)?$' THEN floor(lv::numeric)::bigint END AS level,
           CASE WHEN xp ~ '^[0-9]{1,15}(\.[0-9]+)?$' THEN floor(xp::numeric)::bigint END AS experience
      FROM src
), owed AS (
    SELECT num.*, ($1::jsonb ->> level::int)::bigint AS need
      FROM num
     WHERE level IS NOT NULL AND level <= 100 AND experience IS NOT NULL
)
SELECT kind, row_id, character_id, user_id, source_alt_uuid, name, level, experience,
       experience - need AS banked
  FROM owed
 WHERE need IS NOT NULL AND experience > need
 ORDER BY kind, banked DESC, row_id
"#;

const COUNT_SQL: &str = r#"
SELECT (SELECT count(*) FROM characters) + (SELECT count(*) FROM character_versions) AS n
"#;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BankLine {
    pub kind: String,
    pub row_id: Uuid,
    pub character_id: Uuid,
    pub alt_key: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub level: u64,
    pub experience: u64,
    pub banked: u64,
    pub resulting_experience: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BankReport {
    pub applied: bool,
    /// Set when a release was already recorded: nothing more is banked.
    pub already_released_at: Option<i64>,
    pub rows_seen: usize,
    pub rows_banked: usize,
    pub total_banked: u64,
    pub lines: Vec<BankLine>,
}

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// The lean plan, shared by the dry run and the release.
async fn plan(
    conn: &mut AsyncPgConnection,
    needs: &[u64],
) -> Result<(usize, Vec<BankLine>), diesel::result::Error> {
    let seen: Count = diesel::sql_query(COUNT_SQL).get_result(conn).await?;
    let rows: Vec<ScanRow> = diesel::sql_query(SCAN_SQL)
        .bind::<Jsonb, _>(Value::from(needs.to_vec()))
        .get_results(conn)
        .await?;
    let lines = rows
        .into_iter()
        .map(|r| {
            let owner = BankOwner::new(r.character_id, r.user_id, r.source_alt_uuid);
            let (experience, banked) = (r.experience as u64, r.banked as u64);
            BankLine {
                kind: r.kind,
                row_id: r.row_id,
                character_id: r.character_id,
                alt_key: owner.alt_key,
                user_id: owner.user_id,
                name: r.name,
                level: r.level as u64,
                experience,
                banked,
                resulting_experience: experience - banked,
            }
        })
        .collect();
    Ok((seen.n as usize, lines))
}

fn report(applied: bool, seen: usize, lines: Vec<BankLine>) -> BankReport {
    BankReport {
        applied,
        already_released_at: None,
        rows_seen: seen,
        rows_banked: lines.len(),
        total_banked: lines.iter().map(|l| l.banked).sum(),
        lines,
    }
}

/// What the release would bank. Read-only: creates nothing, writes nothing.
/// Same lean query as the release.
pub async fn dry_run(conn: &mut AsyncPgConnection, data: &LevelUpData) -> Result<BankReport> {
    let needs = needs_to_retail_max(data).context("level table does not reach level 100")?;
    let already = release(conn).await?.map(|(t, _)| t);
    let (seen, lines) = plan(conn, &needs).await?;
    let mut r = report(false, seen, lines);
    r.already_released_at = already;
    Ok(r)
}

/// Lowers one row's experience in place, in Postgres: the blob never leaves it,
/// and every other key is kept as it was.
const LOWER_LIVE_SQL: &str = "UPDATE characters \
    SET character = jsonb_set(character, '{experience}', to_jsonb($2::bigint)) \
    WHERE id = $1";
const LOWER_VERSION_SQL: &str = "UPDATE character_versions \
    SET character = jsonb_set(character, '{experience}', to_jsonb($2::bigint)) \
    WHERE id = $1";

/// How long the release may wait for a lock, and run, before it gives up and
/// the server starts at cap 100 (to retry on the next start).
const RELEASE_LOCK_TIMEOUT: &str = "5s";
const RELEASE_STATEMENT_TIMEOUT: &str = "60s";

/// Bank every row, once. A second call finds the release row and changes
/// nothing. One transaction: either every row is banked and the release is
/// recorded, or nothing is.
pub async fn apply_release(
    conn: &mut AsyncPgConnection,
    data: &LevelUpData,
    max_level: u16,
    now: i64,
) -> Result<BankReport> {
    let Some(needs) = needs_to_retail_max(data) else {
        bail!("level table does not reach level 100; refusing to bank against a guess");
    };
    if let Some((at, _)) = release(conn).await? {
        let mut r = report(false, 0, Vec::new());
        r.already_released_at = Some(at);
        return Ok(r);
    }
    conn.batch_execute(XP_BANK_DDL)
        .await
        .context("creating the XP bank tables")?;
    let report = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            async move {
                conn.batch_execute(&format!(
                    "SET LOCAL lock_timeout = '{RELEASE_LOCK_TIMEOUT}'; \
                     SET LOCAL statement_timeout = '{RELEASE_STATEMENT_TIMEOUT}';"
                ))
                .await?;
                let claimed = diesel::sql_query(
                    "INSERT INTO xp_bank_release (id, released_at, max_level, needs) \
                     VALUES (1, $1, $2, $3) ON CONFLICT (id) DO NOTHING",
                )
                .bind::<BigInt, _>(now)
                .bind::<Integer, _>(i32::from(max_level))
                .bind::<Jsonb, _>(Value::from(needs.clone()))
                .execute(&mut *conn)
                .await?;
                if claimed == 0 {
                    let at = release(conn).await?.map(|(t, _)| t);
                    let mut r = report(false, 0, Vec::new());
                    r.already_released_at = at;
                    return Ok(r);
                }
                let (seen, lines) = plan(conn, &needs).await?;
                for line in &lines {
                    let sql = if line.kind == "live" {
                        LOWER_LIVE_SQL
                    } else {
                        LOWER_VERSION_SQL
                    };
                    diesel::sql_query(sql)
                        .bind::<SqlUuid, _>(line.row_id)
                        .bind::<BigInt, _>(line.resulting_experience as i64)
                        .execute(&mut *conn)
                        .await?;
                    record(
                        conn,
                        BankOwner {
                            alt_key: line.alt_key,
                            user_id: line.user_id,
                            character_id: line.character_id,
                        },
                        &line.name,
                        line.banked,
                        line.level,
                        line.experience,
                        if line.kind == "live" {
                            "release"
                        } else {
                            "release (parked version)"
                        },
                        now,
                    )
                    .await?;
                }
                let r = report(true, seen, lines);
                diesel::sql_query(
                    "UPDATE xp_bank_release SET rows_banked = $1, total_banked = $2 WHERE id = 1",
                )
                .bind::<Integer, _>(r.rows_banked as i32)
                .bind::<BigInt, _>(r.total_banked as i64)
                .execute(&mut *conn)
                .await?;
                Ok(r)
            }
            .scope_boxed()
        })
        .await
        .context("banking pre-release XP")?;
    Ok(report)
}

/// The startup hook. Below or at 100 nothing happens at all — no table, no
/// write. Above it, banks once; the result decides the cap the server runs at:
/// the requested one when the bank is in place, 100 when it could not be.
pub async fn ensure_released(pool: &DbPool, data: &LevelUpData, max_level: u16) -> u16 {
    if max_level <= RETAIL_MAX_LEVEL {
        return max_level;
    }
    let outcome = async {
        let mut conn = pool.get().await.context("database connection")?;
        apply_release(&mut conn, data, max_level, now_secs()).await
    }
    .await;
    match outcome {
        Ok(r) if r.applied => {
            log::warn!(
                "[xp-bank] released levels past 100: banked {} xp across {} of {} rows \
                 (cap {max_level})",
                r.total_banked,
                r.rows_banked,
                r.rows_seen
            );
            for l in &r.lines {
                log::info!(
                    "[xp-bank] {} {:?} level {} xp {} → banked {}, keeps {}",
                    l.kind,
                    l.name,
                    l.level,
                    l.experience,
                    l.banked,
                    l.resulting_experience
                );
            }
            max_level
        }
        Ok(r) => {
            log::info!(
                "[xp-bank] already released at {:?}; cap {max_level}",
                r.already_released_at
            );
            max_level
        }
        Err(e) => {
            let released = match pool.get().await {
                Ok(mut conn) => release(&mut conn).await.ok().flatten().is_some(),
                Err(_) => false,
            };
            cap_after_failure(released, max_level, &e)
        }
    }
}

/// A failed bank keeps the cap at 100 — unless the release already happened
/// on an earlier start: then the XP is banked, characters may already stand
/// past 100, and dropping the cap would bounce them. Only an UNRELEASED
/// failure (including a lock or statement timeout) keeps 100, to retry on the
/// next start.
fn cap_after_failure(released: bool, max_level: u16, e: &anyhow::Error) -> u16 {
    if released {
        log::error!(
            "[xp-bank] check failed ({e:#}) but the release is recorded; keeping cap {max_level}"
        );
        max_level
    } else {
        log::error!(
            "[xp-bank] could not bank pre-release XP ({e:#}); keeping the level cap at \
             {RETAIL_MAX_LEVEL} so no banked XP can buy a level (retried on the next start)"
        );
        RETAIL_MAX_LEVEL
    }
}

/// The banked amount per alt key for one user, for the alts read. Empty before
/// the release.
pub async fn banked_by_alt(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
) -> Result<std::collections::HashMap<Uuid, i64>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        alt_key: Uuid,
        #[diesel(sql_type = BigInt)]
        banked_xp: i64,
    }
    let present: Present =
        diesel::sql_query("SELECT to_regclass('character_xp_bank') IS NOT NULL AS present")
            .get_result(conn)
            .await?;
    if !present.present {
        return Ok(Default::default());
    }
    let rows: Vec<Row> =
        diesel::sql_query("SELECT alt_key, banked_xp FROM character_xp_bank WHERE user_id = $1")
            .bind::<SqlUuid, _>(user_id)
            .get_results(conn)
            .await?;
    Ok(rows.into_iter().map(|r| (r.alt_key, r.banked_xp)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::features::level_up::{
        EXTENDED_MAX_LEVEL, LevelUpRefusal, apply_level_up, plan_level_up,
    };
    use diesel_async::AsyncConnection;

    const RELEASE_AT: i64 = 1_795_000_000;

    fn table() -> LevelUpData {
        LevelUpData::from_json(
            &serde_json::from_str(include_str!("../../deploy/static/level_rewards.json"))
                .expect("valid level_rewards.json"),
        )
    }

    /// XP a level-95 character spends to reach 100 (96..=100 summed).
    const NEED_95_TO_100: u64 = 12_513_190;

    #[test]
    fn needs_count_what_is_left_to_reach_100() {
        let needs = needs_to_retail_max(&table()).unwrap();
        assert_eq!(needs[100], 0);
        assert_eq!(needs[99], 2_593_953, "level 100 alone costs 2,593,953");
        assert_eq!(needs[95], NEED_95_TO_100);
        assert!(needs_to_retail_max(&LevelUpData::default()).is_none());
    }

    #[test]
    fn the_rule() {
        let needs = needs_to_retail_max(&table()).unwrap();
        assert_eq!(
            excess(100, 27_300_000, &needs),
            27_300_000,
            "level 100: all of it"
        );
        assert_eq!(excess(95, 50_000_000, &needs), 50_000_000 - NEED_95_TO_100);
        assert_eq!(
            excess(95, 1_000, &needs),
            0,
            "below the threshold: untouched"
        );
        assert_eq!(excess(40, 0, &needs), 0);
        assert_eq!(
            excess(101, 9_000_000, &needs),
            0,
            "past 100 was earned here"
        );
    }

    async fn fixture() -> Option<AsyncPgConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let mut conn = AsyncPgConnection::establish(&url)
            .await
            .expect("TEST_DATABASE_URL is set but unreachable");
        conn.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        conn.batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema};\
             CREATE TABLE characters (id UUID PRIMARY KEY, user_id UUID NOT NULL, \
               character JSONB NOT NULL, data JSONB NOT NULL DEFAULT '{{}}', \
               inventory JSONB NOT NULL DEFAULT '{{}}', wallet JSONB NOT NULL DEFAULT '{{}}', \
               town JSONB, server_state JSONB NOT NULL DEFAULT '{{}}', source_alt_uuid UUID);"
        ))
        .await
        .unwrap();
        conn.batch_execute(include_str!(
            "../../migrations/2026-09-21-000000-0000_add_character_versions/up.sql"
        ))
        .await
        .unwrap();
        Some(conn)
    }

    macro_rules! db {
        () => {
            match fixture().await {
                Some(c) => c,
                None => {
                    eprintln!("SKIP: TEST_DATABASE_URL unset — XP bank NOT verified");
                    return;
                }
            }
        };
    }

    async fn seed(
        c: &mut AsyncPgConnection,
        name: &str,
        level: u64,
        xp: u64,
        alt: Option<Uuid>,
    ) -> (Uuid, Uuid) {
        let (id, user) = (Uuid::new_v4(), Uuid::new_v4());
        diesel::sql_query(
            "INSERT INTO characters (id, user_id, character, source_alt_uuid) VALUES ($1, $2, $3, $4)",
        )
        .bind::<SqlUuid, _>(id)
        .bind::<SqlUuid, _>(user)
        .bind::<Jsonb, _>(serde_json::json!({"name": name, "level": level, "experience": xp}))
        .bind::<Nullable<SqlUuid>, _>(alt)
        .execute(c)
        .await
        .unwrap();
        (id, user)
    }

    async fn seed_version(
        c: &mut AsyncPgConnection,
        cid: Uuid,
        user: Uuid,
        alt: Option<Uuid>,
        level: u64,
        xp: u64,
        saved_at: i64,
    ) -> Uuid {
        let id = Uuid::new_v4();
        diesel::sql_query(
            "INSERT INTO character_versions (id, character_id, user_id, source_alt_uuid, name, \
               level, character, data, inventory, wallet, server_state, reason, saved_at) \
             VALUES ($1, $2, $3, $4, 'v', $5, $6, '{}', '{}', '{}', '{}', 'switch', $7)",
        )
        .bind::<SqlUuid, _>(id)
        .bind::<SqlUuid, _>(cid)
        .bind::<SqlUuid, _>(user)
        .bind::<Nullable<SqlUuid>, _>(alt)
        .bind::<Integer, _>(level as i32)
        .bind::<Jsonb, _>(serde_json::json!({"name": "v", "level": level, "experience": xp}))
        .bind::<BigInt, _>(saved_at)
        .execute(c)
        .await
        .unwrap();
        id
    }

    #[derive(diesel::QueryableByName)]
    struct Blob {
        #[diesel(sql_type = Jsonb)]
        character: Value,
    }

    async fn live_xp(c: &mut AsyncPgConnection, id: Uuid) -> u64 {
        let b: Blob = diesel::sql_query("SELECT character FROM characters WHERE id = $1")
            .bind::<SqlUuid, _>(id)
            .get_result(c)
            .await
            .unwrap();
        read_character(&b.character).1
    }

    async fn version_xp(c: &mut AsyncPgConnection, id: Uuid) -> u64 {
        let b: Blob = diesel::sql_query("SELECT character FROM character_versions WHERE id = $1")
            .bind::<SqlUuid, _>(id)
            .get_result(c)
            .await
            .unwrap();
        read_character(&b.character).1
    }

    #[derive(diesel::QueryableByName, Debug)]
    struct BankRow {
        #[diesel(sql_type = BigInt)]
        banked_xp: i64,
        #[diesel(sql_type = Integer)]
        entry_level: i32,
        #[diesel(sql_type = BigInt)]
        entry_experience: i64,
        #[diesel(sql_type = Text)]
        reason: String,
    }

    async fn bank(c: &mut AsyncPgConnection, alt_key: Uuid) -> Option<BankRow> {
        diesel::sql_query(
            "SELECT banked_xp, entry_level, entry_experience, reason FROM character_xp_bank \
             WHERE alt_key = $1",
        )
        .bind::<SqlUuid, _>(alt_key)
        .get_result(c)
        .await
        .optional_row()
        .unwrap()
    }

    async fn table_exists(c: &mut AsyncPgConnection, name: &str) -> bool {
        let p: Present = diesel::sql_query(format!(
            "SELECT to_regclass('{name}') IS NOT NULL AS present"
        ))
        .get_result(c)
        .await
        .unwrap();
        p.present
    }

    /// HauDrauf's case: a level-100 row holding 27M. After the release the
    /// client sees 0, the bank holds 27M, and the level-up it would have offered
    /// is refused — and with 0 xp the client no longer offers it.
    #[tokio::test]
    async fn a_level_100_row_banks_everything_and_cannot_level() {
        let mut c = db!();
        let alt = Uuid::new_v4();
        let (cid, _) = seed(&mut c, "HauDrauf", 100, 27_300_000, Some(alt)).await;

        // CONTROL: unbanked, the raised cap would hand out levels at once.
        let open = table().with_max_level(EXTENDED_MAX_LEVEL);
        assert!(plan_level_up(100, 27_300_000, &open).is_ok());

        let r = apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        assert!(r.applied);
        assert_eq!(r.total_banked, 27_300_000);

        assert_eq!(
            live_xp(&mut c, cid).await,
            0,
            "the client-visible pool is emptied"
        );
        let b = bank(&mut c, alt).await.expect("banked under the alt");
        assert_eq!(b.banked_xp, 27_300_000);
        assert_eq!((b.entry_level, b.entry_experience), (100, 27_300_000));
        assert_eq!(b.reason, "release");
        assert!(matches!(
            plan_level_up(100, live_xp(&mut c, cid).await, &open),
            Err(LevelUpRefusal::NotEnoughExperience { have: 0, .. })
        ));
    }

    /// Below 100: keeps exactly what reaches 100, banks the rest; and a row
    /// that cannot reach 100 is not touched.
    #[tokio::test]
    async fn a_level_95_row_keeps_what_reaches_100() {
        let mut c = db!();
        let (cid, _) = seed(&mut c, "Yaskrava", 95, 50_000_000, None).await;
        let (low, _) = seed(&mut c, "Low", 95, 1_000_000, None).await;

        apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();

        assert_eq!(live_xp(&mut c, cid).await, NEED_95_TO_100);
        assert_eq!(
            bank(&mut c, cid).await.unwrap().banked_xp as u64,
            50_000_000 - NEED_95_TO_100,
            "no alt id: keyed by the character id"
        );
        assert_eq!(live_xp(&mut c, low).await, 1_000_000);
        assert!(bank(&mut c, low).await.is_none());

        // And it really reaches 100 and no further.
        let open = table().with_max_level(EXTENDED_MAX_LEVEL);
        let (mut level, mut xp) = (95u16, NEED_95_TO_100);
        while let Ok((next, spent, _, _)) = plan_level_up(level, xp, &open) {
            level = next;
            xp -= spent;
        }
        assert_eq!((level, xp), (100, 0));
    }

    /// Parked versions are banked in place too, and an alt's bank holds the
    /// greatest excess of its copies, not their sum.
    #[tokio::test]
    async fn parked_versions_are_banked_and_never_summed() {
        let mut c = db!();
        let alt = Uuid::new_v4();
        let (cid, uid) = seed(&mut c, "Swanne", 100, 76_200_000, Some(alt)).await;
        let v = seed_version(
            &mut c,
            cid,
            uid,
            Some(alt),
            100,
            70_000_000,
            RELEASE_AT - 86_400,
        )
        .await;

        let r = apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        assert_eq!(r.rows_banked, 2);
        assert_eq!(version_xp(&mut c, v).await, 0);
        assert_eq!(bank(&mut c, alt).await.unwrap().banked_xp, 76_200_000);
    }

    /// An AI copy carries its original's alt id under its own user: each keeps
    /// its own bank row (prod holds one such pair, HauDrauf's).
    #[tokio::test]
    async fn an_ai_copy_banks_under_its_own_user() {
        let mut c = db!();
        let alt = Uuid::new_v4();
        let (_, real) = seed(&mut c, "HauDrauf", 100, 27_273_128, Some(alt)).await;
        let (_, copy) = seed(&mut c, "HauDrauf", 100, 22_679_295, Some(alt)).await;
        apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        assert_eq!(banked_by_alt(&mut c, real).await.unwrap()[&alt], 27_273_128);
        assert_eq!(banked_by_alt(&mut c, copy).await.unwrap()[&alt], 22_679_295);
    }

    /// The release edits only `experience`, in Postgres: every other key of
    /// the blob survives, and junk level/experience text is skipped, not cast.
    #[tokio::test]
    async fn the_release_touches_only_experience_and_skips_junk() {
        let mut c = db!();
        let (cid, _) = seed(&mut c, "Swanne", 100, 76_163_908, None).await;
        diesel::sql_query(
            "UPDATE characters SET character = character || '{\"abilities\":[1,2],\"tagId\":\"0042\"}' \
             WHERE id = $1",
        )
        .bind::<SqlUuid, _>(cid)
        .execute(&mut c)
        .await
        .unwrap();
        let junk = Uuid::new_v4();
        diesel::sql_query(
            "INSERT INTO characters (id, user_id, character) VALUES \
             ($1, $1, '{\"name\":\"J\",\"level\":\"x\",\"experience\":\"99999999999999999999\"}')",
        )
        .bind::<SqlUuid, _>(junk)
        .execute(&mut c)
        .await
        .unwrap();

        let r = apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        assert_eq!(r.rows_banked, 1);
        assert_eq!(r.rows_seen, 2);
        let b: Blob = diesel::sql_query("SELECT character FROM characters WHERE id = $1")
            .bind::<SqlUuid, _>(cid)
            .get_result(&mut c)
            .await
            .unwrap();
        assert_eq!(b.character["experience"], 0);
        assert_eq!(b.character["abilities"], serde_json::json!([1, 2]));
        assert_eq!(b.character["tagId"], "0042");
        assert_eq!(b.character["name"], "Swanne");
    }

    /// The release cannot hang a start: it runs under a lock and a statement
    /// timeout (a timeout is a failed release → cap 100, retried next start).
    #[tokio::test]
    async fn the_release_runs_under_timeouts() {
        let mut c = db!();
        apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        #[derive(diesel::QueryableByName)]
        struct T {
            #[diesel(sql_type = Text)]
            lock_timeout: String,
        }
        // The test wraps everything in one outer transaction, so the release's
        // SET LOCAL is still visible here.
        let t: T = diesel::sql_query("SELECT current_setting('lock_timeout') AS lock_timeout")
            .get_result(&mut c)
            .await
            .unwrap();
        assert_eq!(t.lock_timeout, RELEASE_LOCK_TIMEOUT);
    }

    /// A failure keeps 100 only while unreleased; after a release it keeps the
    /// requested cap so characters already past 100 do not bounce.
    #[test]
    fn a_failure_after_the_release_keeps_the_cap() {
        let e = anyhow::anyhow!("boom");
        assert_eq!(cap_after_failure(false, 150, &e), RETAIL_MAX_LEVEL);
        assert_eq!(cap_after_failure(true, 150, &e), 150);
    }

    /// The per-alt entry point follows the largest banked copy.
    #[tokio::test]
    async fn the_entry_point_follows_the_largest_bank() {
        let mut c = db!();
        apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        let (cid, uid, alt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let owner = BankOwner::new(cid, uid, Some(alt));
        let mut small = 13_000_000u64;
        bank_experience(&mut c, owner, "A", 99, &mut small, "import", None)
            .await
            .unwrap();
        let mut big = 40_000_000u64;
        bank_experience(&mut c, owner, "A", 100, &mut big, "restore", None)
            .await
            .unwrap();
        let mut smaller = 1_000u64;
        bank_experience(&mut c, owner, "A", 100, &mut smaller, "import", None)
            .await
            .unwrap();
        let b = bank(&mut c, alt).await.unwrap();
        assert_eq!(b.banked_xp, 40_000_000);
        assert_eq!((b.entry_level, b.entry_experience), (100, 40_000_000));
        assert_eq!(b.reason, "restore");
    }

    /// Running twice changes nothing the second time.
    #[tokio::test]
    async fn the_release_is_idempotent() {
        let mut c = db!();
        let alt = Uuid::new_v4();
        let (cid, uid) = seed(&mut c, "Vlad", 100, 11_500_000, Some(alt)).await;
        apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();

        // Newblades play since: some XP earned here.
        diesel::sql_query("UPDATE characters SET character = jsonb_set(character, '{experience}', '5000') WHERE id = $1")
            .bind::<SqlUuid, _>(cid)
            .execute(&mut c)
            .await
            .unwrap();
        let again = apply_release(&mut c, &table(), 150, RELEASE_AT + 60)
            .await
            .unwrap();
        assert!(!again.applied);
        assert_eq!(again.already_released_at, Some(RELEASE_AT));
        assert_eq!(live_xp(&mut c, cid).await, 5_000, "earned XP is not banked");
        assert_eq!(bank(&mut c, alt).await.unwrap().banked_xp, 11_500_000);

        // A write-time re-bank of an already-banked copy adds nothing either.
        let mut blob = serde_json::json!({"name": "Vlad", "level": 100, "experience": 0});
        let n = bank_on_write(
            &mut c,
            BankOwner::new(cid, uid, Some(alt)),
            &mut blob,
            "restore",
            Some(RELEASE_AT - 1),
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        assert_eq!(bank(&mut c, alt).await.unwrap().banked_xp, 11_500_000);
    }

    /// After the release, XP earned here levels past 100 normally.
    #[tokio::test]
    async fn xp_earned_after_the_release_levels_to_101() {
        let mut c = db!();
        let (cid, _) = seed(&mut c, "HauDrauf", 100, 27_300_000, None).await;
        apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        assert_eq!(live_xp(&mut c, cid).await, 0);

        let open = table().with_max_level(EXTENDED_MAX_LEVEL);
        let mut ch: blades_lib::user_data::CompleteCharacter = Default::default();
        ch.level = 100;
        ch.experience = 2_638_269 + 10; // a dungeon's worth or two, earned here
        let out = apply_level_up(
            &mut ch,
            blades_lib::features::character_ops::Attribute::Stamina,
            &open,
        )
        .expect("newblades XP buys level 101");
        assert_eq!(out.new_level, 101);
        assert_eq!(ch.experience, 10);
    }

    /// A retail transfer after the release brings its excess into the bank at
    /// write time; a second import of the same capture does not double it.
    #[tokio::test]
    async fn an_import_after_the_release_banks_at_write() {
        let mut c = db!();
        apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        let (cid, uid, alt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let owner = BankOwner::new(cid, uid, Some(alt));
        for _ in 0..2 {
            let mut xp = 40_000_000u64;
            let n = bank_experience(&mut c, owner, "Retail", 100, &mut xp, "import", None)
                .await
                .unwrap();
            assert_eq!((n, xp), (40_000_000, 0));
        }
        let b = bank(&mut c, alt).await.unwrap();
        assert_eq!(b.banked_xp, 40_000_000, "the same capture twice is not 80M");
        assert_eq!(b.reason, "import");
    }

    /// A version kept AFTER the release holds XP earned here: restoring it banks
    /// nothing. One kept before (and somehow missed by the release) is banked.
    #[tokio::test]
    async fn restores_bank_only_versions_from_before_the_release() {
        let mut c = db!();
        apply_release(&mut c, &table(), 150, RELEASE_AT)
            .await
            .unwrap();
        let owner = BankOwner::new(Uuid::new_v4(), Uuid::new_v4(), None);
        let mut after = serde_json::json!({"name": "A", "level": 100, "experience": 900_000});
        assert_eq!(
            bank_on_write(&mut c, owner, &mut after, "restore", Some(RELEASE_AT + 5))
                .await
                .unwrap(),
            0
        );
        assert_eq!(after["experience"], 900_000);
        let mut before = serde_json::json!({"name": "A", "level": 100, "experience": 900_000});
        assert_eq!(
            bank_on_write(&mut c, owner, &mut before, "restore", Some(RELEASE_AT - 5))
                .await
                .unwrap(),
            900_000
        );
        assert_eq!(before["experience"], 0);
    }

    /// The feature off: cap 100 banks nothing and creates nothing, and the
    /// write-time hook is inert.
    #[tokio::test]
    async fn at_cap_100_nothing_is_banked() {
        let mut c = db!();
        let (cid, uid) = seed(&mut c, "HauDrauf", 100, 27_300_000, None).await;
        let mut blob = serde_json::json!({"name": "X", "level": 100, "experience": 27_300_000});
        let n = bank_on_write(
            &mut c,
            BankOwner::new(cid, uid, None),
            &mut blob,
            "import",
            None,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        assert_eq!(blob["experience"], 27_300_000);
        assert!(!table_exists(&mut c, "xp_bank_release").await);
        assert_eq!(live_xp(&mut c, cid).await, 27_300_000);
    }

    /// The dry run reports what the release would do and writes nothing.
    #[tokio::test]
    async fn the_dry_run_lists_and_writes_nothing() {
        let mut c = db!();
        let (cid, _) = seed(&mut c, "Swanne", 100, 76_200_000, None).await;
        seed(&mut c, "Fresh", 1, 0, None).await;
        let r = dry_run(&mut c, &table()).await.unwrap();
        assert!(!r.applied);
        assert_eq!(r.rows_banked, 1);
        let l = &r.lines[0];
        assert_eq!(
            (
                l.name.as_str(),
                l.level,
                l.experience,
                l.banked,
                l.resulting_experience
            ),
            ("Swanne", 100, 76_200_000, 76_200_000, 0)
        );
        assert_eq!(live_xp(&mut c, cid).await, 76_200_000);
        assert!(!table_exists(&mut c, "character_xp_bank").await);
    }
}
