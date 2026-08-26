use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{Data, Json, Path},
};
use diesel::{
    ExpressionMethods, QueryDsl, SelectableHelper,
    dsl::sql,
    sql_types::Jsonb,
};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, json_db::JsonDbWrapper,
    models::CharacterDbEntryCharacterWalletInventory, session::SessionLookedUpMaybe,
};

#[derive(Deserialize)]
struct DataUpdateRequest {
    data: DataUpdateRequestInner,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataUpdateRequestInner {
    dialog: Option<Value>,
    /// Full character customization blob (Name/Morphs/CharacterUID/tints/presets).
    /// The in-town appearance NPC ("Theodor Gorlash") posts this to change appearance,
    /// SEX and RACE (sex/race are expressed through `Morphs` + the face/body presets).
    /// Previously `Option<()>` — the real payload failed to deserialize (→400) and the
    /// `assert!` below panicked, so the change never saved and the client hung. Now a
    /// passthrough `Value` we persist verbatim into `data.customization`.
    customization: Option<Value>,
    #[serde(rename = "new-flags")]
    new_flags: Option<Value>,
}

#[post("/api/game/v1/public/characters/{character_id}/data")]
async fn update_data(
    session: SessionLookedUpMaybe,
    app_state: Data<Arc<ServerGlobal>>,
    body: Json<DataUpdateRequest>,
    path: Path<Uuid>,
) -> Result<Json<Value>, BladeApiError> {
    if body.data.dialog.is_none()
        && body.data.new_flags.is_none()
        && body.data.customization.is_none()
    {
        return Ok(Json(json!(null)));
    }
    let character_id = path.into_inner();
    let session = session.get_session_or_error()?;
    // Clone the (small) appearance cost out before the transaction closure captures
    // things by move — `conn` is derived from `app_state`, so the closure can't also
    // move `app_state` (mirrors the repair/shop handlers taking `conn` off the Data
    // handle and cloning only what they need).
    let appearance_cost = app_state.appearance_change_cost.clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(|mut conn| {
        async move {
            use crate::schema::characters::dsl::*;

            // ONE write, not one per field.
            //
            // This used to take the row lock and then issue a separate UPDATE per
            // present field, each a `jsonb_set` over the whole `data` column. That
            // column averages 96 kB and reaches 244 kB, so every request rewrote a
            // TOASTed value up to three times and wrote three sets of WAL. Measured
            // on prod, median latency for this route went from 12-31 ms through
            // 20 August to ~900 ms after — the volume never changed, the payloads
            // grew. (The old code carried a comment saying a single request could
            // not be worked out; it can.)
            //
            // All three destinations are TOP-LEVEL keys, so `data || patch` is
            // exactly equivalent to three depth-1 `jsonb_set` calls with
            // create_if_missing — same create-or-replace semantics, one pass.
            let mut patch = serde_json::Map::new();
            if let Some(new_flags) = body.0.data.new_flags {
                patch.insert("new-flags".to_string(), new_flags);
            }
            if let Some(dialog) = body.0.data.dialog {
                patch.insert("dialog".to_string(), dialog);
            }
            // `customization` is merged here too, but the appearance branch below
            // still runs for the wallet debit and the response body it owes the
            // client. It no longer performs its own write.
            let has_customization = body.0.data.customization.is_some();
            if let Some(customization) = body.0.data.customization.clone() {
                patch.insert("customization".to_string(), customization);
            }

            if !patch.is_empty() {
                let updated = diesel::update(characters)
                    .filter(id.eq(character_id))
                    .filter(user_id.eq(session.session.user_id))
                    .set(data.eq(sql::<Jsonb>("data || ")
                        .bind::<Jsonb, _>(serde_json::Value::Object(patch))))
                    .execute(&mut conn)
                    .await?;
                if updated == 0 {
                    return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 1003, 2));
                }
            }

            // APPEARANCE / SEX / RACE change (the in-town "Theodor Gorlash" NPC).
            // Persist the customization blob verbatim into `data.customization` — sex and
            // race live in its `Morphs` + face/body presets, so this one write covers all
            // three. Previously discarded (Option<()> + assert) → the change never saved
            // and the client hung after the NPC (Viventus). [ground truth: SA3 capture]
            if has_customization {
                // The customization blob was already persisted by the single merge
                // above; this branch exists for the wallet debit and the response
                // body retail owes the client.

                // Retail's appearance-change response is the (charged) wallet + the
                // inventory versions — the client updates its wallet display from it, so
                // returning `null` risks the "stuck after" hang. Echo the (now charged)
                // wallet + versions (appearance touches no items → no version bump).
                //
                // Debit the faithful appearance-change currency cost (APK-derived
                // `appearance_change_cost.json`, loaded into `app_state.appearance_change_cost`
                // — Gem 50). If the player can't afford it we still keep the persisted
                // customization (the write already happened above and retail treats the
                // change as committed); the wallet simply floors at what they have. We
                // never panic here — a bad/absent cost table just skips the debit.
                let mut row = characters
                    .filter(id.eq(character_id))
                    .filter(user_id.eq(session.session.user_id))
                    .select(CharacterDbEntryCharacterWalletInventory::as_select())
                    .first(&mut conn)
                    .await?;

                if let Some((currency, amount)) = parse_appearance_cost(&appearance_cost) {
                    // Charge what they can afford (min of cost and balance) so the change
                    // — which the client has already applied locally — never bounces to a
                    // hang; a fully-broke wallet is left at 0 rather than erroring.
                    let have = row.wallet.0.balance(currency);
                    let charge = amount.min(have);
                    if charge > 0 {
                        // Infallible: `charge <= have`.
                        let _ = row.wallet.0.debit(currency, charge);
                        diesel::update(characters)
                            .filter(id.eq(character_id))
                            .filter(user_id.eq(session.session.user_id))
                            .set(wallet.eq(JsonDbWrapper(row.wallet.0.clone())))
                            .execute(&mut conn)
                            .await?;
                    }
                }

                return Ok(Json(json!({
                    "wallet": row.wallet.0,
                    "inventory": {
                        "backpackVersion": row.inventory.0.backpack_version,
                        "treasuryVersion": row.inventory.0.treasury_version,
                    }
                })));
            }

            Ok(Json(json!(null)))
        }
        .scope_boxed()
    })
    .await
}

