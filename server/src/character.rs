use std::{collections::HashMap, str::FromStr, sync::Arc};

use crate::{
    json_db::JsonDbWrapper,
    models::{CharacterDbEntry, CharacterDbEntryCharacterAndData},
    schema::{self, characters},
    util::get_only_single_character_and_check_permission,
};
use actix_web::{
    get, post,
    web::{self, Json},
};
use blades_lib::user_data::{
    Backpack, CompleteCharacter, CompleteCharacterData, CompleteCharacterWithIdAndData,
    CompleteInventory, CompleteWallet, EquippedItems, Item, ItemPropertiesAll, Loadout,
    SingleEquippedItem, Treasury,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper, insert_into};
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, character_data::parse_appearance_cost,
    session::SessionLookedUpMaybe,
};

#[derive(Serialize)]
struct CharacterListResponse {
    characters: Vec<CompleteCharacterWithIdAndData>,
}

#[get("/blades.bgs.services/api/game/v1/public/characters")]
async fn list_characters(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<web::Json<CharacterListResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let mut conn = app_state.db_pool.get().await.unwrap();
    let query_result = {
        use schema::characters::dsl::*;
        characters
            .filter(user_id.eq(session.session.user_id))
            .select(CharacterDbEntryCharacterAndData::as_select())
            .load(&mut conn)
            .await
            .unwrap()
    };

    let mut result = Vec::with_capacity(query_result.len());
    for character in query_result.iter() {
        result.push(CompleteCharacterWithIdAndData {
            id: character.id,
            character: character.character.0.clone(),
            data: character.data.0.clone(),
        });
    }
    Ok(web::Json(CharacterListResponse { characters: result }))
}

#[derive(Serialize)]
struct CompleteCharacterWithIdAndDataContainer {
    character: CompleteCharacterWithIdAndData,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}")]
async fn get_character(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<CompleteCharacterWithIdAndDataContainer>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    let character_entries = {
        use schema::characters::dsl::*;
        characters
            .filter(id.eq(character_id))
            .select(CharacterDbEntryCharacterAndData::as_select())
            .load(&mut conn)
            .await
            .unwrap()
    };

    let character =
        get_only_single_character_and_check_permission(character_entries, &session.session)?;

    Ok(Json(CompleteCharacterWithIdAndDataContainer {
        character: CompleteCharacterWithIdAndData {
            id: character_id,
            character: character.character.0.clone(),
            data: character.data.0.clone(),
        },
    }))
}

#[derive(Deserialize)]
struct DataOnlyCustomization {
    customization: serde_json::Value,
}

#[derive(Deserialize)]
struct CharacterCreationRequest {
    name: String,
    data: DataOnlyCustomization,
}

#[derive(Serialize)]
pub(crate) struct CharacterCreationResponse {
    character: CompleteCharacterWithIdAndData,
    inventory: CompleteInventory,
}

#[post("/blades.bgs.services/api/game/v1/public/characters")]
async fn create_characters(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<CharacterCreationRequest>,
) -> Result<web::Json<CharacterCreationResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let created = build_starter_character(
        &app_state,
        session.session.user_id,
        body.name.clone(),
        body.0.data.customization,
    )
    .await
    .map_err(|e| {
        log::error!("character creation failed: {e}");
        BladeApiError::new(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, 3, 0)
    })?;
    Ok(web::Json(created))
}

