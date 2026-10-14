//! Which alt a per-character loot or purchase roll belongs to (tracker #387).
//!
//! A player's alts share ONE live character row: switching alts swaps a kept
//! version into it (`admin::apply_switch_alt`). So every roll seeded by
//! `character_id` alone — shop purchases, store bundles, treasury chests, the
//! daily chest — handed two alts at the same level and the same counts the very
//! same sequence. Alts are fully separate characters (owner decision
//! 2026-10-10), so each alt's rolls must be its own.
//!
//! THE ORIGINAL KEEPS ITS SEQUENCE. Folding the alt into every seed would have
//! re-dealt every stamped character on the server, including ones whose drops a
//! player has checked against the roll (Sephoris, #387). So the alt is folded in
//! only for an alt that is NOT the row's original:
//!
//! - a row with no alt id, or whose alt id is its own character id, is the
//!   original;
//! - a row with no kept version under an alt id has only ever held this one
//!   character, so it is the original too;
//! - otherwise the original is the alt of the row's EARLIEST kept version — the
//!   first character ever set aside on this row. Versions are never deleted, so
//!   once a row has one its original never changes.
//!
//! Measured on prod 2026-10-14: of 589 live rows, 567 carry no alt id, 18 are
//! their row's original by the rules above, and 4 are later alts — only those 4
//! roll differently after this.

use diesel::sql_types::{Nullable, Uuid as SqlUuid};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use uuid::Uuid;

/// The alt to fold into a roll seed, or `None` to keep the seed exactly as it
/// was before alts existed. `live_alt` is the row's `source_alt_uuid`;
/// `original_alt` the alt of its earliest kept version, if it has one.
pub fn roll_alt(character_id: Uuid, live_alt: Option<Uuid>, original_alt: Option<Uuid>) -> Option<Uuid> {
    let alt = live_alt.filter(|alt| *alt != character_id)?;
    let original = original_alt?;
    (alt != original).then_some(alt)
}