/// Parse the appearance-change cost `{currencyId, amount}` out of the raw
/// `app_state.appearance_change_cost` `Value`. The file nests it under
/// `characterCustomizationCost` (see `appearance_change_cost.json`), but we also
/// accept the bare object. Returns `None` (→ no debit) if the table is missing or
/// malformed — the appearance change must never fail on a cost-table problem.
fn parse_appearance_cost(cost: &Value) -> Option<(Uuid, u64)> {
    let obj = cost
        .get("characterCustomizationCost")
        .filter(|v| v.is_object())
        .unwrap_or(cost);
    let currency = obj
        .get("currencyId")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())?;
    let amount = obj.get("amount").and_then(Value::as_u64)?;
    Some((currency, amount))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_cost_parses_nested_and_bare() {
        let nested = json!({
            "characterCustomizationCost": {
                "currencyId": "470c8f58-a8dd-4c07-8c92-843b785e1139",
                "amount": 50
            }
        });
        let (cur, amt) = parse_appearance_cost(&nested).unwrap();
        assert_eq!(amt, 50);
        assert_eq!(cur, blades_lib::economy::GEMS);

        let bare = json!({
            "currencyId": "470c8f58-a8dd-4c07-8c92-843b785e1139",
            "amount": 25
        });
        assert_eq!(parse_appearance_cost(&bare).unwrap().1, 25);

        // Missing / malformed → None (change stays free rather than erroring).
        assert!(parse_appearance_cost(&json!(null)).is_none());
        assert!(parse_appearance_cost(&json!({"amount": 50})).is_none());
    }

    #[test]
    fn customization_payload_deserializes() {
        // Real Theodor-Gorlash appearance-change shape (tagged-value blob). With the old
        // `customization: Option<()>` this FAILED to deserialize (→400) and the removed
        // `assert!` panicked; now it parses into Some(Value) so it can be persisted.
        let raw = r#"{"data":{"customization":{
            "Name":{"_t":"String","_v":"RmxhcHBldHk="},
            "Morphs":[{"Name":{"_t":"String","_v":"HeadMaleOld"},"Weight":{"_t":"Single","_v":0.0}}],
            "CharacterUID":{"id":{"_t":"String","_v":"81c01573-0000-0000-0000-000000000000"}}
        }}}"#;
        let req: DataUpdateRequest =
            serde_json::from_str(raw).expect("customization payload must deserialize");
        assert!(req.data.customization.is_some());
        assert!(req.data.dialog.is_none());
        assert!(req.data.new_flags.is_none());
    }

    #[test]
    fn dialog_and_new_flags_still_parse() {
        let raw = r#"{"data":{"dialog":{"x":1},"new-flags":{"y":2}}}"#;
        let req: DataUpdateRequest = serde_json::from_str(raw).unwrap();
        assert!(req.data.dialog.is_some());
        assert!(req.data.new_flags.is_some());
        assert!(req.data.customization.is_none());
    }
}