/// Build and persist one fresh character with retail's starter loadout.
///
/// Extracted from the POST route so the anonymous-login path can call it too.
/// Our shipped APK has the FTUE patched out, so it never POSTs here: it asks
/// for its characters, and if the list comes back empty it sits on the loading
/// screen forever with every request answered 200. See
/// [`ensure_starter_character`].
pub(crate) async fn build_starter_character(
    app_state: &ServerGlobal,
    owner: Uuid,
    name: String,
    customization: serde_json::Value,
) -> Result<CharacterCreationResponse, diesel::result::Error> {
    let mut new_character = CompleteCharacter::default();
    new_character.name = name;

    let mut new_data = CompleteCharacterData::default();
    new_data.customization = customization;
    make_bootstrap_data_loadable(&mut new_data, &new_character.name);

    let character_uuid = Uuid::new_v4();

    let mut equipped_items = HashMap::new();
    let item1_slot_uuid = Uuid::from_str("417e79de-c810-42f8-8273-f9759df6ae25").unwrap();
    equipped_items.insert(
        item1_slot_uuid,
        SingleEquippedItem {
            id: Uuid::new_v4(),
            slot: item1_slot_uuid,
            item: Item {
                item_template_id: Uuid::from_str("606c8bf6-9dc7-4c5f-b44b-36eb02306c96").unwrap(),
                durability: 75.0,
                tempering_level: 0,
                properties: ItemPropertiesAll::default(),
                // Starter gear: retail's own starter loadout carries neither key, and
                // both are omitted-when-absent on the wire (see `Item`).
                grade: None,
                arcane_tier: None,
            },
        },
    );

    let item2_slot_uuid = Uuid::from_str("862605de-c67f-4bce-b527-4e5fb6f25162").unwrap();
    equipped_items.insert(
        item2_slot_uuid,
        SingleEquippedItem {
            id: Uuid::new_v4(),
            slot: item2_slot_uuid,
            item: Item {
                item_template_id: Uuid::from_str("c6f7fab4-eadc-4e8c-bf7f-e0ea095a3acf").unwrap(),
                tempering_level: 0,
                durability: 100.0,
                properties: ItemPropertiesAll::default(),
                // Starter gear: retail's own starter loadout carries neither key, and
                // both are omitted-when-absent on the wire (see `Item`).
                grade: None,
                arcane_tier: None,
            },
        },
    );

    let item3_slot_uuid = Uuid::from_str("897a600c-91d6-4449-af09-173da88a907e").unwrap();
    equipped_items.insert(
        item3_slot_uuid,
        SingleEquippedItem {
            id: Uuid::new_v4(),
            slot: item3_slot_uuid,
            item: Item {
                item_template_id: Uuid::from_str("42b6fad8-5ac9-4215-aeff-133715c4c22e").unwrap(),
                durability: 0.0,
                tempering_level: 0,
                properties: ItemPropertiesAll::default(),
                // Starter gear: retail's own starter loadout carries neither key, and
                // both are omitted-when-absent on the wire (see `Item`).
                grade: None,
                arcane_tier: None,
            },
        },
    );

    let item4_slot_uuid = Uuid::from_str("e273a4d7-fb87-4f7e-8f1e-398be59afbcb").unwrap();
    equipped_items.insert(
        item4_slot_uuid,
        SingleEquippedItem {
            id: Uuid::new_v4(),
            slot: item4_slot_uuid,
            item: Item {
                item_template_id: Uuid::from_str("2571f818-6ae4-4355-b89a-4a6253089e6c").unwrap(),
                tempering_level: 0,
                durability: 0.0,
                properties: ItemPropertiesAll::default(),
                // Starter gear: retail's own starter loadout carries neither key, and
                // both are omitted-when-absent on the wire (see `Item`).
                grade: None,
                arcane_tier: None,
            },
        },
    );

    let inventory = CompleteInventory {
        backpack: Backpack::default(),
        loadout: Loadout {
            equipped_items: EquippedItems(equipped_items),
            equipped_consumables: Vec::new(),
        },
        treasury: Treasury::default(),
        overflow_treasury: Treasury::default(),
        backpack_version: 1,
        treasury_version: 0,
    };

    let to_insert = CharacterDbEntry {
        id: character_uuid,
        user_id: owner,
        character: JsonDbWrapper(new_character),
        data: JsonDbWrapper(new_data),
        wallet: JsonDbWrapper(starter_wallet(&app_state.appearance_change_cost)),
        inventory: JsonDbWrapper(inventory.clone()),
        // Fresh character → no captured town; get_town serves default_town.json.
        town: None,
    };

    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| diesel::result::Error::BrokenTransactionManager)?;
    insert_into(characters::table)
        .values(&to_insert)
        .execute(&mut conn)
        .await?;

    // Best-effort: the allowance is already in the wallet above, and a character
    // can only be created once, so a missing ledger table must not fail the
    // creation. It records WHAT was granted for the next economy question.
    if let Some((currency, amount)) = parse_appearance_cost(&app_state.appearance_change_cost) {
        let _ = record_creation_allowance(&mut conn, character_uuid, currency, amount, "creation")
            .await;
    }

    Ok(CharacterCreationResponse {
        character: CompleteCharacterWithIdAndData {
            id: character_uuid,
            character: to_insert.character.0,
            data: to_insert.data.0,
        },
        inventory,
    })
}

