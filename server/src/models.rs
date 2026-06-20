use blades_lib::server_state::ServerState;
use blades_lib::user_data::{
    B64EncodedData, CompleteCharacter, CompleteCharacterData, CompleteInventory, CompleteWallet,
    DungeonGeneratedData, DungeonState, Quest, UserAccount,
};
use diesel::prelude::*;
use serde_json::Value;
use uuid::Uuid;

use crate::{json_db::JsonDbWrapper, util::CharacterHolder};

#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = crate::schema::users)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct UserDBEntry {
    pub id: Uuid,
    /// The user id that is actually communicated with the client, and should be kept secret
    pub secret_id: Uuid,
    pub data: JsonDbWrapper<UserAccount>,
}

#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntry {
    pub id: Uuid,
    pub user_id: Uuid,
    pub character: JsonDbWrapper<CompleteCharacter>,
    pub data: JsonDbWrapper<CompleteCharacterData>,
    pub wallet: JsonDbWrapper<CompleteWallet>,
    pub inventory: JsonDbWrapper<CompleteInventory>,
    /// The character's own captured town (arbitrary JSON, served verbatim by
    /// `get_town`). `None` for fresh/un-transferred characters → the static
    /// `default_town.json` is served instead.
    pub town: Option<JsonDbWrapper<Value>>,
}

/// Thin select for `get_town`: the requesting character's `town` column plus the
/// `user_id` for the ownership check. Mirrors `CharacterDbEntryInventory`.
#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntryTown {
    pub user_id: Uuid,
    pub town: Option<JsonDbWrapper<Value>>,
}

impl CharacterHolder for CharacterDbEntryTown {
    fn get_user_id(&self) -> &Uuid {
        &self.user_id
    }
}

#[derive(Queryable, Selectable, AsChangeset)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntryCharacterWalletInventory {
    pub id: Uuid,
    pub user_id: Uuid,
    pub character: JsonDbWrapper<CompleteCharacter>,
    // The character's `data` (incl. `customization` → the avatar appearance /
    // CharacterUID). Needed in the op54 round-start PROFILE so the client can build
    // the OPPONENT's avatar visual; without it the client's resource-load hangs at
    // "connecting" (no-frida) / crashes (frida). [arena-journey-log §7]
    pub data: JsonDbWrapper<CompleteCharacterData>,
    pub wallet: JsonDbWrapper<CompleteWallet>,
    pub inventory: JsonDbWrapper<CompleteInventory>,
}

/// Workhorse for the town/RPG economy handlers (shops, crafts, chests, gifts, the
/// global store, challenges, …): the character + wallet + inventory plus the
/// server-managed `server_state`. `AsChangeset` so a handler loads it
/// `for_no_key_update`, mutates the JSONB in place, and writes it all back.
#[derive(Queryable, Selectable, AsChangeset)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntryEconomy {
    pub id: Uuid,
    pub user_id: Uuid,
    pub character: JsonDbWrapper<CompleteCharacter>,
    pub wallet: JsonDbWrapper<CompleteWallet>,
    pub inventory: JsonDbWrapper<CompleteInventory>,
    pub server_state: JsonDbWrapper<ServerState>,
}

impl CharacterHolder for CharacterDbEntryEconomy {
    fn get_user_id(&self) -> &Uuid {
        &self.user_id
    }
}

/// Read-only thin select for endpoints that only need the server-managed state
/// (e.g. the claimed-gifts list) plus the ownership check.
#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntryServerState {
    pub user_id: Uuid,
    pub server_state: JsonDbWrapper<ServerState>,
}

impl CharacterHolder for CharacterDbEntryServerState {
    fn get_user_id(&self) -> &Uuid {
        &self.user_id
    }
}

#[derive(Queryable, Selectable, AsChangeset)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntryCharacterAlone {
    pub id: Uuid,
    pub user_id: Uuid,
    pub character: JsonDbWrapper<CompleteCharacter>,
}

impl CharacterHolder for CharacterDbEntryCharacterAlone {
    fn get_user_id(&self) -> &Uuid {
        &self.user_id
    }
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntryCharacterAndData {
    pub id: Uuid,
    pub user_id: Uuid,
    pub character: JsonDbWrapper<CompleteCharacter>,
    pub data: JsonDbWrapper<CompleteCharacterData>,
}

impl CharacterHolder for CharacterDbEntryCharacterAndData {
    fn get_user_id(&self) -> &Uuid {
        &self.user_id
    }
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntryWallet {
    pub user_id: Uuid,
    pub character: JsonDbWrapper<CompleteCharacter>,
    pub wallet: JsonDbWrapper<CompleteWallet>,
}

impl CharacterHolder for CharacterDbEntryWallet {
    fn get_user_id(&self) -> &Uuid {
        &self.user_id
    }
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbEntryInventory {
    pub user_id: Uuid,
    pub inventory: JsonDbWrapper<CompleteInventory>,
}

impl CharacterHolder for CharacterDbEntryInventory {
    fn get_user_id(&self) -> &Uuid {
        &self.user_id
    }
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::schema::characters)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CharacterDbAlone {
    pub id: Uuid,
    pub user_id: Uuid,
}

impl CharacterHolder for CharacterDbAlone {
    fn get_user_id(&self) -> &Uuid {
        &self.user_id
    }
}

#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = crate::schema::quests)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct QuestDbEntry {
    pub id: Uuid,
    pub character_id: Uuid,
    pub info: JsonDbWrapper<Quest>,
    pub generated_data: JsonDbWrapper<Option<DungeonGeneratedData>>,
    pub dungeon_state: Option<JsonDbWrapper<DungeonState>>,
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::schema::quests)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct QuestDbEntryDungeonStateAndInitialState {
    pub id: Uuid,
    pub dungeon_state: Option<JsonDbWrapper<DungeonState>>,
    pub initial_state: Option<JsonDbWrapper<B64EncodedData>>,
}

#[derive(Queryable, Selectable, AsChangeset)]
#[diesel(table_name = crate::schema::quests)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct QuestDbEntryDungeonStateAndGeneratedData {
    pub id: Uuid,
    pub dungeon_state: Option<JsonDbWrapper<DungeonState>>,
    pub generated_data: JsonDbWrapper<Option<DungeonGeneratedData>>,
}

/// Thin changeset for `POST /quests/{id}/complete` and `POST /quests/{id}/objectives`:
/// only the `info` column (the `Quest` state) is updated.
#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::quests)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct QuestDbEntryInfo {
    pub info: JsonDbWrapper<Quest>,
}
