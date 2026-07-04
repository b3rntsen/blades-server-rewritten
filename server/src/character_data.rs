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
                // returning `null` risks the "stuck after" hang. Echo the current wallet +
                // versions (appearance touches no items → no version bump).
                // TODO(economy): debit the faithful appearance-change currency cost once
                // the APK cost table (appearance_change_cost.json) is wired — see the
                // building/economy pass. For now the change is free (no phantom debit).
                let row = characters
                    .filter(id.eq(character_id))
                    .filter(user_id.eq(session.session.user_id))
                    .select(CharacterDbEntryCharacterWalletInventory::as_select())
                    .first(&mut conn)
                    .await?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