/// Pay the creation allowance to a character that predates it (#193).
///
/// Returns whether anything was granted. Every failure path is "no" and logs
/// nothing louder than a debug line: this runs inside a login, and a login must
/// not break because an economy nicety could not be applied. In particular, if
/// the ledger table has not been created yet the insert errors and the grant is
/// SKIPPED — never repeated, which is the failure that would actually cost
/// something.
async fn backfill_creation_allowance(
    app_state: &ServerGlobal,
    conn: &mut diesel_async::AsyncPgConnection,
    existing: &CharacterDbEntryCharacterAndData,
) -> bool {
    let Some((currency, amount)) = parse_appearance_cost(&app_state.appearance_change_cost) else {
        return false;
    };
    let wallet: Option<JsonDbWrapper<CompleteWallet>> = {
        use crate::schema::characters::dsl as ch;
        ch::characters
            .filter(ch::id.eq(existing.id))
            .select(ch::wallet)
            .first(conn)
            .await
            .ok()
    };
    let Some(mut wallet) = wallet else {
        return false;
    };
    if !owes_creation_allowance(
        customization_visual(&existing.data.0),
        existing.character.0.level as i64,
        wallet.0.balance(currency),
        amount,
    ) {
        return false;
    }
    match record_creation_allowance(conn, existing.id, currency, amount, "backfill").await {
        Ok(true) => {}
        // Already paid, or the ledger is not there yet. Either way: do not pay.
        Ok(false) => return false,
        Err(e) => {
            log::debug!("creation allowance ledger unavailable for {}: {e}", existing.id);
            return false;
        }
    }
    wallet.0.credit(currency, amount);
    use crate::schema::characters::dsl as ch;
    if let Err(e) = diesel::update(ch::characters.filter(ch::id.eq(existing.id)))
        .set(ch::wallet.eq(wallet))
        .execute(conn)
        .await
    {
        log::warn!("creation allowance credited in ledger but not wallet for {}: {e}", existing.id);
        return false;
    }
    log::info!(
        "granted the character-creation allowance ({amount} of {currency}) to {}",
        existing.id
    );
    true
}

/// The wallet a server-provisioned character is born with: the one-time
/// character-creation allowance, and nothing else (#193).
///
/// In retail you chose race, sex and name during the FTUE, free, before any cost
/// existed. Our FTUE is patched out by design and this starter character is the
/// compensation — but it is a male Argonian called "Adventurer", and the only way
/// to change any of that is the town appearance NPC, which the APK prices at 50
/// Gems. A fresh character has none, so the one free choice retail gave everybody
/// is unreachable here: 30 of the 73 characters on the box still wearing the
/// default Argonian visual cannot afford it.
///
/// The amount is read from the same cost table the debit reads, so the player is
/// left with exactly nothing spare after making the choice and the two numbers
/// cannot drift. Retail's own level-1 wallets carried 0 Gems (measured: four
/// captured level-1 characters, 0 Gems each, 4-10 Gold), which is why this is an
/// allowance for one specific action rather than a starting balance.
fn starter_wallet(appearance_cost: &Value) -> CompleteWallet {
    let mut wallet = CompleteWallet::default();
    if let Some((currency, amount)) = parse_appearance_cost(appearance_cost) {
        wallet.credit(currency, amount);
    }
    wallet
}

