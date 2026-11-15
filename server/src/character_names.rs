//! One name, one character (owner decision 2026-11-15).
//!
//! The arena Top 100 showed four "Adventurer"s and two "Markus Valerius". Retail
//! told same-named characters apart with a hidden 4-digit tag; our boards show
//! the bare name. So a character name now belongs to one character:
//!
//! - **The database enforces it.** `character_names` (migration
//!   `2026-11-16-000000-0000_unique_character_names`) is a registry of every
//!   alt's current name with a unique index on the normalised name, recomputed
//!   per user by a DEFERRED trigger on `characters` and `character_versions`.
//!   Switch-alt moves a name between those tables mid-transaction; the check
//!   only sees the end state, so it never trips.
//! - **Automatic paths pick a free name** ([`settle_name`]): an import, a new
//!   alt or a game-created character whose name is taken gets the lowest free
//!   number ("Markus Valerius 2"). The starter placeholder "Adventurer" is
//!   replaced by a generated "<Colour> <Adjective> <Creature>" name.
//! - **An explicit rename is refused** ([`claim_exact`]) with
//!   [`NAME_TAKEN`] — the player chose that name and should choose another.
//! - **Existing duplicates** are grandfathered by the migration and renamed by
//!   the owner-approved [`backfill_character_names`] (oldest keeps the bare
//!   name), which records every rename for [`undo_character_name_backfill`].
//!
//! Names stay within the client's own limit — `NameValidator` refuses more than
//! 16 UTF-16 units, and the appearance screen re-runs it, so a longer name would
//! strand the player there (#372).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use actix_web::{HttpRequest, http::StatusCode, post, web};
use diesel::sql_types::{BigInt, Bool, Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::{BladeApiError, ServerGlobal, admin::IMPORT_SERVICE_ID};

/// `IMPORT_SERVICE_ID` error: the name belongs to another character. The
/// website shows it as "that name is taken".
pub(crate) const NAME_TAKEN: u64 = 120;

/// The client's `NameValidator` maximum, in UTF-16 units.
pub(crate) const MAX_NAME_LEN: usize = 16;

// --------------------------------------------------------------------------
// Generated starter names.
//
// Friendly, lore-safe words only: no races or peoples (a "Grim Orc" reads as a
// slur), nothing that could combine into an insult. Every word is plain ASCII,
// so every combination passes the game's character whitelist.
// --------------------------------------------------------------------------

pub(crate) const COLOURS: &[&str] = &[
    "Amber", "Ash", "Auburn", "Azure", "Bark", "Berry", "Blaze", "Blond", "Blue", "Bone", "Brass",
    "Brick", "Bronze", "Cherry", "Clay", "Coal", "Cobalt", "Cocoa", "Copper", "Coral", "Cream",
    "Cyan", "Dawn", "Dew", "Dun", "Dusk", "Ember", "Fern", "Flame", "Flax", "Frost", "Gilt",
    "Glow", "Gold", "Green", "Grey", "Hazel", "Honey", "Ice", "Indigo", "Ink", "Iron", "Ivory",
    "Jade", "Leaf", "Lemon", "Lilac", "Lime", "Mauve", "Mint", "Mist", "Misty", "Moon", "Moss",
    "Navy", "Ochre", "Olive", "Onyx", "Opal", "Peach", "Pearl", "Pine", "Pink", "Plum", "Plume",
    "Rain", "Red", "Roan", "Rose", "Ruby", "Russet", "Rust", "Rusty", "Sable", "Sand", "Sea",
    "Silver", "Sky", "Slate", "Smoke", "Snow", "Star", "Steel", "Storm", "Sun", "Tan", "Tawny",
    "Teal", "Tin", "Topaz", "Umber", "Violet", "Wheat", "Wine",
];

pub(crate) const ADJECTIVES: &[&str] = &[
    "Bold", "Bouncy", "Brave", "Breezy", "Brisk", "Bubbly", "Calm", "Cheery", "Chummy", "Clever",
    "Comfy", "Cozy", "Crafty", "Daring", "Dandy", "Dapper", "Deft", "Dreamy", "Droll", "Eager",
    "Elated", "Fair", "Fancy", "Feisty", "Fond", "Frisky", "Fuzzy", "Gentle", "Giddy", "Glad",
    "Grand", "Gusty", "Hale", "Handy", "Happy", "Hardy", "Hasty", "Hearty", "Honest", "Humble",
    "Jaunty", "Jazzy", "Jolly", "Jovial", "Jumpy", "Keen", "Kind", "Lanky", "Lively", "Lofty",
    "Loyal", "Lucid", "Lucky", "Mellow", "Merry", "Mighty", "Modest", "Nifty", "Nimble", "Noble",
    "Peppy", "Perky", "Plucky", "Proud", "Quiet", "Quirky", "Rosy", "Shy", "Silly", "Sleepy",
    "Sly", "Snappy", "Snug", "Spiffy", "Sporty", "Spry", "Spunky", "Steady", "Stout", "Sturdy",
    "Sunny", "Super", "Sweet", "Swift", "Tidy", "Tough", "Trusty", "Upbeat", "Vast", "Vivid",
    "Wise", "Witty", "Wry", "Zany", "Zesty", "Zippy",
];

pub(crate) const CREATURES: &[&str] = &[
    "Acorn", "Ant", "Archer", "Badger", "Baker", "Bard", "Bat", "Bear", "Bee", "Beetle", "Boar",
    "Carp", "Cat", "Colt", "Cook", "Crab", "Crow", "Drake", "Druid", "Eel", "Elk", "Envoy",
    "Farmer", "Finch", "Fox", "Friar", "Frog", "Goat", "Golem", "Guar", "Guard", "Hare", "Hawk",
    "Healer", "Herald", "Hermit", "Heron", "Horker", "Hound", "Hunter", "Imp", "Jester", "Kit",
    "Knight", "Kwama", "Lark", "Lynx", "Mage", "Mare", "Miner", "Mole", "Monk", "Moth", "Mudcrab",
    "Netch", "Newt", "Nix", "Nomad", "Otter", "Owl", "Ox", "Paladin", "Pebble", "Pike", "Pilgrim",
    "Pony", "Pup", "Ram", "Ranger", "Raven", "Rogue", "Sailor", "Scamp", "Scholar", "Scout",
    "Scribe", "Seer", "Sentry", "Skeever", "Slug", "Smith", "Snail", "Spriggan", "Sprite",
    "Sprout", "Squire", "Stag", "Swan", "Tinker", "Toad", "Troll", "Trout", "Warden", "Wasp",
    "Wisp", "Wizard", "Wolf", "Wren", "Wyrm",
];

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Combination `index` of the three lists, or `None` when it would not fit the
/// client's name limit.
pub(crate) fn combo(index: usize) -> Option<String> {
    let (c, a, n) = (COLOURS.len(), ADJECTIVES.len(), CREATURES.len());
    let index = index % (c * a * n);
    let name = format!(
        "{} {} {}",
        COLOURS[index / (a * n)],
        ADJECTIVES[(index / n) % a],
        CREATURES[index % n]
    );
    (utf16_len(&name) <= MAX_NAME_LEN).then_some(name)
}

/// How many distinct generated names fit the client's limit.
pub(crate) fn combo_space() -> usize {
    (0..COLOURS.len() * ADJECTIVES.len() * CREATURES.len())
        .filter(|&i| combo(i).is_some())
        .count()
}

/// SplitMix64: a deterministic stream per character, so a dry run names each
/// character exactly as the real run will.
fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The `attempt`-th generated-name candidate for one character (always fits).
pub(crate) fn combo_candidate(seed: Uuid, attempt: u64) -> String {
    let (hi, lo) = seed.as_u64_pair();
    let total = (COLOURS.len() * ADJECTIVES.len() * CREATURES.len()) as u64;
    let mut state = splitmix(hi ^ splitmix(lo ^ attempt));
    loop {
        if let Some(name) = combo((state % total) as usize) {
            return name;
        }
        state = splitmix(state);
    }
}

/// `base` with " n" appended, the base shortened so the result still fits.
pub(crate) fn numbered(base: &str, n: u64) -> String {
    let suffix = format!(" {n}");
    let room = MAX_NAME_LEN.saturating_sub(suffix.len());
    let mut kept = String::new();
    for ch in base.trim().chars() {
        if utf16_len(&kept) + ch.len_utf16() > room {
            break;
        }
        kept.push(ch);
    }
    format!("{}{suffix}", kept.trim_end())
}

/// Rust mirror of SQL `character_name_norm`: NFC, trim, collapse whitespace,
/// lower-case. The database's is authoritative; this one is for in-memory
/// bookkeeping (the backfill's plan).
pub(crate) fn norm(name: &str) -> String {
    let nfc: String = name.nfc().collect();
    nfc.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The starter placeholder: "Adventurer", and the website's "Adventurer 2", …
pub(crate) fn is_default_name(name: &str) -> bool {
    let n = norm(name);
    match n.strip_prefix("adventurer") {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix(' ')
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit())),
        None => false,
    }
}

