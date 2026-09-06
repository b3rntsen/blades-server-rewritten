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
use uuid::Uuid;

use crate::{BladeApiError, ServerGlobal, session::SessionLookedUpMaybe};

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
struct CharacterCreationResponse {
    character: CompleteCharacterWithIdAndData,
    inventory: CompleteInventory,
}

/// Build a brand-new character with retail's starter loadout.
///
/// Extracted from `create_characters` so the FTUE route and the auto-provision
/// path below cannot drift: a starter character made for a new player must be
/// byte-identical to one the real first-time flow would have produced.
/// The appearance a brand-new player starts with.
///
/// NOT `CompleteCharacterData::default()`, which is `customization: {}`. An empty
/// customization is the "stub character" shape that leaves the client on the
/// loading screen forever — the same failure this auto-provisioning exists to
/// prevent, so seeding an empty one would have swapped one hang for another.
///
/// This blob is a real 48-key appearance produced by retail's own FTUE (lifted
/// from a level-1 character that loads), so it is the shape the client expects
/// rather than one we invented. The embedded `Name` is base64 of the character
/// name and is rewritten per player below.
const STARTER_CUSTOMIZATION: &str = include_str!("../assets/starter_customization.json");

/// The name a new player gets before they rename themselves. Retail's own
/// default, and what every FTUE-created character in the capture set carries.
const STARTER_NAME: &str = "Adventurer";

/// `STARTER_CUSTOMIZATION` with the embedded name replaced by `name`.
fn starter_customization(name: &str) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_str(STARTER_CUSTOMIZATION)
        .expect("starter_customization.json is committed and valid");
    // customization.Name is `{"_t":"String","_v":"<base64>"}`; leaving the donor
    // name in would give every new player the same in-fiction name.
    if let Some(slot) = v.pointer_mut("/Name/_v") {
        *slot = serde_json::Value::String(b64(name.as_bytes()));
    }
    v
}

/// Standard base64, no padding omitted. Twelve lines rather than a new
/// dependency for one call site — every added crate is supply-chain surface.
fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for c in input.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Give `owner` a starter character if they have none.
///
/// WHY THIS EXISTS
///
/// A brand-new player installs the APK, logs in anonymously, asks for their
/// characters and gets `{"characters":[]}` — then sits on the loading screen
/// forever. The thing that would have created a character is the FTUE, and the
/// distributed APK has FTUE patched out so it can boot straight to our server.
/// So the shipped client can only play as a character that already exists,
/// which means the game was unplayable for anyone who had never played before.
///
/// Reproduced on a clean rig running the byte-identical distributed APK against
/// an unclaimed device: `auth/anon` 200, `sync` 200, `characters` 200 with a
/// 17-byte body, then nothing ever again.
///
/// Returns Ok(true) if it created one. Idempotent: a user who already has any
/// character is left completely alone, so this cannot touch a returning player.
pub async fn ensure_starter_character(
    app_state: &ServerGlobal,
    owner: Uuid,
) -> Result<bool, diesel::result::Error> {
    let mut conn = match app_state.db_pool.get().await {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    let existing: i64 = {
        use schema::characters::dsl::*;
        characters
            .filter(user_id.eq(owner))
            .count()
            .get_result(&mut conn)
            .await?
    };
    if existing > 0 {
        return Ok(false);
    }

    let (_uuid, to_insert, _inv) =
        build_new_character(owner, STARTER_NAME.to_string(), starter_customization(STARTER_NAME));

    // ON CONFLICT DO NOTHING is not enough on its own (the id is fresh every
    // call), so the count above is the guard. Two racing logins for the same
    // brand-new user could still both insert; that is a cosmetic duplicate on a
    // first login, not a lost character, and it is preferable to holding a
    // transaction open across the whole build.
    insert_into(characters::table)
        .values(&to_insert)
        .execute(&mut conn)
        .await?;
    Ok(true)
}

use blades_lib::user_data::WalletEntry;

/// Gems a brand-new character starts with.
///
/// Exactly the cost of one appearance change (`appearance_change_cost.json`,
/// `characterCustomizationCost.amount`, taken from the APK's `UpdateCostData`).
///
/// Retail never needed this: the player built their character during creation,
/// for free. We hand out a preset instead, so a new player who wants a different
/// face has to pay the town NPC's 50 gems and starts with none. This buys back
/// the one change retail gave away, and nothing more.
///
/// NOT derived from retail's own starting wallet -- the capture corpus contains
/// wallet DELTAS rather than a creation snapshot, so there is no observation of
/// what a brand-new retail character held. This is the customization cost, which
/// is authoritative, applied for a stated reason.
const STARTER_GEMS: u64 = 50;

/// The gem currency, as used by `appearance_change_cost.json` and the shop.
const GEM_CURRENCY_ID: &str = "470c8f58-a8dd-4c07-8c92-843b785e1139";

/// The wallet a new character is created with.
fn starter_wallet() -> CompleteWallet {
    let mut wallet = CompleteWallet::default();
    match Uuid::from_str(GEM_CURRENCY_ID) {
        Ok(gem) => {
            wallet.0.insert(gem, WalletEntry { balance: STARTER_GEMS });
        }
        // A malformed constant must not stop a player being created; they simply
        // start with an empty wallet, which is where they were before.
        Err(e) => log::error!("[character] GEM_CURRENCY_ID is not a uuid: {e}"),
    }
    wallet
}

fn build_new_character(
    owner: Uuid,
    name: String,
    customization: serde_json::Value,
) -> (Uuid, CharacterDbEntry, CompleteInventory) {
    //TODO: make sure the user name, or at least the tag id, is unique. Good luck getting it to work with the current (lack of) transaction model. An extra unique key in the table?
    let mut new_character = CompleteCharacter::default();
    new_character.name = name;

    let mut new_data = CompleteCharacterData::default();
    new_data.customization = customization;

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
        wallet: JsonDbWrapper(starter_wallet()),
        inventory: JsonDbWrapper(inventory.clone()),
        // Fresh character → no captured town; get_town serves default_town.json.
        town: None,
    };
    (character_uuid, to_insert, inventory)
}