/// Does this character still owe its creation allowance?
///
/// Pure so the rule is testable without a database, because the rule is the
/// whole risk: too loose and it pays 50 Gems per relaunch, too tight and the
/// player it exists for never gets it.
///
/// `visual` is `data.customization.CharacterUID.id._v` — the only field that says
/// "this player has never chosen". A character that has been through the
/// appearance NPC wears something else and is not owed anything.
fn owes_creation_allowance(visual: Option<&str>, level: i64, balance: u64, cost: u64) -> bool {
    visual == Some(STARTER_VISUAL) && level <= 1 && balance < cost
}

/// The male-Argonian visual every server-provisioned character is born wearing
/// (`Visual_Player_MaleArgonians…`), from `assets/starter_customization.json`.
const STARTER_VISUAL: &str = "9c2cc2b3-804c-4e97-8ad5-56371690bdf5";

/// `data.customization.CharacterUID.id._v`, the race/sex visual.
fn customization_visual(data: &CompleteCharacterData) -> Option<&str> {
    data.customization
        .get("CharacterUID")?
        .get("id")?
        .get("_v")?
        .as_str()
}

/// Write the ledger row, returning whether THIS call created it.
///
/// `ON CONFLICT DO NOTHING` is the guard: two logins racing each other cannot
/// both win the same character, so the caller credits only when this returns
/// `Ok(true)`. A missing table is an error, which the caller treats as "not
/// granted" — the grant is skipped entirely rather than repeated.
async fn record_creation_allowance(
    conn: &mut diesel_async::AsyncPgConnection,
    character: Uuid,
    currency: Uuid,
    amount: u64,
    reason: &str,
) -> Result<bool, diesel::result::Error> {
    use crate::schema::character_creation_allowance::dsl as led;
    let inserted = insert_into(led::character_creation_allowance)
        .values((
            led::character_id.eq(character),
            led::currency_id.eq(currency),
            led::amount.eq(amount as i64),
            led::reason.eq(reason.to_string()),
        ))
        .on_conflict(led::character_id)
        .do_nothing()
        .execute(conn)
        .await?;
    Ok(inserted > 0)
}

/// Give a brand-new player a character if they have none.
///
/// WHY THIS EXISTS. Our shipped APK has the first-time-user experience patched
/// out, so it never runs character creation. It logs in anonymously, asks for
/// its characters, and on an empty list sits on the loading screen forever —
/// with every single request answered 200, which is what makes it so hard to
/// report. That is exactly what reports #155/#158 are: 40 login cycles from one
/// device, each one `auth/anon` 200 → `sync` 200 → `characters` 200 with a
/// 17-byte empty list, and never a request after it.
///
/// This was here before, and commit bb0ecc0 (upstream, 2026-09-04) removed it.
/// That removal is right for a client that still has its FTUE and wrong for
/// ours, and it left 118 of 282 accounts on prod with no character at all.
///
/// Best-effort on purpose: a failure here must never break the login itself,
/// so the caller logs and carries on. `Ok(false)` means the player already had
/// a character and nothing was done.
pub(crate) async fn ensure_starter_character(
    app_state: &ServerGlobal,
    owner: Uuid,
) -> Result<bool, diesel::result::Error> {
    let mut conn = match app_state.db_pool.get().await {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    let existing: Option<CharacterDbEntryCharacterAndData> = {
        use crate::schema::characters::dsl::*;
        characters
            .filter(user_id.eq(owner))
            .select(CharacterDbEntryCharacterAndData::as_select())
            .load(&mut conn)
            .await?
            .into_iter()
            .next()
    };
    if let Some(mut existing) = existing {
        // Characters that predate the creation allowance (#193) are stranded as a
        // male Argonian they cannot afford to change. Pay it once, here, on the
        // login they were already making. Separate from the bootstrap repair
        // below because a character can need either, both, or neither.
        let backfilled = backfill_creation_allowance(app_state, &mut conn, &existing).await;

        if !make_bootstrap_data_loadable(&mut existing.data.0, &existing.character.0.name) {
            return Ok(backfilled);
        }
        use crate::schema::characters::dsl::*;
        diesel::update(characters.filter(id.eq(existing.id)))
            .set(data.eq(existing.data))
            .execute(&mut conn)
            .await?;
        log::info!(
            "repaired unloadable bootstrap data for starter character {}",
            existing.id
        );
        return Ok(true);
    }
    drop(conn);

    build_starter_character(
        app_state,
        owner,
        STARTER_NAME.to_string(),
        serde_json::Value::Object(serde_json::Map::new()),
    )
    .await?;
    Ok(true)
}

/// The name a server-provisioned starter character is given. Players rename in
/// game; this only has to be recognisable as "we made this for you".
pub(crate) const STARTER_NAME: &str = "Adventurer";

/// Fill the three player-data blocks that the retail client dereferences while
/// constructing the town scene.
///
/// Report #191 supplied the cleanest production control: a fresh no-VPN client
/// received a server-created character with all three blocks empty, completed
/// every HTTP bootstrap request with status 200, and then stopped forever on the
/// loading spinner. A healthy character loading minutes later had the same ten
/// misc-flag keys as retail plus a 48-key customization block. This is the same
/// invariant already enforced by the capture-platform importer; server-created
/// characters must not be the one path that still emits an unloadable model.
///
/// Return true when any repair was made. This lets `ensure_starter_character`
/// heal the already-stranded characters on their next anonymous login as well
/// as making new characters valid from the start.
fn make_bootstrap_data_loadable(data: &mut CompleteCharacterData, name: &str) -> bool {
    let mut changed = false;
    if object_is_missing_or_empty(&data.customization) {
        data.customization = default_starter_customization(name);
        changed = true;
    }
    if object_is_missing_or_empty(&data.new_flags) {
        data.new_flags = default_starter_new_flags();
        changed = true;
    }
    if !dialog_is_loadable(&data.dialog) {
        data.dialog = json!({ "Flags": [] });
        changed = true;
    }
    changed
}

fn object_is_missing_or_empty(value: &Value) -> bool {
    match value.as_object() {
        Some(object) => object.is_empty(),
        None => true,
    }
}

fn dialog_is_loadable(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("Flags"))
        .is_some_and(Value::is_array)
}