/// Names that never take part (SQL `character_name_exempt`, minus the default
/// name, which the server replaces rather than exempts): blank, AI copies,
/// operator test characters.
pub(crate) fn is_exempt(name: &str) -> bool {
    let n = norm(name);
    n.is_empty() || n.ends_with("(ai)") || n.ends_with(" (test)")
}

/// Write `name` into `data.customization.Name`, honouring `NameVersion` (the
/// client reads the in-game name from there). No `Name` key = nothing to do;
/// a fresh character's customization is built from the name already.
pub(crate) fn set_customization_name(customization: &mut Value, name: &str) {
    let Some(obj) = customization.as_object_mut() else {
        return;
    };
    if !obj.contains_key("Name") {
        return;
    }
    let base64 = obj
        .get("NameVersion")
        .and_then(|v| v.get("_v"))
        .and_then(Value::as_str)
        == Some("base64");
    let value = if base64 {
        base64_utf8(name)
    } else {
        name.to_string()
    };
    obj.insert(
        "Name".to_string(),
        serde_json::json!({ "_t": "String", "_v": value }),
    );
}

fn base64_utf8(value: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(ALPHABET[((bits >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((bits >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((bits >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(bits & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

// --------------------------------------------------------------------------
// Claiming names.
// --------------------------------------------------------------------------

#[derive(diesel::QueryableByName)]
struct Flag {
    #[diesel(sql_type = Bool)]
    flag: bool,
}

/// Whether the registry exists. Older test fixtures (and a database the
/// migration has not reached) have none; the rule then simply is not applied.
async fn registry_present(conn: &mut AsyncPgConnection) -> diesel::QueryResult<bool> {
    let row: Flag = diesel::sql_query("SELECT to_regclass('character_names') IS NOT NULL AS flag")
        .get_result(conn)
        .await?;
    Ok(row.flag)
}

/// Whether another character holds `name`. Takes a transaction-scoped advisory
/// lock on the normalised name first, so two transactions settling the same
/// name queue instead of both seeing it free; the registry row written by
/// [`claim`] (or by the commit-time sync) is visible to the one that waited.
async fn taken_by_other(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    alt_key: Uuid,
    name: &str,
) -> diesel::QueryResult<bool> {
    diesel::sql_query("SELECT pg_advisory_xact_lock(7262, hashtext(character_name_norm($1)))")
        .bind::<Text, _>(name)
        .execute(conn)
        .await?;
    let row: Flag = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM character_names \
                         WHERE norm_name = character_name_norm($1) \
                           AND NOT (user_id = $2 AND alt_key = $3)) AS flag",
    )
    .bind::<Text, _>(name)
    .bind::<SqlUuid, _>(user_id)
    .bind::<SqlUuid, _>(alt_key)
    .get_result(conn)
    .await?;
    Ok(row.flag)
}

/// Record the claim now, inside the caller's transaction, so a concurrent
/// settler that waited on the lock sees it before this one commits.
async fn claim(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    alt_key: Uuid,
    name: &str,
) -> diesel::QueryResult<()> {
    if is_exempt(name) || is_default_name(name) {
        return Ok(());
    }
    diesel::sql_query(
        "INSERT INTO character_names (user_id, alt_key, name, norm_name, grandfathered) \
         VALUES ($1, $2, $3, character_name_norm($3), false) \
         ON CONFLICT (user_id, alt_key) DO UPDATE \
            SET name = EXCLUDED.name, norm_name = EXCLUDED.norm_name, grandfathered = false, \
                updated_at = EXTRACT(EPOCH FROM now())::bigint",
    )
    .bind::<SqlUuid, _>(user_id)
    .bind::<SqlUuid, _>(alt_key)
    .bind::<Text, _>(name)
    .execute(conn)
    .await?;
    Ok(())
}

/// A free generated name for this character. 64 tries of its own candidate
/// stream; with ~600,000 names that only fails when the space is nearly full,
/// and then numbers are appended, as the owner asked.
async fn generate_free(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    alt_key: Uuid,
) -> diesel::QueryResult<String> {
    for attempt in 0..64 {
        let candidate = combo_candidate(alt_key, attempt);
        if !taken_by_other(conn, user_id, alt_key, &candidate).await? {
            return Ok(candidate);
        }
    }
    let base = combo_candidate(alt_key, 0);
    for n in 2.. {
        let candidate = numbered(&base, n);
        if !taken_by_other(conn, user_id, alt_key, &candidate).await? {
            return Ok(candidate);
        }
    }
    unreachable!("the number space is unbounded")
}

/// The name an automatically created or imported character actually gets:
/// the placeholder becomes a generated name, a taken name gets the lowest free
/// number, anything else is kept. Call inside the transaction that writes it.
pub(crate) async fn settle_name(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    alt_key: Uuid,
    desired: &str,
) -> diesel::QueryResult<String> {
    if !registry_present(conn).await? {
        return Ok(desired.to_string());
    }
    let desired = desired.trim();
    if desired.is_empty() || is_default_name(desired) {
        let name = generate_free(conn, user_id, alt_key).await?;
        claim(conn, user_id, alt_key, &name).await?;
        return Ok(name);
    }
    if is_exempt(desired) {
        return Ok(desired.to_string());
    }
    let mut candidate = desired.to_string();
    let mut n = 1;
    while taken_by_other(conn, user_id, alt_key, &candidate).await? {
        n += 1;
        candidate = numbered(desired, n);
    }
    claim(conn, user_id, alt_key, &candidate).await?;
    Ok(candidate)
}

/// An explicit rename: the exact name or [`NAME_TAKEN`]. Renaming a character
/// to its own name (any case) is fine — the check excludes the character itself.
pub(crate) async fn claim_exact(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    alt_key: Uuid,
    name: &str,
) -> Result<(), BladeApiError> {
    let internal = |e: diesel::result::Error| {
        log::warn!("character name claim failed: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 27)
    };
    if !registry_present(conn).await.map_err(internal)? {
        return Ok(());
    }
    if taken_by_other(conn, user_id, alt_key, name)
        .await
        .map_err(internal)?
    {
        return Err(name_taken());
    }
    claim(conn, user_id, alt_key, name).await.map_err(internal)
}

pub(crate) fn name_taken() -> BladeApiError {
    BladeApiError::new(StatusCode::CONFLICT, IMPORT_SERVICE_ID, NAME_TAKEN)
}

/// Whether a database error is the registry's uniqueness check.
pub(crate) fn is_name_conflict(e: &diesel::result::Error) -> bool {
    matches!(
        e,
        diesel::result::Error::DatabaseError(
            diesel::result::DatabaseErrorKind::UniqueViolation,
            info
        ) if info.constraint_name() == Some("character_names_unique_name")
            || info.message().contains("is already taken")
    )
}

// --------------------------------------------------------------------------
// Backfill of existing characters (owner-approved, run by hand).
// --------------------------------------------------------------------------

/// Every alt of every user — the live row plus each other alt's newest kept
/// version — that takes part in the rule, default-named ones included.
const ALL_ALTS_SQL: &str = r#"
WITH live AS (
  SELECT c.user_id, COALESCE(c.source_alt_uuid, c.id) AS alt_key,
         COALESCE(c."character"->>'name', '') AS name,
         COALESCE((c."character"->>'level')::int, 0) AS level
    FROM characters c
), parked AS (
  SELECT DISTINCT ON (v.user_id, v.source_alt_uuid)
         v.user_id, v.source_alt_uuid AS alt_key, v.name, v.level
    FROM character_versions v
   WHERE v.source_alt_uuid IS NOT NULL
   ORDER BY v.user_id, v.source_alt_uuid, v.saved_at DESC, v.id DESC
), alts AS (
  SELECT * FROM live
  UNION ALL
  SELECT p.* FROM parked p
   WHERE NOT EXISTS (SELECT 1 FROM live l WHERE l.user_id = p.user_id AND l.alt_key = p.alt_key)
)
SELECT a.user_id, a.alt_key, a.name, a.level,
       (SELECT min(LEAST(d.bound_at, d.last_seen)) FROM device_bindings d
         WHERE d.user_id = a.user_id) AS account_first_seen,
       (SELECT min(v.saved_at) FROM character_versions v
         WHERE v.user_id = a.user_id AND v.source_alt_uuid = a.alt_key) AS alt_first_saved,
       EXISTS (SELECT 1 FROM test_characters t WHERE t.alt_uuid = a.alt_key) AS is_test
  FROM alts a
"#;

#[derive(diesel::QueryableByName, Clone)]
struct AltRow {
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    alt_key: Uuid,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Integer)]
    level: i32,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    account_first_seen: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = Nullable<BigInt>)]
    alt_first_saved: Option<i64>,
    #[diesel(sql_type = Bool)]
    is_test: bool,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BackfillChange {
    pub user_id: Uuid,
    pub alt_key: Uuid,
    pub level: i32,
    pub old_name: String,
    pub new_name: String,
    /// `default-name` (the "Adventurer" placeholder) or `duplicate`.
    pub reason: String,
    /// For a duplicate: the character that keeps the bare name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_by: Option<Uuid>,
}

/// The renames the backfill would make, oldest-wins, deterministic.
async fn plan_backfill(conn: &mut AsyncPgConnection) -> diesel::QueryResult<Vec<BackfillChange>> {
    let rows: Vec<AltRow> = diesel::sql_query(ALL_ALTS_SQL).get_results(conn).await?;
    let rows: Vec<AltRow> = rows
        .into_iter()
        .filter(|r| !r.is_test && !is_exempt(&r.name))
        .collect();

    // Every name in use stays taken while new names are chosen.
    let mut taken: HashSet<String> = rows
        .iter()
        .filter(|r| !is_default_name(&r.name))
        .map(|r| norm(&r.name))
        .collect();

    let mut groups: HashMap<String, Vec<AltRow>> = HashMap::new();
    let mut defaults: Vec<AltRow> = Vec::new();
    for r in rows {
        if is_default_name(&r.name) {
            defaults.push(r);
        } else {
            groups.entry(norm(&r.name)).or_default().push(r);
        }
    }

    let mut changes = Vec::new();
    let mut keys: Vec<&String> = groups.keys().collect();
    keys.sort();
    for key in keys {
        let mut group = groups[key].clone();
        if group.len() < 2 {
            continue;
        }
        // Oldest first: the account seen first, then the alt kept first (an alt
        // never parked is the account's original), then the higher level.
        group.sort_by(|a, b| {
            let first = |r: &AltRow| (r.account_first_seen.is_none(), r.account_first_seen);
            first(a)
                .cmp(&first(b))
                .then(
                    (a.alt_first_saved.is_some(), a.alt_first_saved)
                        .cmp(&(b.alt_first_saved.is_some(), b.alt_first_saved)),
                )
                .then(b.level.cmp(&a.level))
                .then(a.user_id.cmp(&b.user_id))
                .then(a.alt_key.cmp(&b.alt_key))
        });
        let holder = group[0].alt_key;
        for r in group.into_iter().skip(1) {
            let mut n = 2;
            let mut candidate = numbered(&r.name, n);
            while taken.contains(&norm(&candidate)) {
                n += 1;
                candidate = numbered(&r.name, n);
            }
            taken.insert(norm(&candidate));
            changes.push(BackfillChange {
                user_id: r.user_id,
                alt_key: r.alt_key,
                level: r.level,
                old_name: r.name.clone(),
                new_name: candidate,
                reason: "duplicate".into(),
                kept_by: Some(holder),
            });
        }
    }

    defaults.sort_by_key(|r| (r.user_id, r.alt_key));
    for r in defaults {
        let mut attempt = 0;
        let mut candidate = combo_candidate(r.alt_key, attempt);
        while taken.contains(&norm(&candidate)) {
            attempt += 1;
            candidate = if attempt < 64 {
                combo_candidate(r.alt_key, attempt)
            } else {
                numbered(&combo_candidate(r.alt_key, 0), attempt - 62)
            };
        }
        taken.insert(norm(&candidate));
        changes.push(BackfillChange {
            user_id: r.user_id,
            alt_key: r.alt_key,
            level: r.level,
            old_name: r.name.clone(),
            new_name: candidate,
            reason: "default-name".into(),
            kept_by: None,
        });
    }
    Ok(changes)
}

/// Put `name` on one alt everywhere it is stored: the live row (if this alt is
/// live) and EVERY kept version of it — so restoring an older version cannot
/// bring the old, duplicate name back. Returns rows touched.
const RENAME_LIVE_SQL: &str = r#"
UPDATE characters
   SET "character" = jsonb_set("character", '{name}', to_jsonb($3::text), true),
       data = CASE WHEN data->'customization' ? 'Name' THEN
                jsonb_set(data, '{customization,Name}', jsonb_build_object('_t', 'String', '_v',
                  CASE WHEN data#>>'{customization,NameVersion,_v}' = 'base64'
                       THEN replace(encode(convert_to($3, 'UTF8'), 'base64'), E'\n', '')
                       ELSE $3 END), true)
              ELSE data END
 WHERE user_id = $1 AND COALESCE(source_alt_uuid, id) = $2
"#;

const RENAME_VERSIONS_SQL: &str = r#"
UPDATE character_versions
   SET name = $3,
       "character" = jsonb_set("character", '{name}', to_jsonb($3::text), true),
       data = CASE WHEN data->'customization' ? 'Name' THEN
                jsonb_set(data, '{customization,Name}', jsonb_build_object('_t', 'String', '_v',
                  CASE WHEN data#>>'{customization,NameVersion,_v}' = 'base64'
                       THEN replace(encode(convert_to($3, 'UTF8'), 'base64'), E'\n', '')
                       ELSE $3 END), true)
              ELSE data END
 WHERE user_id = $1 AND source_alt_uuid = $2
"#;

async fn rename_alt_everywhere(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    alt_key: Uuid,
    name: &str,
) -> diesel::QueryResult<usize> {
    let live = diesel::sql_query(RENAME_LIVE_SQL)
        .bind::<SqlUuid, _>(user_id)
        .bind::<SqlUuid, _>(alt_key)
        .bind::<Text, _>(name)
        .execute(conn)
        .await?;
    let versions = diesel::sql_query(RENAME_VERSIONS_SQL)
        .bind::<SqlUuid, _>(user_id)
        .bind::<SqlUuid, _>(alt_key)
        .bind::<Text, _>(name)
        .execute(conn)
        .await?;
    Ok(live + versions)
}

/// Plan, and unless `dry_run`, apply — one transaction, every rename recorded
/// in `character_name_backfill_undo` under one `run_id`. Idempotent: a second
/// run finds nothing to do.
pub(crate) async fn run_backfill(
    conn: &mut AsyncPgConnection,
    dry_run: bool,
) -> diesel::QueryResult<BackfillResponse> {
    let changes = plan_backfill(conn).await?;
    if dry_run || changes.is_empty() {
        return Ok(BackfillResponse {
            dry_run,
            run_id: None,
            changes,
        });
    }
    let run_id = Uuid::new_v4();
    for c in &changes {
        rename_alt_everywhere(conn, c.user_id, c.alt_key, &c.new_name).await?;
        diesel::sql_query(
            "INSERT INTO character_name_backfill_undo \
               (run_id, user_id, alt_key, old_name, new_name, reason) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind::<SqlUuid, _>(run_id)
        .bind::<SqlUuid, _>(c.user_id)
        .bind::<SqlUuid, _>(c.alt_key)
        .bind::<Text, _>(&c.old_name)
        .bind::<Text, _>(&c.new_name)
        .bind::<Text, _>(&c.reason)
        .execute(conn)
        .await?;
    }
    Ok(BackfillResponse {
        dry_run,
        run_id: Some(run_id),
        changes,
    })
}

#[derive(diesel::QueryableByName)]
struct UndoRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    alt_key: Uuid,
    #[diesel(sql_type = Text)]
    old_name: String,
    #[diesel(sql_type = Text)]
    new_name: String,
}

/// Put one run's old names back. A character renamed again since is skipped
/// (and reported): undo restores the backfill's work, never a later choice.
/// The restored names are duplicates again, so each is re-registered as
/// grandfathered BEFORE its name changes; the commit-time sync keeps the flag.
pub(crate) async fn run_undo(
    conn: &mut AsyncPgConnection,
    run_id: Uuid,
) -> diesel::QueryResult<UndoResponse> {
    let rows: Vec<UndoRow> = diesel::sql_query(
        "SELECT id, user_id, alt_key, old_name, new_name FROM character_name_backfill_undo \
          WHERE run_id = $1 AND undone_at IS NULL ORDER BY id DESC",
    )
    .bind::<SqlUuid, _>(run_id)
    .get_results(conn)
    .await?;
    let mut restored = 0;
    let mut skipped = Vec::new();
    for r in rows {
        let current: Option<Flag> = diesel::sql_query(
            "SELECT EXISTS ( \
               SELECT 1 FROM characters WHERE user_id = $1 AND COALESCE(source_alt_uuid, id) = $2 \
                  AND \"character\"->>'name' = $3 \
               UNION ALL \
               SELECT 1 FROM character_versions WHERE user_id = $1 AND source_alt_uuid = $2 \
                  AND name = $3) AS flag",
        )
        .bind::<SqlUuid, _>(r.user_id)
        .bind::<SqlUuid, _>(r.alt_key)
        .bind::<Text, _>(&r.new_name)
        .get_result(conn)
        .await
        .ok();
        if !current.is_some_and(|f| f.flag) {
            skipped.push(r.alt_key);
            continue;
        }
        if !is_exempt(&r.old_name) && !is_default_name(&r.old_name) {
            diesel::sql_query(
                "INSERT INTO character_names (user_id, alt_key, name, norm_name, grandfathered) \
                 VALUES ($1, $2, $3, character_name_norm($3), true) \
                 ON CONFLICT (user_id, alt_key) DO UPDATE \
                    SET name = EXCLUDED.name, norm_name = EXCLUDED.norm_name, grandfathered = true",
            )
            .bind::<SqlUuid, _>(r.user_id)
            .bind::<SqlUuid, _>(r.alt_key)
            .bind::<Text, _>(&r.old_name)
            .execute(conn)
            .await?;
        }
        rename_alt_everywhere(conn, r.user_id, r.alt_key, &r.old_name).await?;
        diesel::sql_query(
            "UPDATE character_name_backfill_undo \
                SET undone_at = EXTRACT(EPOCH FROM now())::bigint WHERE id = $1",
        )
        .bind::<BigInt, _>(r.id)
        .execute(conn)
        .await?;
        restored += 1;
    }
    Ok(UndoResponse {
        run_id,
        restored,
        skipped,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillRequest {
    /// Defaults to TRUE: applying is the explicit choice.
    #[serde(default = "yes")]
    pub dry_run: bool,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackfillResponse {
    pub dry_run: bool,
    pub run_id: Option<Uuid>,
    pub changes: Vec<BackfillChange>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoRequest {
    pub run_id: Uuid,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UndoResponse {
    pub run_id: Uuid,
    pub restored: usize,
    pub skipped: Vec<Uuid>,
}

fn backfill_failed(e: diesel::result::Error) -> BladeApiError {
    log::warn!("character-name backfill failed: {e}");
    if is_name_conflict(&e) {
        return name_taken();
    }
    BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 121)
}

/// `POST /…/dev/v1/character-names/backfill {dryRun}` — number existing
/// duplicates (oldest keeps the bare name) and replace every "Adventurer" with
/// a generated name. Dry run by default.
#[post("/blades.bgs.services/api/dev/v1/character-names/backfill")]
pub async fn backfill_character_names(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<BackfillRequest>,
) -> Result<web::Json<BackfillResponse>, BladeApiError> {
    crate::admin::check_import_token(&app_state, &req)?;
    let dry_run = body.dry_run;
    let mut conn =
        app_state.db_pool.get().await.map_err(|_| {
            BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, IMPORT_SERVICE_ID, 5)
        })?;
    let out = conn
        .transaction::<_, diesel::result::Error, _>(|c| {
            async move { run_backfill(c, dry_run).await }.scope_boxed()
        })
        .await
        .map_err(backfill_failed)?;
    log::info!(
        "character-name backfill: dry_run={} run={:?} changes={}",
        out.dry_run,
        out.run_id,
        out.changes.len()
    );
    Ok(web::Json(out))
}

/// `POST /…/dev/v1/character-names/backfill/undo {runId}`.
#[post("/blades.bgs.services/api/dev/v1/character-names/backfill/undo")]
pub async fn undo_character_name_backfill(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<UndoRequest>,
) -> Result<web::Json<UndoResponse>, BladeApiError> {
    crate::admin::check_import_token(&app_state, &req)?;
    let run_id = body.run_id;
    let mut conn =
        app_state.db_pool.get().await.map_err(|_| {
            BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, IMPORT_SERVICE_ID, 5)
        })?;
    let out = conn
        .transaction::<_, diesel::result::Error, _>(|c| {
            async move { run_undo(c, run_id).await }.scope_boxed()
        })
        .await
        .map_err(backfill_failed)?;
    log::info!(
        "character-name backfill undo: run={} restored={} skipped={}",
        out.run_id,
        out.restored,
        out.skipped.len()
    );
    Ok(web::Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_name_space_is_at_least_100k_and_every_name_fits_the_client() {
        let space = combo_space();
        assert!(space >= 100_000, "only {space} generated names fit");
        for i in (0..COLOURS.len() * ADJECTIVES.len() * CREATURES.len()).step_by(97) {
            if let Some(name) = combo(i) {
                assert!(
                    crate::admin::is_game_name(&name),
                    "{name:?} fails the game rule"
                );
            }
        }
    }

    #[test]
    fn word_lists_have_no_duplicates_and_no_placeholder() {
        for list in [COLOURS, ADJECTIVES, CREATURES] {
            let set: HashSet<_> = list.iter().collect();
            assert_eq!(set.len(), list.len());
            assert!(
                list.iter()
                    .all(|w| w.chars().all(|c| c.is_ascii_alphabetic()))
            );
        }
        let all: Vec<_> = COLOURS.iter().chain(ADJECTIVES).chain(CREATURES).collect();
        assert!(!all.iter().any(|w| w.eq_ignore_ascii_case("adventurer")));
    }

    #[test]
    fn candidates_are_deterministic_per_character_and_differ_between_characters() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(combo_candidate(a, 0), combo_candidate(a, 0));
        assert_ne!(combo_candidate(a, 0), combo_candidate(a, 1));
        assert_ne!(combo_candidate(a, 0), combo_candidate(b, 0));
        let distinct: HashSet<_> = (0..1000)
            .map(|i| combo_candidate(Uuid::from_u128(i), 0))
            .collect();
        assert!(distinct.len() > 990, "{} of 1000 distinct", distinct.len());
    }

    #[test]
    fn numbering_keeps_the_name_within_the_client_limit() {
        assert_eq!(numbered("Markus Valerius", 2), "Markus Valeriu 2");
        assert_eq!(numbered("Ravena", 2), "Ravena 2");
        assert_eq!(numbered("Ravena", 12), "Ravena 12");
        assert!(utf16_len(&numbered("Sixteen chars ok", 10)) <= MAX_NAME_LEN);
    }

    #[test]
    fn normalisation_is_nfc_trimmed_collapsed_and_case_folded() {
        assert_eq!(norm("  Markus   Valerius "), "markus valerius");
        assert_eq!(norm("ÍSDÍS"), "ísdís");
        assert_eq!(norm("I\u{301}sdi\u{301}s"), "ísdís"); // decomposed accents
    }

    #[test]
    fn the_placeholder_and_exempt_names_are_recognised() {
        assert!(is_default_name("Adventurer"));
        assert!(is_default_name(" adventurer 12 "));
        assert!(!is_default_name("Adventurer Bob"));
        assert!(!is_default_name("Adventurers"));
        assert!(is_exempt("Ravena (AI)"));
        assert!(is_exempt("Ravena (Test)"));
        assert!(!is_exempt("Ravena"));
    }

    #[test]
    fn customization_name_follows_name_version() {
        let mut c = serde_json::json!({
            "Name": {"_t": "String", "_v": "QWR2ZW50dXJlcg=="},
            "NameVersion": {"_t": "String", "_v": "base64"}
        });
        set_customization_name(&mut c, "Teal Shy Fox");
        assert_eq!(c["Name"]["_v"], base64_utf8("Teal Shy Fox"));
        let mut plain = serde_json::json!({"Name": {"_t": "String", "_v": "Adventurer"}});
        set_customization_name(&mut plain, "Teal Shy Fox");
        assert_eq!(plain["Name"]["_v"], "Teal Shy Fox");
        let mut none = serde_json::json!({});
        set_customization_name(&mut none, "X");
        assert_eq!(none, serde_json::json!({}));
    }

    #[test]
    fn sample_names() {
        let sample: Vec<String> = (0..10u128)
            .map(|i| combo_candidate(Uuid::from_u128(0xC0FFEE + i), 0))
            .collect();
        eprintln!("sample generated names: {sample:?}");
        eprintln!(
            "lists: {} colours x {} adjectives x {} creatures; {} fit 16",
            COLOURS.len(),
            ADJECTIVES.len(),
            CREATURES.len(),
            combo_space()
        );
    }
}