#[post("/blades.bgs.services/api/game/v1/public/characters")]
async fn create_characters(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<CharacterCreationRequest>,
) -> Result<web::Json<CharacterCreationResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let (character_uuid, to_insert, inventory) = build_new_character(
        session.session.user_id,
        body.name.clone(),
        body.0.data.customization.clone(),
    );


    let mut conn = app_state.db_pool.get().await.unwrap();
    //TODO: convert error
    //TODO: explicit no async commit (start a new transaction)
    insert_into(characters::table)
        .values(&to_insert)
        .execute(&mut conn)
        .await
        .unwrap();

    Ok(web::Json(CharacterCreationResponse {
        character: CompleteCharacterWithIdAndData {
            id: character_uuid,
            character: to_insert.character.0,
            data: to_insert.data.0,
        },
        inventory,
    }))
}


#[cfg(test)]
mod starter_tests {
    use super::*;

    /// The whole point of the embedded blob: `CompleteCharacterData::default()`
    /// is `customization: {}`, and an empty customization is the stub-character
    /// shape that leaves the client on the loading screen — the exact failure
    /// auto-provisioning exists to remove. Seeding an empty one would have
    /// swapped one hang for another, silently.
    #[test]
    fn starter_customization_is_not_the_empty_stub() {
        let c = starter_customization("Adventurer");
        let obj = c.as_object().expect("customization must be an object");
        assert!(
            obj.len() >= 40,
            "expected a real ~48-key appearance, got {} keys — an empty or thin \
             customization is the stub-character shape that hangs the client",
            obj.len()
        );
        assert!(obj.contains_key("CharacterUID"), "race/gender live in CharacterUID");
    }

    /// The donor blob carries the donor's name, base64 in `Name._v`. Shipping it
    /// unchanged would give every new player the same in-fiction name.
    #[test]
    fn starter_customization_carries_the_requested_name() {
        let c = starter_customization("Adventurer");
        let got = c.pointer("/Name/_v").and_then(|v| v.as_str()).expect("Name/_v");
        assert_eq!(got, "QWR2ZW50dXJlcg==", "Name must be base64 of the new name");
        let other = starter_customization("Bob");
        assert_ne!(
            other.pointer("/Name/_v"),
            c.pointer("/Name/_v"),
            "two different names must not produce the same embedded name"
        );
    }

    #[test]
    fn b64_matches_known_vectors() {
        assert_eq!(b64(b"Adventurer"), "QWR2ZW50dXJlcg==");
        assert_eq!(b64(b"StormLord"), "U3Rvcm1Mb3Jk");
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64(b"ab"), "YWI=");
        assert_eq!(b64(b"abc"), "YWJj");
    }

    /// A starter character must be a playable one: the auto-provision path and
    /// the FTUE route go through the same builder precisely so this holds.
    #[test]
    fn a_starter_character_has_starter_gear_and_the_right_owner() {
        let owner = Uuid::new_v4();
        let (id, entry, inv) =
            build_new_character(owner, STARTER_NAME.to_string(), starter_customization(STARTER_NAME));
        assert_eq!(entry.user_id, owner, "character must belong to the caller");
        assert_eq!(entry.id, id);
        assert_eq!(entry.character.0.name, STARTER_NAME);
        assert!(
            !inv.loadout.equipped_items.0.is_empty(),
            "a starter character with no equipped items cannot fight in the arena"
        );
        assert!(entry.town.is_none(), "fresh character serves the default town");
    }
}


#[cfg(test)]
mod starter_gems {
    use super::*;

    /// The grant must equal the ACTUAL appearance-change cost, read from the
    /// shipped data rather than repeated as a literal.
    ///
    /// The whole justification for the grant is "exactly one customization". If
    /// someone retunes `appearance_change_cost.json` and this stays at 50, the
    /// grant silently stops meaning that -- either stranding new players again or
    /// handing out more than intended. This is the test that notices.
    #[test]
    fn the_grant_is_exactly_one_appearance_change() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/appearance_change_cost.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid cost file");

        let cost = doc["characterCustomizationCost"]["amount"]
            .as_u64()
            .expect("the cost file must state an amount");
        let currency = doc["characterCustomizationCost"]["currencyId"]
            .as_str()
            .expect("the cost file must state a currency");

        assert_eq!(
            STARTER_GEMS, cost,
            "a new character is given {STARTER_GEMS} but one appearance change costs {cost}"
        );
        assert_eq!(
            GEM_CURRENCY_ID, currency,
            "we grant a different currency than the one the change is priced in"
        );
    }

    /// And the wallet actually carries it.
    #[test]
    fn a_new_character_can_afford_one_change() {
        let wallet = starter_wallet();
        let gem = Uuid::from_str(GEM_CURRENCY_ID).unwrap();
        let balance = wallet.0.get(&gem).map(|e| e.balance).unwrap_or(0);
        assert_eq!(balance, STARTER_GEMS, "the starter wallet must hold the gems");

        // Control: nothing else is handed out. A starter wallet quietly seeded
        // with gold would be a much bigger change than the one asked for.
        assert_eq!(wallet.0.len(), 1, "only gems, got {:?}", wallet.0.keys().collect::<Vec<_>>());
    }
}