/// A captured, known-loadable 48-key appearance template. The source identity
/// is always replaced before the value is returned; only its coherent model,
/// presets, morphs and tints are reused.
fn default_starter_customization(name: &str) -> Value {
    let mut customization: Value =
        serde_json::from_str(include_str!("../assets/starter_customization.json"))
            .expect("bundled starter customization must be valid JSON");
    let object = customization
        .as_object_mut()
        .expect("bundled starter customization must be an object");
    object.insert(
        "Name".to_string(),
        json!({ "_t": "String", "_v": base64_utf8(name) }),
    );
    object.insert("TagId".to_string(), json!({ "_t": "String", "_v": "" }));
    customization
}

/// Exact ten-key block the client itself produced for a fresh character
/// (capture 331319), with a distinct analytics id for each new character.
fn default_starter_new_flags() -> Value {
    json!({
        "FulfillmentPurchases": [],
        "NewLastChanceOffers": {},
        "GuildData": {
            "GuildsRemovedFrom": {},
            "ReceivedGuildApplicationSeen": {},
            "SentGuildApplication": {}
        },
        "EmoteLoadout": { "Version": { "_t": "Int32", "_v": 1 }, "Slots": {} },
        "QuestStatusData": { "ActiveQuests": {}, "UnlockableQuests": {} },
        "IsEulaShown": { "_t": "Boolean", "_v": false },
        "SessionCount": { "_t": "Int32", "_v": 1 },
        "LootAlgorithmVersion": { "_t": "Int32", "_v": 4 },
        "DebriefingNPCId": { "id": { "_t": "String", "_v": "" } },
        "AnalyticsId": { "_t": "String", "_v": Uuid::new_v4().to_string() }
    })
}

/// The customization name uses the game's `NameVersion: base64` convention.
/// Keep this tiny encoder local rather than adding a dependency for one field.
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

#[cfg(test)]
mod starter_character_tests {
    use super::*;

