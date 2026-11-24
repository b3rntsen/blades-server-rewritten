//! `POST /…/dev/v1/character-names/lookup` — the arena's current name of a
//! batch of alts, read from the `character_names` registry.
//!
//! The website keeps its own label per captured alt (`blades_alts.name`) and
//! pushes its renames here, but nothing came back the other way: the
//! 2026-11-15 uniqueness backfill renamed "N'wah" to "N'wah 2" (and every
//! duplicate "Imported" to "Imported 2…7"), and the website kept showing the
//! old, duplicate names. This route lets it read the arena's name for many
//! alts in one round trip, with the time the arena last set it, so the newer
//! of the two renames wins.
//!
//! Read-only. Behind the import token like every other dev route.

use std::sync::Arc;

use actix_web::{HttpRequest, http::StatusCode, post, web};
use diesel::sql_types::{Array, BigInt, Text, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{BladeApiError, ServerGlobal, admin::IMPORT_SERVICE_ID};

/// The most keys one request may ask for. A profile has at most a few dozen
/// alts; the website's one-off backfill batches to stay under this.
pub(crate) const MAX_KEYS: usize = 500;

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct NameKey {
    pub user_id: Uuid,
    pub alt_key: Uuid,
}

#[derive(Deserialize)]
pub struct LookupRequest {
    pub keys: Vec<NameKey>,
}

#[derive(Serialize, Debug, PartialEq, Eq, diesel::QueryableByName)]
#[serde(rename_all = "camelCase")]
pub struct NameEntry {
    #[diesel(sql_type = SqlUuid)]
    pub user_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    pub alt_key: Uuid,
    #[diesel(sql_type = Text)]
    pub name: String,
    /// When the arena last set this name (unix seconds).
    #[diesel(sql_type = BigInt)]
    pub updated_at: i64,
}

#[derive(Serialize)]
pub struct LookupResponse {
    pub names: Vec<NameEntry>,
}

#[derive(diesel::QueryableByName)]
struct Present {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    present: bool,
}

/// The registry rows for `keys` (pairs the registry does not hold are simply
/// absent). No registry yet = nothing to report.
pub(crate) async fn lookup_names(
    conn: &mut AsyncPgConnection,
    keys: &[NameKey],
) -> diesel::QueryResult<Vec<NameEntry>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let present: Present =
        diesel::sql_query("SELECT to_regclass('character_names') IS NOT NULL AS present")
            .get_result(conn)
            .await?;
    if !present.present {
        return Ok(Vec::new());
    }
    let users: Vec<Uuid> = keys.iter().map(|k| k.user_id).collect();
    let alts: Vec<Uuid> = keys.iter().map(|k| k.alt_key).collect();
    diesel::sql_query(
        "SELECT n.user_id, n.alt_key, n.name, n.updated_at \
           FROM character_names n \
           JOIN unnest($1::uuid[], $2::uuid[]) AS k(user_id, alt_key) \
             ON n.user_id = k.user_id AND n.alt_key = k.alt_key",
    )
    .bind::<Array<SqlUuid>, _>(users)
    .bind::<Array<SqlUuid>, _>(alts)
    .load(conn)
    .await
}

#[post("/blades.bgs.services/api/dev/v1/character-names/lookup")]
pub async fn lookup_character_names(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<LookupRequest>,
) -> Result<web::Json<LookupResponse>, BladeApiError> {
    crate::admin::check_import_token(&app_state, &req)?;
    let keys = body.into_inner().keys;
    if keys.len() > MAX_KEYS {
        return Err(BladeApiError::new(
            StatusCode::BAD_REQUEST,
            IMPORT_SERVICE_ID,
            1,
        ));
    }
    let mut conn =
        app_state.db_pool.get().await.map_err(|_| {
            BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, IMPORT_SERVICE_ID, 5)
        })?;
    let names = lookup_names(&mut conn, &keys).await.map_err(|e| {
        log::warn!("character-name lookup failed: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 121)
    })?;
    Ok(web::Json(LookupResponse { names }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel::sql_query;
    use diesel_async::AsyncConnection;

    async fn fixture() -> Option<AsyncPgConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let mut conn = AsyncPgConnection::establish(&url)
            .await
            .expect("TEST_DATABASE_URL is set but unreachable");
        conn.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        sql_query(format!("CREATE SCHEMA {schema}"))
            .execute(&mut conn)
            .await
            .unwrap();
        sql_query(format!("SET LOCAL search_path TO {schema}"))
            .execute(&mut conn)
            .await
            .unwrap();
        Some(conn)
    }

    async fn create_registry(c: &mut AsyncPgConnection) {
        // The columns the lookup reads, as the migration defines them.
        sql_query(
            "CREATE TABLE character_names ( \
               user_id uuid NOT NULL, alt_key uuid NOT NULL, name text NOT NULL, \
               norm_name text NOT NULL, grandfathered boolean NOT NULL DEFAULT false, \
               updated_at bigint NOT NULL, PRIMARY KEY (user_id, alt_key))",
        )
        .execute(c)
        .await
        .unwrap();
    }

    async fn put(c: &mut AsyncPgConnection, user: Uuid, alt: Uuid, name: &str, at: i64) {
        sql_query(
            "INSERT INTO character_names (user_id, alt_key, name, norm_name, updated_at) \
             VALUES ($1, $2, $3, lower($3), $4)",
        )
        .bind::<SqlUuid, _>(user)
        .bind::<SqlUuid, _>(alt)
        .bind::<Text, _>(name)
        .bind::<BigInt, _>(at)
        .execute(c)
        .await
        .unwrap();
    }

    /// The backfill numbered a duplicate; the lookup reports the numbered
    /// name for exactly the pair asked for, and nothing for pairs it lacks.
    #[tokio::test]
    async fn reports_the_registry_name_of_each_asked_pair_only() {
        let Some(mut c) = fixture().await else {
            return eprintln!("SKIP: TEST_DATABASE_URL unset — name lookup NOT verified");
        };
        create_registry(&mut c).await;
        let (owner, copy, alt, other_alt) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        put(&mut c, owner, alt, "HauDrauf 2", 1_794_784_421).await;
        // Same alt key under another account (the website's AI copy): not asked.
        put(&mut c, copy, alt, "HauDrauf 3", 1_794_784_421).await;

        let got = lookup_names(
            &mut c,
            &[
                NameKey {
                    user_id: owner,
                    alt_key: alt,
                },
                NameKey {
                    user_id: owner,
                    alt_key: other_alt,
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            got,
            vec![NameEntry {
                user_id: owner,
                alt_key: alt,
                name: "HauDrauf 2".into(),
                updated_at: 1_794_784_421,
            }]
        );
    }

    #[tokio::test]
    async fn no_registry_and_no_keys_report_nothing() {
        let Some(mut c) = fixture().await else {
            return eprintln!("SKIP: TEST_DATABASE_URL unset — name lookup NOT verified");
        };
        let key = NameKey {
            user_id: Uuid::new_v4(),
            alt_key: Uuid::new_v4(),
        };
        assert!(lookup_names(&mut c, &[key]).await.unwrap().is_empty());
        create_registry(&mut c).await;
        assert!(lookup_names(&mut c, &[]).await.unwrap().is_empty());
    }
}
