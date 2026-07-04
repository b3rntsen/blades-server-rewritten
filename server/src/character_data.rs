use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{Data, Json, Path},
};
use diesel::{
    ExpressionMethods, QueryDsl, SelectableHelper,
    dsl::jsonb_set_create_if_missing,
    sql_types::{Array, Text},
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

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/data")]
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

            // I can’t figure how to do that in a single request. And I can’t put a for_update in an update statement...

            // lock the row fo update (that would have been avoidable if I could put everything in a single transaction)
            characters
                .filter(id.eq(character_id))
                .filter(user_id.eq(session.session.user_id))
                .for_update()
                .execute(&mut conn)
                .await
                .unwrap();

            if let Some(new_flags) = body.0.data.new_flags {
                let new_data_updated_row = diesel::update(characters)
                    .filter(id.eq(character_id))
                    .filter(user_id.eq(session.session.user_id))
                    .set(
                        data.eq(jsonb_set_create_if_missing::<_, Array<Text>, _, _, _, _>(
                            data,
                            vec!["new-flags"],
                            JsonDbWrapper(new_flags),
                            true,
                        )),
                    )
                    .execute(&mut conn)
                    .await?;

                if new_data_updated_row == 0 {
                    return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 1003, 2));
                }
            };

            if let Some(dialog) = body.0.data.dialog {
                let dialog_updated_row = diesel::update(characters)
                    .filter(id.eq(character_id))
                    .filter(user_id.eq(session.session.user_id))
                    .set(
                        data.eq(jsonb_set_create_if_missing::<_, Array<Text>, _, _, _, _>(
                            data,
                            vec!["dialog"],
                            JsonDbWrapper(dialog),
                            true,
                        )),
                    )
                    .execute(&mut conn)
                    .await?;

                if dialog_updated_row == 0 {
                    return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 1003, 2));
                }
            }

            // APPEARANCE / SEX / RACE change (the in-town "Theodor Gorlash" NPC).
            // Persist the customization blob verbatim into `data.customization` — sex and
            // race live in its `Morphs` + face/body presets, so this one write covers all
            // three. Previously discarded (Option<()> + assert) → the change never saved
            // and the client hung after the NPC (Viventus). [ground truth: SA3 capture]
            if let Some(customization) = body.0.data.customization {
                let cust_updated_row = diesel::update(characters)
                    .filter(id.eq(character_id))
                    .filter(user_id.eq(session.session.user_id))
                    .set(
                        data.eq(jsonb_set_create_if_missing::<_, Array<Text>, _, _, _, _>(
                            data,
                            vec!["customization"],
                            JsonDbWrapper(customization),
                            true,
                        )),
                    )
                    .execute(&mut conn)
                    .await?;
                if cust_updated_row == 0 {
                    return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 1003, 2));
                }

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