    #[test]
    fn generated_player_data_is_loadable_and_retail_shaped() {
        let mut data = CompleteCharacterData::default();
        assert!(make_bootstrap_data_loadable(&mut data, STARTER_NAME));

        let customization = data.customization.as_object().unwrap();
        assert_eq!(customization.len(), 48);
        assert_eq!(
            customization["Name"],
            json!({ "_t": "String", "_v": "QWR2ZW50dXJlcg==" })
        );
        assert_eq!(
            customization["NameVersion"],
            json!({ "_t": "String", "_v": "base64" })
        );
        assert_eq!(
            customization["CharacterUID"]["id"]["_v"],
            "9c2cc2b3-804c-4e97-8ad5-56371690bdf5"
        );

        assert_eq!(data.new_flags.as_object().unwrap().len(), 10);
        assert_eq!(
            data.new_flags["LootAlgorithmVersion"],
            json!({ "_t": "Int32", "_v": 4 })
        );
        assert_eq!(data.dialog, json!({ "Flags": [] }));
    }

    #[test]
    fn bootstrap_repair_preserves_good_captured_values() {
        let mut data = CompleteCharacterData {
            customization: json!({ "real": "appearance" }),
            new_flags: json!({ "real": "flags" }),
            dialog: json!({ "Flags": ["met_blacksmith"] }),
        };
        let original = format!("{data:?}");
        assert!(!make_bootstrap_data_loadable(&mut data, "Keep Me"));
        assert_eq!(format!("{data:?}"), original);
    }

    #[test]
    fn base64_name_encoding_handles_utf8_and_padding() {
        assert_eq!(base64_utf8(""), "");
        assert_eq!(base64_utf8("A"), "QQ==");
        assert_eq!(base64_utf8("Alt"), "QWx0");
        assert_eq!(base64_utf8("Søvngård"), "U8O4dm5nw6VyZA==");
    }

    /// Every exit from `anon_log_in` must provision a starter character.
    ///
    /// The APK we ship has the FTUE patched out, so a player with no character
    /// is never offered creation: the client asks for its characters, gets an
    /// empty list, and sits on the loading screen forever with every request
    /// answered 200. Reports #155/#158 are 40 such cycles from one device.
    ///
    /// This is a source assertion rather than a handler test because the
    /// handler needs a database and a session, and the bug is precisely that
    /// ONE path forgets the call. Counting them is what a compiler cannot do:
    /// the previous version had it on two of the five, and adding a sixth exit
    /// without the call is exactly how this regresses.
    #[test]
    fn every_anon_login_exit_provisions_a_character() {
        let src = include_str!("authentification.rs");
        let start = src
            .find("async fn anon_log_in")
            .expect("anon_log_in must exist");
        let end = src[start..]
            .find("\n#[cfg(test)]")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let body = &src[start..end];

        let exits = body
            .matches("return Ok(web::Json(SessionResponse {")
            .count();
        let guards = body.matches("ensure_starter_character(&app_state").count();
        assert!(exits > 0, "the function must still return a session");
        assert_eq!(
            guards, exits,
            "every one of the {exits} anon-login exits must call \
             ensure_starter_character; {guards} do",
        );
    }

    /// The removal this restores was upstream commit bb0ecc0 (2026-09-04). It is
    /// correct for a client that still has its FTUE and wrong for ours, and it
    /// left 118 of 282 accounts on prod with no character. Pin the reason so a
    /// future upstream merge does not quietly drop it again.
    #[test]
    fn the_starter_name_is_recognisable() {
        assert_eq!(super::STARTER_NAME, "Adventurer");
    }
}

#[cfg(test)]
mod creation_route_tests {
    /// PR #212 shortened only the POST route while the adjacent GET routes and
    /// every retail request retain the virtual-host prefix. Actix registers the
    /// shorter path successfully, so compilation cannot catch the resulting 404.
    #[test]
    fn character_creation_uses_the_retail_virtual_host_path() {
        let src = include_str!("character.rs");
        assert!(
            src.contains(
                "#[post(\"/blades.bgs.services/api/game/v1/public/characters\")]\nasync fn create_characters",
            ),
            "character creation must be registered at the path the retail client calls"
        );
    }
}

#[cfg(test)]
mod report193_creation_allowance_tests {
    use super::*;