/// A 64-bit value standing for `alt` in an integer nonce: both halves mixed
/// (splitmix64's finaliser), so alts differing in only one half still differ.
pub fn salt(alt: Uuid) -> u64 {
    let v = alt.as_u128();
    let mut z = (v as u64) ^ ((v >> 64) as u64).rotate_left(32);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `nonce` with `alt` folded in; unchanged when there is no alt to fold.
pub fn fold_nonce(nonce: u64, alt: Option<Uuid>) -> u64 {
    match alt {
        Some(alt) => nonce ^ salt(alt),
        None => nonce,
    }
}

/// `key` with `alt` appended; unchanged when there is no alt to fold.
pub fn fold_key(key: String, alt: Option<Uuid>) -> String {
    match alt {
        Some(alt) => format!("{key}:alt:{alt}"),
        None => key,
    }
}

#[derive(diesel::QueryableByName)]
struct RollAltRow {
    #[diesel(sql_type = Nullable<SqlUuid>)]
    live: Option<Uuid>,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    original: Option<Uuid>,
}

const ROLL_ALT_SQL: &str = "\
    SELECT c.source_alt_uuid AS live, \
           (SELECT v.source_alt_uuid FROM character_versions v \
             WHERE v.character_id = c.id AND v.source_alt_uuid IS NOT NULL \
             ORDER BY v.saved_at, v.id LIMIT 1) AS original \
      FROM characters c WHERE c.id = $1";

/// [`roll_alt`] for the live row `character_id`, read inside the caller's
/// transaction (call it after the row is locked, so a switch cannot land
/// between this read and the roll).
///
/// In a savepoint: a database without `character_versions` (or any other read
/// failure) rolls as the original did rather than aborting the purchase.
pub async fn for_character(conn: &mut AsyncPgConnection, character_id: Uuid) -> Option<Uuid> {
    let row = conn
        .transaction::<_, diesel::result::Error, _>(|c| {
            async move {
                diesel::sql_query(ROLL_ALT_SQL)
                    .bind::<SqlUuid, _>(character_id)
                    .get_results::<RollAltRow>(c)
                    .await
            }
            .scope_boxed()
        })
        .await;
    match row {
        Ok(rows) => rows
            .into_iter()
            .next()
            .and_then(|r| roll_alt(character_id, r.live, r.original)),
        Err(e) => {
            log::warn!("[alt-roll] could not read character {character_id}'s alt ({e}); rolling as the original");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: Uuid = Uuid::from_u128(0x387);
    const ORIGINAL: Uuid = Uuid::from_u128(0xA1);
    const LATER: Uuid = Uuid::from_u128(0xA2);

    #[test]
    fn only_a_later_alt_is_folded_in() {
        // Never stamped, or stamped with its own id: the original.
        assert_eq!(roll_alt(ROW, None, None), None);
        assert_eq!(roll_alt(ROW, None, Some(ORIGINAL)), None);
        assert_eq!(roll_alt(ROW, Some(ROW), Some(ORIGINAL)), None);
        // Stamped but nothing ever set aside: the only character this row held.
        assert_eq!(roll_alt(ROW, Some(ORIGINAL), None), None);
        // The earliest kept version's alt is the original, live again.
        assert_eq!(roll_alt(ROW, Some(ORIGINAL), Some(ORIGINAL)), None);
        // Any other alt is folded in.
        assert_eq!(roll_alt(ROW, Some(LATER), Some(ORIGINAL)), Some(LATER));
    }

    #[test]
    fn folding_nothing_changes_nothing() {
        assert_eq!(fold_nonce(12345, None), 12345);
        assert_eq!(fold_key("k".into(), None), "k");
        assert_ne!(fold_nonce(12345, Some(LATER)), 12345);
        assert_ne!(fold_key("k".into(), Some(LATER)), "k");
    }

    #[test]
    fn alts_get_distinct_salts() {
        let salts: std::collections::HashSet<u64> =
            (1..=10_000u128).map(|n| salt(Uuid::from_u128(n))).collect();
        assert_eq!(salts.len(), 10_000);
        // Uuids differing only in the high half differ too.
        assert_ne!(salt(Uuid::from_u128(1 << 64)), salt(Uuid::from_u128(2 << 64)));
    }
}

/// Against a real Postgres; SKIPS without TEST_DATABASE_URL (CI provides one).
/// The alt-switch round trip itself is tested with the other alt tests in
/// `admin.rs` (`only_a_later_alt_rolls_apart_from_the_original`).
#[cfg(test)]
mod db {
    use super::*;
    use diesel_async::SimpleAsyncConnection;

    /// A database without `character_versions` rolls as the original, and the
    /// caller's transaction is still usable afterwards — the purchase goes on.
    #[tokio::test]
    async fn a_missing_versions_table_rolls_as_the_original() {
        let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — alt roll read NOT verified");
            return;
        };
        let mut c = AsyncPgConnection::establish(&url).await.expect("TEST_DATABASE_URL unreachable");
        c.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        c.batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema}; \
             CREATE TABLE characters (id UUID PRIMARY KEY, source_alt_uuid UUID);"
        ))
        .await
        .unwrap();
        let (id, alt) = (Uuid::new_v4(), Uuid::new_v4());
        diesel::sql_query("INSERT INTO characters (id, source_alt_uuid) VALUES ($1, $2)")
            .bind::<SqlUuid, _>(id)
            .bind::<SqlUuid, _>(alt)
            .execute(&mut c)
            .await
            .unwrap();

        assert_eq!(for_character(&mut c, id).await, None);
        // Not poisoned: the transaction still runs statements.
        diesel::sql_query("UPDATE characters SET source_alt_uuid = NULL WHERE id = $1")
            .bind::<SqlUuid, _>(id)
            .execute(&mut c)
            .await
            .expect("the caller's transaction survives a failed alt read");
    }
}
