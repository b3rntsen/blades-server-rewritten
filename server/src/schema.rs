// @generated automatically by Diesel CLI.

diesel::table! {
    characters (id) {
        id -> Uuid,
        user_id -> Uuid,
        character -> Jsonb,
        data -> Jsonb,
        inventory -> Jsonb,
        wallet -> Jsonb,
        town -> Nullable<Jsonb>,
        server_state -> Jsonb,
    }
}

diesel::table! {
    quests (id, character_id) {
        id -> Uuid,
        character_id -> Uuid,
        info -> Jsonb,
        generated_data -> Jsonb,
        dungeon_state -> Nullable<Jsonb>,
        initial_state -> Nullable<Jsonb>,
    }
}

diesel::table! {
    users (id) {
        id -> Uuid,
        secret_id -> Uuid,
        data -> Jsonb,
    }
}

diesel::table! {
    guilds (id) {
        id -> Text,
        name -> Text,
        tag_id -> Text,
        guild_type -> Text,
        short_description -> Text,
        long_description -> Text,
        badge_icon_index -> Int4,
        region_index -> Int4,
        trophies -> Int8,
        created_at -> Int8,
    }
}

diesel::table! {
    guild_members (guild_id, user_id) {
        guild_id -> Text,
        user_id -> Uuid,
        character_id -> Uuid,
        rank -> Text,
        join_date -> Int8,
    }
}

diesel::table! {
    guild_messages (message_id) {
        message_id -> Text,
        guild_id -> Text,
        user_id -> Uuid,
        character_id -> Uuid,
        message_type -> Text,
        type_specific_data -> Jsonb,
        creation_time -> Int8,
    }
}

diesel::joinable!(characters -> users (user_id));
diesel::joinable!(quests -> characters (character_id));

diesel::allow_tables_to_appear_in_same_query!(
    characters,
    quests,
    users,
    guilds,
    guild_members,
    guild_messages,
);