    /// The shipped cost table: Gem 50, from the APK's own `UpdateCostData`.
    fn shipped_cost() -> Value {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/appearance_change_cost.json");
        serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}")))
            .expect("appearance_change_cost.json parses")
    }

    /// THE BUG (#193). A server-provisioned character is a male Argonian called
    /// "Adventurer", and the appearance NPC — the only way to change race, sex or
    /// name — costs 50 Gems the character does not have. Retail gave that choice
    /// away free in the FTUE we patch out, so here it was simply unreachable.
    #[test]
    fn a_new_character_can_afford_the_choice_retail_gave_for_free() {
        let cost = shipped_cost();
        let (currency, amount) = parse_appearance_cost(&cost).expect("the shipped table parses");
        assert_eq!(amount, 50, "the APK's price for a customization change");
        assert_eq!(currency, blades_lib::economy::GEMS);

        let wallet = starter_wallet(&cost);
        assert_eq!(wallet.balance(currency), amount);
    }

    /// CONTROL: and nothing more than the choice. Retail's own level-1 wallets
    /// carried 0 Gems — four captured level-1 characters, 0 each, 4–10 Gold — so
    /// this is an allowance for one action, not a starting balance. After using
    /// it the player is back to nothing, exactly as a retail player was.
    #[test]
    fn and_is_left_with_nothing_spare() {
        let cost = shipped_cost();
        let (currency, amount) = parse_appearance_cost(&cost).unwrap();
        let mut wallet = starter_wallet(&cost);
        wallet.debit(currency, amount).expect("the change is affordable");
        assert_eq!(wallet.balance(currency), 0);
        assert_eq!(wallet.balance(blades_lib::economy::GOLD), 0, "no gold either");
    }

    /// A broken or absent cost table must not mint currency.
    #[test]
    fn a_missing_cost_table_grants_nothing() {
        assert_eq!(starter_wallet(&json!(null)).balance(blades_lib::economy::GEMS), 0);
        assert_eq!(
            starter_wallet(&json!({"characterCustomizationCost": {"amount": 50}}))
                .balance(blades_lib::economy::GEMS),
            0
        );
    }

    /// The backfill rule, which is the whole risk: too loose and it pays 50 Gems
    /// every relaunch, too tight and the 30 players it exists for never get it.
    #[test]
    fn the_backfill_rule_picks_the_stranded_and_nobody_else() {
        // Stranded: still the default Argonian, level 1, cannot afford it.
        assert!(owes_creation_allowance(Some(STARTER_VISUAL), 1, 0, 50));
        assert!(owes_creation_allowance(Some(STARTER_VISUAL), 1, 49, 50));

        // Has already chosen — a different visual is the proof, and the one
        // signal a player cannot fake without having made the choice.
        assert!(!owes_creation_allowance(
            Some("46b0d965-00be-478d-914a-996ff8f3e5a0"),
            1,
            0,
            50
        ));
        // Can already afford it: 43 of the 73 default-Argonian characters can,
        // and paying them would be a gem grant rather than an allowance.
        assert!(!owes_creation_allowance(Some(STARTER_VISUAL), 1, 50, 50));
        assert!(!owes_creation_allowance(Some(STARTER_VISUAL), 1, 24_790, 50));
        // A progressed character is not a fresh one, whatever it is wearing.
        assert!(!owes_creation_allowance(Some(STARTER_VISUAL), 48, 0, 50));
        // No customization at all → not eligible; the bootstrap repair runs first.
        assert!(!owes_creation_allowance(None, 1, 0, 50));
    }

    /// The visual is read from the same place the client writes it, and the
    /// constant must match the bundled starter asset — if the asset changes and
    /// this does not, the backfill silently stops finding anybody.
    #[test]
    fn the_starter_visual_matches_the_bundled_asset() {
        let data = default_starter_customization("Adventurer");
        assert_eq!(
            data.get("CharacterUID")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.get("_v"))
                .and_then(Value::as_str),
            Some(STARTER_VISUAL),
        );
        let mut complete = CompleteCharacterData::default();
        complete.customization = data;
        assert_eq!(customization_visual(&complete), Some(STARTER_VISUAL));
    }
}
